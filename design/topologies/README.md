# Demiurge topology reference

ELI5 diagram: [`demiurge-topologies-el5.svg`](demiurge-topologies-el5.svg)

**Rule of thumb:** client traffic and KV traffic use different networks and different
code paths. Admit (bouncer) is north–south on the **client NIC**. KV hand-off is
east–west on **TCP today / InfiniBand target**.

---

## Config matrix

| Topology | Platform | Client ingress NIC | `DEMIURGE_ADMIT_MODE` | Kernel XDP | L7 forward | KV hand-off | Topology labels | Primary validation |
|----------|----------|-------------------|------------------------|------------|------------|-------------|-----------------|-------------------|
| **A — local dev** | macOS / Linux | lo / veth (mock) | `userspace` (default) | off | TCP | TCP headers (`HeaderPassthroughTransport`) | optional | `./scripts/gate.sh --quick` |
| **A — load / stress** | Linux VM | mock TCP backends | `userspace` | off | TCP | TCP + KV pool | optional | `./scripts/load-bench.sh`, `load-stress.sh` |
| **A+ — fleet shadow** | macOS + Linux | n/a (offline) | n/a | off | n/a | shadow only | trace JSONL | `cargo xtask fleet-pilot` |
| **'sim L1/L2** | macOS + Linux | mock TCP | `userspace` (via load-bench) | off | TCP | TCP + KV pool | trace-driven knobs | `./scripts/apostrophe-sim.sh` |
| **B — VM smoke** | Linux + root | veth (`demi-a*`) | `hybrid` or `xdp` | **on** (`DEMIURGE_XDP_IFACE`) | TCP or `DEMIURGE_IOURING=1` | TCP | optional | `./scripts/xdp-veth-smoke.sh`, `track-b-gate.sh` |
| **B — CI Track B** | Linux CI | veth / io_uring | `kernel_xdp` (LOAD-TRACK-B-KERNEL) | on | io_uring | TCP | optional | `LOAD-TRACK-B-*` in CI |
| **B — prod NIC (target)** | Linux x86_64 | **eth0 / bond0** (real NIC) | **`hybrid`** (recommended) | on same **client** iface | io_uring | TCP until Track C | `DEMIURGE_TOPOLOGY` | Track B exit gates (open) |
| **C — IB GPU fleet (target)** | Linux + GPU | **Ethernet** (API) | **`hybrid`** or `xdp` | on **client** NIC only | io_uring | **TCP P/D proof PASS**; RDMA/IB target | **required** `label@node/rack/cluster` | `track-c-verify` PASS; RDMA prod open |

---

## Concrete env blocks

### Track A — macOS or Linux dev (default)

```bash
export DEMIURGE_LISTEN=127.0.0.1:8080
export DEMIURGE_ADMIT_MODE=userspace
export DEMIURGE_PREFILL='pf0@127.0.0.1:9001@0.01'
export DEMIURGE_DECODE='dc0@127.0.0.1:9002@0.01'
# DEMIURGE_TOPOLOGY optional for RDMA shadow experiments
cargo run --release -p demiurge-router
```

### Track B — Linux VM / veth XDP smoke

```bash
./scripts/build-bpf.sh
export DEMIURGE_ADMIT_MODE=hybrid          # or xdp
export DEMIURGE_XDP_IFACE=demi-a0          # client-side veth leg
export DEMIURGE_XDP_FLAGS=skb              # veth-friendly
export DEMIURGE_BPF_OBJECT=target/bpf/admit_shed.o
export DEMIURGE_IOURING=1                  # optional Track B L7 path
./scripts/xdp-veth-smoke.sh
```

Load-bench scenario equivalent: `LOAD-TRACK-B-KERNEL` (`admit_mode = "kernel_xdp"`, `track_b_kernel = true`).

### Track B — production client NIC (rollout pattern)

```bash
export DEMIURGE_ADMIT_MODE=hybrid
export DEMIURGE_XDP_IFACE=eth0             # API / north-south ONLY
export DEMIURGE_IOURING=1
export DEMIURGE_REBALANCER_ACTUATE=1       # π → admit capacity sync
export DEMIURGE_PREFILL='pf0@10.0.0.11:9001@0.01,pf1@10.0.0.12:9001@0.01'
export DEMIURGE_DECODE='dc0@10.0.0.21:9002@0.01,dc1@10.0.0.22:9002@0.01'
export DEMIURGE_TOPOLOGY='pf0@node0/rack0/cluster0,pf1@node1/rack0/cluster0,dc0@node2/rack1/cluster0,dc1@node3/rack1/cluster0'
```

**Do not** attach XDP admit to `ib0` / RoCE devices — that is not the client front door.

### Track C — InfiniBand KV fleet (target; TCP proof until RDMA transport lands)

Same as Track B for **admit** on Ethernet. KV hand-off switches at the router layer:

| Component | Track B (now) | Track C (target) |
|-----------|---------------|------------------|
| Client API | TCP → router | TCP → router (unchanged) |
| Admit | userspace / XDP on **eth** | same |
| Prefill → decode KV | TCP proof / headers | **RDMA over IB** (`HandoffTransport`) |
| Singularity reference (Jul 2026) | TCP handoff + KV ledger + warmth **PASS** | RDMA prod transport open |
| Cost model | flat + topology shadow | topology-aware routing + shadow eval |
| Config | default transport | `with_handoff_transport(ModeledRdmaTransport { ... })` or prod verbs |

```bash
# Admit + API — still Ethernet
export DEMIURGE_ADMIT_MODE=hybrid
export DEMIURGE_XDP_IFACE=eth0

# East-west — topology required for RDMA shadow / future prod routing
export DEMIURGE_TOPOLOGY='pf0@node0/rack0/cluster0,dc0@node0/rack0/cluster0,pf1@node1/rack1/cluster0,dc1@node2/rack1/cluster0'

# Router binary: production RDMA transport = Track C (not yet default)
export DEMIURGE_HANDOFF_TRANSPORT=modeled_rdma   # tcp | mock_rdma | modeled_rdma
export DEMIURGE_RDMA_ROUTING=1                   # topology-aware decode placement
# Today: LOAD-RDMA-TOPO uses ModeledRdmaTransport + rdma_routing in harden/load scenarios
```

---

## Admit mode decision table

| Mode | Kernel XDP attached? | Who sheds overload? | Use when |
|------|----------------------|---------------------|----------|
| `userspace` | ignored | `AdmitBucket` only | macOS, dev, CI smoke, 'sim |
| `xdp` | **required** | BPF token bucket only | Linux prod NIC, no fallback |
| `hybrid` | optional | XDP if attached, else userspace | **recommended prod rollout** |

Both buckets are **reseeded together** on π actuation so fallback capacity matches kernel — this is sync, not double admission.

### XDP reseed semantics (best-effort)

The BPF map stores `{ tokens, capacity, shed_total }` in one struct write on reseed (see `xdp_linux.rs`).
In-flight XDP handlers may decrement `tokens` concurrently — same best-effort contract as userspace
`AdmitBucket::reseed`. Reseed preserves `shed_total`; only capacity/tokens are refreshed.

### Hybrid rollout playbook

1. Build BPF: `./scripts/build-bpf.sh` → `target/bpf/admit_shed.o`
2. Set `DEMIURGE_ADMIT_MODE=hybrid`, attach XDP on **client NIC only** (`DEMIURGE_XDP_IFACE=eth0`)
3. Enable π actuation: `DEMIURGE_REBALANCER_ACTUATE=1` (userspace + kernel buckets reseed together)
4. Optional L7: `DEMIURGE_IOURING=1`
5. KV / RDMA (shadow today): `DEMIURGE_TOPOLOGY=...`, `DEMIURGE_HANDOFF_TRANSPORT=modeled_rdma`, `DEMIURGE_RDMA_ROUTING=1`
6. Validate: `./scripts/track-b-gate.sh` (veth) → `./scripts/verify.sh track-b` → production NIC load (exit gate)

### Stress / port recovery

Scenarios with `isolate_recovery = true` spawn isolated subprocesses and pause **30s** between runs
so ephemeral TCP ports are released. See `LOAD-STRESS-*` and `LOAD-RDMA-TOPO` in `load-bench.toml`.
Run full stress locally: `./scripts/load-stress.sh` or `./scripts/verify.sh stress`.

---

## What runs where (ELI5)

| Question | Answer |
|----------|--------|
| Where do clients connect? | Ethernet (or dev lo/veth) → router listen addr |
| Where does overload get dropped? | Same client NIC path — XDP earliest, else userspace before L7 |
| Where does InfiniBand matter? | GPU ↔ GPU KV bytes after prefill completes |
| Can I use userspace + kernel together? | **One bouncer per packet** — Hybrid picks kernel OR userspace |
| Does IB replace XDP? | **No** — different direction, different job |

---

## Related files

| Path | Role |
|------|------|
| `bpf/admit_shed.bpf.c` | Kernel token bucket (front door) |
| `crates/demiurge-dataplane/src/admission.rs` | Userspace token bucket |
| `crates/demiurge-dataplane/src/admit_mode.rs` | `userspace` / `xdp` / `hybrid` |
| `crates/demiurge-handoff/src/transport.rs` | TCP vs mock/modeled RDMA |
| `design/load-bench.toml` | Scenario presets (`LOAD-TRACK-B-*`, `LOAD-RDMA-TOPO`, `SIM-FLEET-*`) |
| `design/apostrophe-sim/README.md` | 'sim fleet tiers |
