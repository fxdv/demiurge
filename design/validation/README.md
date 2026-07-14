# Validation archives

Frozen reports from manual verification runs (lab / reference hosts). These complement
ephemeral `target/track-b-verify/` output from local `./scripts/track-b-verify.sh`.

| Archive | Host | Date | Scope |
|---------|------|------|-------|
| [`finland-track-b-2026-06-20/`](finland-track-b-2026-06-20/) | `finland.fxdv.cc` (82.26.171.22) | 2026-06-20 | Track B full verify + harden + BPF |
| [`singularity-2026-07-14/`](singularity-2026-07-14/) | singularity (176.123.167.143) | 2026-07-14 | Track C P/D proof + `benchmark-all` + kernel XDP veth |

**Labeling:** archives record **engineering proof** on the named host. Track B **production exit gates** (real NIC XDP under load, x86 p99 under CP slowdown on reference hardware) remain open until measured and recorded here.

**Track D** (fleet economics — $/token, goodput, OOM delta) uses the same archive layout with prefix `<host>-track-d-<date>/`. Protocol: [`design/track-d/README.md`](../track-d/README.md). *No Track D archive yet.*
