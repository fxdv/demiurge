# Contributing to Demiurge

Demiurge is **design-driven**: the spec in [`spec/`](spec/) is the contract, and
the code is checked against it in CI. A few rules keep the two from drifting.

## The rules

1. **Same-PR rule.** A behavior change and its spec change land in the same PR.
   If you change a normative claim, update `\req{}` in the spec, the matching
   `[ID]` in code, and `design/requirements.toml` together.
2. **One source of truth.** Tunable constants live only in
   [`design/demiurge.params.toml`](design/demiurge.params.toml). Never hand-edit
   anything under `spec/generated/` or `crates/demiurge-cost/src/generated_params.rs`
   — run `cargo xtask gen` instead.
3. **New normative requirement?** Add a row to `design/requirements.toml`,
   reference its ID from the spec and (if `requires_test = true`) from a test.
   `cargo xtask lint` must pass.
4. **Decisions go in ADRs**, not in the spec. The spec is steady-state truth;
   the *why* is an architecture decision record.
5. **Phased delivery.** Pick work from [`ROADMAP.md`](ROADMAP.md); register new
   requirement IDs before implementing; close a phase by flipping `status` to
   `implemented` with named tests.

## Before you push

```bash
./scripts/bootstrap.sh   # once: installs components + a pre-push gate hook
./scripts/gate.sh        # runs the same checks CI runs
```

`scripts/gate.sh` regenerates artifacts, fails on drift, runs the traceability
lint, `cargo fmt --check`, release build, `cargo clippy -D warnings`, the test
suite, CPU bench gates, load regression smoke (`load-bench --ci`), and (if
`latexmk` is installed) compiles the spec.

For heavy local validation after Phase 2 changes, run `./scripts/load-stress.sh`
(strict zero-error gates; not part of `gate.sh` or CI).

## CI gates

| Workflow | What it enforces |
|----------|------------------|
| `design-conformance` | generated artifacts are not stale; spec ⇄ code ⇄ test links are intact |
| `ci` | **Build** (release workspace + binary check); **lint & test**; **regression** (CPU bench gates + load smoke) |
| `spec` | the design PDF compiles from regenerated inputs |
