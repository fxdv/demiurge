//! Phase-aware, cost-based TCP forwarder.
//!
//! **Phase 0:** min-cost selection over phase pools (`select`, `Router::pick`).
//! **Phase 3:** RCU state snapshot, warmth discounts, fast-path override ([DEMI-WARM-DISCOUNT], [DEMI-STATE-AP]).

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use demiurge_control::{AdmitError, ReservationGuard, ReservationLedger};
use demiurge_cost::ROUTING_SHORT_CONTEXT_TOKENS;
use demiurge_cost::ROUTING_SHORT_CONTEXT_WARMTH_OVERRIDE;
use demiurge_cost::{
    compose, kv_breakdown, warmth_discount, BarrierFactor, Corrector, Cost, TimeCore,
};
use demiurge_handoff::{parse_prefill_handoff, HandoffRegistry};
use demiurge_state::default_routing_blocks;

pub use demiurge_control::{LedgerMetrics, ReservationLedger as KvReservationLedger};
pub use demiurge_handoff::{
    HandoffRegistry as KvHandoffRegistry, HandoffTransferMetrics, KvHandle,
};
pub use demiurge_state::{BackendSnapshot, StatePlane, StateSnapshot, WarmthMap};

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

/// Request phase. Prefill is compute-bound and cache-producing; decode is
/// memory-bandwidth-bound and cache-consuming.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Prefill,
    Decode,
}

/// Opaque correlation handle for disaggregated prefill → decode continuations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestId(u64);

impl RequestId {
    pub fn new() -> Self {
        Self(NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed))
    }

    pub fn raw(self) -> u64 {
        self.0
    }
}

impl Default for RequestId {
    fn default() -> Self {
        Self::new()
    }
}

/// Telemetry produced when prefill finishes; feeds decode placement.
#[derive(Debug, Clone, Copy)]
pub struct PrefillSignals {
    pub request_id: RequestId,
    pub prompt_tokens: u64,
    /// Wall time for prefill I/O including KV hand-off header receipt.
    pub prefill_wall: Duration,
}

/// Admission outcome from [`route`].
#[derive(Debug, Clone)]
pub enum RoutePath {
    /// Short context: colocated prefill+decode on one backend. [DEMI-SHORT-FASTPATH]
    Colocated(Arc<Backend>),
    /// Long (or unknown) context: async prefill, decode in [`on_prefill_complete`].
    /// [ALG-ROUTE]
    Disaggregated {
        prefill: Arc<Backend>,
        request_id: RequestId,
        prompt_tokens: u64,
    },
    /// Client declared decode phase only (`X-Demiurge-Phase: decode` or `/decode`).
    DecodeOnly(Arc<Backend>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteError {
    NoBackend,
    HandoffMissing,
    KvAdmitRejected,
}

impl std::fmt::Display for RouteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RouteError::NoBackend => write!(f, "no backend available for route"),
            RouteError::HandoffMissing => write!(f, "prefill completed without KV hand-off"),
            RouteError::KvAdmitRejected => write!(f, "decode pool rejected KV reservation"),
        }
    }
}

impl std::error::Error for RouteError {}

/// A backend instance plus its live load signal.
#[derive(Debug)]
pub struct Backend {
    pub label: String,
    pub addr: SocketAddr,
    base_service_seconds: f64,
    inflight: AtomicUsize,
}

impl Backend {
    pub fn new(label: impl Into<String>, addr: SocketAddr, base_service_seconds: f64) -> Arc<Self> {
        Arc::new(Self {
            label: label.into(),
            addr,
            base_service_seconds,
            inflight: AtomicUsize::new(0),
        })
    }

    pub fn inflight(&self) -> usize {
        self.inflight.load(Ordering::Relaxed)
    }

    pub fn incr_inflight(&self) {
        self.inflight.fetch_add(1, Ordering::Relaxed);
    }

    pub fn decr_inflight(&self) {
        self.inflight.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn cost(&self) -> Cost {
        let core = TimeCore::clamped(self.base_service_seconds);
        let queue = BarrierFactor::clamped(1.0 + self.inflight() as f64);
        compose(core, &[queue], &[], Corrector::identity())
    }

    pub fn cost_with_warmth(
        &self,
        extra: &[BarrierFactor],
        snapshot: Option<&StateSnapshot>,
        blocks: &[u64],
        decode_pool: bool,
    ) -> Cost {
        let mut discounts = Vec::new();
        if let Some(snap) = snapshot {
            let pool = if decode_pool {
                &snap.decode
            } else {
                &snap.prefill
            };
            if let Some(bs) = pool.get(&self.label) {
                if let Some(d) = warmth_discount(bs.warmth.hit_strength(blocks)) {
                    discounts.push(d);
                }
            }
        }
        let core = TimeCore::clamped(self.base_service_seconds);
        let queue = BarrierFactor::clamped(1.0 + self.inflight() as f64);
        let mut barriers = Vec::with_capacity(1 + extra.len());
        barriers.push(queue);
        barriers.extend_from_slice(extra);
        compose(core, &barriers, &discounts, Corrector::identity())
    }

    pub fn cost_with_barriers(&self, extra: &[BarrierFactor]) -> Cost {
        let core = TimeCore::clamped(self.base_service_seconds);
        let queue = BarrierFactor::clamped(1.0 + self.inflight() as f64);
        if extra.is_empty() {
            return compose(core, &[queue], &[], Corrector::identity());
        }
        let mut barriers = Vec::with_capacity(1 + extra.len());
        barriers.push(queue);
        barriers.extend_from_slice(extra);
        compose(core, &barriers, &[], Corrector::identity())
    }
}

pub fn select_with_warmth(
    candidates: &[Arc<Backend>],
    extra: &[BarrierFactor],
    snapshot: Option<&StateSnapshot>,
    blocks: &[u64],
    decode_pool: bool,
) -> Option<Arc<Backend>> {
    candidates
        .iter()
        .min_by(|a, b| {
            a.cost_with_warmth(extra, snapshot, blocks, decode_pool)
                .ln()
                .total_cmp(
                    &b.cost_with_warmth(extra, snapshot, blocks, decode_pool)
                        .ln(),
                )
        })
        .cloned()
}

/// Select minimum-cost backend, optionally with extra barriers (e.g. Φ). [DEMI-ROUTE-MINCOST]
pub fn select(candidates: &[Arc<Backend>]) -> Option<Arc<Backend>> {
    select_with_barriers(candidates, &[])
}

pub fn select_with_barriers(
    candidates: &[Arc<Backend>],
    extra: &[BarrierFactor],
) -> Option<Arc<Backend>> {
    candidates
        .iter()
        .min_by(|a, b| {
            a.cost_with_barriers(extra)
                .ln()
                .total_cmp(&b.cost_with_barriers(extra).ln())
        })
        .cloned()
}

#[derive(Clone)]
pub struct Router {
    prefill: Vec<Arc<Backend>>,
    decode: Vec<Arc<Backend>>,
    ledger: Option<Arc<ReservationLedger>>,
    handoffs: Option<Arc<HandoffRegistry>>,
    bytes_per_token: u64,
    state: Option<StateSnapshot>,
}

impl Router {
    pub fn new(prefill: Vec<Arc<Backend>>, decode: Vec<Arc<Backend>>) -> Self {
        Self {
            prefill,
            decode,
            ledger: None,
            handoffs: None,
            bytes_per_token: 128,
            state: None,
        }
    }

    /// Phase 2 router with KV reservation ledger and hand-off registry.
    pub fn with_kv_pool(
        prefill: Vec<Arc<Backend>>,
        decode: Vec<Arc<Backend>>,
        decode_capacity_bytes: u64,
        bytes_per_token: u64,
    ) -> (Self, Arc<ReservationLedger>, Arc<HandoffRegistry>) {
        let ledger = ReservationLedger::new(decode_capacity_bytes);
        let handoffs = HandoffRegistry::new();
        let router = Self {
            prefill,
            decode,
            ledger: Some(Arc::clone(&ledger)),
            handoffs: Some(Arc::clone(&handoffs)),
            bytes_per_token,
            state: None,
        };
        (router, ledger, handoffs)
    }

    pub fn with_state(mut self, state: StateSnapshot) -> Self {
        self.state = Some(state);
        self
    }

    pub fn state(&self) -> Option<&StateSnapshot> {
        self.state.as_ref()
    }

    pub fn ledger(&self) -> Option<&Arc<ReservationLedger>> {
        self.ledger.as_ref()
    }

    pub fn handoffs(&self) -> Option<&Arc<HandoffRegistry>> {
        self.handoffs.as_ref()
    }

    pub fn bytes_per_token(&self) -> u64 {
        self.bytes_per_token
    }

    pub fn pool(&self, phase: Phase) -> &[Arc<Backend>] {
        match phase {
            Phase::Prefill => &self.prefill,
            Phase::Decode => &self.decode,
        }
    }

    pub fn pick(&self, phase: Phase) -> Option<Arc<Backend>> {
        self.pick_with_phi(phase, None)
    }

    pub fn pick_with_phi(&self, phase: Phase, phi: Option<BarrierFactor>) -> Option<Arc<Backend>> {
        let extra: [BarrierFactor; 1] = [phi.unwrap_or(BarrierFactor::clamped(1.0))];
        let barriers = if phi.is_some() { &extra[..] } else { &[] };
        match phase {
            Phase::Prefill => select(self.pool(Phase::Prefill)),
            Phase::Decode => select_with_barriers(self.pool(Phase::Decode), barriers),
        }
    }

    /// Colocated fast path uses the prefill pool (single hop prefill+decode).
    pub fn pick_colocated(&self) -> Option<Arc<Backend>> {
        self.pick(Phase::Prefill)
    }
}

pub fn parse_pool(spec: &str) -> Result<Vec<Arc<Backend>>, String> {
    let mut out = Vec::new();
    for item in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let parts: Vec<&str> = item.split('@').collect();
        if parts.len() != 3 {
            return Err(format!(
                "bad backend spec {item:?}; want label@host:port@seconds"
            ));
        }
        let addr: SocketAddr = parts[1]
            .parse()
            .map_err(|e| format!("bad address {:?}: {e}", parts[1]))?;
        let secs: f64 = parts[2]
            .parse()
            .map_err(|e| format!("bad seconds {:?}: {e}", parts[2]))?;
        out.push(Backend::new(parts[0], addr, secs));
    }
    Ok(out)
}

const MAX_HEAD: usize = 64 * 1024;

/// Parse `X-Demiurge-Tokens: N` from the request head.
pub fn parse_prompt_tokens(head: &[u8]) -> Option<u64> {
    let text = String::from_utf8_lossy(head);
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("x-demiurge-tokens:") {
            if let Ok(n) = rest.trim().parse::<u64>() {
                return Some(n);
            }
        }
    }
    None
}

/// Parse token count from `/prefill/<n>` or `/long/<n>` path segments.
pub fn parse_path_tokens(head: &[u8]) -> Option<u64> {
    let first = head.split(|&b| b == b'\r' || b == b'\n').next()?;
    let first = std::str::from_utf8(first).ok()?;
    let path = first.split_whitespace().nth(1)?;
    for prefix in ["/prefill/", "/long/"] {
        if let Some(rest) = path.strip_prefix(prefix) {
            let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(n) = num.parse() {
                return Some(n);
            }
        }
    }
    None
}

/// Estimate prompt tokens for admission. Unknown prompts default to above the
/// fast-path threshold so we never colocate a long unknown request.
pub fn estimate_prompt_tokens(head: &[u8]) -> u64 {
    parse_prompt_tokens(head)
        .or_else(|| parse_path_tokens(head))
        .unwrap_or(ROUTING_SHORT_CONTEXT_TOKENS + 1)
}

/// True when the client declared decode-only routing.
pub fn is_decode_only(head: &[u8]) -> bool {
    let text = String::from_utf8_lossy(head).to_ascii_lowercase();
    text.contains("x-demiurge-phase: decode")
        || text.lines().next().is_some_and(|l| l.contains(" /decode"))
}

/// Classify admission path from the HTTP head. [ALG-ROUTE] [DEMI-SHORT-FASTPATH] [DEMI-WARM-DISCOUNT]
pub fn route(router: &Router, head: &[u8]) -> Result<RoutePath, RouteError> {
    if is_decode_only(head) {
        return router
            .pick(Phase::Decode)
            .map(RoutePath::DecodeOnly)
            .ok_or(RouteError::NoBackend);
    }

    let prompt_tokens = estimate_prompt_tokens(head);
    if prompt_tokens <= ROUTING_SHORT_CONTEXT_TOKENS {
        if let Some((prefill, _strength)) = warmth_override_target(router, prompt_tokens) {
            return Ok(RoutePath::Disaggregated {
                prefill,
                request_id: RequestId::new(),
                prompt_tokens,
            });
        }
        return router
            .pick_colocated()
            .map(RoutePath::Colocated)
            .ok_or(RouteError::NoBackend);
    }

    let blocks = default_routing_blocks(prompt_tokens);
    let prefill = select_with_warmth(
        router.pool(Phase::Prefill),
        &[],
        router.state.as_ref(),
        &blocks,
        false,
    )
    .ok_or(RouteError::NoBackend)?;
    Ok(RoutePath::Disaggregated {
        prefill,
        request_id: RequestId::new(),
        prompt_tokens,
    })
}

fn warmth_override_target(router: &Router, prompt_tokens: u64) -> Option<(Arc<Backend>, f64)> {
    let snap = router.state.as_ref()?;
    let blocks = default_routing_blocks(prompt_tokens);
    let mut best: Option<(Arc<Backend>, f64)> = None;
    for backend in router.pool(Phase::Prefill) {
        let warmth = snap.prefill.get(&backend.label)?;
        let strength = warmth.warmth.hit_strength(&blocks);
        if strength < ROUTING_SHORT_CONTEXT_WARMTH_OVERRIDE {
            continue;
        }
        if best.as_ref().is_none_or(|(_, s)| strength > *s) {
            best = Some((Arc::clone(backend), strength));
        }
    }
    best
}

/// Decode placement after prefill; requires valid hand-off when KV pool is wired.
/// [ALG-ROUTE] [DEMI-KV-HANDOFF]
pub struct DecodePlacement {
    pub backend: Arc<Backend>,
    reservation: Option<ReservationGuard>,
}

impl DecodePlacement {
    pub fn backend(&self) -> &Arc<Backend> {
        &self.backend
    }
}

pub fn on_prefill_complete(
    router: &Router,
    signals: &PrefillSignals,
    prefill_response: &[u8],
    prefill_label: &str,
) -> Result<DecodePlacement, RouteError> {
    let (handoff, reservation) = match (&router.ledger, &router.handoffs) {
        (Some(ledger), Some(_handoffs)) => {
            let handoff =
                parse_prefill_handoff(prefill_response, signals.request_id.raw(), prefill_label)
                    .filter(|h| h.is_valid())
                    .ok_or(RouteError::HandoffMissing)?;

            let expected = kv_breakdown(signals.prompt_tokens, router.bytes_per_token).kv_reserved;
            if handoff.byte_len < expected {
                return Err(RouteError::HandoffMissing);
            }

            let reservation = ledger
                .try_reserve(handoff.request_id, handoff.byte_len)
                .map_err(|e| match e {
                    AdmitError::OverCapacity | AdmitError::DuplicateRequest => {
                        RouteError::KvAdmitRejected
                    }
                })?;

            (Some(handoff), Some(reservation))
        }
        _ => (None, None),
    };

    let phi = router
        .ledger
        .as_ref()
        .map(|l| l.phi_barrier())
        .filter(|b| b.get() > 1.0);
    let extra: Vec<BarrierFactor> = phi.into_iter().collect();
    let blocks = default_routing_blocks(signals.prompt_tokens);

    let backend = select_with_warmth(
        router.pool(Phase::Decode),
        &extra,
        router.state.as_ref(),
        &blocks,
        true,
    )
    .or_else(|| router.pick_with_phi(Phase::Decode, phi))
    .or_else(|| router.pick_colocated())
    .ok_or(RouteError::NoBackend)?;

    if let Some(h) = handoff {
        if let Some(reg) = &router.handoffs {
            reg.publish(h.clone());
            reg.record_transfer(h.byte_len, signals.prefill_wall);
        }
    }

    Ok(DecodePlacement {
        backend,
        reservation,
    })
}

/// Classify and spawn async prefill; return before prefill I/O completes.
/// [ALG-ROUTE]
pub fn admit_disaggregated(router: &Router, head: &[u8]) -> Result<Duration, RouteError> {
    let start = std::time::Instant::now();
    match route(router, head)? {
        RoutePath::Disaggregated {
            prefill,
            request_id,
            prompt_tokens,
        } => {
            let _worker =
                dispatch_prefill(prefill, head.to_vec(), request_id, prompt_tokens, |_, _| {});
        }
        RoutePath::Colocated(_) | RoutePath::DecodeOnly(_) => {
            return Err(RouteError::NoBackend);
        }
    }
    Ok(start.elapsed())
}

/// Dispatch prefill I/O on a worker thread; invoke `on_complete` when done.
pub fn dispatch_prefill(
    prefill: Arc<Backend>,
    head: Vec<u8>,
    request_id: RequestId,
    prompt_tokens: u64,
    on_complete: impl FnOnce(PrefillSignals, Vec<u8>) + Send + 'static,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let started = std::time::Instant::now();
        let response = run_prefill_io(&prefill, &head).unwrap_or_default();
        let prefill_wall = started.elapsed();
        on_complete(
            PrefillSignals {
                request_id,
                prompt_tokens,
                prefill_wall,
            },
            response,
        );
    })
}

fn run_prefill_io(prefill: &Backend, head: &[u8]) -> io::Result<Vec<u8>> {
    let mut upstream = TcpStream::connect(prefill.addr)?;
    upstream.write_all(head)?;
    upstream.shutdown(Shutdown::Write)?;
    let mut resp = Vec::new();
    upstream.read_to_end(&mut resp)?;
    Ok(resp)
}

fn read_head(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];
    while stream.read(&mut byte)? == 1 {
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") || buf.len() >= MAX_HEAD {
            break;
        }
    }
    Ok(buf)
}

struct InflightGuard<'a>(&'a Backend);

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        self.0.decr_inflight();
    }
}

fn proxy_to_backend(client: &mut TcpStream, head: &[u8], backend: &Backend) -> io::Result<()> {
    backend.incr_inflight();
    let _guard = InflightGuard(backend);

    let mut upstream = TcpStream::connect(backend.addr)?;
    upstream.write_all(head)?;

    let mut up_read = upstream.try_clone()?;
    let mut client_write = client.try_clone()?;
    let pump = thread::spawn(move || {
        let _ = io::copy(&mut up_read, &mut client_write);
        let _ = client_write.shutdown(Shutdown::Write);
    });
    let mut client_read = client.try_clone()?;
    let _ = io::copy(&mut client_read, &mut upstream);
    let _ = upstream.shutdown(Shutdown::Write);
    let _ = pump.join();
    Ok(())
}

fn handle_disaggregated(
    mut client: TcpStream,
    head: Vec<u8>,
    router: Arc<Router>,
    prefill: Arc<Backend>,
    request_id: RequestId,
    prompt_tokens: u64,
) -> io::Result<()> {
    let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
    let router2 = Arc::clone(&router);
    let prefill_label = prefill.label.clone();
    let _prefill_worker = dispatch_prefill(
        prefill,
        head.clone(),
        request_id,
        prompt_tokens,
        move |signals, response| {
            let placement = match on_prefill_complete(&router2, &signals, &response, &prefill_label)
            {
                Ok(p) => p,
                Err(e) => {
                    let _ = done_tx.send(Err(e));
                    return;
                }
            };
            let _ = done_tx.send(Ok(placement));
        },
    );

    let placement = match done_rx
        .recv()
        .map_err(|_| io::Error::other("prefill channel"))?
    {
        Ok(p) => p,
        Err(RouteError::NoBackend | RouteError::HandoffMissing | RouteError::KvAdmitRejected) => {
            let _ =
                client.write_all(b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\r\n");
            return Ok(());
        }
    };

    let result = proxy_to_backend(&mut client, &head, placement.backend.as_ref());
    drop(placement.reservation);
    result
}

fn handle_conn(client: TcpStream, router: Arc<Router>) -> io::Result<()> {
    let mut client = client;
    let head = read_head(&mut client)?;

    match route(&router, &head) {
        Ok(RoutePath::Colocated(b) | RoutePath::DecodeOnly(b)) => {
            proxy_to_backend(&mut client, &head, b.as_ref())
        }
        Ok(RoutePath::Disaggregated {
            prefill,
            request_id,
            prompt_tokens,
        }) => handle_disaggregated(client, head, router, prefill, request_id, prompt_tokens),
        Err(RouteError::NoBackend)
        | Err(RouteError::HandoffMissing)
        | Err(RouteError::KvAdmitRejected) => {
            let _ =
                client.write_all(b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\r\n");
            Ok(())
        }
    }
}

pub fn serve(listener: TcpListener, router: Arc<Router>) -> io::Result<()> {
    for conn in listener.incoming() {
        let Ok(client) = conn else { continue };
        let router = Arc::clone(&router);
        thread::spawn(move || {
            let _ = handle_conn(client, router);
        });
    }
    Ok(())
}

/// Test helper: prefill backend that blocks until `release()` is called.
pub fn spawn_latch_prefill_backend() -> (SocketAddr, LatchBackend) {
    let latch = Arc::new((Mutex::new(false), Condvar::new()));
    let latch2 = Arc::clone(&latch);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut s) = conn else { continue };
            let mut buf = [0u8; 2048];
            let _ = s.read(&mut buf);
            let (lock, cv) = &*latch2;
            let mut started = lock.lock().expect("lock");
            while !*started {
                started = cv.wait(started).expect("wait");
            }
            let _ = s.write_all(
                b"HTTP/1.1 200 OK\r\nx-demiurge-prefill-done: 1\r\nx-demiurge-kv-handle: 1\r\nx-demiurge-kv-bytes: 4096\r\ncontent-length: 0\r\n\r\n",
            );
            let _ = s.shutdown(Shutdown::Write);
        }
    });
    (addr, LatchBackend { latch })
}

pub struct LatchBackend {
    latch: Arc<(Mutex<bool>, Condvar)>,
}

impl LatchBackend {
    pub fn release(&self) {
        let (lock, cv) = &*self.latch;
        *lock.lock().expect("lock") = true;
        cv.notify_all();
    }
}

/// Test helper: marker backend for E2E proxy tests.
pub fn spawn_marker_backend(marker: u8) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut s) = conn else { continue };
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf);
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: 1\r\n\r\n{}",
                marker as char
            );
            let _ = s.write_all(resp.as_bytes());
            let _ = s.shutdown(Shutdown::Write);
        }
    });
    addr
}

/// Sleep backend for timing tests.
pub fn spawn_delay_backend(delay: Duration) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut s) = conn else { continue };
            let mut buf = [0u8; 2048];
            let _ = s.read(&mut buf);
            thread::sleep(delay);
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n");
            let _ = s.shutdown(Shutdown::Write);
        }
    });
    addr
}
