//! Request classification and the async prefill → decode continuation.
//! [ALG-ROUTE] [DEMI-SHORT-FASTPATH] [DEMI-KV-HANDOFF]

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use demiurge_control::{AdmitError, CorrectorShadowSample, RdmaCostShadowSample, ReservationGuard};
use demiurge_cost::{
    kv_breakdown, rdma_distance, rdma_transfer_ln, BarrierFactor,
    DATAPLANE_PREFILL_RESPONSE_MAX_BYTES, ROUTING_SHORT_CONTEXT_TOKENS,
    ROUTING_SHORT_CONTEXT_WARMTH_OVERRIDE, ROUTING_TRANSFER_PENALTY,
};
use demiurge_handoff::parse_prefill_handoff;
use demiurge_state::default_routing_blocks;

use crate::backend::{select_decode_with_pi, select_prefill_with_identity_pi, DecodePlacementCtx};
use crate::http::{estimate_prompt_tokens, is_decode_only};
use crate::{Backend, GroupId, PrefixFingerprint, Router, SharedPrefixGroupRegistry, TenantId};

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
    #[must_use]
    pub fn new() -> Self {
        Self(NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed))
    }

    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

impl Default for RequestId {
    fn default() -> Self {
        Self::new()
    }
}

/// Already-authenticated request identity for cache-domain isolation.
/// [DEMI-S1-DOMAIN]
///
/// The router never authenticates a tenant or verifies content itself —
/// the caller establishes `tenant` and `content_fp` on its own strongly
/// consistent path (see [`SharedPrefixGroupRegistry`]) before calling
/// [`route_with_identity`]. The router's job is only to resolve the
/// cache-domain key for that identity and measure warmth under it, so a
/// non-member or template mismatch can discount against nothing but its
/// own tenant-private warmth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestIdentity {
    pub tenant: TenantId,
    pub group: GroupId,
    pub content_fp: PrefixFingerprint,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteError {
    NoBackend,
    HandoffMissing,
    KvAdmitRejected,
    /// Prefill backend I/O failed — the error message is preserved for logging.
    PrefillIo(String),
}

impl std::fmt::Display for RouteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RouteError::NoBackend => write!(f, "no backend available for route"),
            RouteError::HandoffMissing => write!(f, "prefill completed without KV hand-off"),
            RouteError::KvAdmitRejected => write!(f, "decode pool rejected KV reservation"),
            RouteError::PrefillIo(msg) => write!(f, "prefill I/O error: {msg}"),
        }
    }
}

impl std::error::Error for RouteError {}

/// Classify admission path from the HTTP head. [ALG-ROUTE] [DEMI-SHORT-FASTPATH] [DEMI-WARM-DISCOUNT]
pub fn route(router: &Router, head: &[u8]) -> Result<RoutePath, RouteError> {
    let path = route_impl(router, head, None)?;
    router.schedule_control_tick(&path, head);
    Ok(path)
}

/// `route`, but warmth-driven decisions are gated by cache-domain isolation.
/// [DEMI-S1-DOMAIN]
///
/// When both `router.cache_registry()` and `identity` are present, the
/// short-context warmth override and the long-context prefill selection
/// measure warmth only under the cache-domain key the registry resolves
/// for `identity` — exactly [`demiurge_state::gated_hit_strength`] applied
/// on the live routing path, not just at the state-plane unit level. A
/// non-member or template mismatch can therefore only ever benefit from
/// its own tenant-private warmth, never another tenant's shared cache.
/// Missing either the registry or the identity falls back to identical
/// behavior as [`route`].
pub fn route_with_identity(
    router: &Router,
    head: &[u8],
    identity: Option<&RequestIdentity>,
) -> Result<RoutePath, RouteError> {
    let isolation = router.cache_registry().map(Arc::as_ref).zip(identity);
    let path = route_impl(router, head, isolation)?;
    router.schedule_control_tick(&path, head);
    Ok(path)
}

fn route_impl(
    router: &Router,
    head: &[u8],
    isolation: Option<(&SharedPrefixGroupRegistry, &RequestIdentity)>,
) -> Result<RoutePath, RouteError> {
    if is_decode_only(head) {
        let path = router
            .pick(Phase::Decode)
            .map(RoutePath::DecodeOnly)
            .ok_or(RouteError::NoBackend)?;
        router.bump_route_counters(false);
        return Ok(path);
    }

    let prompt_tokens = estimate_prompt_tokens(head);
    let snap = router.routing_snapshot();
    let snap_ref = snap.as_deref();
    if prompt_tokens <= ROUTING_SHORT_CONTEXT_TOKENS {
        if let Some((prefill, _strength)) =
            warmth_override_target_with_identity(router, prompt_tokens, isolation, snap_ref)
        {
            router.bump_route_counters(false);
            router.note_fastpath_misroute(prompt_tokens);
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
        router.bump_route_counters(true);
        return Ok(path);
    }

    let pool_pi = router.dataplane_pi();
    let prefill = select_prefill_with_identity_pi(
        router.pool(Phase::Prefill),
        snap_ref,
        prompt_tokens,
        pool_pi,
        isolation,
    )
    .ok_or(RouteError::NoBackend)?;
    router.bump_route_counters(false);
    router.note_fastpath_misroute(prompt_tokens);
    Ok(RoutePath::Disaggregated {
        prefill,
        request_id: RequestId::new(),
        prompt_tokens,
    })
}

/// Short-context warmth-override target, with the warmth strength driving
/// the decision optionally gated by cache-domain isolation. [DEMI-S1-DOMAIN]
fn warmth_override_target_with_identity(
    router: &Router,
    prompt_tokens: u64,
    isolation: Option<(&SharedPrefixGroupRegistry, &RequestIdentity)>,
    snap: Option<&demiurge_state::StateSnapshot>,
) -> Option<(Arc<Backend>, f64)> {
    let snap = snap?;
    let blocks = default_routing_blocks(prompt_tokens);
    let mut best: Option<(Arc<Backend>, f64)> = None;
    for backend in router.pool(Phase::Prefill) {
        snap.prefill.get(&backend.label)?;
        let strength = snap.pool_hit_strength(
            false,
            &backend.label,
            &blocks,
            isolation.map(|(reg, id)| (reg, id.tenant, id.group, id.content_fp)),
        );
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
    pub(crate) reservation: Option<ReservationGuard>,
}

impl DecodePlacement {
    #[must_use]
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
    let (handoff, reservation) = match (router.ledger(), router.handoffs()) {
        (Some(ledger), Some(_handoffs)) => {
            let handoff =
                parse_prefill_handoff(prefill_response, signals.request_id.raw(), prefill_label)
                    .filter(|h| h.is_valid())
                    .ok_or(RouteError::HandoffMissing)?;

            let expected =
                kv_breakdown(signals.prompt_tokens, router.bytes_per_token()).kv_reserved;
            let ceiling = expected.saturating_mul(demiurge_cost::KV_HANDOFF_BYTE_CEILING_MULTIPLE);
            // Floor (analytic) and ceiling (G3) — reject under/over-claim.
            if handoff.byte_len < expected || handoff.byte_len > ceiling {
                return Err(RouteError::HandoffMissing);
            }

            let reservation = ledger
                .try_reserve_from(Some(prefill_label), handoff.request_id, handoff.byte_len)
                .map_err(|e| match e {
                    AdmitError::OverCapacity | AdmitError::DuplicateRequest => {
                        RouteError::KvAdmitRejected
                    }
                })?;

            (Some(handoff), Some(reservation))
        }
        _ => (None, None),
    };

    // T6: fold measured prefill wall into the prefill backend's effective T_core.
    if let Some(pf) = router
        .pool(Phase::Prefill)
        .iter()
        .find(|b| b.label == prefill_label)
    {
        pf.note_observed_seconds(signals.prefill_wall.as_secs_f64());
    }

    let phi = router
        .ledger()
        .map(|l| l.phi_barrier())
        .filter(|b| b.get() > 1.0);
    let mut extra_buf = [BarrierFactor::clamped(1.0); demiurge_cost::MAX_SERVICE_BARRIERS];
    let mut extra_len = 0;
    if let Some(phi) = phi {
        demiurge_cost::append_barriers_fail_expensive(&mut extra_buf, &mut extra_len, &[phi]);
    }

    let pool_pi = router.dataplane_pi();
    let snap = router.routing_snapshot();
    let snap_ref = snap.as_deref();
    let pf_topo = router.topology_for_label(prefill_label);
    let kv_bytes = handoff.as_ref().map(|h| h.byte_len).unwrap_or_else(|| {
        kv_breakdown(signals.prompt_tokens, router.bytes_per_token()).kv_reserved
    });
    let backend = select_decode_with_pi(
        &DecodePlacementCtx {
            prefill_label,
            pf_topo: &pf_topo,
            kv_bytes,
            use_rdma_routing: router.rdma_routing(),
        },
        router.pool(Phase::Decode),
        snap_ref,
        signals.prompt_tokens,
        &extra_buf[..extra_len],
        pool_pi,
    )
    .or_else(|| router.pick_with_phi(Phase::Decode, phi))
    .or_else(|| router.pick_colocated())
    .ok_or(RouteError::NoBackend)?;

    if let Some(mut h) = handoff {
        if let Some(reg) = router.handoffs() {
            h.decode_label = Some(backend.label.clone());
            reg.publish(h.clone());
            let transport = router.handoff_transport_or_default();
            let outcome = transport.transfer(&h, signals.prefill_wall);
            reg.record_transfer(outcome.bytes, outcome.wall);

            if prefill_label != backend.label {
                let pf_topo = router.topology_for_label(prefill_label);
                let dc_topo = backend.topology().clone();
                router.record_rdma_cost_shadow(RdmaCostShadowSample {
                    pf_label: prefill_label.to_string(),
                    dc_label: backend.label.clone(),
                    distance: rdma_distance(&pf_topo, &dc_topo),
                    kv_bytes: h.byte_len,
                    analytic_transfer_ln: rdma_transfer_ln(h.byte_len, &pf_topo, &dc_topo),
                    flat_penalty_ln: ROUTING_TRANSFER_PENALTY.ln(),
                    observed_transfer_secs: outcome.wall.as_secs_f64(),
                });
            }
        }
    }

    let blocks = default_routing_blocks(signals.prompt_tokens);
    let analytic_ln = backend
        .cost_with_warmth_pi(&extra_buf[..extra_len], snap_ref, &blocks, true, pool_pi)
        .ln();
    router.record_corrector_shadow(CorrectorShadowSample {
        prompt_tokens: signals.prompt_tokens,
        analytic_ln,
        observed_us: signals.prefill_wall.as_micros().min(u64::MAX as u128) as u64,
        pool_pi,
        backend_label: backend.label.clone(),
    });

    router.deferred_control_tick(Some(signals.prompt_tokens), true);

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
            let _worker = dispatch_prefill(
                prefill,
                head.to_vec(),
                request_id,
                prompt_tokens,
                |_, _r| {},
            );
        }
        RoutePath::Colocated(_) | RoutePath::DecodeOnly(_) => {
            return Err(RouteError::NoBackend);
        }
    }
    Ok(start.elapsed())
}

/// Dispatch prefill I/O on a worker thread; invoke `on_complete` when done.
///
/// The callback receives the raw I/O outcome so callers can distinguish a
/// network failure (`Err`) from a successful but empty response (`Ok(vec![])`).
pub fn dispatch_prefill(
    prefill: Arc<Backend>,
    head: Vec<u8>,
    request_id: RequestId,
    prompt_tokens: u64,
    on_complete: impl FnOnce(PrefillSignals, io::Result<Vec<u8>>) + Send + 'static,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let started = std::time::Instant::now();
        let response = run_prefill_io(&prefill, &head);
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
    // Cap the buffered prefill response: only the hand-off headers matter
    // here, and an unbounded `read_to_end` would let one misbehaving backend
    // exhaust router memory.
    let max = DATAPLANE_PREFILL_RESPONSE_MAX_BYTES;
    let mut resp = Vec::new();
    (&mut upstream).take(max + 1).read_to_end(&mut resp)?;
    if resp.len() as u64 > max {
        return Err(io::Error::other(format!(
            "prefill response from {} exceeds {max} bytes",
            prefill.label
        )));
    }
    Ok(resp)
}
