# Quod

Sound, certificate-based test selection for AI-agent-scale CI.

Quod runs only the tests a change can affect. Every skipped test gets a
certificate that an independent verifier can check. Results are shared across
branches and agents off the same base. The full suite still runs in the
background, so any missed failure is caught and published.

> Status: **M1 in progress.** The end-to-end flow works for Python/pytest (record → select → verify). Not yet benchmarked on real repositories.

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
