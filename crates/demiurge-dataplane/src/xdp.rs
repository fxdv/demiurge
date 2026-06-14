//! Production XDP admission shed (Phase 5+). [DEMI-XDP-SHED]
//!
//! Userspace proof lives in [`super::AdmitBucket`]. The kernel program is
//! `bpf/admit_shed.bpf.c` (compile via `./scripts/build-bpf.sh`). Runtime attach
//! with `aya`/libbpf follows in a later P5+ PR.

/// Compiled object file name under `target/bpf/`.
pub const OBJECT_FILE: &str = "admit_shed.o";

/// BPF program section name.
pub const PROGRAM_NAME: &str = "xdp_admit_shed";

/// Default token-bucket capacity (matches `DATAPLANE_ADMIT_BURST` from params).
pub const DEFAULT_CAPACITY: u64 = demiurge_cost::DATAPLANE_ADMIT_BURST;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XdpAttachError {
    UnsupportedPlatform,
    ObjectNotBuilt,
    AttachFailed,
}

impl std::fmt::Display for XdpAttachError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedPlatform => write!(f, "XDP attach requires Linux"),
            Self::ObjectNotBuilt => {
                write!(
                    f,
                    "missing target/bpf/{OBJECT_FILE}; run ./scripts/build-bpf.sh"
                )
            }
            Self::AttachFailed => write!(f, "XDP program attach failed"),
        }
    }
}

impl std::error::Error for XdpAttachError {}

/// Placeholder for production XDP loader (Phase 5+).
#[derive(Debug)]
pub struct XdpAdmitShed;

impl XdpAdmitShed {
    /// Attach the admit-shed XDP program to `iface`. Not yet wired — returns
    /// [`XdpAttachError::ObjectNotBuilt`] until libbpf/aya integration lands.
    pub fn attach(_iface: &str, _capacity: u64) -> Result<Self, XdpAttachError> {
        #[cfg(target_os = "linux")]
        {
            let path = std::path::Path::new("target/bpf").join(OBJECT_FILE);
            if !path.is_file() {
                return Err(XdpAttachError::ObjectNotBuilt);
            }
            // TODO(P5+): load with aya, seed admit_state map, attach XDP hook.
            Err(XdpAttachError::AttachFailed)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (_iface, _capacity);
            Err(XdpAttachError::UnsupportedPlatform)
        }
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
}
