# Contributing to Quod

Thanks for helping. Bug reports, reproductions and pull requests are all
welcome.

## The one rule

**Quod must never skip a test that the change can make fail.** When in
doubt, a change should run more tests, not fewer. A pull request that
improves precision has to show that it keeps every soundness invariant
(I1 to I6 in the [README](README.md#soundness-invariants)), usually with a
test that would catch the regression.

If you found a test that Quod skipped and that fails on the change, report
it as described in [SECURITY.md](SECURITY.md).

## Getting started

You need Rust (the toolchain is pinned in `rust-toolchain.toml`), and for
the probes Python 3.12+ with [uv](https://docs.astral.sh/uv/) and Node 22.12+.

```sh
git config core.hooksPath .githooks   # once per clone: local CI before each push
scripts/ci.sh                         # fmt, clippy, Rust tests, both probes
```

On Windows: `powershell -ExecutionPolicy Bypass -File scripts/ci.ps1`.

| Change | Where | Tests |
|---|---|---|
| Engine, policy, certificates | `crates/` | `cargo test` |
| Verifier | `verifier/` | `cargo test -p quod-verify` |
| Python probe | `probes/python` | `uv run pytest -q` |
| JS probe | `probes/js` | `npm test` |
| GitHub Action | `integrations/github-action` | run it on a fork |
| Benchmarks | `conformance/` | see [docs/benchmarks](docs/benchmarks) |

## Pull requests

- Keep each pull request to one change, and say why it's needed.
- Add or update tests. For probe or engine changes, a small repository in
  the test fixtures that reproduces the case is best.
- Decisions that change what Quod records or selects get an ADR in
  [docs/adr](docs/adr/README.md).
- `scripts/ci.sh` has to pass.

## Licence

Quod is licensed under the [Apache License 2.0](LICENSE). By submitting a
contribution you agree that it is licensed under the same terms (section 5
of the licence).
