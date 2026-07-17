# Demiurge Threat Model

Status: **normative for wire-protocol design**. Any feature that moves state
across a machine boundary (gossip, KV hand-off, control-plane RPC) must cite
the relevant section of this document in its design review *before* the wire
format is fixed. This document precedes — deliberately — the design of the
gossip wire protocol.

Scope: the router binary (`demiurge-router`), the state plane
(`demiurge-state`), the shared-prefix authorization registry
(`demiurge-auth`), the KV hand-off path (`demiurge-handoff`), and the kernel
admission shed (`bpf/admit_shed.bpf.c`).

---

## 1. Assets

| # | Asset | Why it matters |
|---|---|---|
| A1 | **Tenant cache isolation** | A warmth discount derived from another tenant's KV cache leaks membership/content information (a prefix-cache timing oracle) and steers traffic in attacker-observable ways. [DEMI-S1-DOMAIN] |
| A2 | **Routing integrity** | Whoever influences the cost function influences placement: a poisoned warmth or occupancy signal can concentrate load, starve a victim tenant, or steer requests to a compromised backend. |
| A3 | **Availability** | The router is a single ingress choke point; connection floods, oversized responses, and slow-loris bodies are the cheap attacks. [DEMI-XDP-SHED] |
| A4 | **KV hand-off confidentiality/integrity** | Hand-off descriptors name KV handles and byte counts; a forged descriptor can reserve ledger capacity (DoS) or misdirect a decode continuation. [DEMI-KV-HANDOFF] |
| A5 | **Control telemetry** | π actuation, admit capacity, and shed counters are driven by aggregated signals; falsified signals move real capacity. [DEMI-DP-RCU] |

## 2. Trust boundaries

```
 client ──▶ [authenticating edge] ──▶ [router] ──▶ backends (prefill/decode)
                                        │  ▲
                       gossip peers ────┘  └──── KV hand-off transport
```

- **B1 — Client ↔ edge.** Untrusted. The edge terminates client auth
  (API keys, mTLS — out of scope here) and is the *only* component allowed to
  set the identity headers `X-Demiurge-Tenant`, `X-Demiurge-Group`,
  `X-Demiurge-Prefix-Fp`. The edge MUST strip these headers from inbound
  client traffic; if it does not, any client can claim any tenant identity
  and the isolation gating in `parse_request_identity` is void.
- **B2 — Edge ↔ router.** Semi-trusted (same operator). The router treats
  the identity headers as pre-authenticated ([`RequestIdentity`] docs); it
  performs authorization (group membership, template match) but not
  authentication. Deployments crossing untrusted networks need mTLS on this
  hop.
- **B3 — Router ↔ backends.** Semi-trusted. Backends supply KV hand-off
  headers (`x-demiurge-kv-handle`, `x-demiurge-kv-bytes`) that the router
  currently believes after plausibility checks only (`is_valid()`, expected
  byte floor). A compromised backend is in the threat model — see T4.
- **B4 — Router ↔ gossip peers.** **Untrusted until authenticated.** Today
  gossip (`demiurge_state::gossip`) is in-process only; the moment it gets a
  socket, every input in `apply_gossip`/`heal_merge` is attacker-controlled.
  See §4 — this boundary has the strictest requirements for future work.
- **B5 — Kernel/dataplane.** Trusted (same host, root-installed XDP).

## 3. Adversaries

- **M1 — Malicious tenant.** Valid credentials at the edge; crafts headers,
  token counts, and request timing. Goals: read another tenant's cache
  warmth (timing oracle), free-ride on shared caches, exhaust capacity.
- **M2 — Compromised backend.** Full control of one pool member's responses
  and timing. Goals: attract or repel traffic, forge hand-offs, exhaust
  router memory.
- **M3 — Network attacker (gossip/hand-off path).** Can inject, replay, or
  tamper with any unauthenticated cross-machine message. Relevant the moment
  gossip or hand-off leaves localhost.
- **M4 — Volumetric attacker.** No credentials; floods connections/packets.

## 4. Threats, mitigations, gaps

### T1 — Cross-tenant cache-warmth leakage (M1 → A1)

A tenant presents another group's prefix content (byte-identical system
prompt) hoping to inherit its warmth discount and, via latency, confirm
cache residency.

**Mitigated.** Membership and template match are checked on the strongly
consistent registry path *before* any discount applies
(`SharedPrefixGroupRegistry::resolve_shared_key`); non-members and template
mismatches fall back to a tenant-private cache-domain key, so their lookup
can only hit their own warmth. Enforced end-to-end on the live TCP path
(`p7_live_wire::live_tcp_path_gates_warmth_by_identity`). [DEMI-S1-DOMAIN]

**Mitigated (G1 / G1b):** `CacheDomainKey::salt` and
`PrefixFingerprint::of` use a BLAKE3 keyed PRF: `DEMIURGE_AUTH_SECRET` is
fed through `blake3::derive_key` into a 256-bit key, then
`blake3::Hasher::new_keyed` over domain-separated inputs. Optional
`X-Demiurge-Prefix-Content` must keyed-hash to the claimed `Prefix-Fp` or
identity is rejected. Production must set a strong secret; the unset
default is a fixed **dev-only** placeholder.

**Residual gap — G2 (medium):** warmth timing is still observable *within*
a legitimately shared group; a member can probe whether a co-member has
already warmed a template. Accepted: sharing a cache domain is opt-in and
implies this visibility.

### T2 — Identity forgery at the edge (M1 → A1, A2)

**Mitigated by contract, not by code.** The router cannot distinguish a
forged `X-Demiurge-Tenant` from a real one (B1/B2). The deployment
requirement is: edge strips inbound identity headers, edge-to-router link is
authenticated. Missing headers fail closed (no identity → no shared-domain
warmth benefit). Do **not** expose the router port directly to clients.

### T3 — Gossip poisoning (M3 → A2, A5)

`heal_merge` takes `max()` of occupancy/KV bytes and inserts unknown
backends (`or_insert_with`); `apply_gossip` inserts warmth blocks without
proof of origin. A network attacker could: inflate a victim backend's
occupancy (traffic repulsion), advertise phantom warm backends (traffic
attraction), or wedge occupancy at 1.0 forever (monotonic max has no decay).

**Current stance:** gossip is compiled in-process only; B4 does not exist on
any wire today, so the attack surface is nil. **Wire-protocol requirements
(normative, from this document):**

1. Peer authentication: mTLS with a fleet-internal CA, or signed updates
   (Ed25519) with per-peer keys. No unauthenticated UDP merge, ever.
2. Origin binding: a peer may only assert state for backends it owns
   (`source_label` must match the authenticated peer identity); merging
   another peer's claims about a third backend requires that third party's
   signature (or is dropped).
3. Freshness: updates carry a monotonic per-peer sequence + timestamp;
   replays and stale generations are rejected. Occupancy must be
   last-writer-wins with decay, not monotonic max.
4. Bounded influence: warmth insertions are rate-limited per peer and capped
   by the cuckoo load factor (already enforced by `WarmthMap::insert`);
   a peer that persistently fills maps gets quarantined.
5. Membership: peers join via the control plane (CP, strongly consistent),
   never by being mentioned in gossip (`or_insert_with` on an unknown label
   must become a drop + alert on the wire path).

### T4 — Forged or oversized prefill responses (M2 → A3, A4)

A compromised prefill backend can claim arbitrary `x-demiurge-kv-bytes`
(ledger exhaustion → 503s for everyone) or stream an unbounded body into the
router's buffer.

**Mitigated (partially):**
- Response buffering is capped at `dataplane.prefill_response_max_bytes`
  (default 1 MiB); an oversized response fails that hand-off gracefully
  (503 to the one client, no router memory growth) —
  `p7_live_wire::oversized_prefill_response_sheds_gracefully`.
- Byte-count floor: claimed KV bytes below the analytic expectation for the
  prompt are rejected (`on_prefill_complete`).
- Duplicate reservations are rejected by request id; reservations are
  RAII-released. [DEMI-KV-HANDOFF]

**Mitigated (G3, partial):** claimed KV bytes must lie in
`[expected, expected × handoff_byte_ceiling_multiple]` and each prefill
source label is capped at `max_outstanding_per_source_fraction` of ledger
capacity. Handoff headers are parsed only up to the first `\r\n\r\n` so
body-injected `x-demiurge-kv-*` lines cannot override claims.
**Residual gap — G4 (medium):** hand-off descriptors are unsigned; when the
hand-off transport leaves localhost it needs the same authentication story
as gossip (§T3 requirements apply verbatim).

### T5 — Resource exhaustion (M4 → A3)

**Mitigated, defense in depth:**
- L4: XDP token-bucket shed (`admit_shed.bpf.c`) gates *new-connection TCP
  SYNs only* (optionally scoped to the router's listen port); established
  flows, ICMP, and ARP always pass, so shedding rejects new work without
  severing in-flight connections or management traffic. Tokens are signed
  with fail-expensive compensation — an exhausted bucket cannot wrap open
  under concurrent multi-CPU shed
  (`bpf_model_never_over_admits_concurrently`) — and refill in kernel at
  `dataplane.admit_refill_per_sec` with no userspace liveness dependency;
  capacity remains π-scaled via reseed. Requires Linux >= 5.12
  (`BPF_ATOMIC`, `-mcpu=v3`). [DEMI-XDP-SHED]
- L7 admission: userspace `AdmitBucket` (one token per connection, RAII).
- L7 concurrency: `serve` caps live proxied connections
  (`dataplane.max_conns`, default 1024, `DEMIURGE_MAX_CONNS`); excess
  connections get an immediate 503 instead of an unbounded thread —
  `p7_live_wire::serve_sheds_503_over_connection_cap`.
- Head parsing is bounded (`MAX_HEAD` = 64 KiB); prefill buffering is
  capped (T4).

**Closed — G5:** the BPF bucket previously used an *unsigned*
`__sync_fetch_and_sub`, which wrapped to `2^64-1` under concurrent
exhaustion and left the bucket permanently fail-open (unbounded over-admit
during overload — worse than the original "at most one packet" estimate).
Tokens are now signed with `prev <= 0` compensation; the concurrent model
test would fail on the old logic.
**Residual gap — G5b (low):** Hybrid-mode kernel-link liveness is detected
via interface existence on the RCU heartbeat (falls back to the userspace
bucket); an admin-forced `ip link set … xdp off` detach is not observable
this way and would leave Hybrid unenforced at L4 (L7 caps still hold).
**Mitigated (G6):** On Linux with `DEMIURGE_IOURING=1` (or a router built
with io_uring), accept is owned by an io_uring `Accept` loop
(`IoUringAcceptLoop`); accepted fds are still dispatched to a bounded
`handle_conn` worker pool under the `max_conns` cap (excess shed 503).
Non-io_uring platforms keep std `TcpListener::incoming()`.

### T6 — Cost-function manipulation (M1, M2 → A2)

A backend that under-reports latency (or a tenant that inflates
`X-Demiurge-Tokens`) shifts placement.

**Mitigated structurally:** the cost algebra is fail-expensive — broken or
out-of-range signals saturate toward *more expensive*, never cheaper
([DEMI-FAIL-EXPENSIVE], [DEMI-COST-POS]); `Cost::from_ln` now saturates
non-finite logs the same way, so no arithmetic path can mint an artificially
cheap target. Token-count inflation only pushes a request onto the slower
disaggregated path (self-harm). The corrector multiplier is clamped to
`[1−α, 1+α]`, bounding any learned component's influence.
[DEMI-CORR-CLAMP]

## 5. Explicit non-goals (current phase)

- Client authentication and API-key management (edge responsibility, B1).
- Encryption of KV cache contents at rest on backends.
- Byzantine control-plane consensus; the CP registry is single-process and
  strongly consistent by construction on Track A.

## 6. Hardening backlog (priority order)

1. **T3** — authenticated gossip wire protocol per §4 requirements
   (blocks: any networked state plane).
2. **G4** — signed hand-off descriptors (blocks: cross-host hand-off transport).
3. **G5b** — observe admin-forced XDP detach under Hybrid (not only iface
   disappearance on the RCU heartbeat).
