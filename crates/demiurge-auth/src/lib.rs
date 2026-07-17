//! Shared-prefix cache authorization (Track C). [DEMI-S1-DOMAIN]
//!
//! Cross-tenant prefix-cache reuse is opt-in via a Shared-Prefix Group with a
//! content-verified template. Cache-domain keys are tenant-salted: members of a
//! group that present matching content resolve to a *shared* key (identical salt
//! across members), while everyone else falls back to a tenant-*private* key.
//! A non-member never resolves to a shared cache-domain key. Membership and
//! template match are checked on the strongly consistent (synchronous,
//! in-process) authorization path before any warmth discount is applied.
//!
//! Salts and prefix fingerprints use a BLAKE3 keyed PRF over `DEMIURGE_AUTH_SECRET` (G1/G1b).

mod secret;

pub use secret::{configure_auth_secret, configure_auth_secret_from_env, keyed_hash};

use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TenantId(u64);

impl TenantId {
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GroupId(u64);

impl GroupId {
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Content fingerprint of a shared-prefix template (e.g. a system-prompt hash).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PrefixFingerprint(u64);

impl PrefixFingerprint {
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Fingerprint arbitrary prefix bytes with the deployment auth secret (G1).
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self(keyed_hash(b"prefix-fp", bytes))
    }

    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Tenant-salted cache-domain key. Warmth lookups salt block ids with
/// [`CacheDomainKey::salt`] so distinct domains never alias. A `shared` key
/// derives its salt from the *group*, so members collide on purpose; a private
/// key derives its salt from the *tenant*, so tenants are isolated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheDomainKey {
    /// Issuing identity: group id when `shared`, tenant id when private.
    pub owner: u64,
    pub domain: u64,
    pub shared: bool,
}

impl CacheDomainKey {
    /// Salt mixed into warmth block ids. Identical for all members of a shared
    /// domain; unique per tenant for a private domain. Keyed with the
    /// deployment auth secret so salts are not offline-computable (G1).
    #[must_use]
    pub fn salt(&self) -> u64 {
        let mut payload = [0u8; 17];
        payload[..8].copy_from_slice(&self.owner.to_le_bytes());
        payload[8..16].copy_from_slice(&self.domain.to_le_bytes());
        payload[16] = u8::from(self.shared);
        keyed_hash(b"cache-domain-salt", &payload)
    }
}

/// Tenant-private cache-domain key — never shared; isolates a tenant's warmth.
#[must_use]
pub fn private_domain_key(tenant: TenantId, domain: u64) -> CacheDomainKey {
    CacheDomainKey {
        owner: tenant.raw(),
        domain,
        shared: false,
    }
}

#[derive(Debug, Clone)]
struct GroupEntry {
    members: HashSet<TenantId>,
    template: PrefixFingerprint,
    domain: u64,
}

/// Strongly consistent (synchronous, in-process) Shared-Prefix Group authority.
///
/// On Track A this stands in for the control-plane consensus path: membership
/// and the registered content template are read synchronously, so a stale
/// "authorized share" is impossible — the registry is the single source.
#[derive(Debug, Clone, Default)]
pub struct SharedPrefixGroupRegistry {
    groups: HashMap<GroupId, GroupEntry>,
}

impl SharedPrefixGroupRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            groups: HashMap::new(),
        }
    }

    /// Register (or replace) a group's members and content-verified template.
    /// This is the `RegisterTemplate` op on the strongly consistent path.
    /// [DEMI-S1-DOMAIN]
    pub fn register_template(
        &mut self,
        group: GroupId,
        members: impl IntoIterator<Item = TenantId>,
        template: PrefixFingerprint,
        domain: u64,
    ) {
        self.groups.insert(
            group,
            GroupEntry {
                members: members.into_iter().collect(),
                template,
                domain,
            },
        );
    }

    #[must_use]
    pub fn is_member(&self, group: GroupId, tenant: TenantId) -> bool {
        self.groups
            .get(&group)
            .is_some_and(|g| g.members.contains(&tenant))
    }

    /// `MatchTemplate`: content-verify a request prefix against the registered
    /// template for `group`.
    #[must_use]
    pub fn matches_template(&self, group: GroupId, fp: PrefixFingerprint) -> bool {
        self.groups.get(&group).is_some_and(|g| g.template == fp)
    }

    /// Resolve a *shared* cache-domain key — only for a member presenting
    /// content that matches the registered template. A non-member or a template
    /// mismatch yields `None`, never a shared key. [DEMI-S1-DOMAIN]
    #[must_use]
    pub fn resolve_shared_key(
        &self,
        requester: TenantId,
        group: GroupId,
        content_fp: PrefixFingerprint,
    ) -> Option<CacheDomainKey> {
        let entry = self.groups.get(&group)?;
        if !entry.members.contains(&requester) {
            return None;
        }
        if entry.template != content_fp {
            return None;
        }
        Some(CacheDomainKey {
            owner: group.raw(),
            domain: entry.domain,
            shared: true,
        })
    }

    /// Resolve the effective cache-domain key for a request: the shared key when
    /// authorized, else the tenant-private key. The private fallback can only
    /// ever hit the requester's own warmth, never the shared domain.
    /// [DEMI-S1-DOMAIN]
    #[must_use]
    pub fn resolve_domain_key(
        &self,
        requester: TenantId,
        group: GroupId,
        content_fp: PrefixFingerprint,
    ) -> CacheDomainKey {
        self.resolve_shared_key(requester, group, content_fp)
            .unwrap_or_else(|| private_domain_key(requester, 0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> SharedPrefixGroupRegistry {
        let mut reg = SharedPrefixGroupRegistry::new();
        reg.register_template(
            GroupId::new(7),
            [TenantId::new(1), TenantId::new(2)],
            PrefixFingerprint::of(b"shared system prompt"),
            42,
        );
        reg
    }

    #[test]
    fn keyed_salt_differs_from_unmixed_layout() {
        let key = CacheDomainKey {
            owner: 7,
            domain: 42,
            shared: true,
        };
        // Stable for a fixed secret; changing DEMIURGE_AUTH_SECRET changes salts.
        assert_ne!(key.salt(), 0);
        assert_eq!(
            PrefixFingerprint::of(b"shared system prompt"),
            PrefixFingerprint::of(b"shared system prompt")
        );
        assert_ne!(
            PrefixFingerprint::of(b"shared system prompt"),
            PrefixFingerprint::of(b"other prompt")
        );
    }

    #[test]
    fn non_member_never_resolves_shared_key() {
        let reg = registry();
        let fp = PrefixFingerprint::of(b"shared system prompt");
        // Member resolves; non-member does not, even with matching content.
        assert!(reg
            .resolve_shared_key(TenantId::new(1), GroupId::new(7), fp)
            .is_some());
        assert!(reg
            .resolve_shared_key(TenantId::new(99), GroupId::new(7), fp)
            .is_none());
        // Unknown group never resolves.
        assert!(reg
            .resolve_shared_key(TenantId::new(1), GroupId::new(8), fp)
            .is_none());
    }

    #[test]
    fn member_with_matching_template_resolves_shared_key() {
        let reg = registry();
        let fp = PrefixFingerprint::of(b"shared system prompt");
        let k1 = reg
            .resolve_shared_key(TenantId::new(1), GroupId::new(7), fp)
            .expect("member 1");
        let k2 = reg
            .resolve_shared_key(TenantId::new(2), GroupId::new(7), fp)
            .expect("member 2");
        // Both members resolve to the SAME shared salt (cache actually shared).
        assert!(k1.shared && k2.shared);
        assert_eq!(k1.salt(), k2.salt());
        // A private key for either member is distinct from the shared salt.
        assert_ne!(k1.salt(), private_domain_key(TenantId::new(1), 0).salt());
    }

    #[test]
    fn template_mismatch_no_shared_key() {
        let reg = registry();
        let wrong = PrefixFingerprint::of(b"different prompt");
        // Co-member presenting non-matching content gets no shared key.
        assert!(reg
            .resolve_shared_key(TenantId::new(1), GroupId::new(7), wrong)
            .is_none());
        assert!(!reg.matches_template(GroupId::new(7), wrong));
    }

    #[test]
    fn non_member_isolated_under_fuzz() {
        let reg = registry();
        let good = PrefixFingerprint::of(b"shared system prompt");
        // Deterministic LCG over (tenant, group, content) combinations.
        let mut state: u64 = 0x9E3779B97F4A7C15;
        for _ in 0..50_000 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let tenant = TenantId::new((state >> 33) % 200);
            let group = GroupId::new((state >> 20) % 16);
            let fp = if state & 1 == 0 {
                good
            } else {
                PrefixFingerprint::new(state)
            };
            if let Some(key) = reg.resolve_shared_key(tenant, group, fp) {
                // Any Some implies BOTH membership and template match held.
                assert!(reg.is_member(group, tenant));
                assert!(reg.matches_template(group, fp));
                assert!(key.shared);
            }
        }
    }
}
