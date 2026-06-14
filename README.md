<div align="center">

# Demiurge

**A phase-aware, cache-locality-first load balancer for inference fleets** *(Phases 0–5 shipped locally; production economics TBD).*

*We're building the missing control/dataplane layer for disaggregated LLM serving — early proof is green; disruption depends on production economics.*

*Target: route prefill and decode as independent phases across two pools, with the KV cache as the explicit hand-off artifact — because an inference request is a lease on stateful accelerator memory, not a packet.*

[![design-conformance](https://github.com/fxdv/demiurge/actions/workflows/design-conformance.yml/badge.svg)](https://github.com/fxdv/demiurge/actions/workflows/design-conformance.yml)
[![ci](https://github.com/fxdv/demiurge/actions/workflows/ci.yml/badge.svg)](https://github.com/fxdv/demiurge/actions/workflows/ci.yml)
[![spec](https://github.com/fxdv/demiurge/actions/workflows/spec.yml/badge.svg)](https://github.com/fxdv/demiurge/actions/workflows/spec.yml)
[![invariant: C&gt;0](https://img.shields.io/badge/invariant-C%3E0%20by%20construction-005aa0)](#invariants-that-cant-rot)
[![license](https://img.shields.io/badge/license-Apache--2.0%20OR%20MIT-blue)](#license)

</div>

> **The name.** In Platonic cosmology the *demiurge* is the craftsman who shapes
> formless chaos into an ordered cosmos — which is precisely this system's job:
> imposing locality-aware order on chaotic inference traffic.

> **Status.** Phases **0–4** are implemented and gated in CI: cost algebra, async
> routing with short-context fast path, KV hand-off (TCP proof transport),
> overhead-aware reservation, the Φ memory-pressure barrier, warmth-aware state
> plane (AP gossip + RCU snapshots), and shadow control-plane pairing/rebalancing.
> XDP admission, io_uring data plane, live migration, and cross-tenant cache
> sharing remain **design intent** for later phases. See
> [Status](#status-what-exists).

---

## Table of contents

- [Why Demiurge](#why-demiurge)
- [The bet](#the-bet)
- [Architecture at a glance](#architecture-at-a-glance)
- [Repository layout](#repository-layout)
- [Quickstart](#quickstart)
- [Design-driven development](#design-driven-development)
  - [The single source of truth](#the-single-source-of-truth)
  - [Invariants that can't rot](#invariants-that-cant-rot)
  - [Traceability: spec ⇄ code ⇄ test](#traceability-spec--code--test)
- [Everyday workflows](#everyday-workflows)
- [Roadmap & gates](#roadmap--gates)
- [Contributing](#contributing)
- [License](#license)

---

## Why Demiurge

Round-robin and least-connections optimize for connection equivalence. For LLM
inference that's wrong on three counts, all at once:

- the most valuable backend state — the **KV cache** — is request-correlated, not interchangeable;
- the cost of a request depends on the target's **current batch and active KV footprint**, not a fixed weight;
- occupancy is a **random variable**, not a constant.

Demiurge is built to exploit exactly those three facts.

## Status: what exists

| Area | State |
|------|-------|
| Cost-factor algebra (`demiurge-cost`), log-space, positive by construction, fail-expensive | **implemented + property-tested** (P0) |
| Minimal forwarder (`demiurge-router`): phase pools, least-cost selection, live in-flight load | **implemented + tested** (P0) |
| Async `Route` / non-blocking prefill + short-context fast path | **implemented + tested** (P1) |
| KV hand-off (`demiurge-handoff`), reservation ledger (`demiurge-control`), Φ barrier | **implemented + tested** (P2) |
| State plane (`demiurge-state`): warmth map, occupancy gossip, RCU snapshots | **implemented + tested** (P3) |
| Control plane: greedy pf→dc pairing, length predictor, shadow rebalancer | **implemented + tested** (P4) |
| Design-conformance tooling (`xtask gen`/`lint`), CI, spec PDF | **implemented** |
| CPU bench gates (`bench-gates.toml`, `cargo xtask bench-gate`) | **implemented** — in CI |
| Local load bench (`load-bench.sh`, pseudo report) | **implemented** — CI runs `load-bench --ci` smoke |
| Real stress suite (`load-stress.sh`, strict zero-error gates) | **implemented** — local only, not in CI |
| XDP/L4 admission, io_uring data plane, RCU data-plane serving | design intent (P5) — RCU + admit shed proof landed |
| RDMA hand-off production path, live migration | design intent (P6) |
| Cross-tenant cache sharing, learned corrector graduation | design intent (P7–P8) |

The `spec/` document is the *target* design; the generated conformance matrix
marks each requirement `implemented` or `intended` so the two never blur.

## The bet

> **Disaggregated prefill/decode-aware routing as the organizing principle of the entire balancer.**

Prefill is compute-bound, bursty, embarrassingly parallel, and cache-*producing*.
Decode is memory-bandwidth-bound, long-lived, latency-sensitive, and
cache-*consuming*. Demiurge schedules the two phases independently across two
pools and treats the KV cache as the explicit hand-off between them. Full
reasoning — alternatives rejected and what we deliberately sacrifice — lives in
[`spec/`](spec/).

## Architecture at a glance

Three planes, three consistency models, three blast radii:

```mermaid
flowchart TB
    subgraph DP["Data plane · microsecond budget"]
        XDP["eBPF / XDP — L4 admission, DDoS shed"]
        L7["Rust io_uring L7 forwarder — routing key, RCU snapshot"]
    end
    subgraph CP["Control plane · millisecond budget · consensus (CP)"]
        COST["Cost-function evaluator + bounded corrector"]
        PRED["Length predictor (p50/p90/p99)"]
        POOL["Pool-ratio autoscaler"]
    end
    subgraph SP["State plane · gossip (AP)"]
        WARM["KV warmth map (Cuckoo filters)"]
        OCC["Live occupancy / batch state"]
    end
    XDP --> L7 --> COST
    COST <--> WARM
    COST <--> OCC
    PRED --> COST
    POOL --> COST
```

- **Data plane** never blocks on the control plane; it serves the last RCU snapshot.
- **Control plane** holds the policy and republishes weights at a bounded cadence.
- **State plane** is eventually consistent on purpose — a wrong guess costs a cache miss, never a correctness violation. *Authorization* (who may share a cache line) is the one thing kept strongly consistent.

## Repository layout

| Path | What it is |
|------|------------|
| [`design/demiurge.params.toml`](design/demiurge.params.toml) | **Single source of truth** for every tunable constant. |
| [`design/bench-gates.toml`](design/bench-gates.toml) | CPU hot-path gate thresholds (median ns/op, release). |
| [`design/load-bench.toml`](design/load-bench.toml) | Local TCP load scenarios + optional p99 soft gates. |
| [`design/requirements.toml`](design/requirements.toml) | Registry of normative/structural requirement IDs + phase tags. |
| [`ROADMAP.md`](ROADMAP.md) | **Concrete build plan** — phased deliverables, gates, burndown. |
| [`spec/demiurge.tex`](spec/) | Full target design; §1 lists shipped vs intended scope |
| `spec/generated/` | `@generated` parameter & conformance tables — never hand-edited. |
| [`crates/demiurge-cost/`](crates/demiurge-cost/) | The cost-function factor algebra and its property tests. |
| [`crates/demiurge-router/`](crates/demiurge-router/) | Phase-aware forwarder: async route, fast path, KV pool integration. |
| [`crates/demiurge-handoff/`](crates/demiurge-handoff/) | KV hand-off descriptor, registry, TCP transport (RDMA trait later). |
| [`crates/demiurge-control/`](crates/demiurge-control/) | Reservation ledger, TTL release, admit/reject metrics. |
| [`xtask/`](xtask/) | `gen`, `lint`, `bench-gate`, `load-bench`, `load-report`. |
| [`scripts/`](scripts/) | `bootstrap.sh`, `gate.sh`, `gen.sh`, `load-bench.sh`, `load-stress.sh`. |

## Quickstart

```bash
./scripts/bootstrap.sh        # once: toolchain components + pre-push gate hook
cargo xtask gen               # regenerate everything derived from canonical inputs
cargo xtask lint              # enforce the spec ⇄ code ⇄ test join
cargo run --release -q --package xtask -- bench-gate  # CPU hot-path gates
cargo run --release -q --package xtask -- bench-probe  # floor/p95 probe + thin-gate report
./scripts/load-bench.sh       # local TCP load + pseudo report (optional)
./scripts/load-stress.sh      # strict heavy stress — local only, not in gate.sh
cargo test --all              # run the executable invariants (C>0, ±α)
./scripts/gate.sh             # run the full local gate (mirrors CI)
```

If `cargo xtask gen` changes any tracked file, commit it — CI fails on stale
generated artifacts.

## Design-driven development

The spec isn't documentation that trails the code; it's the contract the code is
checked against. Three mechanisms keep them honest, all enforced in CI.

### The single source of truth

Every constant lives in **one** file:

```toml
# design/demiurge.params.toml
[corrector]
alpha = 0.20
```

`cargo xtask gen` projects it into both worlds:

- `crates/demiurge-cost/src/generated_params.rs` → the Rust constants the binary uses,
- `spec/generated/params_table.tex` → the table the spec prints.

Change `α` once, regenerate, and the prose and the binary move together.

### Invariants that can't rot

Cost is represented by its **natural logarithm** and composed by *adding* logs:

```
ln C = ln(TimeCore>0) + Σ ln(Barrier≥1) + Σ ln(Discount∈(0,1]) + ln(Corrector∈[1−α,1+α])
```

A finite log is the logarithm of a strictly-positive real, so positivity is
genuinely by construction — there is no linear product to underflow to `0.0` or
flip sign (the failure mode an earlier draft had), and comparison uses the exact
log. Rewards enter only as discounts (never subtraction), and invalid hot-path
signals saturate *toward expensive*, so a broken metric can't make a sick
backend look cheap. The properties are enforced in four layers:

| Layer | Mechanism | Guards against |
|-------|-----------|----------------|
| Types | log-space `Cost`, positive factor newtypes | structurally invalid composition |
| CI tests | `proptest` + named traceability tests | regressions in algebra and routing |
| CI bench | `cargo xtask bench-gate` | hot-path CPU regressions |
| Prod | `FACTOR_CLAMP_EVENTS` metric / alarms | drift the first two miss |

### Traceability: spec ⇄ code ⇄ test

Every normative claim has a stable ID — `DEMI-COST-POS`, `DEMI-CORR-CLAMP`,
`DEMI-S1-DOMAIN`, … — appearing verbatim in all three places:

```text
spec:  \req{DEMI-COST-POS}            (prose, §4.3)
code:  /// [DEMI-COST-POS] ...        (doc-comment on the function)
test:  tests = ["cost_strictly_positive", ...]  (named in requirements.toml)
```

`cargo xtask lint` enforces: (1) every reference resolves to a declared
requirement; (2) every declared requirement is referenced in the spec or code;
(3) every `implemented` requirement lists named `#[test]` / proptest functions
that exist.

## Everyday workflows

**Change a parameter**

```bash
$EDITOR design/demiurge.params.toml   # edit the one value
cargo xtask gen                       # propagate to code + spec
cargo test --all                      # confirm invariants still hold
git add -A && git commit              # spec + code move in lockstep
```

**Add a normative requirement** — add `\req{DEMI-NEW-THING}` in the spec, a row in
`requirements.toml`, and reference `[DEMI-NEW-THING]` in the function and its test;
`cargo xtask lint` must pass.

**Land a new module** — flip its requirement from `requires_test = false` to
`true` in the same PR. Conformance ratchets tighter as the system grows, never
looser.

## Roadmap & gates

The full phased plan — deliverables, requirement IDs, exit gates, cross-cutting
plans (short-context fast path, KV overhead accounting, dynamic pool
rebalancing), and the live burndown — lives in **[`ROADMAP.md`](ROADMAP.md)**.

Track progress: `cargo xtask lint` prints per-phase burndown
(`P0: 4/4`, `P1: 2/2`, `P2: 5/5`, `P3: 2/2`, `P4: 2/2`, …). The spec conformance matrix includes a Phase column.

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md). External contributors must sign the
[`CLA.md`](CLA.md) before merge (see PR template).

## License

Dual-licensed under **Apache-2.0 OR MIT** — see [`LICENSE-APACHE`](LICENSE-APACHE)
and [`LICENSE-MIT`](LICENSE-MIT).

---

<div align="center">
<sub>Demiurge — design spec v1.4 · the spec is the contract, the code is the proof.</sub>
</div>
