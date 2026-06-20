# Track B validation archive — finland.fxdv.cc

**Date:** 2026-06-20 · **Duration:** ~6 min · **Verdict:** PASS (0 hard errors)

## Host

| Field | Value |
|-------|--------|
| Hostname | `finland.fxdv.cc` |
| Address | `82.26.171.22` |
| Arch | x86_64 |
| Virt | KVM (2 vCPU, 7.7 GiB RAM, 92 GiB root) |
| OS / kernel | Ubuntu 24.04.4 · 6.8.0-124-generic |
| NIC | `ens1` · Intel 82540EM · driver `e1000` (emulated) |
| Sudo | passwordless for `fxdv` |

## Scope

Gate (11 CPU bench gates) · load bench (13 scenarios) · stress (4 strict) · harden (3) · BPF veth XDP smoke.

**Not in scope for this archive:** XDP attach on `ens1` under saturation load; CP-stall p99 injection; bare-metal reference NIC.

---

## CPU bench gates (ns/op)

| Gate | median | p95 | limit | headroom |
|------|-------:|----:|------:|---------:|
| COMPOSE-8 | 22 | 25 | 50 | 127% |
| SELECT-64 | 332 | 367 | 1000 | 201% |
| BACKEND-COST | 3 | 5 | 8 | 167% |
| CLASSIFY | 91 | 107 | 350 | 285% |
| ROUTE-DISPATCH | 100 | 105 | 350 | 250% |
| KV-RESERVE | 9 | 11 | 200 | 2122% |
| WARM-LOOKUP | 81 | 86 | 500 | 517% |
| PAIR-GREEDY | 723 | 769 | 5000 | 592% |
| REBALANCE | 25 | 27 | 800 | 3100% |
| RCU-SNAPSHOT | 3 | 3 | 50 | 1567% |
| IOURING-FWD | 369 | 376 | 1500 | 307% |

Thin gates (COMPOSE-8, BACKEND-COST): noise-floor measurements; still passing.

---

## Load bench — 13 scenarios, 7060 req, 0 errors

| Scenario | req | rps | p99 |
|----------|----:|----:|----:|
| CI-SMOKE | 200 | 4766 | 4.4ms |
| TRACK-B-IOURING | 640 | 3250 | 14.6ms |
| STEADY-PREFILL | 800 | 4369 | 10.5ms |
| MIXED-PHASE | 1920 | 3914 | 14.5ms |
| KV-BURST | 48 | 3240 | 6.3ms |
| LARGE-POOL | 400 | 5104 | 7.5ms |
| CLASSIFY-MIX | 480 | 4051 | 9.5ms |
| DISAGG-CHAIN | 180 | 2902 | 8.6ms |
| P1-ACCEPT-DECOUPLE | 1600 | 1444 | 5.9ms |
| HOT-SPOT | 960 | 3678 | 19.7ms |
| PAIR-KV-PRESSURE | 192 | 3663 | 6.5ms |
| STEP-ACTUATE | 1280 | 3741 | 9.5ms |
| TRACK-B-KERNEL | 360 | 3346 | 10.5ms |

π* tracked correctly: colocated paths → π*=1.0; disaggregated → 0.0–0.5.

**Tightest p99 scout:** HOT-SPOT (19.7ms) — still well under soft gates (250–500ms).

---

## Stress — 21200 req, 363 graceful rejects (design-intent)

| Scenario | req | err | rps | p99 |
|----------|----:|----:|----:|----:|
| STRESS-REAL | 5000 | 0 | 3404 | 5.7ms |
| STRESS-KV-ARMY | 4800 | 0 | 4489 | 6.5ms |
| STRESS-FLOOD | 1800 | 0 | 3457 | 4.3ms |
| STRESS-ADMIT-FLOOD | 9600 | 363 | 4982 | 16.3ms |

ADMIT-FLOOD: 363 graceful rejects with admit capacity=32 — expected shedding.

---

## Harden — 532 req, 177 graceful KV rejects (design-intent)

| Scenario | req | err | p99 |
|----------|----:|----:|----:|
| KV-EXHAUST | 320 | 177 | 7.9ms |
| IOURING-LARGE-BODY | 32 | 0 | 4.4ms |
| RDMA-TOPO | 180 | 0 | 7.5ms |

---

## BPF

- Object: `admit_shed.o` (7080 bytes)
- XDP veth smoke: attach · drop · reseed — **PASS**

---

## Totals

| Metric | Value |
|--------|------:|
| Total requests | 28,792 |
| Hard errors | 0 |
| Graceful rejects | 540 (363 admit + 177 KV) |
| Pass rate | 100% |

---

## What this proves vs what remains open

**Proven on this host**

- x86_64 Track B engineering path: gate, io_uring, kernel XDP integration, load, stress, harden
- Fail-closed admit and KV shedding under intentional pressure
- Hot-path CPU headroom on 2-vCPU KVM

**Still open (Track B exit gates)**

- XDP shed on production-facing NIC (`ens1`) before decode saturation under load
- Dataplane p99 budget under control-plane slowdown (reference measurement)
- Bare-metal or virtio reference NIC (this run used emulated e1000)

See [`../README.md`](../README.md) for archive index.
