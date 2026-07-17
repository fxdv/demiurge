//! Deployment-keyed hashing for cache-domain salts and prefix fingerprints.
//! [DEMI-S1-DOMAIN] Threat-model G1.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::OnceLock;

static AUTH_SECRET: OnceLock<Vec<u8>> = OnceLock::new();

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

/// Keyed digest — secret + domain tag + data, then re-mixed. Not a full PRF,
/// but removes the public DefaultHasher offline collision surface of G1.
#[must_use]
pub fn keyed_hash(domain: &[u8], data: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    auth_secret().hash(&mut h);
    domain.hash(&mut h);
    data.hash(&mut h);
    let a = h.finish();
    let mut h2 = DefaultHasher::new();
    auth_secret().hash(&mut h2);
    a.hash(&mut h2);
    data.len().hash(&mut h2);
    h2.finish()
}
