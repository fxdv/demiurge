# Demiurge roadmap

This document is the **build plan** for implementing [`spec/demiurge.tex`](spec/demiurge.tex). It defines phased deliverables, requirement IDs, exit criteria, and explicit non-goals. Phases are **dependency-ordered gates**, not calendar commitments.

**Audience.** Contributors and maintainers use this file day to day. Partners and investors should start with [`docs/PRODUCT-AND-DESIGN.md`](docs/PRODUCT-AND-DESIGN.md). The LaTeX spec remains the formal contract.

---

## 1. Progress and governance

### How progress is measured

| Mechanism | Purpose |
|-----------|---------|
| [`design/requirements.toml`](design/requirements.toml) | Every normative claim has `status` (`implemented` \| `intended`) and a `phase`. |
| `cargo xtask lint` | Traceability join and phase burndown (`P0: 4/4`, …). |
| `cargo xtask bench-gate` | Release-mode CPU gates vs [`design/bench-gates.toml`](design/bench-gates.toml). |
| `spec/generated/conformance_matrix.tex` | Generated requirement snapshot (never hand-edited). |
| [`gate.yml`](.github/workflows/gate.yml) | CI: Verify, Track A, Track B, Policy (PRs), Spec when design changes. |
| [`./scripts/gate.sh`](scripts/gate.sh) | Local CI mirror; run before every merge. |
| [`./scripts/verify.sh`](scripts/verify.sh) | Optional deep validation: harden tiers, load, stress, reports. |

### Operating rules

1. **Ratchet only tighter.** Close a phase by moving requirements from `intended` to `implemented` with named tests — never the reverse.
2. **Same-PR spec and code.** Behavior and `\req{ID}` change together ([`CONTRIBUTING.md`](CONTRIBUTING.md)).
3. **Honest scope.** Each phase states what is explicitly out of scope.
4. **Hot paths stay fast.** CPU bench gates are part of `gate.sh` and CI; regressions fail the build.

---

## 2. Execution model

Work is organized by **where it runs**. Requirement **phase numbers (0–8)** in `requirements.toml` are unchanged; the table below maps them to three tracks.

| Track | Platform | Scope | Validation | Status |
|-------|----------|-------|------------|--------|
| **A — Local development** | macOS (primary), portable Rust | Phases 0–5 proof | `./scripts/gate.sh`, optional `./scripts/verify.sh full` | **Complete** |
| **B — Linux production** | Linux x86_64 | Kernel dataplane (XDP, io_uring), `linux-nightly` | Gate Track B, `./scripts/track-b-verify.sh` | **In progress** |
| **C — Fleet and scale** | Linux + GPU fleet | Migration, tenancy, corrector production | Reference hardware | **In progress** (P6 + P7 + P8 logic done on Track A; fleet-measured gates open) |

### Platform matrix

| Capability | macOS | Linux | GPU fleet |
|------------|:-----:|:-----:|:---------:|
| Cost algebra, router, TCP hand-off, control/state planes | Yes | Yes | — |
| CPU gates, load bench, stress (mock backends) | Yes | Yes | — |
| Userspace RCU and admit bucket (Phase 5 proof) | Yes | Yes | — |
| Lint, spec PDF, Gate CI | Yes | Yes (CI) | — |
| Tagged release tarball | Yes | — | — |
| XDP attach, io_uring forwarder | — | Yes | — |
| `linux-nightly` pre-release | — | Yes | — |
| Production RDMA transport | Mock | Mock | Planned |
| Live migration cutover logic + atomic KV transfer | Yes | Yes | Fleet p99 gate open |
| Cross-tenant cache-domain isolation, wired into the router | Yes | Yes | Real tenant auth/content verification open |
| Corrector shadow → canary → production graduation state machine | Yes | Yes | Live-traffic wiring open |
| Pool actuation at scale | — | — | Planned |

**Scope note.** `DEMI-XDP-SHED` at `implemented` is the **userspace proof** (Track A). Runtime XDP shedding before decode saturation is Track B (Phase 5+).

---

## 3. Phase burndown

Live counts: `cargo xtask lint`.

| Phase | Track | Name | Requirements | Status |
|------:|-------|------|:------------:|--------|
| 0 | A | Foundations | 4 / 4 | Complete |
| 1 | A | Non-blocking routing | 2 / 2 | Complete |
| 2 | A | KV hand-off and memory barriers | 6 / 6 | Complete |
| 3 | A | State plane | 2 / 2 | Complete |
| 4 | A | Control plane and pairing | 2 / 2 | Complete |
| 5 | A | Data plane hardening (proof) | 2 / 2 | Complete |
| 5+ | B | Data plane production | — | In progress |
| 6 | C | Live migration | 1 / 1 | Logic done (Track A; fleet p99 gate open) |
| 7 | C | Multi-tenancy and cache security | 1 / 1 | Logic done, wired into the router (Track A; fleet-traffic rollout open) |
| 8 | C | Learned corrector graduation | 2 / 2 | Logic done (Track A; live-traffic wiring open) |

---

## 4. Verification

### CPU bench gates

Hot-path code is gated on **median nanoseconds per operation** in `--release`. Thresholds live in [`design/bench-gates.toml`](design/bench-gates.toml). CI applies `settings.ci_slack` for runner jitter.

```bash
cargo run --release -q --package xtask -- bench-gate    # gate.sh / CI Track A
cargo run --release -q --package xtask -- bench-probe   # tune limits locally
cargo run --release -q --package xtask -- bench-flame   # flame SVG + headroom trends → target/bench-probe/flame.svg (--theme blueprint: drafting sheet, red=thin blue=so-so green=ok)
```

| ID | Phase | Measures | Local limit (median) |
|----|------:|----------|---------------------:|
| `BENCH-COMPOSE-8` | 0 | Cost composition (8 factors) | 50 ns |
| `BENCH-BACKEND-COST` | 0 | Single backend cost | 8 ns |
| `BENCH-SELECT-64` | 0 | Min-cost over 64 backends | 1 µs |
| `BENCH-CLASSIFY` | 1 | Request classification | 350 ns |
| `BENCH-ROUTE-DISPATCH` | 1 | Disaggregated dispatch (no I/O) | 350 ns |
| `BENCH-KV-RESERVE` | 2 | KV reservation hot path | 200 ns |
| `BENCH-WARM-LOOKUP` | 3 | Warmth probe | 500 ns |
| `BENCH-PAIR-GREEDY` | 4 | Greedy pf→dc pairing | 5 µs |
| `BENCH-REBALANCE` | 4 | Pool ratio π* | 800 ns |
| `BENCH-RCU-SNAPSHOT` | 5 | RCU routing read | 50 ns |
| `BENCH-IOURING-FWD` | 5+ | io_uring forward micro-path | 1.5 µs |

Track B gates ship in the same PR as the code they measure. Production TCP io_uring is validated by integration tests and `LOAD-TRACK-B-*` scenarios on Linux.

### Load and stress

**Load bench** — end-to-end TCP against router + mock backends. CI runs `load-bench --ci` (`LOAD-CI-SMOKE`, `LOAD-TRACK-B-IOURING`, `LOAD-TRACK-B-KERNEL`). Scenarios and soft p99 gates: [`design/load-bench.toml`](design/load-bench.toml).

```bash
./scripts/load-bench.sh                              # full suite + pseudo report
cargo run --release -q --package xtask -- load-bench --ci
```

**Stress** — strict local runs with zero-error gates. Not in `gate.sh` or CI.

```bash
./scripts/load-stress.sh    # → target/load-bench/stress.json
./scripts/pre-release.sh    # gate + load + stress + harden (pre-tag)
```

---

## 5. Cross-cutting design

These mechanisms span multiple phases. Parameters are canonical in [`design/demiurge.params.toml`](design/demiurge.params.toml).

### Short-context fast path

**Problem.** Disaggregated prefill→decode adds fixed overhead. Short prompts do not benefit from cross-pool transfer.

**Policy.** Classify at admission:

| Path | Condition | Behavior |
|------|-----------|----------|
| Colocated | Prompt ≤ `routing.short_context_tokens`, no warmth override | Single backend; no cross-pool hand-off |
| Disaggregated | Long context, warmth override, or saturation | Full async prefill → decode chain |

**Status.** Implemented (`DEMI-SHORT-FASTPATH`). Warmth override landed in Phase 3.

### KV cache overhead

**Problem.** Routing must account for real KV footprint, not idealized token bytes.

**Model.** Reservation includes payload, metadata overhead, and fragmentation slack. Fleet-level Φ barrier uses **aggregate** occupancy, not summed per-request p90.

**Status.** Implemented (Phase 2): `DEMI-KV-HANDOFF`, `DEMI-KV-OVERHEAD`, `DEMI-BARRIER-PHI`, `DEMI-KV-RELEASE`, `DEMI-KV-TRANSFER-TELEM`.

### Dynamic pool rebalancing

**Problem.** Static prefill/decode split wastes capacity as load mix shifts.

**Policy.** Compute target prefill share π* from queue, KV, and SLO pressure; apply with hysteresis and cooldown. **Shadow mode** logs counterfactuals without actuation until explicitly enabled.

**Status.** Shadow and actuation hooks implemented (`DEMI-POOL-RATIO`). Fleet-scale actuation validation is Track C.

---

## 6. Track A — Local development

Portable proof on macOS: mock TCP, userspace dataplane, full cost/control/state stack. Phases **0–5 are complete**.

### Phase 0 — Foundations

**Objective.** Design-driven toolchain and minimal honest router: log-space cost and min-cost selection.

**Deliverables.** `demiurge-cost`, `demiurge-router` shell, `xtask gen` / `xtask lint`, Gate CI.

**Requirements closed.** `DEMI-COST-POS`, `DEMI-CORR-CLAMP`, `DEMI-FAIL-EXPENSIVE`, `DEMI-ROUTE-MINCOST`.

**Out of scope.** XDP, RDMA, gossip, warmth, async prefill, migration, learned corrector in production.

---

### Phase 1 — Non-blocking routing

**Objective.** `Route` / `OnPrefillComplete` shape; accept path decoupled from prefill duration; short-context colocated branch.

**Requirements closed.** `ALG-ROUTE`, `DEMI-SHORT-FASTPATH`.

**Exit criteria.** Accept p99 independent of prefill delay; hand-off only on disaggregated path; `BENCH-CLASSIFY`, `BENCH-ROUTE-DISPATCH` pass.

---

### Phase 2 — KV hand-off and memory barriers

**Objective.** Explicit KV artifact between prefill and decode; overhead-aware reservation; Φ barrier prevents decode OOM.

**Deliverables.** `demiurge-handoff`, reservation ledger, hand-off telemetry, load/stress scenarios.

**Requirements closed.** `DEMI-KV-HANDOFF`, `DEMI-KV-OVERHEAD`, `DEMI-BARRIER-PHI`, `DEMI-KV-RELEASE`, `DEMI-KV-TRANSFER-TELEM`.

**Exit criteria.** 10× prefill burst without OOM; transfer p50/p99 logged; `BENCH-KV-RESERVE` pass.

---

### Phase 3 — State plane

**Objective.** AP warmth and occupancy gossip; stale state fails toward neutral, not cheap.

**Requirements closed.** `DEMI-WARM-DISCOUNT`, `DEMI-STATE-AP`.

**Exit criteria.** Warmth-aware routing beats baseline on replay; partition heals without CP; warmth override forces disaggregated path; `BENCH-WARM-LOOKUP` pass.

---

### Phase 4 — Control plane and pairing

**Objective.** Greedy pf→dc pairing, length predictor, pool rebalancer (shadow default), pairing-regret monitor.

**Requirements closed.** `DEMI-PAIR-GREEDY`, `DEMI-POOL-RATIO`.

**Exit criteria.** Pairing regret within budget; corrector **off** in production path (identity only); `BENCH-PAIR-GREEDY`, `BENCH-REBALANCE` pass.

---

### Phase 5 — Data plane hardening (proof)

**Objective.** Userspace proof that the data plane never blocks on control-plane publish; admit shedding; π actuation on hot path.

**Deliverables.** `demiurge-dataplane` (RCU, `AdmitBucket`, io_uring skeleton), router integration, load scenarios (`LOAD-STEP-ACTUATE`).

**Requirements closed.** `DEMI-DP-RCU`, `DEMI-XDP-SHED` (userspace bucket).

**Exit criteria.** CP stall does not inflate hot-path p99; admit shed on exhaustion; actuation under load; `BENCH-RCU-SNAPSHOT` pass.

**Portable extensions (complete).** Fleet pilot shadow (`cargo xtask fleet-pilot`), fleet simulation (`./scripts/apostrophe-sim.sh`), harden verify tiers, corrector shadow logging, RDMA transport trait + mock, topology reference ([`design/topologies/`](design/topologies/)).

Optional deep report: `./scripts/track-a-verify.sh` → `target/track-a-verify/report.md`.

---

## 7. Track B — Linux production

**Platform.** Linux x86_64 — XDP, io_uring, real NIC path.

**CI.** Gate Track B (BPF compile, XDP veth, p5 tests, `LOAD-TRACK-B-KERNEL`). [`publish-linux.yml`](.github/workflows/publish-linux.yml) publishes rolling [`linux-nightly`](https://github.com/fxdv/demiurge/releases/tag/linux-nightly) after green Gate on `main`.

### Phase 5+ — Kernel dataplane

**Objective.** Production kernel dataplane: XDP admission, io_uring L7 forwarder, reference-hardware exit gates.

**Status (June 2026).** Engineering path green on Linux VM (`./scripts/track-b-verify.sh` PASS): veth XDP, kernel admit + BPF map sync, `IoUringProxySession` on production `serve()`, Track B load scenarios, full load/stress.

**Shipped.**

| Component | Notes |
|-----------|--------|
| `bpf/admit_shed.bpf.c` | XDP token-bucket; CI artifact `target/bpf/admit_shed.o` |
| `XdpAdmitShed` (aya) | Load, attach, map seed/reseed; veth packet shed tests |
| Router kernel admit | `AdmitMode`, hybrid mode, actuation map sync |
| `IoUringProxySession` | Production TCP recv/send; `DEMIURGE_IOURING=1` |
| Load / bench | `LOAD-TRACK-B-*`, `BENCH-IOURING-FWD` |
| Scripts | `track-b-gate.sh`, `track-b-verify.sh`, `track-b-bench.sh`, Vagrant bootstrap |

**Exit criteria — open.**

- [ ] XDP on **production NIC** sheds before decode pool saturation under load.
- [ ] Data-plane admit p99 within budget under CP slowdown on **x86_64 reference** hardware.

**Exit criteria — met.**

- [x] BPF compiles in CI; runtime attach on veth; map sync with router actuation.
- [x] `linux-nightly` green on Ubuntu.
- [x] io_uring serves production TCP path.
- [x] `BENCH-IOURING-FWD` passes.

**Validation.**

```bash
./scripts/track-b-verify.sh           # full (~5–10 min)
./scripts/track-b-verify.sh --quick   # gate + benches + p5
./scripts/track-b-bench.sh            # ~1 min smoke
```

VM setup: [`scripts/linux-vm/README.md`](scripts/linux-vm/README.md).

---

## 8. Track C — Fleet and scale

Exit gates are measured on **reference fleet hardware**, not mock TCP alone.

### Phase 6 — Live migration

**Objective.** Abortable chunked migration; cutover only if estimated stall ≤ ε × ITL; atomic KV reservation transfer.

**Requirement.** `DEMI-MIG-SUBITL` (implemented — Track A logic + tests; fleet p99 gate open).

**Status.** Abortable chunked quiesce model, migration-stall telemetry, and atomic `ReservationGuard::resolve_migration` (commit/abort) ship as portable control-plane logic.

**Risk.** Sub-ITL cutover must still be **measured** on target hardware before fleet actuation; otherwise migration stays shadow-only.

---

### Phase 7 — Multi-tenancy and cache security

**Objective.** Opt-in prefix-cache sharing with CP-authorized membership; AP warmth, CP membership.

**Requirement.** `DEMI-S1-DOMAIN` (implemented — Track A logic, tests, and live-router wiring; fleet-traffic rollout open).

**Status.** `demiurge-auth` ships the Shared-Prefix Group registry with content-verified templates and tenant-salted cache-domain keys; `demiurge-state` gates salted warmth lookups on synchronous membership. `demiurge-router` now wires this into the live routing decision: `Router::with_cache_registry` attaches the registry, and `route_with_identity` gates the short-context warmth override and long-context prefill selection through `gated_hit_strength` — proven end-to-end against real `Backend`/`Router` selection (`p7_cache_isolation` integration tests), not just at the state-plane unit level. `route` (no identity) is unchanged. What remains open is wiring real tenant authentication and content verification — currently the caller's responsibility per `RequestIdentity`'s contract — onto live production traffic.

---

### Phase 8 — Learned corrector graduation

**Objective.** Shadow → canary → production corrector without violating `DEMI-CORR-CLAMP` or `DEMI-COST-POS`. Production actuation only after Phase 4 exit with corrector off.

**Requirement.** `DEMI-CORR-GRAD` (implemented — Track A logic + tests; live-traffic wiring open).

**Status.** `GraduationController` (`demiurge-control::corrector_grad`) implements the one-way-gated shadow → canary → production state machine: each evaluated window promotes one stage only if the trained δ clears `DEMI-CORR-CLAMP` (not pinned to the envelope boundary, checked via `is_clamp_saturated`) and the violation/goodput gate; any failure — at any stage, including `Production` — demotes straight back to `Shadow`. Shadow pipeline and offline eval are complete on Track A (`fleet-pilot`, corrector shadow tests).

**Risk.** Wiring the controller's window cadence and violation counters to live production traffic (vs. replayed/shadow samples) is Track C work; until that rollout the router runs `δ=1`.

**Validation (P/D proof gate on reference GPU fleet).**

```bash
./scripts/track-c-verify.sh              # logic + live smoke + warmth skew
./scripts/track-c-verify.sh --quick        # skip warmth bench
./scripts/track-c-verify.sh --logic-only   # P6/P7/P8 unit tests only
./scripts/track-c-verify.sh --ensure-up    # start vLLM + router, then verify
```

Artifacts: `target/track-c-verify/report.md`. Passing closes the **P/D proof slice**; RDMA prod handoff, fleet-measured migration p99, live corrector wiring, and tenant auth on production traffic remain open (listed in the report).

---

## 9. Adding a requirement

1. Add `[[requirement]]` to `design/requirements.toml` with `status = "intended"` and correct `phase`.
2. Add `\req{ID}` to `spec/demiurge.tex`.
3. Run `cargo xtask gen && cargo xtask lint`.
4. Implement with tests; set `status = "implemented"`, `requires_test = true`, list test names.
5. `./scripts/gate.sh` green; update the burndown table in this file if needed.

---

## 10. Related documents

| Document | Role |
|----------|------|
| [`spec/demiurge.tex`](spec/demiurge.tex) | Formal design contract |
| [`design/demiurge.params.toml`](design/demiurge.params.toml) | Tunable constants |
| [`design/requirements.toml`](design/requirements.toml) | Requirement registry |
| [`design/bench-gates.toml`](design/bench-gates.toml) | CPU gate thresholds |
| [`design/load-bench.toml`](design/load-bench.toml) | Load scenarios |
| [`docs/PRODUCT-AND-DESIGN.md`](docs/PRODUCT-AND-DESIGN.md) | Product narrative |
| [`README.md`](README.md) | Quickstart |
| [`CONTRIBUTING.md`](CONTRIBUTING.md) | Contribution and CI policy |
