//! Block-granularity KV warmth maps per backend. [DEMI-WARM-DISCOUNT]

use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};

use demiurge_auth::{
    CacheDomainKey, GroupId, PrefixFingerprint, SharedPrefixGroupRegistry, TenantId,
};
use demiurge_cost::CACHE_BLOCK_TOKENS;
use demiurge_cost::CACHE_CUCKOO_MAX_LOADFACTOR;

/// Mix a routing-key block with a cache-domain salt so two domains never alias.
#[must_use]
fn salt_block(block_id: u64, salt: u64) -> u64 {
    let mut h = DefaultHasher::new();
    (block_id, salt).hash(&mut h);
    h.finish()
}

/// Salt a list of routing blocks under `key`'s cache domain. [DEMI-S1-DOMAIN]
#[must_use]
pub fn salted_blocks(blocks: &[u64], key: &CacheDomainKey) -> Vec<u64> {
    let salt = key.salt();
    blocks.iter().map(|b| salt_block(*b, salt)).collect()
}

/// Routing-key blocks covered by a prompt (block-aligned).
pub fn routing_blocks(prompt_tokens: u64, block_tokens: u64) -> Vec<u64> {
    let block = block_tokens.max(1);
    let blocks = prompt_tokens.div_ceil(block);
    (0..blocks).map(|i| i.saturating_mul(block)).collect()
}

/// Default block size from canonical params.
pub fn default_routing_blocks(prompt_tokens: u64) -> Vec<u64> {
    routing_blocks(prompt_tokens, CACHE_BLOCK_TOKENS)
}

#[derive(Debug, Clone)]
pub struct WarmthMap {
    keys: HashSet<u64>,
    capacity: usize,
}

impl WarmthMap {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            keys: HashSet::new(),
            capacity: capacity.max(1),
        }
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn load_factor(&self) -> f64 {
        self.keys.len() as f64 / self.capacity as f64
    }

    pub fn insert(&mut self, block_id: u64) -> bool {
        if self.keys.contains(&block_id) {
            return true;
        }
        if self.load_factor() >= CACHE_CUCKOO_MAX_LOADFACTOR {
            return false;
        }
        self.keys.insert(block_id);
        true
    }

    pub fn contains(&self, block_id: u64) -> bool {
        self.keys.contains(&block_id)
    }

    /// Fraction of routing blocks present in the map, in `[0, 1]`.
    pub fn hit_strength(&self, blocks: &[u64]) -> f64 {
        if blocks.is_empty() {
            return 0.0;
        }
        let hits = blocks.iter().filter(|b| self.keys.contains(b)).count();
        hits as f64 / blocks.len() as f64
    }

    pub fn merge(&mut self, other: &WarmthMap) {
        for &key in &other.keys {
            let _ = self.insert(key);
        }
    }

    /// Insert a routing block recorded under `key`'s cache domain. The block id
    /// is salted, so the same raw block produced by two distinct domains lands
    /// at different keys and never aliases. [DEMI-S1-DOMAIN]
    pub fn insert_salted(&mut self, block_id: u64, key: &CacheDomainKey) -> bool {
        self.insert(salt_block(block_id, key.salt()))
    }

    /// Hit strength for `blocks` probed under `key`'s cache domain. A lookup
    /// under a different domain key sees none of another domain's warmth.
    /// [DEMI-S1-DOMAIN]
    #[must_use]
    pub fn hit_strength_salted(&self, blocks: &[u64], key: &CacheDomainKey) -> f64 {
        self.hit_strength(&salted_blocks(blocks, key))
    }
}

/// Membership-gated warmth lookup: resolve the request's cache-domain key on the
/// strongly consistent authorization path *first*, then measure warmth only
/// under the resolved domain. A non-member or template mismatch falls back to
/// the tenant-private domain, so it can only ever hit its own warmth — never the
/// shared cache. This realizes "membership checked before any discount applies".
/// [DEMI-S1-DOMAIN]
#[must_use]
pub fn gated_hit_strength(
    warmth: &WarmthMap,
    registry: &SharedPrefixGroupRegistry,
    requester: TenantId,
    group: GroupId,
    content_fp: PrefixFingerprint,
    blocks: &[u64],
) -> f64 {
    let key = registry.resolve_domain_key(requester, group, content_fp);
    warmth.hit_strength_salted(blocks, &key)
}

impl Default for WarmthMap {
    fn default() -> Self {
        Self::with_capacity(1024)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenant_private_warmth_not_shared_across_tenants() {
        let blocks = [0u64, 256, 512];
        let key_a = demiurge_auth::private_domain_key(TenantId::new(1), 0);
        let key_b = demiurge_auth::private_domain_key(TenantId::new(2), 0);

        let mut warmth = WarmthMap::default();
        for &b in &blocks {
            warmth.insert_salted(b, &key_a);
        }

        // Tenant A sees its own warmth; tenant B (same raw blocks) sees none.
        assert_eq!(warmth.hit_strength_salted(&blocks, &key_a), 1.0);
        assert_eq!(warmth.hit_strength_salted(&blocks, &key_b), 0.0);
    }

    #[test]
    fn shared_group_members_share_salted_warmth() {
        let blocks = [0u64, 256, 512];
        let content = PrefixFingerprint::of(b"shared system prompt");
        let mut reg = SharedPrefixGroupRegistry::new();
        reg.register_template(
            GroupId::new(7),
            [TenantId::new(1), TenantId::new(2)],
            content,
            42,
        );

        // Member 1 warms the cache under the resolved shared domain key.
        let shared = reg
            .resolve_shared_key(TenantId::new(1), GroupId::new(7), content)
            .expect("member resolves shared key");
        let mut warmth = WarmthMap::default();
        for &b in &blocks {
            warmth.insert_salted(b, &shared);
        }

        // Member 2 presenting matching content shares the warmth.
        assert_eq!(
            gated_hit_strength(
                &warmth,
                &reg,
                TenantId::new(2),
                GroupId::new(7),
                content,
                &blocks
            ),
            1.0
        );
        // Non-member presenting the same content is isolated (private fallback).
        assert_eq!(
            gated_hit_strength(
                &warmth,
                &reg,
                TenantId::new(99),
                GroupId::new(7),
                content,
                &blocks
            ),
            0.0
        );
        // Member presenting non-matching content gets no shared hit.
        let wrong = PrefixFingerprint::of(b"different prompt");
        assert_eq!(
            gated_hit_strength(
                &warmth,
                &reg,
                TenantId::new(1),
                GroupId::new(7),
                wrong,
                &blocks
            ),
            0.0
        );
    }
}
