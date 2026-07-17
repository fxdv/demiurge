//! Admission path selection (userspace bucket vs kernel XDP). [DEMI-XDP-SHED]

/// Which layer enforces L4 admission before L7 decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AdmitMode {
    /// Token bucket in userspace (`AdmitBucket`) on each accepted TCP connection.
    #[default]
    Userspace,
    /// Kernel XDP program on `DEMIURGE_XDP_IFACE` when attached; userspace
    /// bucket as fallback when the shed is missing (same honesty as Hybrid —
    /// never silently lose L4 admission).
    KernelXdp,
    /// Kernel XDP when attached; userspace bucket as fallback when not.
    Hybrid,
}

impl AdmitMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "userspace" | "user" | "bucket" => Some(Self::Userspace),
            "xdp" | "kernel" | "kernel_xdp" => Some(Self::KernelXdp),
            "hybrid" => Some(Self::Hybrid),
            _ => None,
        }
    }

    pub fn from_env() -> Self {
        std::env::var("DEMIURGE_ADMIT_MODE")
            .ok()
            .and_then(|v| Self::parse(&v))
            .unwrap_or(Self::Userspace)
    }

    /// Whether `handle_conn` should call userspace `AdmitBucket::try_admit`.
    /// Both `KernelXdp` and `Hybrid` fall back when the shed is not attached
    /// so a failed attach or cleared link never opens L4 unbounded.
    pub fn uses_userspace_admit(self, kernel_attached: bool) -> bool {
        match self {
            Self::Userspace => true,
            Self::KernelXdp | Self::Hybrid => !kernel_attached,
        }
    }

    pub fn wants_kernel(self) -> bool {
        matches!(self, Self::KernelXdp | Self::Hybrid)
    }
}
