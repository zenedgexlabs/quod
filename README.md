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

> Status: the selection engine and its certificates work for Python (pytest) and JS/TS (Vitest), with 0 dependency escapes on 11 open-source repositories ([E1](docs/benchmarks/e1.md)). Verdict lenses are next (V1).

## Quickstart (Python / pytest)

```sh
pip install ./probes/python                 # the pytest plugin, into the project's env
cargo build --release                       # builds `quod` and `quod-verify`

quod keygen --out ci-key.seed           # keep the seed secret; note the printed key id

# On a clean checkout of the base commit (e.g. main, after merge):
quod record --out .quod/map.qmap -- python -m pytest

# On a PR (working tree or --head <rev>):
quod select --map .quod/map.qmap --key ci-key.seed \
  --skip-out skip.txt --certs-out certs.jsonl --report-out report.json
python -m pytest --quod-skip=skip.txt   # runs everything not certified as skippable

# Anyone can re-check the skips independently:
quod-verify --certs certs.jsonl --map .quod/map.qmap --trusted-key ed25519:<key id>
```

Agents and local runs use `--advisory` (no key). Advisory certificates are never
accepted as proof by the verifier.

## Layout

| Path | Contents |
|---|---|
| `crates/quod-hash` | BLAKE3 hashing with domain separation, canonical CBOR, Merkle roots |
| `crates/quod-model` | v1 schemas: stable test IDs, footprint vocabulary, outcome-key envelope, certificates, journal events |
| `docs/architecture.md` | Production architecture (v4) |
| `docs/adr/` | Architecture decision records |

Planned crates (see the architecture doc): `quod-graph`, `quod-change`, `quod-policy`,
`quod-ladder`, `quod-cert`, `sc-store`, `quod-client`, `quod-cli`, `sc-mcp`, plus a
separate `verifier`, language `probes/`, and the `server/` control plane.

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
