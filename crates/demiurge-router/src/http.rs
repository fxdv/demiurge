//! Bounded HTTP head parsing: token estimation, phase declaration, and the
//! trusted-edge identity headers. All parsers operate on the raw head bytes,
//! are overflow-checked, and never allocate.

use std::io::{self, Read};
use std::net::TcpStream;

use demiurge_cost::ROUTING_SHORT_CONTEXT_TOKENS;

use crate::routing::RequestIdentity;
use crate::{GroupId, PrefixFingerprint, TenantId};

pub(crate) const MAX_HEAD: usize = 64 * 1024;

const HDR_TOKENS: &[u8] = b"x-demiurge-tokens";
const HDR_PHASE: &[u8] = b"x-demiurge-phase";
const HDR_TENANT: &[u8] = b"x-demiurge-tenant";
const HDR_GROUP: &[u8] = b"x-demiurge-group";
const HDR_PREFIX_FP: &[u8] = b"x-demiurge-prefix-fp";

#[inline]
fn trim_ascii_ws(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map(|p| p + 1)
        .unwrap_or(start);
    &bytes[start..end]
}

#[inline]
fn ascii_eq_ci(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|(&x, &y)| x.eq_ignore_ascii_case(&y))
}

#[inline]
fn header_value_ci<'a>(head: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    let mut i = 0;
    while i < head.len() {
        let line_end = head[i..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|p| i + p)
            .unwrap_or(head.len());
        let mut line = &head[i..line_end];
        if let Some(stripped) = line.strip_suffix(b"\r") {
            line = stripped;
        }
        if line.len() > name.len()
            && line[name.len()] == b':'
            && ascii_eq_ci(&line[..name.len()], name)
        {
            return Some(trim_ascii_ws(&line[name.len() + 1..]));
        }
        if line_end >= head.len() {
            break;
        }
        i = line_end + 1;
    }
    None
}

#[inline]
fn parse_u64_digits(bytes: &[u8]) -> Option<u64> {
    let mut n = 0u64;
    let mut any = false;
    for b in bytes {
        if !b.is_ascii_digit() {
            break;
        }
        any = true;
        n = n.checked_mul(10)?.checked_add(u64::from(b - b'0'))?;
    }
    if any {
        Some(n)
    } else {
        None
    }
}

/// Parse a `u64` value, accepting decimal or `0x`-prefixed hex.
#[inline]
pub(crate) fn parse_u64_maybe_hex(bytes: &[u8]) -> Option<u64> {
    if let Some(hex) = bytes
        .strip_prefix(b"0x")
        .or_else(|| bytes.strip_prefix(b"0X"))
    {
        let s = std::str::from_utf8(hex).ok()?;
        return u64::from_str_radix(s, 16).ok();
    }
    parse_u64_digits(bytes)
}

#[inline]
fn contains_subslice(hay: &[u8], needle: &[u8]) -> bool {
    hay.len() >= needle.len() && hay.windows(needle.len()).any(|w| w == needle)
}

/// Parse `X-Demiurge-Tokens: N` from the request head.
pub fn parse_prompt_tokens(head: &[u8]) -> Option<u64> {
    header_value_ci(head, HDR_TOKENS).and_then(parse_u64_digits)
}

/// Parse token count from `/prefill/<n>` or `/long/<n>` path segments.
pub fn parse_path_tokens(head: &[u8]) -> Option<u64> {
    let first = head.split(|&b| b == b'\r' || b == b'\n').next()?;
    let mut parts = first.split(|&b| b == b' ').filter(|p| !p.is_empty());
    parts.next()?;
    let path = parts.next()?;
    for prefix in [b"/prefill/" as &[u8], b"/long/"] {
        if path.starts_with(prefix) {
            return parse_u64_digits(&path[prefix.len()..]);
        }
    }
    None
}

/// Estimate prompt tokens for admission. Unknown prompts default to above the
/// fast-path threshold so we never colocate a long unknown request.
pub fn estimate_prompt_tokens(head: &[u8]) -> u64 {
    parse_prompt_tokens(head)
        .or_else(|| parse_path_tokens(head))
        .unwrap_or(ROUTING_SHORT_CONTEXT_TOKENS + 1)
}

/// Parse the pre-authenticated request identity from trusted edge headers.
/// [DEMI-S1-DOMAIN]
///
/// **Trust boundary:** the router performs no authentication itself. These
/// headers (`X-Demiurge-Tenant`, `X-Demiurge-Group`, `X-Demiurge-Prefix-Fp`)
/// must be set by an authenticating edge (and stripped from raw client
/// traffic) — the router treats them as an already-verified identity, exactly
/// as [`RequestIdentity`] documents. All three must be present; a partial set
/// yields `None` and the request routes without any warmth discount gating
/// benefit (fail-closed: no identity, no shared-domain warmth).
pub fn parse_request_identity(head: &[u8]) -> Option<RequestIdentity> {
    let tenant = header_value_ci(head, HDR_TENANT).and_then(parse_u64_maybe_hex)?;
    let group = header_value_ci(head, HDR_GROUP).and_then(parse_u64_maybe_hex)?;
    let fp = header_value_ci(head, HDR_PREFIX_FP).and_then(parse_u64_maybe_hex)?;
    Some(RequestIdentity {
        tenant: TenantId::new(tenant),
        group: GroupId::new(group),
        content_fp: PrefixFingerprint::new(fp),
    })
}

/// True when the client declared decode-only routing.
pub fn is_decode_only(head: &[u8]) -> bool {
    if header_value_ci(head, HDR_PHASE).is_some_and(|v| ascii_eq_ci(v, b"decode")) {
        return true;
    }
    if head
        .split(|&b| b == b'\r' || b == b'\n')
        .next()
        .is_some_and(|line| contains_subslice(line, b" /decode"))
    {
        return true;
    }
    is_admin_probe_request(head)
}

fn request_line_parts(head: &[u8]) -> Option<(&[u8], &[u8])> {
    let line = head.split(|&b| b == b'\r' || b == b'\n').next()?;
    let mut parts = line.split(|&b| b == b' ').filter(|p| !p.is_empty());
    let method = parts.next()?;
    let path = parts.next()?;
    Some((method, path))
}

fn is_admin_probe_request(head: &[u8]) -> bool {
    let Some((method, path)) = request_line_parts(head) else {
        return false;
    };
    if ascii_eq_ci(method, b"POST") {
        return false;
    }
    let path = path.split(|&b| b == b'?').next().unwrap_or(path);
    path.ends_with(b"/models")
        || path.ends_with(b"/version")
        || matches!(
            path,
            b"/health" | b"/healthz" | b"/ready" | b"/readyz" | b"/metrics"
        )
}

/// Read an HTTP request head, bounded at [`MAX_HEAD`].
pub(crate) fn read_head(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];
    while stream.read(&mut byte)? == 1 {
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") || buf.len() >= MAX_HEAD {
            break;
        }
    }
    Ok(buf)
}
