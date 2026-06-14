# Contributing to Demiurge

Demiurge is **design-driven**: the spec in [`spec/`](spec/) is the contract, and
the code is checked against it in CI. A few rules keep the two from drifting.

## Contributor License Agreement (required)

**External contributions require a signed CLA** before merge.

1. Read [`CLA.md`](CLA.md) (Individual Contributor License Agreement).
2. On your first pull request, check the CLA box in the PR template **or** comment:
   `I have read the CLA and I sign it`.

The CLA grants the Maintainers the right to use your work under the project’s
**Apache-2.0 OR MIT** license and to relicense it in the future (including
commercial / dual-license offerings). This keeps monetization paths open while
the core stays open source.

Contributions from **employees of a company** require a Corporate CLA — open a
discussion with the Maintainers before your first merge.

Maintainers and pre-CLA history: commits already on `main` before this policy
remain under the repository license.

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
(strict zero-error gates; not part of `gate.sh` or CI). Before a release tag,
run `./scripts/pre-release.sh` (full gate + load bench incl. `LOAD-STEP-ACTUATE`
+ stress). To ship a local release artifact (binaries, validation logs, technical
one-pager), run `./scripts/publish.sh`; CI publishes **Linux** weekly via the
`publish-linux` workflow (rolling [`linux-nightly`](https://github.com/fxdv/demiurge/releases/tag/linux-nightly)
release) and on demand via the **release** workflow for tagged semver builds.

## CI gates

| Workflow | What it enforces |
|----------|------------------|
| `design-conformance` | generated artifacts are not stale; spec ⇄ code ⇄ test links are intact |
| `ci` | **Quality** (fmt, clippy, test, release build); **Regression** (CPU bench gates + load smoke) |
| `spec` | the design PDF compiles from regenerated inputs |
| `publish-linux` | **Weekly** Linux tarball + rolling [`linux-nightly`](https://github.com/fxdv/demiurge/releases/tag/linux-nightly) release (Mon 06:00 UTC); manual dispatch |
| `release` | manual semver tag release (Linux artifact + one-pager) |

All workflows share [`.github/actions/setup-rust`](.github/actions/setup-rust/) (toolchain + cache).
Local `./scripts/gate.sh` still mirrors **design-conformance + ci quality + regression**; it does not run pre-release or publish.

### CI structure (refactored)

| Before | After |
|--------|-------|
| 3 `ci` jobs each checkout + toolchain + cache | 2 jobs (`quality` → `regression`); shared `setup-rust` action |
| Toolchain/cache duplicated in 4 workflows | `.github/actions/setup-rust` reused everywhere |
| Release path hard-coded in workflow YAML | `scripts/publish.sh` writes `target/release-artifacts/publish.env` |

**Further opportunities** (not done — trade-offs):

- Merge `design-conformance` into `ci` quality job (one less workflow badge, ~30s saved on PRs).
- Pass release `target/` via artifacts from `quality` to `regression` (skip second `cargo build --release`; cache usually makes this marginal).
- Reusable workflow wrapping `./scripts/gate.sh` flags so CI and local gate share one entrypoint.
- `publish-linux` on every green `main` push (currently weekly only — pre-release is ~4 min + 2 min port recovery).
