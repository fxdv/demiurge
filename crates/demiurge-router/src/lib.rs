//! Minimal phase-aware, cost-based TCP forwarder.
//!
//! This is the smallest real thing that earns the word "router": it accepts a
//! connection, classifies it into a prefill or decode pool, selects the
//! minimum-cost backend in that pool using `demiurge-cost`, and proxies bytes
//! to it. It is deliberately *not* the full design in `spec/` — there is no
//! XDP, RDMA, gossip, KV hand-off, or asynchronous prefill dispatch; those
//! remain design intent. This is the load-bearing core the rest grows around.

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

use demiurge_cost::{compose, BarrierFactor, Corrector, Cost, TimeCore};

/// Request phase. Prefill is compute-bound and cache-producing; decode is
/// memory-bandwidth-bound and cache-consuming, so they are scheduled in
/// separate pools.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Prefill,
    Decode,
}

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

    /// Mark a request as dispatched to this backend (raises its load signal).
    pub fn incr_inflight(&self) {
        self.inflight.fetch_add(1, Ordering::Relaxed);
    }

    /// Mark a dispatched request as finished.
    pub fn decr_inflight(&self) {
        self.inflight.fetch_sub(1, Ordering::Relaxed);
    }

    /// Live cost estimate: configured base service time, penalized by a queueing
    /// barrier that grows with in-flight requests. Fail-expensive clamping means
    /// a broken signal can never make this backend look artificially cheap.
    pub fn cost(&self) -> Cost {
        let core = TimeCore::clamped(self.base_service_seconds);
        let queue = BarrierFactor::clamped(1.0 + self.inflight() as f64);
        compose(core, &[queue], &[], Corrector::identity())
    }
}

/// Select the minimum-cost backend from a candidate set (spec §8.1).
/// [DEMI-ROUTE-MINCOST]
pub fn select(candidates: &[Arc<Backend>]) -> Option<Arc<Backend>> {
    candidates
        .iter()
        .min_by(|a, b| a.cost().ln().total_cmp(&b.cost().ln()))
        .cloned()
}

/// Two phase-keyed pools of backends.
#[derive(Clone, Default)]
pub struct Router {
    prefill: Vec<Arc<Backend>>,
    decode: Vec<Arc<Backend>>,
}

impl Router {
    pub fn new(prefill: Vec<Arc<Backend>>, decode: Vec<Arc<Backend>>) -> Self {
        Self { prefill, decode }
    }

    pub fn pool(&self, phase: Phase) -> &[Arc<Backend>] {
        match phase {
            Phase::Prefill => &self.prefill,
            Phase::Decode => &self.decode,
        }
    }

    /// Minimum-cost backend for the given phase. [DEMI-ROUTE-MINCOST]
    pub fn pick(&self, phase: Phase) -> Option<Arc<Backend>> {
        select(self.pool(phase))
    }
}

/// Parse a pool spec of the form `label@host:port@seconds` items separated by
/// commas, e.g. `p0@127.0.0.1:9001@0.05,p1@127.0.0.1:9002@0.05`.
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

/// Classify a request by its head: an `X-Demiurge-Phase: decode` header or a
/// `/decode` path routes to the decode pool; everything else is prefill.
fn classify(head: &[u8]) -> Phase {
    let text = String::from_utf8_lossy(head).to_ascii_lowercase();
    let decode_hdr = text.contains("x-demiurge-phase: decode");
    let decode_path = text.lines().next().is_some_and(|l| l.contains(" /decode"));
    if decode_hdr || decode_path {
        Phase::Decode
    } else {
        Phase::Prefill
    }
}

/// Read the HTTP request head (through the blank line) so we can classify it
/// before choosing a backend.
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

/// Decrement a backend's in-flight counter when the connection ends.
struct InflightGuard<'a>(&'a Backend);

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        self.0.decr_inflight();
    }
}

fn handle_conn(mut client: TcpStream, router: &Router) -> io::Result<()> {
    let head = read_head(&mut client)?;
    let phase = classify(&head);

    let backend = match router.pick(phase) {
        Some(b) => b,
        None => {
            let _ =
                client.write_all(b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\r\n");
            return Ok(());
        }
    };

    backend.incr_inflight();
    let _guard = InflightGuard(backend.as_ref());

    let mut upstream = TcpStream::connect(backend.addr)?;
    upstream.write_all(&head)?;

    // Full-duplex pump: upstream -> client on a helper thread, client ->
    // upstream on this one.
    let mut up_read = upstream.try_clone()?;
    let mut client_write = client.try_clone()?;
    let pump = thread::spawn(move || {
        let _ = io::copy(&mut up_read, &mut client_write);
        let _ = client_write.shutdown(Shutdown::Write);
    });
    let _ = io::copy(&mut client, &mut upstream);
    let _ = upstream.shutdown(Shutdown::Write);
    let _ = pump.join();
    Ok(())
}

/// Accept connections forever, forwarding each to its phase's minimum-cost
/// backend. [DEMI-ROUTE-MINCOST]
pub fn serve(listener: TcpListener, router: Arc<Router>) -> io::Result<()> {
    for conn in listener.incoming() {
        let client = match conn {
            Ok(c) => c,
            Err(_) => continue,
        };
        let router = Arc::clone(&router);
        thread::spawn(move || {
            let _ = handle_conn(client, &router);
        });
    }
    Ok(())
}
