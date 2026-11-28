# Quod

**Can this change merge, and why should you believe it?** Quod answers with a
signed Verdict for every change, from humans and AI agents alike.

*Quod* is short for *quod erat demonstrandum*: "which was to be proven".

Quod records what every test actually depends on: functions, packages,
environment variables and module state. From that evidence graph it derives:

- the tests a change obligates, with a signed certificate for every test skipped;
- changed code that no test executes;
- tests weakened or removed alongside the code they guard;
- failures that provably aren't caused by the change (flaky or environment);
- the commit that broke main.

An independent open verifier re-checks every claim. See
[docs/architecture-v5-verdict.md](docs/architecture-v5-verdict.md).

> **Status (V1).**
>
> - The selection engine, certificates and signed Verdicts work for Python
>   (pytest) and JS/TS (Vitest, Jest).
> - E1 measured 0 dependency escapes on open-source repositories in both
>   languages ([E1](docs/benchmarks/e1.md)).
> - On GitHub-hosted CI, replaying real history with injected bugs, the
>   Action caught 65 of 65 failing tests with 0 escapes while running 32% to
>   41% of the tests ([E9](docs/benchmarks/e9.md)). The agent loop over MCP
>   had 0 escapes in 58 steps ([E8](docs/benchmarks/e8.md)).
> - Agents get advisory answers over MCP (`quod mcp`).
> - The GitHub Action is in
>   [integrations/github-action](integrations/github-action/README.md).
> - Supported versions, and when Quod runs every test:
>   [docs/limits.md](docs/limits.md).

## Skip tests in CI (GitHub Actions)

Add one workflow. Pushes to `main` record what each test depends on;
every pull request then gets a plan, and a Verdict card in the job summary.

```yaml
# .github/workflows/quod.yml
name: quod
on:
  push: { branches: [main] }
  pull_request:

jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with: { fetch-depth: 0 }
      - uses: actions/setup-python@v5
        with: { python-version: '3.13' }   # the probe needs Python 3.12+
      - run: pip install -e .[test]        # your project and its test deps
      - uses: zenedgexlabs/quod-action@v0.1.0-alpha.7
        with:
          mode: ${{ github.event_name == 'push' && 'record' || 'shadow' }}
          test-command: python -m pytest -q
          python: python
```

See it on a real project in
[zenedgexlabs/quod-demo](https://github.com/zenedgexlabs/quod-demo): one
pull request skips 55 of 59 tests, and another has a bug that Quod
catches.

- **Shadow mode** (start here) still runs every test, and the card says
  which tests Quod would have skipped, each with a certificate. Nothing
  about your CI changes.
- **Enforce mode** skips the certified tests. Switch when the shadow
  Verdicts have earned your trust.
- For Vitest or Jest, set `runner: vitest` (or `jest`) and install your
  dependencies with your own package manager. See the
  [Action's README](integrations/github-action/README.md).

Quod saves the most on slow suites and large repositories. On a suite
that finishes in seconds, start-up dominates, and what Quod adds is the
evidence for each skip rather than speed.

## Install

Each [release](https://github.com/zenedgexlabs/quod/releases) has `quod` and
`quod-verify` for Linux x86_64, macOS (arm64, x86_64) and Windows x86_64,
the Python probe wheel (`quod_probe`, also on PyPI as
[`quod-probe`](https://pypi.org/project/quod-probe/)) and the JS probe
package (`quod-js`, also on npm as
[`quod-js`](https://www.npmjs.com/package/quod-js)), each with a SHA-256
file. The GitHub Action downloads the binary by itself
(`quod-version`, default `latest`). To build from source: `cargo install
--path crates/quod-cli`.

## Quickstart (Python / pytest)

```sh
pip install quod-probe                      # the pytest plugin (PyPI), into the project's env
cargo build --release                       # builds `quod` and `quod-verify`

quod keygen --out ci-key.seed           # keep the seed secret; note the printed key id

# On a clean checkout of the base commit (e.g. main, after merge):
quod record --out .quod/map.qmap -- python -m pytest

# On a PR (working tree or --head <rev>): plan, run, seal.
quod select --map .quod/map.qmap --key ci-key.seed \
  --skip-out skip.txt --certs-out certs.jsonl --report-out report.json
python -m pytest --quod-skip=skip.txt --quod-results=results.jsonl
quod verdict --map .quod/map.qmap --key ci-key.seed --results results.jsonl \
  --out verdict.json --card card.md      # exit 0 = PASS

# Anyone can re-check the skips and the Verdict independently:
quod-verify --certs certs.jsonl --map .quod/map.qmap --trusted-key ed25519:<key id>
quod-verify --verdict verdict.json --map .quod/map.qmap --certs certs.jsonl \
  --trusted-key ed25519:<key id>
```

Agents and local runs use `--advisory` (no key). Advisory certificates and
Verdicts are never accepted as proof.

**Coding agents** get the same answers over MCP. `quod mcp install --client
claude-code|cursor|vscode` registers the server; it pulls the newest green
map CI published and serves `tests_for` (with a command that runs exactly
the obligated tests), `why`, `culprit`, `status` and `check`. See
[docs/agents.md](docs/agents.md).

## JS/TS (Vitest, Jest)

```sh
node probes/js/src/cli.mjs record --out capture.jsonl --root . -- <runner args>
quod ingest --capture capture.jsonl --out .quod/map.qmap
node probes/js/src/cli.mjs run --skip skip.txt --results results.jsonl --root . -- <runner args>
```

The runner is detected (Vitest if configured, else Jest), or set it with
`--runner vitest|jest`. Your config is wrapped, never modified. Jest needs
Node 22.12 or later.

## Layout

| Path | Contents |
|---|---|
| `crates/quod-hash` | BLAKE3 hashing with domain separation, canonical CBOR, Merkle roots |
| `crates/quod-model` | v1 schemas: stable test IDs, footprint vocabulary, outcome-key envelope, certificates, journal events |
| `crates/quod-graph` | Footprint graph (`.qmap`): bidirectional CSR, Merkle-rooted encoding |
| `crates/quod-change` | Git diffs, tree-sitter entity diffs (Python, JS/TS, Go, Rust, Java), lockfile and config deltas |
| `crates/quod-policy` | Fail-closed rule catalog (R001–R018), integrity rules, environment deltas |
| `crates/quod-ladder` | Selection: changed nodes ∩ footprints, certificates |
| `crates/quod-cert` | Ed25519 signing, certificate I/O |
| `crates/quod-verdict` | Lenses (Untested-change, Integrity, Flake-proof) and the signed in-toto Verdict |
| `crates/quod-cli` | The `quod` binary, including `quod mcp` |
| `verifier/` | `quod-verify`: independent checks of certificates and Verdicts |
| `probes/python`, `probes/js` | pytest plugin; Vitest and Jest probe |
| `integrations/github-action` | GitHub Action (record, shadow, enforce) |
| `conformance/` | E1 replay harness over real repositories |
| `docs/` | Architecture ([v4](docs/architecture.md) engine, [v5](docs/architecture-v5-verdict.md) Verdict), [ADRs](docs/adr/README.md), benchmarks |

## Development

Run local CI, which does the same checks as `.github/workflows/ci.yml`:

```sh
scripts/ci.sh                                                # Git Bash / Linux / macOS
powershell -ExecutionPolicy Bypass -File scripts/ci.ps1      # Windows PowerShell
```

Enable the pre-push hook once per clone. It blocks a push when local CI fails:

```sh
git config core.hooksPath .githooks
```

Hosted GitHub Actions is configured but currently blocked at the account
level. Until that's resolved, local CI plus the hook is the gate.

The toolchain is pinned in `rust-toolchain.toml` (stable, with rustfmt and clippy).

## Soundness invariants

- **I1:** a skip requires a valid certificate that the verifier accepts.
- **I2:** a disjoint skip needs an empty intersection between the change and the test's footprint and root scope.
- **I3:** missing, stale or unverifiable data widens selection.
- **I4:** a nondeterministic test is never skipped through dedup.
- **I5:** changed test code always runs.
- **I6:** only trusted CI mints certificates.

## Contributing and security

See [CONTRIBUTING.md](CONTRIBUTING.md). Report a skipped test that fails,
or any signing issue, privately as described in [SECURITY.md](SECURITY.md).

## License

Apache License 2.0. See [LICENSE](LICENSE).
