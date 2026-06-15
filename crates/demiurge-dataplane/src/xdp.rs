//! Production XDP admission shed (Phase 5+). [DEMI-XDP-SHED]
//!
//! Userspace proof lives in [`super::AdmitBucket`]. The kernel program is
//! `bpf/admit_shed.bpf.c` (compile via `./scripts/build-bpf.sh`).

#[cfg(not(target_os = "linux"))]
use std::path::PathBuf;

/// Compiled object file name under `target/bpf/`.
pub const OBJECT_FILE: &str = "admit_shed.o";

/// BPF program section name.
pub const PROGRAM_NAME: &str = "xdp_admit_shed";

/// Default token-bucket capacity (matches `DATAPLANE_ADMIT_BURST` from params).
pub const DEFAULT_CAPACITY: u64 = demiurge_cost::DATAPLANE_ADMIT_BURST;

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
}

#[cfg(target_os = "linux")]
impl std::fmt::Debug for XdpAdmitShed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XdpAdmitShed").finish_non_exhaustive()
    }
}

#[cfg(not(target_os = "linux"))]
#[derive(Debug)]
pub struct XdpAdmitShed;

#[cfg(not(target_os = "linux"))]
impl XdpAdmitShed {
    pub fn attach(_iface: &str, _capacity: u64) -> Result<Self, XdpAttachError> {
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

    pub fn reseed(&mut self, _capacity: u64) -> Result<(), XdpAttachError> {
        Err(XdpAttachError::UnsupportedPlatform)
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
            XdpAdmitShed::attach("eth0", DEFAULT_CAPACITY),
            Err(XdpAttachError::UnsupportedPlatform)
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn xdp_object_not_built_when_missing() {
        use super::xdp_linux::attach_at;
        let missing = std::env::temp_dir().join("demiurge-no-bpf.o");
        let _ = std::fs::remove_file(&missing);
        assert!(matches!(
            attach_at(&missing, "lo", 8),
            Err(XdpAttachError::ObjectNotBuilt)
        ));
    }
}
