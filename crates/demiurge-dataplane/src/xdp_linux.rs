//! XDP admit-shed loader (Linux + aya). [DEMI-XDP-SHED]

use std::path::{Path, PathBuf};

use aya::maps::Array;
use aya::maps::MapError;
use aya::programs::{Xdp, XdpFlags};
use aya::{Ebpf, EbpfError};

use super::{XdpAdmitShed, XdpAttachError, OBJECT_FILE, PROGRAM_NAME};

const KEY_TOKENS: u32 = 0;
const KEY_CAPACITY: u32 = 1;
const KEY_SHED_TOTAL: u32 = 2;

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

fn seed_admit_state(bpf: &mut Ebpf, capacity: u64) -> Result<(), XdpAttachError> {
    let cap = capacity.max(1);
    let map_handle = bpf
        .map_mut("admit_state")
        .ok_or_else(|| XdpAttachError::LoadFailed("admit_state map missing".into()))?;
    let mut map: Array<_, u64> = Array::try_from(map_handle)
        .map_err(|e| XdpAttachError::LoadFailed(format!("admit_state array: {e}")))?;
    map.set(KEY_TOKENS, cap, 0)
        .map_err(|e| map_io("seed tokens", e))?;
    map.set(KEY_CAPACITY, cap, 0)
        .map_err(|e| map_io("seed capacity", e))?;
    map.set(KEY_SHED_TOTAL, 0, 0)
        .map_err(|e| map_io("seed shed_total", e))?;
    Ok(())
}

fn read_map(bpf: &Ebpf, key: u32) -> Result<u64, XdpAttachError> {
    let map_handle = bpf
        .map("admit_state")
        .ok_or_else(|| XdpAttachError::LoadFailed("admit_state map missing".into()))?;
    let map: Array<_, u64> = Array::try_from(map_handle)
        .map_err(|e| XdpAttachError::LoadFailed(format!("admit_state array: {e}")))?;
    map.get(&key, 0).map_err(|e| map_io("map get", e))
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
        read_map(&self.bpf, KEY_TOKENS)
    }

    pub fn capacity(&self) -> Result<u64, XdpAttachError> {
        read_map(&self.bpf, KEY_CAPACITY)
    }

    pub fn shed_total(&self) -> Result<u64, XdpAttachError> {
        read_map(&self.bpf, KEY_SHED_TOTAL)
    }

    pub fn reseed(&mut self, capacity: u64) -> Result<(), XdpAttachError> {
        seed_admit_state(&mut self.bpf, capacity)
    }

    pub fn object_path() -> PathBuf {
        object_path()
    }
}
