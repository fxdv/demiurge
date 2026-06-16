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
4. **Decisions go in ADRs** (when we add `docs/adr/`), not in the spec. The spec is steady-state truth;
   the *why* is an architecture decision record.
5. **Phased delivery.** Pick work from [`ROADMAP.md`](ROADMAP.md); register new
   requirement IDs before implementing; close a phase by flipping `status` to
   `implemented` with named tests.

## Before you push

```bash
./scripts/bootstrap.sh   # once: installs components + a pre-push gate hook (full gate)
./scripts/gate.sh --quick   # inner loop while hacking
./scripts/gate.sh        # full CI mirror before merge
```

`scripts/gate.sh --quick` runs gen, drift check, lint, fmt, clippy, and tests only.
The full gate adds release build, CPU bench gates, load smoke, fleet-pilot, Track B
(Linux), and optional spec PDF — same as CI.

For heavy local validation after Phase 2 changes, run `./scripts/load-stress.sh`
(strict zero-error gates; not part of `gate.sh` or CI). Before a release tag,
run `./scripts/pre-release.sh` (full gate + load bench incl. `LOAD-STEP-ACTUATE`
+ stress).

**Track B (Linux only).** After `./scripts/gate.sh` on a Linux VM or CI mirror:

```bash
./scripts/track-b-verify.sh           # full gate + bench-probe + load + stress + report
./scripts/track-b-verify.sh --quick   # gate + CPU benches + p5 tests
./scripts/track-b-bench.sh            # CPU probe/gate + XDP veth smoke (~1 min)
```

See [`scripts/linux-vm/README.md`](scripts/linux-vm/README.md) for Vagrant/Docker setup.

To ship a local release artifact (binaries, validation logs, technical
one-pager, **product & design PDF**), run `./scripts/publish.sh`. Render the
product brief locally with `cargo xtask product-doc` (needs `pandoc` + TeX).
CI publishes **Linux** weekly via the
`publish-linux` workflow (rolling [`linux-nightly`](https://github.com/fxdv/demiurge/releases/tag/linux-nightly)
release) and on demand via the **release** workflow for tagged semver builds.

## CI gates

| Workflow | What it enforces |
|----------|------------------|
| [`gate.yml`](.github/workflows/gate.yml) | **Policy** (PR-only: same-PR coupling + CLA); **Verify** (gen/drift/lint + fmt/clippy/test/release build); **Track A** (CPU bench gates + load smoke + fleet-pilot); **Track B** (BPF compile + XDP veth + p5 tests + `LOAD-TRACK-B-KERNEL`); **Spec · PDF** when `spec/` or `design/` changes |
| [`publish-linux.yml`](.github/workflows/publish-linux.yml) | Linux tarball + rolling [`linux-nightly`](https://github.com/fxdv/demiurge/releases/tag/linux-nightly) after green Gate on `main`, weekly Mon 06:00 UTC, or manual dispatch |
| [`release.yml`](.github/workflows/release.yml) | manual semver tag release (Linux artifact + one-pager) |

All workflows share [`.github/actions/setup-rust`](.github/actions/setup-rust/) (toolchain + cache).
Gate jobs call [`.github/workflows/gate-phase.yml`](.github/workflows/gate-phase.yml) → `./scripts/gate.sh --ci-*`.
PR-only policy: [`scripts/pr-policy.sh`](scripts/pr-policy.sh) (same-PR file coupling + CLA acknowledgment).
Local `./scripts/gate.sh` mirrors **Verify + Track A + Track B** (sequential); `--quick` skips release bench/load/Track B.

### CI structure

| Item | Status |
|------|--------|
| Shared `setup-rust` action | done |
| Reusable `gate-phase.yml` → `gate.sh --ci-*` | done |
| Design-conformance in Verify job | done |
| Release `target/release/` artifact reuse (Verify → Track A/B) | done |
| `publish-linux` on every green `main` Gate | done |
| `scripts/publish.sh` → `publish.env` for release workflows | done |
