//! Deployment-keyed hashing for cache-domain salts and prefix fingerprints.
//! [DEMI-S1-DOMAIN] Threat-model G1 / G1b.

use std::sync::OnceLock;

static AUTH_SECRET: OnceLock<Vec<u8>> = OnceLock::new();
static AUTH_KEY: OnceLock<[u8; blake3::KEY_LEN]> = OnceLock::new();

/// BLAKE3 derive_key context — hardcoded, app-specific (see blake3::derive_key).
const AUTH_KEY_CONTEXT: &str = "demiurge auth 2026-07-17 keyed salt/fp v1";

/// Install the deployment secret used by [`keyed_hash`]. Idempotent: the first
/// call wins. Prefer setting `DEMIURGE_AUTH_SECRET` before any salt/fp work.
pub fn configure_auth_secret(secret: impl Into<Vec<u8>>) {
    let _ = AUTH_SECRET.set(secret.into());
}

/// Load `DEMIURGE_AUTH_SECRET` when present; otherwise a fixed **dev-only**
/// placeholder (offline-attackable — production must set the env var).
pub fn configure_auth_secret_from_env() {
    if let Ok(secret) = std::env::var("DEMIURGE_AUTH_SECRET") {
        if !secret.is_empty() {
            configure_auth_secret(secret.into_bytes());
        }
    }
}

fn auth_secret() -> &'static [u8] {
    AUTH_SECRET.get_or_init(|| {
        std::env::var("DEMIURGE_AUTH_SECRET")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|s| s.into_bytes())
            .unwrap_or_else(|| b"demiurge-unkeyed-dev-only".to_vec())
    })
}

fn auth_key() -> &'static [u8; blake3::KEY_LEN] {
    AUTH_KEY.get_or_init(|| blake3::derive_key(AUTH_KEY_CONTEXT, auth_secret()))
}

/// Keyed PRF digest (BLAKE3 keyed hash) over `domain || 0x00 || data`.
/// Returns the first 8 bytes of the 256-bit MAC as a little-endian `u64`.
#[must_use]
pub fn keyed_hash(domain: &[u8], data: &[u8]) -> u64 {
    let mut hasher = blake3::Hasher::new_keyed(auth_key());
    hasher.update(domain);
    hasher.update(&[0]);
    hasher.update(data);
    let hash = hasher.finalize();
    let mut out = [0u8; 8];
    out.copy_from_slice(&hash.as_bytes()[..8]);
    u64::from_le_bytes(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyed_hash_stable_and_domain_separated() {
        let a = keyed_hash(b"prefix-fp", b"hello");
        let b = keyed_hash(b"prefix-fp", b"hello");
        let c = keyed_hash(b"cache-domain-salt", b"hello");
        let d = keyed_hash(b"prefix-fp", b"other");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
    }

    #[test]
    fn keyed_hash_differs_from_plain_blake3() {
        // Must not equal an unkeyed BLAKE3 truncation of the same layout.
        let mut h = blake3::Hasher::new();
        h.update(b"prefix-fp");
        h.update(&[0]);
        h.update(b"hello");
        let plain = {
            let mut out = [0u8; 8];
            out.copy_from_slice(&h.finalize().as_bytes()[..8]);
            u64::from_le_bytes(out)
        };
        assert_ne!(keyed_hash(b"prefix-fp", b"hello"), plain);
    }
}
