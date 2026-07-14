//! Phase-aware, cost-based TCP forwarder.
//!
//! **Phase 0:** min-cost selection over phase pools ([`select`], [`Router::pick`]).
//! **Phase 3:** RCU state snapshot, warmth discounts, fast-path override ([DEMI-WARM-DISCOUNT], [DEMI-STATE-AP]).
//! **Phase 4:** Greedy pf→dc pairing on the disaggregated path ([DEMI-PAIR-GREEDY]).
//! **Phase 5:** RCU routing table + admit shed on the live TCP path ([DEMI-DP-RCU], [DEMI-XDP-SHED]).
//! **Phase 7:** Cache-domain isolation on the live path ([DEMI-S1-DOMAIN]).
//!
//! ## Module layout
//!
//! | Module | Contents |
//! |---|---|
//! | [`backend`]  | `Backend` cost surface, min-cost selection |
//! | `http`       | bounded head parsing, identity headers |
//! | `config`     | pool/topology/cache-group spec parsing, env flags |
//! | [`routing`]  | `route`/`route_with_identity`, prefill→decode continuation |
//! | `serve`      | bounded accept loop, admission, TCP proxying |
//! | crate root   | `Router` (pools, control plane, dataplane wiring) |

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use demiurge_control::{
    export_pool_pressure, pairing_regret_targets, CorrectorShadowLog, CorrectorShadowSample,
    LengthPredictor, PoolPressure, PoolRebalancer, RdmaCostShadowLog, RdmaCostShadowSample,
    RebalancerMode, ReservationLedger,
};
use demiurge_cost::{
    BarrierFactor, TopologyId, DATAPLANE_ADMIT_BURST, DATAPLANE_RCU_HEARTBEAT_MS,
    DATAPLANE_RCU_STALE_ALERT_MS, ROUTING_SHORT_CONTEXT_TOKENS, ROUTING_TRANSFER_PENALTY,
};
use demiurge_dataplane::{admit_capacity_for_pi, AdmitBucket, DataPlaneSnapshot};
use demiurge_handoff::{HandoffRegistry, HandoffTransport, HeaderPassthroughTransport};

pub use demiurge_auth::{GroupId, PrefixFingerprint, SharedPrefixGroupRegistry, TenantId};
pub use demiurge_control::{LedgerMetrics, ReservationLedger as KvReservationLedger};
#[cfg(target_os = "linux")]
pub use demiurge_dataplane::IoUringProxySession;
pub use demiurge_dataplane::{
    pool_core_scale, AdmitMode, DataPlaneSnapshot as RcuDataPlaneSnapshot, ForwardDecision,
    IoUringForwarder, RcuRoutingTable, XdpAdmitConfig, XdpAdmitShed, XdpAttachError,
};
pub use demiurge_handoff::{
    HandoffRegistry as KvHandoffRegistry, HandoffTransferMetrics, KvHandle,
};
pub use demiurge_state::{
    default_routing_blocks, BackendSnapshot, StatePlane, StateSnapshot, WarmthMap,
};

pub mod backend;
mod banner;
mod config;
mod http;
pub mod routing;
mod serve;
/// Reusable TCP backend stubs for integration tests.
pub mod testutil;

pub use backend::{
    select, select_with_barriers, select_with_barriers_pi, select_with_warmth,
    select_with_warmth_pi, Backend,
};
pub use banner::print_startup_banner;
pub use config::{parse_cache_groups, parse_pool, parse_pool_with_topology, parse_topology_map};
pub use http::{
    estimate_prompt_tokens, is_decode_only, parse_path_tokens, parse_prompt_tokens,
    parse_request_identity,
};
pub use routing::{
    admit_disaggregated, dispatch_prefill, on_prefill_complete, route, route_with_identity,
    DecodePlacement, Phase, PrefillSignals, RequestId, RequestIdentity, RouteError, RoutePath,
};
pub use serve::{serve, serve_with_max_conns};
pub use testutil::{
    spawn_delay_backend, spawn_large_body_backend, spawn_latch_prefill_backend,
    spawn_marker_backend, spawn_rst_backend, LatchBackend,
};

pub struct Router {
    prefill: Vec<Arc<Backend>>,
    decode: Vec<Arc<Backend>>,
    ledger: Option<Arc<ReservationLedger>>,
    handoffs: Option<Arc<HandoffRegistry>>,
    bytes_per_token: u64,
    state: Option<Arc<StateSnapshot>>,
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
    rdma_routing: bool,
    /// Shared-Prefix Group authority for cache-domain isolation; `None`
    /// disables identity-gated routing entirely (`route_with_identity`
    /// then behaves exactly like `route`). [DEMI-S1-DOMAIN]
    cache_registry: Option<Arc<SharedPrefixGroupRegistry>>,
    /// Live AP warmth plane; when set, [`Self::routing_snapshot`] reads here
    /// instead of the static [`Self::state`] snapshot. [DEMI-STATE-AP]
    state_plane: Option<Arc<StatePlane>>,
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
            rdma_routing: self.rdma_routing,
            cache_registry: self.cache_registry.clone(),
            state_plane: self.state_plane.clone(),
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
    pub rdma_cost_shadow_samples: u64,
}

#[derive(Debug)]
struct ControlPlane {
    rebalancer: PoolRebalancer,
    predictor: LengthPredictor,
    regret_sum: f64,
    regret_samples: u64,
    last_pi_star: f64,
    corrector_shadow: CorrectorShadowLog,
    rdma_cost_shadow: RdmaCostShadowLog,
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
            rdma_cost_shadow: RdmaCostShadowLog::new(4096),
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
            rebalancer_actuation: config::rebalancer_actuation_enabled(),
            colocated_routes: AtomicU64::new(0),
            disagg_routes: AtomicU64::new(0),
            handoff_transport: None,
            rdma_routing: config::rdma_routing_enabled(),
            cache_registry: None,
            state_plane: None,
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
            rebalancer_actuation: config::rebalancer_actuation_enabled(),
            colocated_routes: AtomicU64::new(0),
            disagg_routes: AtomicU64::new(0),
            handoff_transport: Some(Self::default_handoff_transport()),
            rdma_routing: config::rdma_routing_enabled(),
            cache_registry: None,
            state_plane: None,
        };
        (router, ledger, handoffs)
    }

    pub fn with_state(mut self, state: StateSnapshot) -> Self {
        self.state = Some(Arc::new(state));
        self
    }

    /// Attach a live state plane for warmth/occupancy updates on production traffic.
    #[must_use]
    pub fn with_state_plane(mut self, plane: Arc<StatePlane>) -> Self {
        self.state_plane = Some(plane);
        self
    }

    /// Snapshot for routing decisions — live plane when configured, else static seed.
    #[must_use]
    pub fn routing_snapshot(&self) -> Option<Arc<StateSnapshot>> {
        if let Some(plane) = &self.state_plane {
            Some(plane.snapshot())
        } else {
            self.state.as_ref().map(Arc::clone)
        }
    }

    #[must_use]
    pub fn state_plane_active(&self) -> bool {
        self.state_plane.is_some()
    }

    /// Record block-granularity warmth after a request completes on `backend_label`.
    pub fn record_request_warmth(&self, backend_label: &str, phase: Phase, prompt_tokens: u64) {
        let Some(plane) = self.state_plane.as_ref() else {
            return;
        };
        let blocks = default_routing_blocks(prompt_tokens);
        plane.update_snapshot(|snap| {
            let pool = match phase {
                Phase::Prefill => &mut snap.prefill,
                Phase::Decode => &mut snap.decode,
            };
            if let Some(backend) = pool.get_mut(backend_label) {
                for block in blocks {
                    backend.warmth.insert(block);
                }
                snap.generation = snap.generation.saturating_add(1);
            }
        });
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

    pub fn with_rdma_routing(mut self, enabled: bool) -> Self {
        self.rdma_routing = enabled;
        self
    }

    /// Attach a Shared-Prefix Group authority; enables [`route_with_identity`]
    /// to gate warmth discounts by cache-domain isolation. [DEMI-S1-DOMAIN]
    #[must_use]
    pub fn with_cache_registry(mut self, registry: Arc<SharedPrefixGroupRegistry>) -> Self {
        self.cache_registry = Some(registry);
        self
    }

    #[must_use]
    pub fn cache_registry(&self) -> Option<&Arc<SharedPrefixGroupRegistry>> {
        self.cache_registry.as_ref()
    }

    #[must_use]
    pub(crate) const fn rdma_routing(&self) -> bool {
        self.rdma_routing
    }

    pub(crate) fn handoff_transport_or_default(&self) -> Arc<dyn HandoffTransport> {
        self.handoff_transport
            .as_ref()
            .map(Arc::clone)
            .unwrap_or_else(Self::default_handoff_transport)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn io_uring_proxy_session(&self) -> Option<IoUringProxySession> {
        self.io_uring
            .as_ref()
            .and_then(|fwd| fwd.open_proxy_session().ok())
    }

    /// Inject trace window pressure and publish π (fleet replay actuation; bypasses hysteresis).
    pub fn actuate_from_trace_pressure(&self, signals: PoolPressure) {
        let pi_star = {
            let mut cp = self.control.lock().expect("control plane");
            let pi_star = cp.rebalancer.compute_pi_star(&signals);
            cp.last_pi_star = pi_star;
            if self.rebalancer_actuation {
                cp.rebalancer.force_actuate(pi_star);
            }
            pi_star
        };
        if self.rebalancer_actuation {
            self.dataplane.publish(DataPlaneSnapshot::new(
                self.dataplane.generation().saturating_add(1),
                pi_star,
            ));
            self.maybe_sync_admit_for_pi(pi_star);
        }
    }

    pub fn with_admit_mode(mut self, mode: AdmitMode) -> Self {
        self.admit_mode = mode;
        self
    }

    /// Attach kernel XDP admit-shed on `iface` (Linux + built BPF object).
    /// `listen_port` narrows the SYN gate to the router's port; `None` gates
    /// every TCP SYN on the interface (dedicated-iface deployments).
    pub fn with_kernel_admit(
        mut self,
        iface: &str,
        listen_port: Option<u16>,
    ) -> Result<Self, XdpAttachError> {
        if !self.admit_mode.wants_kernel() {
            self.admit_mode = AdmitMode::Hybrid;
        }
        let cap = admit_capacity_for_pi(DATAPLANE_ADMIT_BURST, self.dataplane.read_pi());
        let config = XdpAdmitConfig {
            capacity: cap,
            listen_port,
            ..XdpAdmitConfig::default()
        };
        let shed = XdpAdmitShed::attach(iface, config)?;
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

    /// Drop a dead kernel admit link so Hybrid mode fails back to the
    /// userspace bucket instead of running with no admission at all.
    /// Called on the RCU heartbeat cadence.
    fn check_kernel_admit_link(&self) {
        let Ok(mut guard) = self.kernel_admit.lock() else {
            return;
        };
        if guard.as_ref().is_some_and(|s| !s.link_alive()) {
            eprintln!(
                "demiurge-router: kernel admit-shed link lost; falling back to userspace bucket"
            );
            *guard = None;
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

    #[must_use]
    pub const fn rebalancer_actuation(&self) -> bool {
        self.rebalancer_actuation
    }

    #[must_use]
    pub fn dataplane(&self) -> &Arc<RcuRoutingTable> {
        &self.dataplane
    }

    #[must_use]
    pub fn admit_bucket(&self) -> &Arc<AdmitBucket> {
        &self.admit
    }

    #[must_use]
    pub fn dataplane_pi(&self) -> f64 {
        self.dataplane.read_pi()
    }

    #[must_use]
    pub fn dataplane_age_ms(&self) -> u64 {
        self.dataplane.age_ms()
    }

    #[must_use]
    pub fn state(&self) -> Option<&StateSnapshot> {
        self.state.as_deref()
    }

    #[must_use]
    pub fn ledger(&self) -> Option<&Arc<ReservationLedger>> {
        self.ledger.as_ref()
    }

    #[must_use]
    pub fn handoffs(&self) -> Option<&Arc<HandoffRegistry>> {
        self.handoffs.as_ref()
    }

    #[must_use]
    pub const fn bytes_per_token(&self) -> u64 {
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
            rdma_cost_shadow_samples: cp.rdma_cost_shadow.len() as u64,
        }
    }

    pub fn corrector_shadow_samples(&self) -> Vec<CorrectorShadowSample> {
        self.control
            .lock()
            .expect("control plane")
            .corrector_shadow
            .samples()
    }

    pub fn rdma_cost_shadow_samples(&self) -> Vec<RdmaCostShadowSample> {
        self.control
            .lock()
            .expect("control plane")
            .rdma_cost_shadow
            .samples()
    }

    pub(crate) fn record_corrector_shadow(&self, sample: CorrectorShadowSample) {
        self.control
            .lock()
            .expect("control plane")
            .corrector_shadow
            .record(sample);
    }

    pub(crate) fn record_rdma_cost_shadow(&self, sample: RdmaCostShadowSample) {
        self.control
            .lock()
            .expect("control plane")
            .rdma_cost_shadow
            .record(sample);
    }

    pub(crate) fn topology_for_label(&self, label: &str) -> TopologyId {
        self.prefill
            .iter()
            .chain(self.decode.iter())
            .find(|b| b.label == label)
            .map(|b| b.topology().clone())
            .unwrap_or_default()
    }

    pub(crate) fn bump_route_counters(&self, colocated: bool) {
        if colocated {
            self.colocated_routes.fetch_add(1, Ordering::Relaxed);
        } else {
            self.disagg_routes.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn note_fastpath_misroute(&self, prompt_tokens: u64) {
        let threshold = ROUTING_SHORT_CONTEXT_TOKENS.max(1);
        if prompt_tokens <= threshold && prompt_tokens * 100 / threshold >= 80 {
            let frac = (prompt_tokens as f64 / threshold as f64).clamp(0.0, 1.0);
            if let Ok(mut cp) = self.control.try_lock() {
                cp.fastpath_misroute_sum += frac;
                cp.fastpath_misroute_samples += 1;
            }
        }
    }

    /// Control-plane maintenance deferred off the classify hot path.
    pub(crate) fn deferred_control_tick(&self, prompt_tokens: Option<u64>, sample_regret: bool) {
        let need_rebalancer = sample_regret
            || self.rebalancer_actuation
            || self.dataplane.age_ms() >= DATAPLANE_RCU_HEARTBEAT_MS;

        if prompt_tokens.is_none() && !need_rebalancer {
            return;
        }

        let mut cp = self.control.lock().expect("control plane");

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
            self.check_kernel_admit_link();
            if self.rebalancer_actuation || self.kernel_admit_attached() {
                self.maybe_sync_admit_for_pi(pi);
            }
        }

        if sample_regret {
            if let Some(tokens) = prompt_tokens {
                if !self.prefill.is_empty() && !self.decode.is_empty() {
                    let snap = self.routing_snapshot();
                    let regret = pairing_regret_targets(
                        &self.prefill,
                        &self.decode,
                        snap.as_deref(),
                        tokens,
                        ROUTING_TRANSFER_PENALTY,
                    );
                    cp.regret_sum += regret;
                    cp.regret_samples += 1;
                }
            }
        }
    }

    pub(crate) fn schedule_control_tick(&self, path: &RoutePath, head: &[u8]) {
        match path {
            RoutePath::Colocated(_) => {
                self.deferred_control_tick(Some(estimate_prompt_tokens(head)), false);
            }
            RoutePath::Disaggregated { prompt_tokens, .. } => {
                self.deferred_control_tick(Some(*prompt_tokens), false);
            }
            RoutePath::DecodeOnly(_) => {
                self.deferred_control_tick(None, false);
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

    /// Colocated fast path: single-hop inference. Defaults to the prefill pool;
    /// set `DEMIURGE_COLOCATED_PHASE=decode` when prefill workers are handoff-only.
    pub fn pick_colocated(&self) -> Option<Arc<Backend>> {
        let phase = match std::env::var("DEMIURGE_COLOCATED_PHASE").as_deref() {
            Ok("decode") => Phase::Decode,
            Ok("prefill") => Phase::Prefill,
            _ => Phase::Prefill,
        };
        self.pick(phase)
    }
}

fn live_queue_pressure(backends: &[Arc<Backend>]) -> f64 {
    let max = backends.iter().map(|b| b.inflight()).max().unwrap_or(0);
    (max as f64 / (max as f64 + 16.0)).clamp(0.0, 1.0)
}

fn pool_pressure(router: &Router, fp_share: f64) -> PoolPressure {
    let snap = router.routing_snapshot();
    let mut signals = snap
        .as_deref()
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
