# Validation archives

Frozen reports from manual verification runs (lab / reference hosts). These complement
ephemeral `target/track-b-verify/` output from local `./scripts/track-b-verify.sh`.

| Archive | Host | Date | Scope |
|---------|------|------|-------|
| [`finland-track-b-2026-06-20/`](finland-track-b-2026-06-20/) | `finland.fxdv.cc` (82.26.171.22) | 2026-06-20 | Track B full verify + harden + BPF |

**Labeling:** archives record **engineering proof** on the named host. Track B **production exit gates** (real NIC XDP under load, x86 p99 under CP slowdown on reference hardware) remain open until measured and recorded here.
