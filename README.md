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
> - Agents get advisory answers over MCP (`quod mcp`).
> - The GitHub Action is in
>   [integrations/github-action](integrations/github-action/README.md).

## Install

Each [release](https://github.com/zenedgexlabs/quod/releases) has `quod` and
`quod-verify` for Linux x86_64, macOS (arm64, x86_64) and Windows x86_64,
the Python probe wheel (`quod_probe`) and the JS probe package (`quod-js`),
each with a SHA-256 file. The GitHub Action downloads the binary by itself
(`quod-version`, default `latest`). To build from source: `cargo install
--path crates/quod-cli`.

## Quickstart (Python / pytest)

```sh
pip install ./probes/python                 # the pytest plugin, into the project's env
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
