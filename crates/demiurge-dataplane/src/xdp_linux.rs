//! XDP admit-shed loader (Linux + aya). [DEMI-XDP-SHED]
//!
//! Kernel floor: the BPF object uses BPF_ATOMIC fetch/cmpxchg (`-mcpu=v3`),
//! which requires Linux >= 5.12.

use std::path::{Path, PathBuf};

use aya::maps::{Array, MapError, PerCpuArray};
use aya::programs::{Xdp, XdpFlags};
use aya::{Ebpf, EbpfError, Pod};

use super::{XdpAdmitConfig, XdpAdmitShed, XdpAttachError, OBJECT_FILE, PROGRAM_NAME};

const STATE_KEY: u32 = 0;

/// `bpf_ktime_get_ns()` uses `CLOCK_MONOTONIC`; seed the refill clock in
/// userspace so an empty bucket never sees a bogus multi-second accrual when
/// `last_refill_ns` was left at zero and the per-packet CAS loses.
#[cfg(target_os = "linux")]
fn monotonic_ns() -> u64 {
    #[repr(C)]
    struct Timespec {
        tv_sec: i64,
        tv_nsec: i64,
    }
    extern "C" {
        fn clock_gettime(clk_id: i32, tp: *mut Timespec) -> i32;
    }
    const CLOCK_MONOTONIC: i32 = 1;
    let mut ts = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid out-pointer for one `timespec`.
    let rc = unsafe { clock_gettime(CLOCK_MONOTONIC, &mut ts) };
    if rc != 0 {
        return 0;
    }
    (ts.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(ts.tv_nsec as u64)
}

/// Layout must match `struct demi_admit_state` in `bpf/admit_shed.bpf.c`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct AdmitMapState {
    /// Signed on the kernel side: transient dips below zero under concurrent
    /// shed are expected and compensated (never wraps fail-open).
    tokens: i64,
    capacity: u64,
    refill_per_sec: u64,
    last_refill_ns: u64,
    /// Host byte order; 0 gates every TCP SYN on the interface.
    listen_port: u64,
}

unsafe impl Pod for AdmitMapState {}

/// Layout must match `struct demi_admit_stats` (per-CPU; userspace sums).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct AdmitMapStats {
    shed_total: u64,
    pass_total: u64,
}

unsafe impl Pod for AdmitMapStats {}

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

fn state_array(bpf: &Ebpf) -> Result<Array<&aya::maps::MapData, AdmitMapState>, XdpAttachError> {
    let map_handle = bpf
        .map("admit_state")
        .ok_or_else(|| XdpAttachError::LoadFailed("admit_state map missing".into()))?;
    Array::try_from(map_handle)
        .map_err(|e| XdpAttachError::LoadFailed(format!("admit_state array: {e}")))
}

fn read_admit_state(bpf: &Ebpf) -> Result<AdmitMapState, XdpAttachError> {
    state_array(bpf)?
        .get(&STATE_KEY, 0)
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

/// Sum a per-CPU stats field across all CPUs.
fn read_admit_stats(bpf: &Ebpf) -> Result<AdmitMapStats, XdpAttachError> {
    let map_handle = bpf
        .map("admit_stats")
        .ok_or_else(|| XdpAttachError::LoadFailed("admit_stats map missing".into()))?;
    let map: PerCpuArray<_, AdmitMapStats> = PerCpuArray::try_from(map_handle)
        .map_err(|e| XdpAttachError::LoadFailed(format!("admit_stats array: {e}")))?;
    let values = map
        .get(&STATE_KEY, 0)
        .map_err(|e| map_io("read admit_stats", e))?;
    let mut total = AdmitMapStats::default();
    for v in values.iter() {
        total.shed_total += v.shed_total;
        total.pass_total += v.pass_total;
    }
    Ok(total)
}

/// Initial attach: seed the whole state before the program goes live.
fn seed_admit_state(bpf: &mut Ebpf, config: &XdpAdmitConfig) -> Result<(), XdpAttachError> {
    let cap = config.capacity.max(1);
    write_admit_state(
        bpf,
        AdmitMapState {
            tokens: cap as i64,
            capacity: cap,
            refill_per_sec: config.refill_per_sec,
            last_refill_ns: monotonic_ns(),
            listen_port: u64::from(config.listen_port.unwrap_or(0)),
        },
    )
}

/// Actuation refill: reset tokens/capacity, preserve filter + refill config.
/// Stats live in a separate per-CPU map, so this write cannot clobber them.
fn reseed_admit_state(bpf: &mut Ebpf, capacity: u64) -> Result<(), XdpAttachError> {
    let cap = capacity.max(1);
    let mut state = read_admit_state(bpf)?;
    state.tokens = cap as i64;
    state.capacity = cap;
    state.last_refill_ns = monotonic_ns();
    write_admit_state(bpf, state)
}

/// Attach preference: forced SKB via env, otherwise driver/native first with
/// generic (SKB) fallback for NICs without native XDP support.
fn attach_program(program: &mut Xdp, iface: &str) -> Result<&'static str, XdpAttachError> {
    if let Ok(v) = std::env::var("DEMIURGE_XDP_FLAGS") {
        if v.eq_ignore_ascii_case("skb") {
            program
                .attach(iface, XdpFlags::SKB_MODE)
                .map_err(|e| XdpAttachError::AttachFailed(format!("{iface} (skb): {e}")))?;
            return Ok("skb");
        }
    }
    match program.attach(iface, XdpFlags::default()) {
        Ok(_) => Ok("driver"),
        Err(native_err) => match program.attach(iface, XdpFlags::SKB_MODE) {
            Ok(_) => Ok("skb-fallback"),
            Err(skb_err) => Err(XdpAttachError::AttachFailed(format!(
                "{iface}: native: {native_err}; skb: {skb_err}"
            ))),
        },
    }
}

/// Optional bpffs pinning (`DEMIURGE_BPF_PIN_DIR`): shed/pass telemetry
/// survives router restarts as long as the pinned files exist.
fn maybe_pin_maps(bpf: &Ebpf) -> Result<(), XdpAttachError> {
    let Ok(dir) = std::env::var("DEMIURGE_BPF_PIN_DIR") else {
        return Ok(());
    };
    let dir = PathBuf::from(dir);
    std::fs::create_dir_all(&dir)
        .map_err(|e| XdpAttachError::LoadFailed(format!("pin dir {}: {e}", dir.display())))?;
    let pin = |name: &str, path: PathBuf| -> Result<(), XdpAttachError> {
        // A stale pin from a previous run holds the old map; replace it.
        let _ = std::fs::remove_file(&path);
        let map_handle = bpf
            .map(name)
            .ok_or_else(|| XdpAttachError::LoadFailed(format!("{name} map missing")))?;
        match name {
            "admit_state" => Array::<_, AdmitMapState>::try_from(map_handle)
                .map_err(|e| XdpAttachError::LoadFailed(format!("{name} array: {e}")))?
                .pin(&path)
                .map_err(|e| XdpAttachError::LoadFailed(format!("pin {name}: {e}"))),
            _ => PerCpuArray::<_, AdmitMapStats>::try_from(map_handle)
                .map_err(|e| XdpAttachError::LoadFailed(format!("{name} array: {e}")))?
                .pin(&path)
                .map_err(|e| XdpAttachError::LoadFailed(format!("pin {name}: {e}"))),
        }
    };
    pin("admit_state", dir.join("admit_state"))?;
    pin("admit_stats", dir.join("admit_stats"))
}

pub fn attach(iface: &str, config: XdpAdmitConfig) -> Result<XdpAdmitShed, XdpAttachError> {
    let path = object_path();
    if !path.is_file() {
        return Err(XdpAttachError::ObjectNotBuilt);
    }
    attach_at(&path, iface, config)
}

pub fn attach_at(
    path: &Path,
    iface: &str,
    config: XdpAdmitConfig,
) -> Result<XdpAdmitShed, XdpAttachError> {
    if !path.is_file() {
        return Err(XdpAttachError::ObjectNotBuilt);
    }
    let mut bpf = Ebpf::load_file(path).map_err(|e| map_err("load file", e))?;
    seed_admit_state(&mut bpf, &config)?;

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
    let mode = attach_program(program, iface)?;
    maybe_pin_maps(&bpf)?;

    Ok(XdpAdmitShed {
        bpf,
        iface: iface.to_string(),
        mode,
    })
}

impl XdpAdmitShed {
    pub fn attach(iface: &str, config: XdpAdmitConfig) -> Result<Self, XdpAttachError> {
        attach(iface, config)
    }

    /// Available tokens (negative transients clamp to 0).
    pub fn available(&self) -> Result<u64, XdpAttachError> {
        Ok(read_admit_state(&self.bpf)?.tokens.max(0) as u64)
    }

    pub fn capacity(&self) -> Result<u64, XdpAttachError> {
        Ok(read_admit_state(&self.bpf)?.capacity)
    }

    pub fn shed_total(&self) -> Result<u64, XdpAttachError> {
        Ok(read_admit_stats(&self.bpf)?.shed_total)
    }

    /// SYNs admitted through the bucket (shed rate = shed / (shed + pass)).
    pub fn pass_total(&self) -> Result<u64, XdpAttachError> {
        Ok(read_admit_stats(&self.bpf)?.pass_total)
    }

    pub fn reseed(&mut self, capacity: u64) -> Result<(), XdpAttachError> {
        reseed_admit_state(&mut self.bpf, capacity)
    }

    /// Attach mode actually in effect: `driver`, `skb`, or `skb-fallback`.
    pub fn attach_mode(&self) -> &'static str {
        self.mode
    }

    /// Best-effort liveness: false once the interface is gone (its XDP link
    /// died with it). Admin-forced detach (`ip link set … xdp off`) is not
    /// detected. Hybrid mode uses this to fall back to the userspace bucket.
    pub fn link_alive(&self) -> bool {
        Path::new("/sys/class/net").join(&self.iface).exists()
    }

    pub fn object_path() -> PathBuf {
        object_path()
    }
}
