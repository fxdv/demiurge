//! Shared-prefix cache authorization (Track C scaffolding). [DEMI-S1-DOMAIN]

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TenantId(u64);

impl TenantId {
    pub fn new(raw: u64) -> Self {
        Self(raw)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheDomainKey {
    pub tenant: TenantId,
    pub domain: u64,
}

/// Resolve a shared cache-domain key only for group members.
pub fn resolve_shared_key(
    member_tenants: &[TenantId],
    requester: TenantId,
    domain: u64,
) -> Option<CacheDomainKey> {
    if member_tenants.contains(&requester) {
        Some(CacheDomainKey {
            tenant: requester,
            domain,
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_member_never_resolves_shared_key() {
        let members = [TenantId::new(1), TenantId::new(2)];
        assert!(resolve_shared_key(&members, TenantId::new(1), 42).is_some());
        assert!(resolve_shared_key(&members, TenantId::new(99), 42).is_none());
    }
}
