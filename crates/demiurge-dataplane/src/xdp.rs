//! Production XDP admission shed (Phase 5+). [DEMI-XDP-SHED]
//!
//! Userspace proof lives in [`super::AdmitBucket`]. The kernel program is
//! `bpf/admit_shed.bpf.c` (compile via `./scripts/build-bpf.sh`).
//!
//! The kernel bucket gates *new work only* (TCP SYN, optionally one listen
//! port) and refills itself at `refill_per_sec` — no userspace liveness
//! dependency. Kernel floor: Linux >= 5.12 (BPF_ATOMIC, `-mcpu=v3`).

#[cfg(not(target_os = "linux"))]
use std::path::PathBuf;

/// Compiled object file name under `target/bpf/`.
pub const OBJECT_FILE: &str = "admit_shed.o";

/// BPF program section name.
pub const PROGRAM_NAME: &str = "xdp_admit_shed";

/// Default token-bucket capacity (matches `DATAPLANE_ADMIT_BURST` from params).
pub const DEFAULT_CAPACITY: u64 = demiurge_cost::DATAPLANE_ADMIT_BURST;

/// Kernel admit-shed configuration, seeded into the BPF map at attach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XdpAdmitConfig {
    /// Token-bucket capacity (burst).
    pub capacity: u64,
    /// Tokens accrued per second inside the kernel; 0 disables refill
    /// (the bucket then only recovers via `reseed`).
    pub refill_per_sec: u64,
    /// Gate SYNs to this destination port only; `None` gates every TCP SYN
    /// on the interface.
    pub listen_port: Option<u16>,
}

impl XdpAdmitConfig {
    pub fn with_capacity(capacity: u64) -> Self {
        Self {
            capacity,
            refill_per_sec: demiurge_cost::DATAPLANE_ADMIT_REFILL_PER_SEC,
            listen_port: None,
        }
    }
}

impl Default for XdpAdmitConfig {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum XdpAttachError {
    UnsupportedPlatform,
    ObjectNotBuilt,
    LoadFailed(String),
    AttachFailed(String),
}

impl std::fmt::Display for XdpAttachError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedPlatform => write!(f, "XDP attach requires Linux"),
            Self::ObjectNotBuilt => write!(
                f,
                "missing BPF object ({PROGRAM_NAME}); run ./scripts/build-bpf.sh"
            ),
            Self::LoadFailed(msg) | Self::AttachFailed(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for XdpAttachError {}

#[cfg(target_os = "linux")]
#[path = "xdp_linux.rs"]
mod xdp_linux;

/// Production XDP admit-shed loader (Linux attaches via aya; other platforms stub).
#[cfg(target_os = "linux")]
pub struct XdpAdmitShed {
    pub(super) bpf: aya::Ebpf,
    pub(super) iface: String,
    pub(super) mode: &'static str,
}

#[cfg(target_os = "linux")]
impl std::fmt::Debug for XdpAdmitShed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XdpAdmitShed")
            .field("iface", &self.iface)
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

#[cfg(not(target_os = "linux"))]
#[derive(Debug)]
pub struct XdpAdmitShed;

#[cfg(not(target_os = "linux"))]
impl XdpAdmitShed {
    pub fn attach(_iface: &str, _config: XdpAdmitConfig) -> Result<Self, XdpAttachError> {
        Err(XdpAttachError::UnsupportedPlatform)
    }

    pub fn available(&self) -> Result<u64, XdpAttachError> {
        let _ = self;
        Err(XdpAttachError::UnsupportedPlatform)
    }

    pub fn capacity(&self) -> Result<u64, XdpAttachError> {
        let _ = self;
        Err(XdpAttachError::UnsupportedPlatform)
    }

    pub fn shed_total(&self) -> Result<u64, XdpAttachError> {
        let _ = self;
        Err(XdpAttachError::UnsupportedPlatform)
    }

    pub fn pass_total(&self) -> Result<u64, XdpAttachError> {
        let _ = self;
        Err(XdpAttachError::UnsupportedPlatform)
    }

    pub fn reseed(&mut self, _capacity: u64) -> Result<(), XdpAttachError> {
        Err(XdpAttachError::UnsupportedPlatform)
    }

    pub fn attach_mode(&self) -> &'static str {
        "unsupported"
    }

    pub fn link_alive(&self) -> bool {
        false
    }

    pub fn object_path() -> PathBuf {
        PathBuf::from("target/bpf").join(OBJECT_FILE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdp_attach_unsupported_off_linux() {
        if cfg!(target_os = "linux") {
            return;
        }
        assert!(matches!(
            XdpAdmitShed::attach("eth0", XdpAdmitConfig::default()),
            Err(XdpAttachError::UnsupportedPlatform)
        ));
    }

    #[test]
    fn xdp_admit_config_defaults_from_params() {
        let cfg = XdpAdmitConfig::default();
        assert_eq!(cfg.capacity, DEFAULT_CAPACITY);
        assert_eq!(
            cfg.refill_per_sec,
            demiurge_cost::DATAPLANE_ADMIT_REFILL_PER_SEC
        );
        assert_eq!(cfg.listen_port, None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn xdp_object_not_built_when_missing() {
        use super::xdp_linux::attach_at;
        let missing = std::env::temp_dir().join("demiurge-no-bpf.o");
        let _ = std::fs::remove_file(&missing);
        assert!(matches!(
            attach_at(&missing, "lo", XdpAdmitConfig::with_capacity(8)),
            Err(XdpAttachError::ObjectNotBuilt)
        ));
    }
}
