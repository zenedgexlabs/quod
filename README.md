# Skipcert

Sound, certificate-based test selection for AI-agent-scale CI.

Skipcert runs only the tests a change can affect. Every skipped test gets a
certificate that an independent verifier can check. Results are shared across
branches and agents off the same base. The full suite still runs in the
background, so any missed failure is caught and published.

> Status: **M0 (foundations)**. Schemas and hashing only. Nothing is usable yet.

## Layout

| Path | Contents |
|---|---|
| `crates/sc-hash` | BLAKE3 hashing with domain separation, canonical CBOR, Merkle roots |
| `crates/sc-model` | v1 schemas: stable test IDs, footprint vocabulary, outcome-key envelope, certificates, journal events |
| `docs/architecture.md` | Production architecture (v4) |
| `docs/adr/` | Architecture decision records |

Planned crates (see the architecture doc): `sc-graph`, `sc-change`, `sc-policy`,
`sc-ladder`, `sc-cert`, `sc-store`, `sc-client`, `sc-cli`, `sc-mcp`, plus a
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
