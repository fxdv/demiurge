//! XDP admit-shed loader (Linux + aya). [DEMI-XDP-SHED]
//!
//! Kernel floor: the BPF object uses BPF_ATOMIC fetch/cmpxchg (`-mcpu=v3`),
//! which requires Linux >= 5.12.

use std::path::{Path, PathBuf};

use aya::maps::{Array, MapError, PerCpuArray};
use aya::programs::Xdp;
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

/// Netlink XDP attach (not aya `bpf_link`): visible to `IFLA_XDP` / `ip link`,
/// and detachable via `ip link set … xdp[generic] off` — required for Hybrid G5b.
///
/// Flags mirror linux/if_link.h `XDP_FLAGS_*`. Detach must use the same mode
/// bit as attach: flags=0 / DRV clears only driver XDP; SKB needs SKB_MODE
/// (`ip link set … xdpgeneric off`).
const XDP_FLAGS_SKB_MODE: u32 = 1 << 1;
const XDP_FLAGS_DRV_MODE: u32 = 1 << 2;

fn xdp_detach_flags(mode: &str) -> u32 {
    match mode {
        "skb" | "skb-fallback" => XDP_FLAGS_SKB_MODE,
        _ => XDP_FLAGS_DRV_MODE,
    }
}

/// Attach preference: forced SKB via env, otherwise driver/native first with
/// generic (SKB) fallback for NICs without native XDP support.
fn attach_program(program: &Xdp, iface: &str) -> Result<(&'static str, u32), XdpAttachError> {
    use std::os::fd::{AsFd, AsRawFd};

    let ifindex = if_nametoindex(iface)
        .ok_or_else(|| XdpAttachError::AttachFailed(format!("unknown interface {iface}")))?;
    let prog_fd = program
        .fd()
        .map_err(|e| XdpAttachError::AttachFailed(format!("prog fd: {e}")))?
        .as_fd()
        .as_raw_fd();

    if let Ok(v) = std::env::var("DEMIURGE_XDP_FLAGS") {
        if v.eq_ignore_ascii_case("skb") {
            netlink_xdp_set(ifindex as i32, Some(prog_fd), XDP_FLAGS_SKB_MODE)
                .map_err(|e| XdpAttachError::AttachFailed(format!("{iface} (skb): {e}")))?;
            return Ok(("skb", ifindex));
        }
    }
    // flags=0: prefer native; on failure retry SKB (same dance as aya's attach).
    match netlink_xdp_set(ifindex as i32, Some(prog_fd), 0) {
        Ok(()) => Ok(("driver", ifindex)),
        Err(native_err) => match netlink_xdp_set(ifindex as i32, Some(prog_fd), XDP_FLAGS_SKB_MODE)
        {
            Ok(()) => Ok(("skb-fallback", ifindex)),
            Err(skb_err) => Err(XdpAttachError::AttachFailed(format!(
                "{iface}: native: {native_err}; skb: {skb_err}"
            ))),
        },
    }
}

fn netlink_xdp_set(if_index: i32, prog_fd: Option<i32>, flags: u32) -> std::io::Result<()> {
    use std::io;
    use std::mem;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    #[repr(C)]
    struct Request {
        header: libc::nlmsghdr,
        if_info: libc::ifinfomsg,
        attrs: [u8; 256],
    }

    // SAFETY: NETLINK_ROUTE raw socket for RTM_SETLINK.
    let fd = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW, libc::NETLINK_ROUTE) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let sock = unsafe { OwnedFd::from_raw_fd(fd) };

    let mut req: Request = unsafe { mem::zeroed() };
    let base_len = mem::size_of::<libc::nlmsghdr>() + mem::size_of::<libc::ifinfomsg>();
    req.header.nlmsg_type = libc::RTM_SETLINK;
    req.header.nlmsg_flags = (libc::NLM_F_REQUEST | libc::NLM_F_ACK) as u16;
    req.header.nlmsg_seq = 1;
    req.if_info.ifi_family = libc::AF_UNSPEC as u8;
    req.if_info.ifi_index = if_index;

    // Nested IFLA_XDP: IFLA_XDP_FD (+ optional IFLA_XDP_FLAGS).
    let mut nested = Vec::new();
    let fd_val = prog_fd.unwrap_or(-1);
    push_nla_bytes(&mut nested, 1, &fd_val.to_ne_bytes()); // IFLA_XDP_FD = 1
    if flags != 0 {
        push_nla_bytes(&mut nested, 3, &flags.to_ne_bytes()); // IFLA_XDP_FLAGS = 3
    }
    let mut attrs = Vec::new();
    // NLA_F_NESTED = 1 << 15
    push_nla_bytes(&mut attrs, libc::IFLA_XDP | (1u16 << 15), &nested);

    if attrs.len() > req.attrs.len() {
        return Err(io::Error::other("xdp netlink attrs too large"));
    }
    req.attrs[..attrs.len()].copy_from_slice(&attrs);
    req.header.nlmsg_len = (base_len + attrs.len()) as u32;

    let sent = unsafe {
        libc::send(
            sock.as_raw_fd(),
            &req as *const _ as *const libc::c_void,
            req.header.nlmsg_len as usize,
            0,
        )
    };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }

    let mut buf = [0u8; 1024];
    let n = unsafe {
        libc::recv(
            sock.as_raw_fd(),
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
            0,
        )
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    let n = n as usize;
    if n >= mem::size_of::<libc::nlmsghdr>() {
        let hdr = unsafe { &*(buf.as_ptr() as *const libc::nlmsghdr) };
        if hdr.nlmsg_type == NLMSG_ERROR {
            let err = unsafe {
                &*((buf.as_ptr().add(mem::size_of::<libc::nlmsghdr>())) as *const NlMsgErr)
            };
            if err.error != 0 {
                return Err(io::Error::from_raw_os_error(-err.error));
            }
        }
    }
    Ok(())
}

fn push_nla_bytes(buf: &mut Vec<u8>, nla_type: u16, payload: &[u8]) {
    let nla_len = 4 + payload.len();
    buf.extend_from_slice(&(nla_len as u16).to_ne_bytes());
    buf.extend_from_slice(&nla_type.to_ne_bytes());
    buf.extend_from_slice(payload);
    while !buf.len().is_multiple_of(4) {
        buf.push(0);
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
    let (mode, ifindex) = attach_program(program, iface)?;
    let prog_id = program.info().map(|i| i.id()).unwrap_or(0);
    maybe_pin_maps(&bpf)?;

    Ok(XdpAdmitShed {
        bpf,
        iface: iface.to_string(),
        mode,
        ifindex,
        prog_id,
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

    /// Liveness for Hybrid fallback: false when the iface is gone **or** when
    /// XDP was detached/replaced (`ip link set … xdp off` / `xdpgeneric off`).
    /// Query failure fails closed (treat as dead) so Hybrid never silently
    /// loses L4 admit.
    pub fn link_alive(&self) -> bool {
        if !Path::new("/sys/class/net").join(&self.iface).exists() {
            return false;
        }
        let ifindex = if self.ifindex != 0 {
            self.ifindex
        } else {
            match if_nametoindex(&self.iface) {
                Some(i) => i,
                None => return false,
            }
        };
        match query_iface_xdp_prog_id(ifindex) {
            Ok(Some(id)) if self.prog_id == 0 => id != 0,
            Ok(Some(id)) => id == self.prog_id,
            Ok(None) => false,
            Err(_) => false,
        }
    }

    pub fn object_path() -> PathBuf {
        object_path()
    }
}

impl Drop for XdpAdmitShed {
    fn drop(&mut self) {
        if self.ifindex != 0 {
            // Mode-matched detach first; also clear the other mode so a
            // mis-labeled attach cannot leave a stale XDP prog on the iface.
            let _ = netlink_xdp_set(self.ifindex as i32, None, xdp_detach_flags(self.mode));
            let other = if xdp_detach_flags(self.mode) == XDP_FLAGS_SKB_MODE {
                XDP_FLAGS_DRV_MODE
            } else {
                XDP_FLAGS_SKB_MODE
            };
            let _ = netlink_xdp_set(self.ifindex as i32, None, other);
        }
    }
}

/// `if_nametoindex(3)` — 0 means unknown.
fn if_nametoindex(name: &str) -> Option<u32> {
    use std::ffi::CString;
    let c = CString::new(name).ok()?;
    // SAFETY: `c` is a valid NUL-terminated iface name.
    let idx = unsafe { libc::if_nametoindex(c.as_ptr()) };
    if idx == 0 {
        None
    } else {
        Some(idx)
    }
}

// Nested IFLA_XDP attributes (linux/if_link.h) — not all exposed by libc.
const IFLA_XDP_ATTACHED: u16 = 2;
const IFLA_XDP_PROG_ID: u16 = 4;
const IFLA_XDP_DRV_PROG_ID: u16 = 5;
const IFLA_XDP_SKB_PROG_ID: u16 = 6;
const IFLA_XDP_HW_PROG_ID: u16 = 7;
const XDP_ATTACHED_NONE: u8 = 0;
/// Mask off `NLA_F_NESTED` / `NLA_F_NET_BYTEORDER` (linux/netlink.h).
const NLA_TYPE_MASK: u16 = 0x3fff;
const RTM_NEWLINK: u16 = 16;
const NLMSG_ERROR: u16 = 0x2;
const NLMSG_DONE: u16 = 0x3;

#[repr(C)]
struct NlMsgErr {
    error: i32,
    _msg: libc::nlmsghdr,
}

fn nla_align(len: usize) -> usize {
    (len + 3) & !3
}

/// Netlink RTM_GETLINK → current XDP program id on `ifindex`, if any.
fn query_iface_xdp_prog_id(ifindex: u32) -> std::io::Result<Option<u32>> {
    use std::io;
    use std::mem;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    #[repr(C)]
    struct Request {
        header: libc::nlmsghdr,
        if_info: libc::ifinfomsg,
    }

    // SAFETY: NETLINK_ROUTE raw datagram socket.
    let fd = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW, libc::NETLINK_ROUTE) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a freshly opened socket we uniquely own.
    let sock = unsafe { OwnedFd::from_raw_fd(fd) };

    // SAFETY: POD zero-init for netlink request.
    let mut req: Request = unsafe { mem::zeroed() };
    let nlmsg_len = mem::size_of::<libc::nlmsghdr>() + mem::size_of::<libc::ifinfomsg>();
    req.header.nlmsg_len = nlmsg_len as u32;
    req.header.nlmsg_type = libc::RTM_GETLINK;
    req.header.nlmsg_flags = libc::NLM_F_REQUEST as u16;
    req.header.nlmsg_seq = 1;
    req.if_info.ifi_family = libc::AF_UNSPEC as u8;
    req.if_info.ifi_index = ifindex as i32;

    // SAFETY: send the sized request prefix; socket is open.
    let sent = unsafe {
        libc::send(
            sock.as_raw_fd(),
            &req as *const _ as *const libc::c_void,
            nlmsg_len,
            0,
        )
    };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }

    let mut buf = vec![0u8; 8192];
    // SAFETY: recv into owned buffer.
    let n = unsafe {
        libc::recv(
            sock.as_raw_fd(),
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
            0,
        )
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    let n = n as usize;

    let mut offset = 0usize;
    while offset + mem::size_of::<libc::nlmsghdr>() <= n {
        // SAFETY: buffer holds a netlink message at `offset`.
        let hdr = unsafe { &*(buf.as_ptr().add(offset) as *const libc::nlmsghdr) };
        let msg_len = hdr.nlmsg_len as usize;
        if msg_len < mem::size_of::<libc::nlmsghdr>() || offset + msg_len > n {
            break;
        }
        match hdr.nlmsg_type {
            NLMSG_DONE => break,
            NLMSG_ERROR => {
                // SAFETY: error payload follows nlmsghdr.
                let err = unsafe {
                    &*((buf.as_ptr().add(offset + mem::size_of::<libc::nlmsghdr>()))
                        as *const NlMsgErr)
                };
                if err.error != 0 {
                    return Err(io::Error::from_raw_os_error(-err.error));
                }
            }
            RTM_NEWLINK => {
                let ifi_off = offset + mem::size_of::<libc::nlmsghdr>();
                let attrs_off = ifi_off + mem::size_of::<libc::ifinfomsg>();
                if attrs_off <= offset + msg_len {
                    if let Some(id) = parse_xdp_prog_id(&buf[attrs_off..offset + msg_len]) {
                        return Ok(Some(id));
                    }
                    return Ok(None);
                }
            }
            _ => {}
        }
        offset += nla_align(msg_len);
    }
    drop(sock);
    Ok(None)
}

fn parse_xdp_prog_id(attrs: &[u8]) -> Option<u32> {
    use std::mem;

    let mut off = 0usize;
    while off + mem::size_of::<libc::nlattr>() <= attrs.len() {
        // SAFETY: `attrs` holds aligned nlattr stream from the kernel.
        let nla = unsafe { &*(attrs.as_ptr().add(off) as *const libc::nlattr) };
        let nla_len = nla.nla_len as usize;
        if nla_len < mem::size_of::<libc::nlattr>() || off + nla_len > attrs.len() {
            break;
        }
        let nla_type = nla.nla_type & NLA_TYPE_MASK;
        if nla_type == libc::IFLA_XDP {
            let nested = &attrs[off + mem::size_of::<libc::nlattr>()..off + nla_len];
            return parse_xdp_nested(nested);
        }
        off += nla_align(nla_len);
    }
    None
}

fn parse_xdp_nested(attrs: &[u8]) -> Option<u32> {
    use std::mem;

    let mut attached = XDP_ATTACHED_NONE;
    let mut prog_id = 0u32;
    let mut drv_id = 0u32;
    let mut skb_id = 0u32;
    let mut hw_id = 0u32;

    let mut off = 0usize;
    while off + mem::size_of::<libc::nlattr>() <= attrs.len() {
        // SAFETY: nested IFLA_XDP attributes from RTM_NEWLINK.
        let nla = unsafe { &*(attrs.as_ptr().add(off) as *const libc::nlattr) };
        let nla_len = nla.nla_len as usize;
        if nla_len < mem::size_of::<libc::nlattr>() || off + nla_len > attrs.len() {
            break;
        }
        let nla_type = nla.nla_type & NLA_TYPE_MASK;
        let payload = &attrs[off + mem::size_of::<libc::nlattr>()..off + nla_len];
        match nla_type {
            IFLA_XDP_ATTACHED if !payload.is_empty() => attached = payload[0],
            IFLA_XDP_PROG_ID if payload.len() >= 4 => {
                prog_id = u32::from_ne_bytes(payload[..4].try_into().ok()?);
            }
            IFLA_XDP_DRV_PROG_ID if payload.len() >= 4 => {
                drv_id = u32::from_ne_bytes(payload[..4].try_into().ok()?);
            }
            IFLA_XDP_SKB_PROG_ID if payload.len() >= 4 => {
                skb_id = u32::from_ne_bytes(payload[..4].try_into().ok()?);
            }
            IFLA_XDP_HW_PROG_ID if payload.len() >= 4 => {
                hw_id = u32::from_ne_bytes(payload[..4].try_into().ok()?);
            }
            _ => {}
        }
        off += nla_align(nla_len);
    }

    if attached == XDP_ATTACHED_NONE {
        return None;
    }
    let id = [prog_id, drv_id, skb_id, hw_id]
        .into_iter()
        .find(|&id| id != 0)?;
    Some(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_xdp_nested_reads_skb_prog_id() {
        let mut nested = Vec::new();
        push_nla_bytes(&mut nested, IFLA_XDP_ATTACHED, &[2]); // XDP_ATTACHED_SKB
        push_nla_bytes(&mut nested, IFLA_XDP_SKB_PROG_ID, &42u32.to_ne_bytes());
        assert_eq!(parse_xdp_nested(&nested), Some(42));
    }

    #[test]
    fn parse_xdp_nested_none_when_detached() {
        let mut nested = Vec::new();
        push_nla_bytes(&mut nested, IFLA_XDP_ATTACHED, &[XDP_ATTACHED_NONE]);
        push_nla_bytes(&mut nested, IFLA_XDP_PROG_ID, &99u32.to_ne_bytes());
        assert_eq!(parse_xdp_nested(&nested), None);
    }
}
