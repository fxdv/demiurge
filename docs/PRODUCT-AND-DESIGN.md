# Demiurge — Product & Technical Design

**A phase-aware load balancer for LLM inference fleets**

*Human-readable product and design brief. Synthesized from [`README.md`](../README.md), [`ROADMAP.md`](../ROADMAP.md), and the living requirement registry. For machine-checked contracts, see [`design/requirements.toml`](../design/requirements.toml); for academic notation, see [`spec/demiurge.tex`](../spec/demiurge.tex) (PDF is optional).*

**Status (July 2026):** Phases **0–5 proof** shipped and gated on laptop hardware. **26 requirements** in the registry, **all 26 implemented and test-backed**. **Track C P/D proof gate** (`./scripts/track-c-verify.sh`) **passed** on singularity reference hardware (4× V100, Llama 3.1 8B, live vLLM + KV ledger + warmth). Phases 6–8 logic ships on Track A and is re-checked in that gate; fleet-measured migration p99, RDMA prod handoff, live corrector wiring, and tenant auth on production traffic remain open. **Track B** engineering path green on Linux VM; **production exit gates** (real NIC XDP under load, x86_64 p99 budget) remain open. Unified **Gate** CI mirrors `./scripts/gate.sh`; **`'sim`** fleet replay + **`verify.sh`** harden tiers ship observable pseudo reports.

---

## Executive summary

**One line.** Demiurge is the missing **control and dataplane layer** for disaggregated LLM serving: it routes **prefill** and **decode** as independent phases across two pools, with the **KV cache** as the explicit hand-off between them.

**The insight.** An inference request is not a packet. It is a **lease on stateful accelerator memory**. The valuable state on a GPU is the KV cache attached to a specific prompt prefix — not the TCP connection. Round-robin and least-connections ignore that completely.

**What exists today.** A working Rust forwarder with cost-based routing, async prefill→decode flow, KV hand-off and memory barriers, warmth-aware placement, pool rebalancing (shadow mode), userspace dataplane proofs, abortable migration-cutover logic, tenant cache-domain isolation, and a corrector shadow→canary→production graduation state machine — all enforced by CI gates, CPU benchmarks, and load/stress suites. On singularity reference hardware, **real Llama 3.1 8B P/D** (colocated + disaggregated chat, KV ledger, prefix warmth skew) is verified by **`track-c-verify`** (July 2026).

**What we are building toward.** Production-grade kernel admission at fleet scale (real NIC XDP under load), RDMA KV transfer, and pool/corrector actuation on real GPU clusters. The abortable live-migration cutover logic, tenant cache-domain isolation, and corrector graduation state machine already ship as portable, test-backed crates; io_uring L7 forwarding on the production TCP path is shipped. Remaining reference-hardware work: fleet-measured migration p99, live production traffic driving corrector graduation windows, RDMA prod transport, and tenant auth on live traffic.

**Honest caveat.** Early proof is green on mock backends and local TCP. **Disruption depends on production economics** on real accelerators. We do not oversell kernel XDP or RDMA as shipped when they are still Track B/C work.

---

## The problem

Every major inference stack still treats backends as interchangeable buckets:

| What traditional L7 assumes | What LLM serving actually is |
|----------------------------|------------------------------|
| Connections are equal | **KV cache** is request-correlated state |
| Backend cost is a fixed weight | Cost depends on **current batch + KV footprint** |
| Load is predictable | Occupancy is a **random variable** (burst prefill, long decode) |

When prefill and decode share one pool, you get the worst of both worlds: compute-bound bursts stall memory-bound decode, cache locality is accidental, and a prefill spike can **OOM the decode pool** silently.

**Disaggregated serving** (separate prefill and decode pools) is the industry direction — but it introduces a new problem: **who routes the hand-off, and on what signal?** Most teams bolt on a generic proxy. Demiurge treats phase-aware routing and KV locality as the **organizing principle** of the entire balancer.

---

## The product

### What Demiurge does

1. **Classifies** each request: short-context fast path (colocated) vs disaggregated prefill→decode.
2. **Admits** under overload (userspace token bucket on all platforms; kernel XDP on Linux when `DEMIURGE_ADMIT_MODE=xdp` or **`hybrid`** — recommended prod rollout with userspace fallback).
3. **Selects** the minimum-cost backend within each phase pool using live signals — queue depth, KV pressure, warmth hits, length predictions.
4. **Hands off** the KV cache explicitly between prefill and decode backends.
5. **Reserves memory** with honest overhead accounting so bursts cannot silently exhaust decode GPUs.
6. **Rebalances** prefill vs decode capacity share (π) with hysteresis — shadow mode today, fleet actuation next.

### Who it is for

| Customer | Pain | Demiurge value |
|----------|------|----------------|
| **Inference platform teams** (OpenAI-scale down to serious startups) | Disaggregated fleets are operationally brittle | Single routing brain with explicit KV hand-off and memory barriers |
| **Cloud / GPU providers** | Customers want prefill/decode split for $/token | Pluggable dataplane; policy in userspace Rust |
| **Model hosts running vLLM/TGI-style stacks** | Cache locality is leave-it-to-chance | Warmth map + greedy pf→dc pairing |
| **Teams hitting KV OOM under burst** | Static pool sizing | Φ barrier + reservation ledger + pool-ratio controller |

### What we are not (yet)

- A replacement for vLLM, TGI, or TensorRT-LLM — we sit **in front of** inference workers.
- A training platform or model compiler.
- A finished multi-tenant SaaS — the cache-domain **isolation** logic is wired into the router's routing decision (Phase 7), but real tenant authentication, content verification, and billing remain the caller's responsibility, not wired end-to-end.

---

## Why now

1. **Prefill/decode disaggregation** moved from research curiosity to production architecture as context lengths and batch variability grew.
2. **KV cache size** is now the binding constraint on decode — routing without memory accounting is negligent.
3. **Rust + eBPF + io_uring** make it possible to build a microsecond dataplane with millisecond control policy in one repo, with testable invariants.
4. **Fleet operators** are feeling pain from generic proxies that do not understand phase boundaries — the window for a purpose-built layer is open before incumbents fully productize it.

---

## How it works

### Three planes, three consistency models

```
┌─────────────────────────────────────────────────────────────┐
│  DATA PLANE (microseconds)                                  │
│  Admit → classify → forward → proxy bytes                   │
│  Serves last RCU snapshot; never blocks on control plane    │
└──────────────────────────┬──────────────────────────────────┘
                           │
┌──────────────────────────▼──────────────────────────────────┐
│  CONTROL PLANE (milliseconds)                               │
│  Cost function · length predictor · pool-ratio controller   │
│  Publishes routing weights and π at bounded cadence         │
└──────────────────────────┬──────────────────────────────────┘
                           │
┌──────────────────────────▼──────────────────────────────────┐
│  STATE PLANE (eventually consistent / AP)                   │
│  KV warmth map · live occupancy gossip                      │
│  Wrong warmth guess → cache miss, not crash                 │
└─────────────────────────────────────────────────────────────┘
```

**Design choice that matters:** warmth and occupancy are **eventually consistent on purpose**. A stale warmth read costs a cache miss — it never violates correctness. **Authorization** for shared cache lines is resolved on a strongly consistent path — a synchronous Shared-Prefix Group registry today, CP consensus on a fleet — never on the AP plane, so a stale "authorized share" is impossible.

### The routing bet

> **Disaggregated prefill/decode-aware routing as the organizing principle.**

| Phase | Character | Pool |
|-------|-----------|------|
| **Prefill** | Compute-bound, bursty, cache-**producing** | Prefill pool |
| **Decode** | Memory-bandwidth-bound, long-lived, cache-**consuming** | Decode pool |

The **KV cache** is the artifact passed between them — not an implementation detail buried inside the framework.

### Request lifecycle (happy path)

1. Request arrives; dataplane admits or sheds (503 if overloaded).
2. **Short prompt?** Route colocated on one backend — skip hand-off tax.
3. **Long or warmth-forced disagg?** Pick prefill target (min cost), dispatch prefill **async**, return immediately.
4. On prefill complete: valid **KV handle** required before decode placement.
5. Pick decode backend (min cost, conditional on prefill outcome).
6. Proxy response; release KV reservation on session end.

### The cost function (plain English)

Each backend gets a score. Lower is better. The score combines:

- **Time core** — how long work will take on this backend (always strictly positive).
- **Barriers** — queue pressure, fleet KV headroom (Φ barrier raises cost when decode pool is full).
- **Discounts** — warmth hits reduce cost (bounded; never below zero in log space).
- **Corrector** — bounded multiplier δ ∈ [1−α, 1+α]; the router runs δ=1 in production today, with a shadow → canary → production graduation state machine shipped and test-backed but not yet wired to live traffic.

**Invariant we refuse to break:** cost is always **> 0 by construction** (log-space composition). Broken telemetry **fails expensive**, never cheap — a sick backend cannot be accidentally preferred.

---

## What we have built (proof points)

### Requirement burndown

Live count from `cargo xtask lint`:

| Phase | Name | Status |
|------:|------|--------|
| 0 | Cost algebra + min-cost selection | **4 / 4** |
| 1 | Non-blocking route + fast path | **2 / 2** |
| 2 | KV hand-off + memory barriers | **6 / 6** |
| 3 | State plane (warmth, gossip) | **2 / 2** |
| 4 | Control plane + pairing + shadow fleet | **6 / 6** |
| 5 | Dataplane proof (RCU + admit shed) | **2 / 2** |
| 6 | Live migration (logic; fleet-p99 gate open) | **1 / 1** |
| 7 | Multi-tenant cache security | **1 / 1** |
| 8 | Corrector graduation to production | **2 / 2** |

**26 implemented**, **0 intended** — tracked in [`design/requirements.toml`](../design/requirements.toml). Phases 6–8 are logic-complete and test-backed on Track A; their reference-fleet rollout onto live traffic is tracked separately in the Track C roadmap below, not as an open requirement. The spec describes *target* behavior; the registry marks *shipped* vs *planned* so the two never blur.

### Development tracks

| Track | Where it runs | Status |
|-------|---------------|--------|
| **A — Local proof** | macOS + Linux, mock TCP backends | **Done** (Phases 0–5) |
| **A+ — Shadow tooling** | Trace replay, corrector shadow, fleet pilot | **Done** |
| **B — Linux production** | XDP attach, io_uring forwarder, nightly binaries | **In progress** — engineering path green (`track-b-verify`); exit gates open |
| **C — Fleet / GPU** | RDMA hand-off, migration, actuation at scale | **P/D proof PASS** on singularity (`track-c-verify`); RDMA / migration-p99 / live actuation open |
| **D — Market economics** | $/token, goodput, OOM delta vs baselines at fleet scale | **Protocol ready** — [`design/track-d/`](../design/track-d/); no reference archive yet |

### CPU hot path (release benchmarks)

Routing logic is **sub-microsecond to low-microsecond** on laptop-class hardware — not the bottleneck. End-to-end latency in mock benches is TCP/hand-off bound (~2–7 ms), which is expected.

| Gate | What it measures | Typical median | Limit |
|------|------------------|----------------|-------|
| Backend cost | Single target load signal | ~2 ns | 8 ns |
| Compose (8 factors) | Full cost assembly | ~20 ns | 50 ns |
| Classify | Fast vs disagg path | ~88 ns | 350 ns |
| Select (64 backends) | Min-cost over pool | ~325 ns | 1 µs |
| RCU snapshot read | Dataplane hot path | ~3 ns | 50 ns |

### Quality gates (always on in CI)

- **Design conformance** — spec ↔ code ↔ test traceability; generated artifacts cannot drift.
- **Blur guard** — `intended` requirements appear only in “design intent” prose; `implemented` ones appear in normative sections.
- **Property tests** — cost positivity, corrector bounds, fail-expensive clamping.
- **CPU bench gates** — hot-path regressions fail the build.
- **Load regression** — `LOAD-CI-SMOKE` + `LOAD-TRACK-B-IOURING` on Linux CI; kernel XDP+veth load under root (optional locally via `optional = true`).

Local stress suite (`load-stress.sh`): **11k+ requests**, zero-error gates — run before release tags, not on every PR.

### Track B verification (Linux traction)

Full dataplane proof on Linux VM (`./scripts/track-b-verify.sh`) writes **`target/track-b-verify/report.md`** and **`summary.json`** — gate + CPU headroom + load + stress. Use these artifacts in partner conversations alongside stamped **`PRODUCT-AND-DESIGN.pdf`** from `./scripts/publish.sh`.

| Evidence | Where |
|----------|--------|
| CI Track B load | `LOAD-TRACK-B-IOURING` in `ci` regression; `LOAD-TRACK-B-KERNEL` as root step |
| Full Linux verify | `target/track-b-verify/report.md` after `./scripts/track-b-verify.sh` |
| Rolling Linux binary | [`linux-nightly`](https://github.com/fxdv/demiurge/releases/tag/linux-nightly) release |
| Stamped product PDF | `target/product-doc/docs/PRODUCT-AND-DESIGN.pdf` via `cargo xtask product-doc` |

Reference run (Vagrant ARM64, Jun 2026): gate 3/3 XDP veth, `BENCH-IOURING-FWD` ~200 ns, load 11/11 scenarios, stress zero errors — engineering path, not production NIC exit gates.

---

## Technical depth (why this is hard to copy)

### 1. KV is state, not metadata

Generic proxies route on URL and headers. Demiurge routes on **where the cache already lives** (warmth map), **where memory remains** (reservation + Φ barrier), and **which phase** the work belongs to.

### 2. Memory accounting is fleet-level

Summing per-request p90 headroom **over-provisions** and hides OOM risk. The Φ barrier uses **fleet-aggregate** occupancy — a subtle distinction that shows up only under burst prefill.

### 3. Async phase boundary

`Route` returns before prefill completes. Decode placement is a **continuation** with fresh telemetry. Getting this wrong blocks accept threads and collapses throughput under load — we gate accept latency decoupled from prefill duration.

### 4. Three blast radii

| Plane | If it fails | Blast radius |
|-------|-------------|--------------|
| Data | Shed or 503 | Single request |
| Control | Stale weights | Suboptimal routing |
| State | Stale warmth | Cache miss |

We **never** put correctness-critical auth on the AP plane.

### 5. Design-driven velocity

Most infra teams accumulate docs that lie. Demiurge treats the spec as a **contract**:

- Every constant: one TOML file → generated into Rust + docs.
- Every normative claim: stable ID → spec tag + code comment + named test.
- Phases close by flipping `intended` → `implemented` — **never the reverse**.

That discipline is how a small team ships a trustworthy dataplane without a QA army.

---

## Roadmap (next 12–18 months)

### Track B — Linux production dataplane (now)

- [x] Runtime XDP attach + kernel admit on Linux (veth smoke, router `AdmitMode`, actuation map sync)
- [x] io_uring L7 forwarder on production TCP `serve()` loop (`IoUringProxySession` recv/send)
- [x] Router hardening — module split (`backend`/`http`/`config`/`routing`/`serve`), bounded worker pool (`DEMIURGE_WORKER_THREADS`), connection cap (`DEMIURGE_MAX_CONNS`), live tenant wire (`DEMIURGE_CACHE_GROUPS`, `p7_live_wire`)
- [x] Weekly `linux-nightly` release binaries with BPF objects
- [ ] Production exit gates: real NIC XDP under load, x86_64 p99 budget under CP slowdown

### Track C — Fleet economics (engineering)

- [x] **P/D proof on reference GPU** — Llama 3.1 8B, 4× V100, KV ledger, handoff shims, live warmth (`./scripts/track-c-verify.sh` **PASS** 2026-07-14; archive [`design/validation/singularity-2026-07-14/`](../design/validation/singularity-2026-07-14/README.md))
- [ ] RDMA KV hand-off (production transport; TCP proof today)
- [ ] Live migration — abortable sub-ITL cutover **logic shipped**; fleet-measured p99 budget on reference hardware open
- [ ] Pool autoscaler actuation on real GPU fractions (shadow → canary → prod)
- [ ] Cross-tenant cache sharing — isolation + router wiring **shipped**; real tenant auth/content verification on production traffic open
- [ ] Learned corrector graduation — state machine **shipped**; wiring window cadence + violation counters to live production traffic open

### Track D — Market economics (evidence)

Protocol: [`design/track-d/README.md`](../design/track-d/README.md). Gates: [`design/fleet-economics.toml`](../design/fleet-economics.toml).

- [ ] **Goodput A/B** — Demiurge vs round-robin on shared-prefix agent workload (≥10% delta target)
- [ ] **OOM burst A/B** — KV ledger + Φ vs blind admit under long-context ramp
- [ ] **$/token A/B** — mixed short/long workload with documented $/GPU-hour tariff (≥8% cost delta target)
- [ ] Frozen validation archive — `design/validation/<host>-track-d-<date>/`

Until Track D closes, market disruption claims remain **architectural** (see Executive summary caveat).

### Explicit non-goals for near term

- Training orchestration
- Model quantization / kernel fusion
- End-user billing metering (we expose telemetry; billing is upstream)

---

## Risks and open questions

| Risk | Mitigation |
|------|------------|
| **Mock bench ≠ GPU fleet** | `track-c-verify` on singularity (Jul 2026); honest proof vs production labeling in gate report |
| **No fleet economics archive** | Track D A/B protocol + gates — run on partner fleet, freeze under `design/validation/` |
| **Incumbents add phase-aware routing** | Depth on KV accounting + invariants + open core velocity |
| **RDMA / NIC variance** | Pluggable `HandoffTransport`; TCP proof path already shipped |
| **Operational complexity** | Shadow modes default; actuation behind flags; fail-closed admit |
| **Team size** | Design-driven CI reduces regression tax; phases are dependency-ordered |

**Open question for fleet design partners:** What is the acceptable stall budget for live decode migration (ε·ITL)? We encode it as a requirement (`DEMI-MIG-SUBITL`) but need production traces to tune ε.

---

## Repository map (for technical reviewers)

| Path | Purpose |
|------|---------|
| [`crates/demiurge-router/`](../crates/demiurge-router/) | Phase-aware TCP forwarder — main product surface |
| [`crates/demiurge-cost/`](../crates/demiurge-cost/) | Cost algebra and property tests |
| [`crates/demiurge-handoff/`](../crates/demiurge-handoff/) | KV hand-off registry and transports |
| [`crates/demiurge-control/`](../crates/demiurge-control/) | Reservation ledger, rebalancer, predictor |
| [`crates/demiurge-state/`](../crates/demiurge-state/) | Warmth map, gossip, RCU snapshots |
| [`crates/demiurge-dataplane/`](../crates/demiurge-dataplane/) | RCU table, admit bucket, XDP/io_uring hooks |
| [`design/`](../design/) | Canonical params, requirements, bench gates |
| [`xtask/`](../xtask/) | `gen`, `lint`, `bench-gate`, `load-bench`, `fleet-pilot`, `spec` |
| [`scripts/gate.sh`](../scripts/gate.sh) | Local CI mirror — run before merge |

**Try it locally:**

```bash
./scripts/bootstrap.sh
./scripts/gate.sh
cargo run --release -q --package xtask -- load-bench --ci
cargo run --release -q --package xtask -- ab-bench   # routing policy A/B
```

---

## Appendix A — Document hierarchy

| Document | Audience | Role |
|----------|----------|------|
| **This file** | Partners, investors, YC committee | Narrative product + design |
| [`README.md`](../README.md) | Engineers landing in repo | Orientation + quickstart |
| [`ROADMAP.md`](../ROADMAP.md) | Contributors | Phased build plan + exit gates |
| [`docs/THREAT-MODEL.md`](THREAT-MODEL.md) | Security / wire-protocol authors | Trust boundaries, threats, hardening backlog |
| [`spec/demiurge.tex`](../spec/demiurge.tex) | Implementers | Target design contract (PDF optional) |
| [`design/requirements.toml`](../design/requirements.toml) | CI / tooling | Shipped vs intended truth |
| Release one-pager | Per-tag artifact | Validation numbers for a specific build |
| `docs/PRODUCT-AND-DESIGN.pdf` | Per-tag artifact | Stamped product brief + validation header (compiled at publish) |

---

## Appendix B — Glossary

| Term | Meaning |
|------|---------|
| **Prefill** | Process prompt tokens; compute-heavy; builds KV cache |
| **Decode** | Generate output tokens; memory-bandwidth-heavy; consumes KV |
| **KV hand-off** | Explicit transfer of cache ownership prefill → decode |
| **Warmth** | Probability a prefix is cached on a backend (AP gossip) |
| **Φ barrier** | Fleet-level KV pressure signal that raises decode cost |
| **π (pi)** | Target fraction of fleet capacity assigned to prefill pool |
| **Fast path** | Colocated prefill+decode for short contexts |
| **RCU snapshot** | Dataplane reads last published routing table without blocking |
| **Shadow mode** | Compute policy counterfactual without actuating |

---

## License

Apache-2.0 OR MIT — see [`LICENSE-APACHE`](../LICENSE-APACHE) and [`LICENSE-MIT`](../LICENSE-MIT).

---

*Demiurge — design spec v1.5 · the spec is the contract, the code is the proof · [`docs/PRODUCT-AND-DESIGN.md`](PRODUCT-AND-DESIGN.md) is the human-readable product brief.*
