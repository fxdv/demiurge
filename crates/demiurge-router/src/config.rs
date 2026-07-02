//! Pool, topology, and cache-group spec parsing plus `DEMIURGE_*` env flags.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use demiurge_cost::{TopologyId, POOL_ACTUATION_ENABLED};

use crate::http::parse_u64_maybe_hex;
use crate::{Backend, GroupId, PrefixFingerprint, SharedPrefixGroupRegistry, TenantId};

pub fn parse_topology_map(spec: &str) -> Result<HashMap<String, TopologyId>, String> {
    let mut out = HashMap::new();
    for item in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (label, rest) = item
            .split_once('@')
            .ok_or_else(|| format!("bad topology spec {item:?}; want label@node/rack/cluster"))?;
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() != 3 {
            return Err(format!(
                "bad topology spec {item:?}; want label@node/rack/cluster"
            ));
        }
        out.insert(
            label.to_string(),
            TopologyId::new(parts[0], parts[1], parts[2]),
        );
    }
    Ok(out)
}

pub fn parse_pool(spec: &str) -> Result<Vec<Arc<Backend>>, String> {
    parse_pool_with_topology(spec, &HashMap::new())
}

pub fn parse_pool_with_topology(
    spec: &str,
    topology: &HashMap<String, TopologyId>,
) -> Result<Vec<Arc<Backend>>, String> {
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
        let topo = topology.get(parts[0]).cloned().unwrap_or_default();
        out.push(Backend::new_with_topology(parts[0], addr, secs, topo));
    }
    Ok(out)
}

/// Parse a Shared-Prefix Group registry spec:
/// `group@domain@template_fp@tenant1+tenant2,...` (values decimal or `0x` hex).
/// Empty spec yields `None` (identity-gated routing disabled). [DEMI-S1-DOMAIN]
pub fn parse_cache_groups(spec: &str) -> Result<Option<SharedPrefixGroupRegistry>, String> {
    let mut registry = SharedPrefixGroupRegistry::new();
    let mut any = false;
    for item in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let parts: Vec<&str> = item.split('@').collect();
        if parts.len() != 4 {
            return Err(format!(
                "bad cache group spec {item:?}; want group@domain@template_fp@tenant1+tenant2"
            ));
        }
        let field = |name: &str, raw: &str| -> Result<u64, String> {
            parse_u64_maybe_hex(raw.as_bytes())
                .ok_or_else(|| format!("bad {name} {raw:?} in cache group spec {item:?}"))
        };
        let group = field("group", parts[0])?;
        let domain = field("domain", parts[1])?;
        let fp = field("template_fp", parts[2])?;
        let members = parts[3]
            .split('+')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|t| field("tenant", t).map(TenantId::new))
            .collect::<Result<Vec<_>, _>>()?;
        if members.is_empty() {
            return Err(format!("cache group spec {item:?} lists no tenants"));
        }
        registry.register_template(
            GroupId::new(group),
            members,
            PrefixFingerprint::new(fp),
            domain,
        );
        any = true;
    }
    Ok(any.then_some(registry))
}

/// Read a `DEMIURGE_*` env var as a boolean flag, falling back to `default`.
/// Accepted truthy values: `"1"`, `"true"`, `"yes"`.
fn env_bool(key: &str, default: bool) -> bool {
    std::env::var(key)
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
        .unwrap_or(default)
}

pub(crate) fn rebalancer_actuation_enabled() -> bool {
    env_bool("DEMIURGE_REBALANCER_ACTUATE", POOL_ACTUATION_ENABLED)
}

pub(crate) fn rdma_routing_enabled() -> bool {
    env_bool("DEMIURGE_RDMA_ROUTING", false)
}
