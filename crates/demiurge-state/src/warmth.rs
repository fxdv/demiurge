//! Block-granularity KV warmth maps per backend. [DEMI-WARM-DISCOUNT]

use std::collections::HashSet;

use demiurge_cost::CACHE_BLOCK_TOKENS;
use demiurge_cost::CACHE_CUCKOO_MAX_LOADFACTOR;

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
}

impl Default for WarmthMap {
    fn default() -> Self {
        Self::with_capacity(1024)
    }
}
