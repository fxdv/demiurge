//! Cache-domain isolation on the live routing path. [DEMI-S1-DOMAIN]
//!
//! `demiurge-auth` / `demiurge-state` already prove membership-gated salted
//! warmth in isolation (see their own unit tests); these tests prove the
//! same property end-to-end through the real `Router`/`Backend` selection
//! code (`route_with_identity`), not just at the state-plane unit level.

use demiurge_auth::{GroupId, PrefixFingerprint, SharedPrefixGroupRegistry, TenantId};
use demiurge_router::{
    route, route_with_identity, Backend, RequestIdentity, RoutePath, Router, StateSnapshot,
};
use demiurge_state::BackendSnapshot;
use std::sync::Arc;

const LONG_HEAD: &[u8] = b"GET /long/2048 HTTP/1.1\r\nhost: x\r\nx-demiurge-tokens: 2048\r\n\r\n";

fn group_and_content() -> (GroupId, PrefixFingerprint) {
    (
        GroupId::new(7),
        PrefixFingerprint::of(b"shared system prompt"),
    )
}

/// Two identical-cost prefill backends; "pf-shared" is warmed under the
/// group's *shared* cache-domain key (member 1 warmed it), "pf-cold" is
/// never warmed.
fn router_with_shared_warmth() -> (Router, GroupId, PrefixFingerprint) {
    let addr: std::net::SocketAddr = "127.0.0.1:9".parse().unwrap();
    let pf_cold = Backend::new("pf-cold", addr, 0.02);
    let pf_shared = Backend::new("pf-shared", addr, 0.02);
    let decode = Backend::new("dc0", addr, 0.02);

    let (group, content) = group_and_content();
    let mut reg = SharedPrefixGroupRegistry::new();
    reg.register_template(group, [TenantId::new(1), TenantId::new(2)], content, 42);
    let shared_key = reg
        .resolve_shared_key(TenantId::new(1), group, content)
        .expect("member 1 resolves the shared key");

    let mut snap = StateSnapshot::empty();
    let mut hot = BackendSnapshot::new("pf-shared", 0);
    for b in demiurge_state::default_routing_blocks(2048) {
        assert!(hot.warmth.insert_salted(b, &shared_key));
    }
    snap.prefill.insert("pf-shared".into(), hot);
    snap.prefill
        .insert("pf-cold".into(), BackendSnapshot::new("pf-cold", 0));

    let router = Router::new(vec![pf_cold, pf_shared], vec![decode])
        .with_state(snap)
        .with_cache_registry(Arc::new(reg));
    (router, group, content)
}

#[test]
fn member_with_matching_content_routes_to_shared_warm_backend() {
    let (router, group, content) = router_with_shared_warmth();
    let identity = RequestIdentity {
        tenant: TenantId::new(2),
        group,
        content_fp: content,
    };
    match route_with_identity(&router, LONG_HEAD, Some(&identity)).expect("route") {
        RoutePath::Disaggregated { prefill, .. } => assert_eq!(prefill.label, "pf-shared"),
        other => panic!("expected disagg to the shared-warm backend, got {other:?}"),
    }
}

#[test]
fn non_member_does_not_benefit_from_shared_warmth() {
    let (router, group, content) = router_with_shared_warmth();
    let identity = RequestIdentity {
        tenant: TenantId::new(99),
        group,
        content_fp: content,
    };
    // No discount applies to either backend, so the tie breaks to the first
    // candidate ("pf-cold") — never the shared-domain backend it isn't a
    // member of, even though it presents byte-identical content.
    match route_with_identity(&router, LONG_HEAD, Some(&identity)).expect("route") {
        RoutePath::Disaggregated { prefill, .. } => assert_eq!(prefill.label, "pf-cold"),
        other => panic!("expected disagg to the cold backend, got {other:?}"),
    }
}

#[test]
fn template_mismatch_does_not_benefit_from_shared_warmth() {
    let (router, group, _content) = router_with_shared_warmth();
    let identity = RequestIdentity {
        tenant: TenantId::new(2), // a real member...
        group,
        content_fp: PrefixFingerprint::of(b"different prompt"), // ...presenting the wrong content.
    };
    match route_with_identity(&router, LONG_HEAD, Some(&identity)).expect("route") {
        RoutePath::Disaggregated { prefill, .. } => assert_eq!(prefill.label, "pf-cold"),
        other => panic!("expected disagg to the cold backend, got {other:?}"),
    }
}

#[test]
fn missing_identity_or_registry_falls_back_to_plain_route() {
    let (router, _group, _content) = router_with_shared_warmth();
    // `route_with_identity` with no identity is byte-for-byte `route`.
    let a = route_with_identity(&router, LONG_HEAD, None).expect("route");
    let b = route(&router, LONG_HEAD).expect("route");
    let label = |p: &RoutePath| match p {
        RoutePath::Disaggregated { prefill, .. } => prefill.label.clone(),
        other => panic!("expected disagg, got {other:?}"),
    };
    assert_eq!(label(&a), label(&b));
    // Plain `route` never sees the salted shared warmth at all (it looks up
    // raw, unsalted block ids), so it ties and falls back to "pf-cold" —
    // identical to the no-identity / no-registry behavior above.
    assert_eq!(label(&b), "pf-cold");
}
