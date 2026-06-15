//! Phase-aware, cost-based TCP forwarder.
//!
//! **Phase 0:** min-cost selection over phase pools (`select`, `Router::pick`).
//! **Phase 3:** RCU state snapshot, warmth discounts, fast-path override ([DEMI-WARM-DISCOUNT], [DEMI-STATE-AP]).
//! **Phase 4:** Greedy pf→dc pairing on the disaggregated path ([DEMI-PAIR-GREEDY]).
//! **Phase 5:** RCU routing table + admit shed on the live TCP path ([DEMI-DP-RCU], [DEMI-XDP-SHED]).

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use demiurge_control::{
    export_pool_pressure, pairing_regret_targets, AdmitError, CorrectorShadowLog,
    CorrectorShadowSample, LengthPredictor, PairingTarget, PoolPressure, PoolRebalancer,
    RebalancerMode, ReservationGuard, ReservationLedger,
};
use demiurge_cost::ROUTING_SHORT_CONTEXT_TOKENS;
use demiurge_cost::ROUTING_SHORT_CONTEXT_WARMTH_OVERRIDE;
use demiurge_cost::ROUTING_TRANSFER_PENALTY;
use demiurge_cost::{
    compose, kv_breakdown, warmth_discount, BarrierFactor, Corrector, Cost, TimeCore,
    DATAPLANE_ADMIT_BURST, DATAPLANE_RCU_HEARTBEAT_MS, DATAPLANE_RCU_STALE_ALERT_MS,
    POOL_ACTUATION_ENABLED,
};
use demiurge_dataplane::{admit_capacity_for_pi, AdmitBucket, DataPlaneSnapshot};
use demiurge_handoff::{
    parse_prefill_handoff, HandoffRegistry, HandoffTransport, HeaderPassthroughTransport,
};
use demiurge_state::default_routing_blocks;

pub use demiurge_control::{LedgerMetrics, ReservationLedger as KvReservationLedger};
#[cfg(target_os = "linux")]
pub use demiurge_dataplane::IoUringProxySession;
pub use demiurge_dataplane::{
    pool_core_scale, AdmitMode, DataPlaneSnapshot as RcuDataPlaneSnapshot, ForwardDecision,
    IoUringForwarder, RcuRoutingTable, XdpAdmitShed, XdpAttachError,
};
pub use demiurge_handoff::{
    HandoffRegistry as KvHandoffRegistry, HandoffTransferMetrics, KvHandle,
};
pub use demiurge_state::{BackendSnapshot, StatePlane, StateSnapshot, WarmthMap};

mod banner;

pub use banner::print_startup_banner;

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
    /// ln(base_service_seconds) — valid bases only; invalid inputs fail-expensive at construction.
    ln_base: f64,
    inflight: AtomicUsize,
}

#[inline]
fn queue_ln(inflight: usize) -> f64 {
    (1.0 + inflight as f64).ln()
}

impl Backend {
    pub fn new(label: impl Into<String>, addr: SocketAddr, base_service_seconds: f64) -> Arc<Self> {
        let ln_base = if base_service_seconds.is_finite() && base_service_seconds > 0.0 {
            base_service_seconds.ln()
        } else {
            f64::MAX.ln()
        };
        Arc::new(Self {
            label: label.into(),
            addr,
            base_service_seconds,
            ln_base,
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
        Cost::from_ln(self.ln_base + queue_ln(self.inflight()))
    }

    #[inline]
    fn selection_ln(&self, pool_pi: f64, prefill: bool) -> f64 {
        self.ln_base + pool_core_scale(1.0, pool_pi, prefill).ln() + queue_ln(self.inflight())
    }

    pub fn cost_with_warmth(
        &self,
        extra: &[BarrierFactor],
        snapshot: Option<&StateSnapshot>,
        blocks: &[u64],
        decode_pool: bool,
    ) -> Cost {
        self.cost_with_warmth_pi(extra, snapshot, blocks, decode_pool, 0.5)
    }

    pub fn cost_with_warmth_pi(
        &self,
        extra: &[BarrierFactor],
        snapshot: Option<&StateSnapshot>,
        blocks: &[u64],
        decode_pool: bool,
        pool_pi: f64,
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
        if extra.is_empty() && discounts.is_empty() {
            return Cost::from_ln(self.selection_ln(pool_pi, !decode_pool));
        }
        let scaled = pool_core_scale(self.base_service_seconds, pool_pi, !decode_pool);
        let core = TimeCore::clamped(scaled);
        let queue = BarrierFactor::clamped(1.0 + self.inflight() as f64);
        let mut barriers = Vec::with_capacity(1 + extra.len());
        barriers.push(queue);
        barriers.extend_from_slice(extra);
        compose(core, &barriers, &discounts, Corrector::identity())
    }

    pub fn cost_with_barriers(&self, extra: &[BarrierFactor]) -> Cost {
        self.cost_with_barriers_pi(extra, 0.5, true)
    }

    pub fn cost_with_barriers_pi(
        &self,
        extra: &[BarrierFactor],
        pool_pi: f64,
        prefill: bool,
    ) -> Cost {
        if extra.is_empty() {
            return Cost::from_ln(self.selection_ln(pool_pi, prefill));
        }
        let scaled = pool_core_scale(self.base_service_seconds, pool_pi, prefill);
        let core = TimeCore::clamped(scaled);
        let queue = BarrierFactor::clamped(1.0 + self.inflight() as f64);
        let mut barriers = Vec::with_capacity(1 + extra.len());
        barriers.push(queue);
        barriers.extend_from_slice(extra);
        compose(core, &barriers, &[], Corrector::identity())
    }
}

impl PairingTarget for Backend {
    fn label(&self) -> &str {
        &self.label
    }

    fn prefill_ln(&self, snapshot: Option<&StateSnapshot>, blocks: &[u64]) -> f64 {
        self.cost_with_warmth(&[], snapshot, blocks, false).ln()
    }

    fn decode_ln(
        &self,
        snapshot: Option<&StateSnapshot>,
        blocks: &[u64],
        prefill_label: &str,
        transfer_penalty: f64,
        extra_barriers: &[BarrierFactor],
    ) -> f64 {
        let mut barriers = extra_barriers.to_vec();
        if prefill_label != self.label {
            barriers.push(BarrierFactor::clamped(transfer_penalty.max(1.0)));
        }
        self.cost_with_warmth(&barriers, snapshot, blocks, true)
            .ln()
    }
}

fn select_prefill_with_pi(
    candidates: &[Arc<Backend>],
    snapshot: Option<&StateSnapshot>,
    prompt_tokens: u64,
    pool_pi: f64,
) -> Option<Arc<Backend>> {
    if snapshot.is_none() {
        return select_with_barriers_pi(candidates, &[], pool_pi, true);
    }
    let blocks = default_routing_blocks(prompt_tokens);
    candidates
        .iter()
        .min_by(|a, b| {
            a.cost_with_warmth_pi(&[], snapshot, &blocks, false, pool_pi)
                .ln()
                .total_cmp(
                    &b.cost_with_warmth_pi(&[], snapshot, &blocks, false, pool_pi)
                        .ln(),
                )
        })
        .cloned()
}

fn select_decode_with_pi(
    prefill_label: &str,
    candidates: &[Arc<Backend>],
    snapshot: Option<&StateSnapshot>,
    prompt_tokens: u64,
    transfer_penalty: f64,
    extra_barriers: &[BarrierFactor],
    pool_pi: f64,
) -> Option<Arc<Backend>> {
    let blocks = default_routing_blocks(prompt_tokens);
    candidates
        .iter()
        .min_by(|a, b| {
            let mut ba = extra_barriers.to_vec();
            let mut bb = extra_barriers.to_vec();
            if prefill_label != a.label {
                ba.push(BarrierFactor::clamped(transfer_penalty.max(1.0)));
            }
            if prefill_label != b.label {
                bb.push(BarrierFactor::clamped(transfer_penalty.max(1.0)));
            }
            a.cost_with_warmth_pi(&ba, snapshot, &blocks, true, pool_pi)
                .ln()
                .total_cmp(
                    &b.cost_with_warmth_pi(&bb, snapshot, &blocks, true, pool_pi)
                        .ln(),
                )
        })
        .cloned()
}

pub fn select_with_warmth_pi(
    candidates: &[Arc<Backend>],
    extra: &[BarrierFactor],
    snapshot: Option<&StateSnapshot>,
    blocks: &[u64],
    decode_pool: bool,
    pool_pi: f64,
) -> Option<Arc<Backend>> {
    candidates
        .iter()
        .min_by(|a, b| {
            a.cost_with_warmth_pi(extra, snapshot, blocks, decode_pool, pool_pi)
                .ln()
                .total_cmp(
                    &b.cost_with_warmth_pi(extra, snapshot, blocks, decode_pool, pool_pi)
                        .ln(),
                )
        })
        .cloned()
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

pub fn select_with_barriers_pi(
    candidates: &[Arc<Backend>],
    extra: &[BarrierFactor],
    pool_pi: f64,
    prefill: bool,
) -> Option<Arc<Backend>> {
    if extra.is_empty() {
        let ln_scale = pool_core_scale(1.0, pool_pi, prefill).ln();
        return candidates
            .iter()
            .min_by(|a, b| {
                let la = a.ln_base + ln_scale + queue_ln(a.inflight());
                let lb = b.ln_base + ln_scale + queue_ln(b.inflight());
                la.total_cmp(&lb)
            })
            .cloned();
    }
    candidates
        .iter()
        .min_by(|a, b| {
            a.cost_with_barriers_pi(extra, pool_pi, prefill)
                .ln()
                .total_cmp(&b.cost_with_barriers_pi(extra, pool_pi, prefill).ln())
        })
        .cloned()
}

pub fn select_with_barriers(
    candidates: &[Arc<Backend>],
    extra: &[BarrierFactor],
) -> Option<Arc<Backend>> {
    select_with_barriers_pi(candidates, extra, 0.5, true)
}

pub struct Router {
    prefill: Vec<Arc<Backend>>,
    decode: Vec<Arc<Backend>>,
    ledger: Option<Arc<ReservationLedger>>,
    handoffs: Option<Arc<HandoffRegistry>>,
    bytes_per_token: u64,
    state: Option<StateSnapshot>,
    control: Arc<Mutex<ControlPlane>>,
    dataplane: Arc<RcuRoutingTable>,
    admit: Arc<AdmitBucket>,
    admit_mode: AdmitMode,
    kernel_admit: Arc<Mutex<Option<XdpAdmitShed>>>,
    last_admit_capacity: AtomicU64,
    io_uring: Option<IoUringForwarder>,
    rebalancer_actuation: bool,
    colocated_routes: AtomicU64,
    disagg_routes: AtomicU64,
    handoff_transport: Option<Arc<dyn HandoffTransport>>,
}

impl Clone for Router {
    fn clone(&self) -> Self {
        Self {
            prefill: self.prefill.clone(),
            decode: self.decode.clone(),
            ledger: self.ledger.clone(),
            handoffs: self.handoffs.clone(),
            bytes_per_token: self.bytes_per_token,
            state: self.state.clone(),
            control: Arc::clone(&self.control),
            dataplane: Arc::clone(&self.dataplane),
            admit: Arc::clone(&self.admit),
            admit_mode: self.admit_mode,
            kernel_admit: Arc::clone(&self.kernel_admit),
            last_admit_capacity: AtomicU64::new(self.last_admit_capacity.load(Ordering::Relaxed)),
            io_uring: self.io_uring.clone(),
            rebalancer_actuation: self.rebalancer_actuation,
            colocated_routes: AtomicU64::new(self.colocated_routes.load(Ordering::Relaxed)),
            disagg_routes: AtomicU64::new(self.disagg_routes.load(Ordering::Relaxed)),
            handoff_transport: self.handoff_transport.clone(),
        }
    }
}

/// Shadow control-plane telemetry (rebalancer π*, pairing regret, length predictor,
/// fast-path ratio, corrector shadow). [DEMI-FASTPATH-TELEM]
#[derive(Debug, Clone, Copy)]
pub struct ControlMetrics {
    pub pi: f64,
    pub pi_star: f64,
    pub pairing_regret_mean: f64,
    pub pairing_regret_samples: u64,
    pub predictor_p90_tokens: u64,
    pub dataplane_pi: f64,
    pub dataplane_age_ms: u64,
    pub rcu_stale: bool,
    pub rcu_stale_alert_ms: u64,
    pub admit_shed_total: u64,
    /// Fraction of routes on the short-context colocated path. [DEMI-FASTPATH-TELEM]
    pub fast_path_ratio: f64,
    pub colocated_routes: u64,
    pub disagg_routes: u64,
    /// Near-threshold colocated admits (shadow regret telemetry).
    pub fastpath_misroute_mean: f64,
    pub fastpath_misroute_samples: u64,
    pub corrector_shadow_samples: u64,
}

#[derive(Debug)]
struct ControlPlane {
    rebalancer: PoolRebalancer,
    predictor: LengthPredictor,
    regret_sum: f64,
    regret_samples: u64,
    last_pi_star: f64,
    corrector_shadow: CorrectorShadowLog,
    fastpath_misroute_sum: f64,
    fastpath_misroute_samples: u64,
}

impl Default for ControlPlane {
    fn default() -> Self {
        Self {
            rebalancer: PoolRebalancer::new(RebalancerMode::Shadow),
            predictor: LengthPredictor::default(),
            regret_sum: 0.0,
            regret_samples: 0,
            last_pi_star: 0.5,
            corrector_shadow: CorrectorShadowLog::new(4096),
            fastpath_misroute_sum: 0.0,
            fastpath_misroute_samples: 0,
        }
    }
}

impl Router {
    fn fresh_control() -> Arc<Mutex<ControlPlane>> {
        Arc::new(Mutex::new(ControlPlane::default()))
    }

    pub fn new(prefill: Vec<Arc<Backend>>, decode: Vec<Arc<Backend>>) -> Self {
        let dataplane = RcuRoutingTable::new(0.5);
        Self {
            prefill,
            decode,
            ledger: None,
            handoffs: None,
            bytes_per_token: 128,
            state: None,
            control: Self::fresh_control(),
            dataplane: Arc::clone(&dataplane),
            admit: Arc::new(AdmitBucket::new(DATAPLANE_ADMIT_BURST)),
            admit_mode: AdmitMode::from_env(),
            kernel_admit: Arc::new(Mutex::new(None)),
            last_admit_capacity: AtomicU64::new(DATAPLANE_ADMIT_BURST),
            io_uring: Self::io_uring_from_env(&dataplane),
            rebalancer_actuation: rebalancer_actuation_enabled(),
            colocated_routes: AtomicU64::new(0),
            disagg_routes: AtomicU64::new(0),
            handoff_transport: None,
        }
    }

    fn io_uring_from_env(dataplane: &Arc<RcuRoutingTable>) -> Option<IoUringForwarder> {
        if IoUringForwarder::io_uring_enabled_from_env() {
            Some(IoUringForwarder::from_router_dataplane(dataplane))
        } else {
            None
        }
    }

    fn default_handoff_transport() -> Arc<dyn HandoffTransport> {
        Arc::new(HeaderPassthroughTransport)
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
        let dataplane = RcuRoutingTable::new(0.5);
        let router = Self {
            prefill,
            decode,
            ledger: Some(Arc::clone(&ledger)),
            handoffs: Some(Arc::clone(&handoffs)),
            bytes_per_token,
            state: None,
            control: Self::fresh_control(),
            dataplane: Arc::clone(&dataplane),
            admit: Arc::new(AdmitBucket::new(DATAPLANE_ADMIT_BURST)),
            admit_mode: AdmitMode::from_env(),
            kernel_admit: Arc::new(Mutex::new(None)),
            last_admit_capacity: AtomicU64::new(DATAPLANE_ADMIT_BURST),
            io_uring: Self::io_uring_from_env(&dataplane),
            rebalancer_actuation: rebalancer_actuation_enabled(),
            colocated_routes: AtomicU64::new(0),
            disagg_routes: AtomicU64::new(0),
            handoff_transport: Some(Self::default_handoff_transport()),
        };
        (router, ledger, handoffs)
    }

    pub fn with_state(mut self, state: StateSnapshot) -> Self {
        self.state = Some(state);
        self
    }

    pub fn with_rebalancer_actuation(mut self, enabled: bool) -> Self {
        self.rebalancer_actuation = enabled;
        if enabled {
            let mut cp = self.control.lock().expect("control plane");
            cp.rebalancer = PoolRebalancer::new(RebalancerMode::CanActuate);
        }
        self
    }

    pub fn with_handoff_transport(mut self, transport: Arc<dyn HandoffTransport>) -> Self {
        self.handoff_transport = Some(transport);
        self
    }

    pub fn with_admit_mode(mut self, mode: AdmitMode) -> Self {
        self.admit_mode = mode;
        self
    }

    /// Attach kernel XDP admit-shed on `iface` (Linux + built BPF object).
    pub fn with_kernel_admit(mut self, iface: &str) -> Result<Self, XdpAttachError> {
        if !self.admit_mode.wants_kernel() {
            self.admit_mode = AdmitMode::Hybrid;
        }
        let cap = admit_capacity_for_pi(DATAPLANE_ADMIT_BURST, self.dataplane.read_pi());
        let shed = XdpAdmitShed::attach(iface, cap)?;
        *self.kernel_admit.lock().expect("kernel admit") = Some(shed);
        self.sync_admit_capacity(cap);
        Ok(self)
    }

    pub fn with_io_uring(mut self, enabled: bool) -> Self {
        self.io_uring = if enabled {
            Some(IoUringForwarder::from_router_dataplane(&self.dataplane))
        } else {
            None
        };
        self
    }

    pub fn admit_mode(&self) -> AdmitMode {
        self.admit_mode
    }

    pub fn kernel_admit_attached(&self) -> bool {
        self.kernel_admit.lock().expect("kernel admit").is_some()
    }

    pub fn io_uring_enabled(&self) -> bool {
        self.io_uring.is_some()
    }

    pub fn sync_admit_capacity(&self, capacity: u64) {
        let cap = capacity.max(1);
        self.admit.reseed(cap);
        if let Ok(mut guard) = self.kernel_admit.lock() {
            if let Some(shed) = guard.as_mut() {
                let _ = shed.reseed(cap);
            }
        }
    }

    fn maybe_sync_admit_for_pi(&self, pi: f64) {
        let cap = admit_capacity_for_pi(DATAPLANE_ADMIT_BURST, pi);
        let prev = self.last_admit_capacity.load(Ordering::Relaxed);
        if cap != prev {
            self.last_admit_capacity.store(cap, Ordering::Relaxed);
            self.sync_admit_capacity(cap);
        }
    }

    fn total_admit_shed(&self) -> u64 {
        let userspace = self.admit.shed_total();
        let kernel = self
            .kernel_admit
            .lock()
            .expect("kernel admit")
            .as_ref()
            .and_then(|s| s.shed_total().ok())
            .unwrap_or(0);
        userspace.saturating_add(kernel)
    }

    pub fn rebalancer_actuation(&self) -> bool {
        self.rebalancer_actuation
    }

    pub fn dataplane(&self) -> &Arc<RcuRoutingTable> {
        &self.dataplane
    }

    pub fn admit_bucket(&self) -> &Arc<AdmitBucket> {
        &self.admit
    }

    pub fn dataplane_pi(&self) -> f64 {
        self.dataplane.read_pi()
    }

    pub fn dataplane_age_ms(&self) -> u64 {
        self.dataplane.age_ms()
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

    pub fn control_metrics(&self) -> ControlMetrics {
        let cp = self.control.lock().expect("control plane");
        let regret_mean = if cp.regret_samples > 0 {
            cp.regret_sum / cp.regret_samples as f64
        } else {
            0.0
        };
        let misroute_mean = if cp.fastpath_misroute_samples > 0 {
            cp.fastpath_misroute_sum / cp.fastpath_misroute_samples as f64
        } else {
            0.0
        };
        let colocated = self.colocated_routes.load(Ordering::Relaxed);
        let disagg = self.disagg_routes.load(Ordering::Relaxed);
        let routed = colocated.saturating_add(disagg);
        let fast_path_ratio = if routed > 0 {
            colocated as f64 / routed as f64
        } else {
            0.0
        };
        ControlMetrics {
            pi: cp.rebalancer.pi(),
            pi_star: cp.last_pi_star,
            pairing_regret_mean: regret_mean,
            pairing_regret_samples: cp.regret_samples,
            predictor_p90_tokens: cp.predictor.p90(),
            dataplane_pi: self.dataplane.read_pi(),
            dataplane_age_ms: self.dataplane.age_ms(),
            rcu_stale: self.dataplane.is_stale(DATAPLANE_RCU_STALE_ALERT_MS),
            rcu_stale_alert_ms: DATAPLANE_RCU_STALE_ALERT_MS,
            admit_shed_total: self.total_admit_shed(),
            fast_path_ratio,
            colocated_routes: colocated,
            disagg_routes: disagg,
            fastpath_misroute_mean: misroute_mean,
            fastpath_misroute_samples: cp.fastpath_misroute_samples,
            corrector_shadow_samples: cp.corrector_shadow.len() as u64,
        }
    }

    pub fn corrector_shadow_samples(&self) -> Vec<CorrectorShadowSample> {
        self.control
            .lock()
            .expect("control plane")
            .corrector_shadow
            .samples()
    }

    fn tick_control(
        &self,
        colocated: Option<bool>,
        prompt_tokens: Option<u64>,
        sample_regret: bool,
    ) {
        if let Some(colocated) = colocated {
            if colocated {
                self.colocated_routes.fetch_add(1, Ordering::Relaxed);
            } else {
                self.disagg_routes.fetch_add(1, Ordering::Relaxed);
            }
        }

        let need_rebalancer = sample_regret
            || self.rebalancer_actuation
            || self.dataplane.age_ms() >= DATAPLANE_RCU_HEARTBEAT_MS;

        let mut cp = self.control.lock().expect("control plane");
        if let Some(colocated) = colocated {
            if colocated {
                if let Some(tokens) = prompt_tokens {
                    let threshold = ROUTING_SHORT_CONTEXT_TOKENS.max(1);
                    if tokens * 100 / threshold >= 80 {
                        let frac = (tokens as f64 / threshold as f64).clamp(0.0, 1.0);
                        cp.fastpath_misroute_sum += frac;
                        cp.fastpath_misroute_samples += 1;
                    }
                }
            }
        }

        if prompt_tokens.is_none() && !need_rebalancer {
            return;
        }

        if let Some(tokens) = prompt_tokens {
            cp.predictor.record(tokens);
        }

        if !need_rebalancer {
            return;
        }

        let colocated = self.colocated_routes.load(Ordering::Relaxed);
        let disagg = self.disagg_routes.load(Ordering::Relaxed);
        let routed = colocated.saturating_add(disagg);
        let fp_share = if routed > 0 {
            colocated as f64 / routed as f64
        } else {
            0.5
        };
        let signals = pool_pressure(self, fp_share);
        cp.last_pi_star = cp.rebalancer.shadow_pi_star(&signals);
        if self.rebalancer_actuation {
            if let Some(new_pi) = cp.rebalancer.maybe_update(&signals) {
                self.dataplane.publish(DataPlaneSnapshot::new(
                    self.dataplane.generation().saturating_add(1),
                    new_pi,
                ));
            }
            self.maybe_sync_admit_for_pi(cp.rebalancer.pi());
        } else {
            let _ = cp.rebalancer.maybe_update(&signals);
        }

        if self.dataplane.age_ms() >= DATAPLANE_RCU_HEARTBEAT_MS {
            let pi = cp.rebalancer.pi();
            self.dataplane.publish(DataPlaneSnapshot::new(
                self.dataplane.generation().saturating_add(1),
                pi,
            ));
            if self.rebalancer_actuation || self.kernel_admit_attached() {
                self.maybe_sync_admit_for_pi(pi);
            }
        }

        if sample_regret {
            if let Some(tokens) = prompt_tokens {
                if !self.prefill.is_empty() && !self.decode.is_empty() {
                    let regret = pairing_regret_targets(
                        &self.prefill,
                        &self.decode,
                        self.state.as_ref(),
                        tokens,
                        ROUTING_TRANSFER_PENALTY,
                    );
                    cp.regret_sum += regret;
                    cp.regret_samples += 1;
                }
            }
        }
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
        let pool_pi = self.dataplane.read_pi();
        let extra: [BarrierFactor; 1] = [phi.unwrap_or(BarrierFactor::clamped(1.0))];
        let barriers = if phi.is_some() { &extra[..] } else { &[] };
        match phase {
            Phase::Prefill => {
                select_with_barriers_pi(self.pool(Phase::Prefill), barriers, pool_pi, true)
            }
            Phase::Decode => {
                select_with_barriers_pi(self.pool(Phase::Decode), barriers, pool_pi, false)
            }
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

const HDR_TOKENS: &[u8] = b"x-demiurge-tokens";
const HDR_PHASE: &[u8] = b"x-demiurge-phase";

#[inline]
fn trim_ascii_ws(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map(|p| p + 1)
        .unwrap_or(start);
    &bytes[start..end]
}

#[inline]
fn ascii_eq_ci(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|(&x, &y)| x.eq_ignore_ascii_case(&y))
}

#[inline]
fn header_value_ci<'a>(head: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    let mut i = 0;
    while i < head.len() {
        let line_end = head[i..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|p| i + p)
            .unwrap_or(head.len());
        let mut line = &head[i..line_end];
        if let Some(stripped) = line.strip_suffix(b"\r") {
            line = stripped;
        }
        if line.len() > name.len()
            && line[name.len()] == b':'
            && ascii_eq_ci(&line[..name.len()], name)
        {
            return Some(trim_ascii_ws(&line[name.len() + 1..]));
        }
        if line_end >= head.len() {
            break;
        }
        i = line_end + 1;
    }
    None
}

#[inline]
fn parse_u64_digits(bytes: &[u8]) -> Option<u64> {
    let mut n = 0u64;
    let mut any = false;
    for b in bytes {
        if !b.is_ascii_digit() {
            break;
        }
        any = true;
        n = n.checked_mul(10)?.checked_add(u64::from(b - b'0'))?;
    }
    if any {
        Some(n)
    } else {
        None
    }
}

#[inline]
fn contains_subslice(hay: &[u8], needle: &[u8]) -> bool {
    hay.len() >= needle.len() && hay.windows(needle.len()).any(|w| w == needle)
}

/// Parse `X-Demiurge-Tokens: N` from the request head.
pub fn parse_prompt_tokens(head: &[u8]) -> Option<u64> {
    header_value_ci(head, HDR_TOKENS).and_then(parse_u64_digits)
}

/// Parse token count from `/prefill/<n>` or `/long/<n>` path segments.
pub fn parse_path_tokens(head: &[u8]) -> Option<u64> {
    let first = head.split(|&b| b == b'\r' || b == b'\n').next()?;
    let mut parts = first.split(|&b| b == b' ').filter(|p| !p.is_empty());
    parts.next()?;
    let path = parts.next()?;
    for prefix in [b"/prefill/" as &[u8], b"/long/"] {
        if path.starts_with(prefix) {
            return parse_u64_digits(&path[prefix.len()..]);
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
    if header_value_ci(head, HDR_PHASE).is_some_and(|v| ascii_eq_ci(v, b"decode")) {
        return true;
    }
    head.split(|&b| b == b'\r' || b == b'\n')
        .next()
        .is_some_and(|line| contains_subslice(line, b" /decode"))
}

fn live_queue_pressure(backends: &[Arc<Backend>]) -> f64 {
    let max = backends.iter().map(|b| b.inflight()).max().unwrap_or(0);
    (max as f64 / (max as f64 + 16.0)).clamp(0.0, 1.0)
}

fn pool_pressure(router: &Router, fp_share: f64) -> PoolPressure {
    let mut signals = router
        .state
        .as_ref()
        .map(|s| export_pool_pressure(s, fp_share))
        .unwrap_or(PoolPressure {
            fp_share,
            ..Default::default()
        });
    signals.q_prefill = signals
        .q_prefill
        .max(live_queue_pressure(router.pool(Phase::Prefill)));
    signals.q_decode = signals
        .q_decode
        .max(live_queue_pressure(router.pool(Phase::Decode)));
    if let Some(ledger) = router.ledger.as_ref() {
        let kv = ledger.fleet_reserved() as f64 / ledger.capacity_bytes().max(1) as f64;
        signals.kv_decode = signals.kv_decode.max(kv.clamp(0.0, 1.0));
    }
    signals.fp_share = fp_share.clamp(0.0, 1.0);
    signals
}

/// Classify admission path from the HTTP head. [ALG-ROUTE] [DEMI-SHORT-FASTPATH] [DEMI-WARM-DISCOUNT]
pub fn route(router: &Router, head: &[u8]) -> Result<RoutePath, RouteError> {
    if is_decode_only(head) {
        let path = router
            .pick(Phase::Decode)
            .map(RoutePath::DecodeOnly)
            .ok_or(RouteError::NoBackend)?;
        router.tick_control(Some(false), None, false);
        return Ok(path);
    }

    let prompt_tokens = estimate_prompt_tokens(head);
    if prompt_tokens <= ROUTING_SHORT_CONTEXT_TOKENS {
        if let Some((prefill, _strength)) = warmth_override_target(router, prompt_tokens) {
            router.tick_control(Some(false), Some(prompt_tokens), false);
            return Ok(RoutePath::Disaggregated {
                prefill,
                request_id: RequestId::new(),
                prompt_tokens,
            });
        }
        let path = router
            .pick_colocated()
            .map(RoutePath::Colocated)
            .ok_or(RouteError::NoBackend)?;
        router.tick_control(Some(true), Some(prompt_tokens), false);
        return Ok(path);
    }

    let pool_pi = router.dataplane.read_pi();
    let prefill = select_prefill_with_pi(
        router.pool(Phase::Prefill),
        router.state.as_ref(),
        prompt_tokens,
        pool_pi,
    )
    .ok_or(RouteError::NoBackend)?;
    router.tick_control(Some(false), Some(prompt_tokens), false);
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

fn record_corrector_shadow(router: &Router, sample: CorrectorShadowSample) {
    router
        .control
        .lock()
        .expect("control plane")
        .corrector_shadow
        .record(sample);
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

    let pool_pi = router.dataplane.read_pi();
    let backend = select_decode_with_pi(
        prefill_label,
        router.pool(Phase::Decode),
        router.state.as_ref(),
        signals.prompt_tokens,
        ROUTING_TRANSFER_PENALTY,
        &extra,
        pool_pi,
    )
    .or_else(|| router.pick_with_phi(Phase::Decode, phi))
    .or_else(|| router.pick_colocated())
    .ok_or(RouteError::NoBackend)?;

    if let Some(h) = handoff {
        if let Some(reg) = &router.handoffs {
            reg.publish(h.clone());
            let transport = router
                .handoff_transport
                .as_ref()
                .map(Arc::clone)
                .unwrap_or_else(Router::default_handoff_transport);
            let outcome = transport.transfer(&h, signals.prefill_wall);
            reg.record_transfer(outcome.bytes, outcome.wall);
        }
    }

    let blocks = default_routing_blocks(signals.prompt_tokens);
    let analytic_ln = backend
        .cost_with_warmth_pi(&extra, router.state.as_ref(), &blocks, true, pool_pi)
        .ln();
    record_corrector_shadow(
        router,
        CorrectorShadowSample {
            prompt_tokens: signals.prompt_tokens,
            analytic_ln,
            observed_us: signals.prefill_wall.as_micros().min(u64::MAX as u128) as u64,
            pool_pi,
            backend_label: backend.label.clone(),
        },
    );

    router.tick_control(None, Some(signals.prompt_tokens), true);

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

struct AdmitGuard(Arc<AdmitBucket>);

impl Drop for AdmitGuard {
    fn drop(&mut self) {
        self.0.release(1);
    }
}

/// Userspace admit for one TCP connection — at most one guard per `handle_conn`.
enum AdmitConn {
    Shed,
    Proceed(Option<AdmitGuard>),
}

fn admit_conn(router: &Router) -> AdmitConn {
    let kernel_attached = router.kernel_admit_attached();
    if !router.admit_mode.uses_userspace_admit(kernel_attached) {
        return AdmitConn::Proceed(None);
    }
    if router.admit.try_admit().is_err() {
        return AdmitConn::Shed;
    }
    AdmitConn::Proceed(Some(AdmitGuard(Arc::clone(&router.admit))))
}

fn proxy_to_backend(
    client: &mut TcpStream,
    head: &[u8],
    backend: &Backend,
    #[cfg(target_os = "linux")] io_uring_session: Option<&mut IoUringProxySession>,
) -> io::Result<()> {
    backend.incr_inflight();
    let _guard = InflightGuard(backend);

    let mut upstream = TcpStream::connect(backend.addr)?;
    upstream.write_all(head)?;

    #[cfg(target_os = "linux")]
    if let Some(session) = io_uring_session {
        use std::os::fd::AsRawFd;
        let up_read = upstream.try_clone()?;
        let client_write = client.try_clone()?;
        let client_read = client.try_clone()?;
        let pump = thread::spawn(move || {
            if let Ok(mut pump_session) = IoUringProxySession::new() {
                let _ = pump_session.copy_stream(
                    up_read.as_raw_fd(),
                    client_write.as_raw_fd(),
                    256 * 1024,
                );
            }
            let _ = client_write.shutdown(Shutdown::Write);
        });
        session.copy_stream(client_read.as_raw_fd(), upstream.as_raw_fd(), 256 * 1024)?;
        let _ = upstream.shutdown(Shutdown::Write);
        let _ = pump.join();
        return Ok(());
    }

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
    #[cfg(target_os = "linux")] io_uring_session: Option<&mut IoUringProxySession>,
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

    let result = proxy_to_backend(
        &mut client,
        &head,
        placement.backend.as_ref(),
        #[cfg(target_os = "linux")]
        io_uring_session,
    );
    drop(placement.reservation);
    result
}

fn rebalancer_actuation_enabled() -> bool {
    if let Ok(v) = std::env::var("DEMIURGE_REBALANCER_ACTUATE") {
        return matches!(v.as_str(), "1" | "true" | "yes");
    }
    POOL_ACTUATION_ENABLED
}

fn handle_conn(client: TcpStream, router: Arc<Router>) -> io::Result<()> {
    let _admit_guard = match admit_conn(&router) {
        AdmitConn::Shed => {
            let mut client = client;
            let _ =
                client.write_all(b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\r\n");
            return Ok(());
        }
        AdmitConn::Proceed(guard) => guard,
    };

    let mut client = client;
    #[cfg(target_os = "linux")]
    let mut io_uring_session = router
        .io_uring
        .as_ref()
        .and_then(|fwd| fwd.open_proxy_session().ok());

    let head = {
        #[cfg(target_os = "linux")]
        {
            if let Some(ref mut session) = io_uring_session {
                use std::os::fd::AsRawFd;
                session.read_http_head(client.as_raw_fd(), MAX_HEAD)?
            } else {
                read_head(&mut client)?
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            read_head(&mut client)?
        }
    };
    std::hint::black_box(router.dataplane_pi());

    match route(&router, &head) {
        Ok(RoutePath::Colocated(b) | RoutePath::DecodeOnly(b)) => proxy_to_backend(
            &mut client,
            &head,
            b.as_ref(),
            #[cfg(target_os = "linux")]
            io_uring_session.as_mut(),
        ),
        Ok(RoutePath::Disaggregated {
            prefill,
            request_id,
            prompt_tokens,
        }) => handle_disaggregated(
            client,
            head,
            router,
            prefill,
            request_id,
            prompt_tokens,
            #[cfg(target_os = "linux")]
            io_uring_session.as_mut(),
        ),
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

/// Backend that accepts then resets without sending HTTP (proxy fault injection).
pub fn spawn_rst_backend() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(s) = conn else { continue };
            drop(s);
        }
    });
    addr
}

/// Backend returning a fixed-size HTTP body (io_uring / large-response tests).
pub fn spawn_large_body_backend(body_bytes: usize) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut s) = conn else { continue };
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf);
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {body_bytes}\r\nconnection: close\r\n\r\n"
            );
            let _ = s.write_all(head.as_bytes());
            let chunk = vec![b'x'; 64 * 1024];
            let mut sent = 0usize;
            while sent < body_bytes {
                let n = chunk.len().min(body_bytes - sent);
                if s.write_all(&chunk[..n]).is_err() {
                    break;
                }
                sent += n;
            }
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
