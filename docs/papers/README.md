# Demiurge papers

Standalone technical notes complementing [`spec/demiurge.tex`](../../spec/demiurge.tex).

| Paper | Source | Build |
|-------|--------|-------|
| **Cost algebra** | [`cost-algebra.tex`](cost-algebra.tex) | `make -C docs/papers` |

Covers the log-space factor algebra (§3--6) and the decision-makers that consume it (§7): admission, path classification, min-cost pairing, KV ledger, pool rebalancer, migration and corrector gates.

Output: `docs/papers/cost-algebra.pdf`

Requires `latexmk` and a TeX distribution (same as `cargo xtask spec`).
