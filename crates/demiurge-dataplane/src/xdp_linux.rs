//! XDP admit-shed loader (Linux + aya). [DEMI-XDP-SHED]

use std::path::{Path, PathBuf};

use aya::maps::Array;
use aya::maps::MapError;
use aya::programs::{Xdp, XdpFlags};
use aya::{Ebpf, EbpfError, Pod};

use super::{XdpAdmitShed, XdpAttachError, OBJECT_FILE, PROGRAM_NAME};

const STATE_KEY: u32 = 0;

/// Layout must match `struct demi_admit_state` in `bpf/admit_shed.bpf.c`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct AdmitMapState {
    tokens: u64,
    capacity: u64,
    shed_total: u64,
}

unsafe impl Pod for AdmitMapState {}

fn xdp_attach_flags() -> XdpFlags {
    match std::env::var("DEMIURGE_XDP_FLAGS").as_deref() {
        Ok(v) if v.eq_ignore_ascii_case("skb") => XdpFlags::SKB_MODE,
        _ => XdpFlags::default(),
    }
}

pub fn object_path() -> PathBuf {
    if let Ok(path) = std::env::var("DEMIURGE_BPF_OBJECT") {
        return PathBuf::from(path);
    }
    let cwd_object = PathBuf::from("target/bpf").join(OBJECT_FILE);
    if cwd_object.is_file() {
        return cwd_object;
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/bpf")
        .join(OBJECT_FILE)
}

fn map_err(stage: &str, err: EbpfError) -> XdpAttachError {
    XdpAttachError::LoadFailed(format!("{stage}: {err}"))
}

fn map_io(stage: &str, err: MapError) -> XdpAttachError {
    XdpAttachError::LoadFailed(format!("{stage}: {err}"))
}

fn read_admit_state(bpf: &Ebpf) -> Result<AdmitMapState, XdpAttachError> {
    let map_handle = bpf
        .map("admit_state")
        .ok_or_else(|| XdpAttachError::LoadFailed("admit_state map missing".into()))?;
    let map: Array<_, AdmitMapState> = Array::try_from(map_handle)
        .map_err(|e| XdpAttachError::LoadFailed(format!("admit_state array: {e}")))?;
    map.get(&STATE_KEY, 0)
        .map_err(|e| map_io("read admit_state", e))
}

fn write_admit_state(bpf: &mut Ebpf, state: AdmitMapState) -> Result<(), XdpAttachError> {
    let map_handle = bpf
        .map_mut("admit_state")
        .ok_or_else(|| XdpAttachError::LoadFailed("admit_state map missing".into()))?;
    let mut map: Array<_, AdmitMapState> = Array::try_from(map_handle)
        .map_err(|e| XdpAttachError::LoadFailed(format!("admit_state array: {e}")))?;
    map.set(STATE_KEY, state, 0)
        .map_err(|e| map_io("write admit_state", e))
}

/// Initial attach: one map write seeds tokens/capacity; shed starts at zero.
fn seed_admit_state(bpf: &mut Ebpf, capacity: u64) -> Result<(), XdpAttachError> {
    let cap = capacity.max(1);
    write_admit_state(
        bpf,
        AdmitMapState {
            tokens: cap,
            capacity: cap,
            shed_total: 0,
        },
    )
}

/// Actuation refill: single struct write; preserve shed telemetry (matches userspace `AdmitBucket::reseed`).
fn reseed_admit_state(bpf: &mut Ebpf, capacity: u64) -> Result<(), XdpAttachError> {
    let cap = capacity.max(1);
    let mut state = read_admit_state(bpf).unwrap_or(AdmitMapState {
        tokens: cap,
        capacity: cap,
        shed_total: 0,
    });
    state.tokens = cap;
    state.capacity = cap;
    write_admit_state(bpf, state)
}

pub fn attach(iface: &str, capacity: u64) -> Result<XdpAdmitShed, XdpAttachError> {
    let path = object_path();
    if !path.is_file() {
        return Err(XdpAttachError::ObjectNotBuilt);
    }
    attach_at(&path, iface, capacity)
}

pub fn attach_at(path: &Path, iface: &str, capacity: u64) -> Result<XdpAdmitShed, XdpAttachError> {
    if !path.is_file() {
        return Err(XdpAttachError::ObjectNotBuilt);
    }
    let mut bpf = Ebpf::load_file(path).map_err(|e| map_err("load file", e))?;
    seed_admit_state(&mut bpf, capacity)?;

    let program: &mut Xdp = bpf
        .program_mut(PROGRAM_NAME)
        .ok_or_else(|| {
            XdpAttachError::LoadFailed(format!("program {PROGRAM_NAME} not in {}", path.display()))
        })?
        .try_into()
        .map_err(|e| XdpAttachError::LoadFailed(format!("program type: {e}")))?;
    program
        .load()
        .map_err(|e| XdpAttachError::LoadFailed(format!("program load: {e}")))?;
    program
        .attach(iface, xdp_attach_flags())
        .map_err(|e| XdpAttachError::AttachFailed(format!("{iface}: {e}")))?;

    Ok(XdpAdmitShed { bpf })
}

impl XdpAdmitShed {
    pub fn attach(iface: &str, capacity: u64) -> Result<Self, XdpAttachError> {
        attach(iface, capacity)
    }

    pub fn available(&self) -> Result<u64, XdpAttachError> {
        Ok(read_admit_state(&self.bpf)?.tokens)
    }

    pub fn capacity(&self) -> Result<u64, XdpAttachError> {
        Ok(read_admit_state(&self.bpf)?.capacity)
    }

    pub fn shed_total(&self) -> Result<u64, XdpAttachError> {
        Ok(read_admit_state(&self.bpf)?.shed_total)
    }

    pub fn reseed(&mut self, capacity: u64) -> Result<(), XdpAttachError> {
        reseed_admit_state(&mut self.bpf, capacity)
    }

    pub fn object_path() -> PathBuf {
        object_path()
    }
}
