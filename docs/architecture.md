# Skipcert production architecture (v4)

*2026-11-21. This is the engineering architecture derived from `Skipcert use cases.md`. It supersedes v3's system sections; v3's packaging and pricing still apply.*
*Design goal: a production-grade system, not a prototype. The schemas and trust boundaries below are the decisions that can't be undone later.*

---

## 1. Scope

| Phase | Use cases served | Status |
|---|---|---|
| **Now (MVP, through M5 / Apr 2027)** | 1. Agent-PR CI selection<br>2. Cross-branch dedup + merge-queue hook<br>3. Agent inner loop (offline sandbox, CLI + MCP)<br>4. Dependency-bump PRs<br>5. Flaky detection / suite health<br>6. Agent-written-test integrity policy | Build |
| **Next (designed for, not built)** | LLM evals in CI, mutation testing, integration/contract tests, review/impact API for partners, regulated evidence packaging (SOX/PCI/DORA/FDA CSA) | Schemas and extension points reserved |
| **Later** | Monorepo adapters, browser E2E (cross-process tracing), dbt, safety-critical (ISO 26262 / DO-178C), RL harness library, security reachability evidence | Must not be blocked |

**Languages in the MVP:** Python (pytest) first, then TypeScript (Jest and Vitest).

**CI in the MVP:** GitHub Actions first, Buildkite next.

---

## 2. Architectural drivers (quality attributes, in priority order)

1. **Soundness.** No skip without a valid certificate. Any uncertainty fails closed, which means running the test. Stale or missing data *widens* selection.
2. **Trust.** Only trusted CI identities can mint certificates or write to shared outcomes. Sandbox and local decisions are advisory.
3. **Offline-first.** The selection critical path needs no network: agent sandboxes and air-gapped sites must work.
4. **Latency.** Selection takes ≤ 1 s end to end for 100k tests, including diffing a dirty working tree. Lookups are in-process and take milliseconds.
5. **Scale under burst.** CI jobs can grow 25x with no backlog growth. Workers are stateless and the append-only journal is the source of truth (the Anthropic lesson).
6. **Schema durability.** The footprint graph is bidirectional, has external-package nodes and a typed outcome-key envelope, and every format carries a version. Stored data must never need a rewrite.
7. **Privacy.** Source code never leaves the customer. The control plane sees hashes and metrics only.
8. **Auditability.** Every decision can be re-derived bit for bit from a pinned-inputs manifest, checked by a separate small verifier.

---

## 3. Logical architecture (bounded contexts)

```
 ┌───────────── Capture ─────────────┐   ┌──────────── Knowledge ─────────────┐   ┌─────────── Decision ───────────┐
 │ Probes (py, node, syscall shim)   │──►│ Footprint Graph (bidirectional)    │──►│ Change Resolver (git, AST,     │
 │ Trace normalizer                  │   │ Package registry (ext. nodes)      │   │   lockfile delta)              │
 │ Determinism observer              │   │ Test registry (stable IDs, flake)  │   │ Policy engine (fail-closed)    │
 └───────────────────────────────────┘   │ Outcome index (typed keys)         │   │ Skip ladder L0/L1/(L2)/L3      │
                                         └────────────────────────────────────┘   │ Certificate minting            │
 ┌───────────── Trust ───────────────┐   ┌──────────── Operations ────────────┐   └────────────────────────────────┘
 │ Identity (CI OIDC), signing (KMS) │   │ Journal + rollups (stateless)      │   ┌─────────── Interfaces ─────────┐
 │ Verifier (independent package)    │   │ Audit service + miss auditor       │   │ CLI, pytest/Jest plugins, GH   │
 │ Namespaces (trusted / untrusted)  │   │ Map distribution (fetch-map, CDN)  │   │ Action/App, MCP, REST/batch,   │
 └───────────────────────────────────┘   │ Scheduler (merge queue, ordering)  │   │ impact API, dashboard          │
                                         └────────────────────────────────────┘   └────────────────────────────────┘
```

---

## 4. Repository and module layout

```
skipcert/
├── crates/                         # Rust workspace (core is one static binary + libs)
│   ├── sc-model/                   # all on-disk/on-wire schemas, versioned; no logic
│   ├── sc-hash/                    # BLAKE3 content hashing, canonical encoding, Merkle
│   ├── sc-graph/                   # footprint graph: build, mmap read, CSR both directions
│   ├── sc-change/                  # git diff, dirty tree, tree-sitter function diff, lockfile resolvers
│   ├── sc-policy/                  # declarative fail-closed rules, integrity policy, never-skip
│   ├── sc-ladder/                  # selection: L0/L1/L2/L3 orchestration, pure functions
│   ├── sc-cert/                    # certificate minting, hash chain, signing backends
│   ├── sc-store/                   # CAS backends: fs, CI cache, S3, GCS, Azure; journal writer
│   ├── sc-client/                  # control-plane client: outcome lookup, map fetch (opportunistic)
│   ├── sc-cli/                     # `skipcert` binary
│   └── sc-mcp/                     # MCP server (stdio) over the same core
├── verifier/                       # SEPARATE crate + Python wheel; depends only on sc-model + sc-hash
├── probes/
│   ├── python/                     # pytest plugin + PyO3 extension (skipcert_probe)
│   └── node/                       # Jest/Vitest plugin + N-API addon
├── server/                         # control plane (Rust/axum): api, rollup workers, audit, scheduler
├── integrations/
│   ├── github-action/  github-app/  buildkite-plugin/
│   └── recipes/                    # codex setup, cursor environment.json, devin blueprint, copilot setup-steps
├── conformance/                    # golden corpus, replay harness, mutation runner, differential tests
└── docs/adr/                       # architecture decision records
```

**Dependency rule:** `verifier` must never depend on `sc-ladder`, `sc-policy` or `sc-graph` logic. It re-implements the decision check from the specification, so the trusted base is about 1–2 KLOC.

---

## 5. Data formats (all versioned, all content-addressed)

### 5.1 Footprint graph (`.scmap`, format v1)

This is a single mmap-friendly file per (repo, base commit, probe set).

```
Header        magic "SCMAP", format_version, engine_version, repo_id, base_commit, created_at, probe_versions[]
StringTable   deduplicated paths, symbols, package names, test IDs (offset-indexed)
NodeTable     node_id → {kind, string_ref, content_hash, parent}
                kind ∈ {File, Function, Module, ExtPackage, ExtFile, EnvVar, NetEndpoint, Resource(typed), Test}
                ExtPackage = (ecosystem, name, version, dist_hash from wheel RECORD / npm integrity)
TestTable     test_id → {stable_id, node_id, scope (root|session|module|test), cost_ms_p50, flags}
                flags: nondeterministic, mandatory, network, clock, random, subprocess, c_ext, dynamic_import
Forward CSR   test → [node_ids]                    (what this test observed)
Inverse CSR   node → [test_ids]                    (who observed this entity)   ← bidirectional from day 1
RootScope     node_ids observed outside any test (conftest, plugins, startup) → global deps
Footer        merkle_root, section hashes
```

- **Stable test IDs.** `blake3(project_root_rel_path :: qualified_name :: parametrization_canonical)`. They are project-scoped, so monorepo adapters later don't need a migration.
- **Incremental builds.** A new map is the old map plus a patch of re-traced tests. It's materialized on merge to main.
- **Size target.** ≤ 50 MB per 100k tests compressed (zstd), with load (mmap) ≤ 50 ms.

### 5.2 Outcome key (typed envelope, v1)

```
OutcomeKey = blake3( canonical_cbor({
   v: 1,
   base: { test_stable_id, test_code_hash, footprint_merkle, env_subset_hash,
           interpreter_hash, resolved_deps_hash },
   ext:  [ {type: "db.migrations",   hash}, {type: "container.digest", digest},
           {type: "contract.version", provider, version},
           {type: "llm.params", model, prompt_hash, judge, rubric_hash, temperature, seed},
           {type: "data.snapshot", id}, ... ]   // registry of known types; unknown type ⇒ not reusable
}))
```

- A test type that declares a required extension it cannot supply is **not eligible** for L1, so it falls back to L0 or L3.
- Nondeterministic types (LLM evals) produce `recorded-sample` outcomes. They can be reused only if the policy allows it, and they're never certified as `dedup`.

### 5.3 Certificate (v1, JSON Lines, hash-chained)

```json
{
  "v": 1, "kind": "skip",
  "type": "disjoint | dedup | witness",
  "test": "<stable_id>", "test_name": "tests/x.py::test_y[param]",
  "inputs": { "repo": "...", "base": "<sha>", "head": "<sha>", "tree": "<worktree hash>",
              "map": "<scmap merkle>", "policy": "<policy hash>", "engine": "0.4.1",
              "lock_delta": "<hash|null>" },
  "evidence": { "changed_nodes": ["..."], "intersection": [], "outcome_ref": null,
                "rules_passed": ["R001","R007","R012"] },
  "trace": { "change_request": null, "requirements": [], "approver": null },
  "prev": "<hash of previous cert in this run>", "self": "<hash>",
  "sig": { "alg": "ed25519|kms", "key_id": "...", "value": "...", "tsa": null }
}
```

A separate `kind: "evidence"` record holds reachability and impact context. The verifier rejects it if it's used as a skip. The `inputs` block is the **pinned-inputs manifest**, and it's enough to re-derive the decision.

### 5.4 Journal events (append-only, idempotent)

```
{event_id (ulid), ts, run_id, repo, namespace (trusted|untrusted:<fork>), kind, payload}
kind ∈ RunStarted, MapUsed, TestSelected, TestSkipped(cert_ref), TestRan(result, duration, outcome_key),
       FlakeObserved, IntegrityViolation, EscapeDetected, RunFinished
```

Rollups are pure functions over the journal, so any rollup can be rebuilt from scratch.

---

## 6. Key runtime flows

### 6.1 Record (post-merge on main, plus sampled PR runs)

`pytest --skipcert=record` activates the probe: `sys.monitoring`, audit hooks and package attribution. Per-test events are normalized into graph deltas, and the map patch is uploaded to the customer CAS. Outcomes go to the journal. The trusted CI identity signs the map's merkle root.

### 6.2 PR selection (authoritative)

```
1. fetch map for merge-base (CAS / CI cache); verify signature; if missing → full run (fail-open)
2. resolve change: git diff merge-base..head (+ dirty tree) → changed nodes
     · source files → functions via tree-sitter AST hash (format-only edits = no change)
     · lockfile delta → ExtPackage nodes changed (resolver per ecosystem)
3. policy pass: global triggers (CI config, conftest/root scope, policy file, interpreter, unknown file kind,
   C-ext package change, map staleness > k) ⇒ widen; integrity rules on test files ⇒ force
4. ladder per test: L0 disjoint? → L1 outcome key hit (trusted namespace only)? → L2 (gated) → L3 run
5. mint certificates (hash chain, sign with CI OIDC-bound key) → upload; emit journal events
6. run selected tests (ordering by fail-fast prioritiser); record outcomes; PR check + summary
```

### 6.3 Agent inner loop (advisory, offline)

1. **Setup phase**, which has network: `skipcert fetch-map --base <sha>` bakes the map into the sandbox snapshot.
2. **Agent phase**, which has no network: the MCP tool `tests_for(diff)` calls the in-process ladder, using L0 plus the local outcome cache only. The response is compact: test IDs, counts, and flaky annotations.
3. **No signing in the sandbox.** Outputs are marked `advisory: true`, and PR CI recomputes everything authoritatively.

### 6.4 Merge queue / cross-branch dedup

- The scheduler groups queued PRs by footprint disjointness, so disjoint PRs batch together.
- For each batch, a test whose outcome key already exists from the trusted namespace is reused (L1).
- On a batch failure, the failed batch is bisected by footprint overlap first.

### 6.5 Dependency bump

The lockfile resolver maps a bump to `ExtPackage(old → new)`, then to the files that changed inside the package (diffed by dist RECORD). The inverse CSR then gives the tests that observed those files.

Packages with C extensions, or with no RECORD available, are fail-closed: every test that touched the package runs. No global full run is needed.

### 6.6 Audit and escape

1. An async full run happens per merge to main, or is sampled at a configurable rate.
2. The miss auditor joins full-run failures against skip certificates in the journal.
3. Any hit opens an `EscapeDetected` incident with an auto-generated minimal repro: the cert, the diff and the footprint.
4. An engineer adds a policy rule, which bumps the policy hash.
5. The escape log is published (customer-visible and aggregated publicly with consent).

---

## 7. Soundness specification

### Invariants (tested in conformance, §11)

- **I1:** skip(T) ⇒ ∃ valid certificate whose verifier check passes on its pinned inputs.
- **I2:** for a disjoint skip, changed_nodes ∩ forward(T) = ∅ ∧ changed_nodes ∩ RootScope = ∅.
- **I3:** missing, stale or unverifiable data ⇒ superset selection (monotonic widening).
- **I4:** a nondeterministic-flagged test is never skipped via L1, and is skipped via L0 only if the policy permits.
- **I5:** a changed test_code_hash ⇒ the test runs, with no exceptions.
- **I6:** certificates exist only from the trusted namespace. Advisory outputs can never enter the outcome index.

### Fail-closed rule catalog (v1, implemented in `sc-policy`, which is authoritative)

| Rule | Scope | Triggers |
|---|---|---|
| R001 | global | CI configuration changed |
| R002 | global | Test-runner configuration changed (conftest, pytest.ini, pyproject, tox, setup.cfg, jest/vitest config) |
| R003 | global | Runtime version pin changed (.python-version, .nvmrc, rust-toolchain, ...; non-Python pins also in environment mode) |
| R004 | global | Skipcert policy changed |
| R005 | global | Build-affecting input changed (setup.py, build.rs, C/Cython sources, native binaries, .pth, sitecustomize) |
| R006 | global | Lockfile unparseable, requirement options (index, includes, editables) changed, or a dependency manifest of an ecosystem without a delta parser changed (package.json, go.sum, Cargo.lock, Gradle, ...; ADR 013) |
| R007 | global | Map staleness unknown or above `max_staleness` |
| R008 | test | Dynamic import / eval with non-constant input observed |
| R009 | test | Network contacted without a declared stub |
| R010 | test (L1 only) | Nondeterminism, clock or randomness: blocks outcome reuse, not L0 skipping |
| R011 | report | Test integrity: test removed, test file deleted, skip/xfail markers added |
| R012 | test | No complete footprint (new or untraced test) |
| R013 | test | Subprocess spawned (child process not traced) |
| R014 | test | Never-skip (policy glob or marker) |
| R015 | global | Path in the policy's `run_all_paths` |
| R016 | test | Native code executed and `native_code = "run"` |
| R017 | global (ladder) | A changed node was observed in the root scope (conftest, plugins, startup) |

Other changed files map to graph nodes, and the inverse index decides who runs:
- data files map to file nodes;
- module-level Python changes map to file nodes;
- body-only Python changes map to function nodes, falling back to the file node when the map has no function-level observations for that file;
- lockfile package changes map to `pypi:<name>` package nodes.

A file no test read can't affect any test, so it needs no rule of its own.

### Determinism gate

- **A/A reruns** on record runs, sampled by test cost.
- **Flake statistics** from the journal.
- **DeFlaker-style check:** if a failure did not touch changed nodes, it's marked "likely flaky".
- **Quarantine:** quarantined tests are never cached. Agents receive the flake rate instead.

---

## 8. Trust architecture

| Zone | Identity | Can do | Cannot do |
|---|---|---|---|
| Agent sandbox / local dev | none | Read maps, advisory selection | Mint certs, write the outcome index |
| Untrusted CI (forks, unapproved agent PRs) | CI OIDC, untrusted claim | Run, write to `untrusted:<fork>` namespace | Read-through to trusted outcomes is allowed; write-through is not |
| Trusted CI (protected branches, approved workflows) | CI OIDC (GitHub `id-token`) → short-lived signing key or KMS | Mint certs, write trusted outcomes, publish maps | Change the policy without a full run |
| Control plane | service identity | Index hashes, rollups, audit scheduling | See source code |

**Signing backends:**
- Ed25519 per-repo key (MVP).
- Sigstore keyless, bound to the CI OIDC identity (preferred default once stable).
- KMS or HSM with optional RFC 3161 timestamps (regulated tier).

**Retention classes:**
- `working`: 30 days (maps, caches).
- `evidence`: configurable, from 1 to 10+ years (certificates, policy and engine versions, pinned manifests).

---

## 9. Performance and capacity budgets

| Operation | Budget |
|---|---|
| Map load (mmap) | ≤ 50 ms / 100k tests |
| Change resolution incl. dirty tree + AST diff | ≤ 300 ms typical PR |
| Ladder over 100k tests | ≤ 200 ms (bitset ops on inverse CSR) |
| Certificate minting + signing | ≤ 200 ms batch |
| MCP `tests_for` round trip (offline) | ≤ 1 s, p95 |
| Record-run probe overhead | ≤ 15% (sys.monitoring), sampled A/A extra |
| Control plane | Horizontally sharded by repo; ingest 10k events/s/shard; lag alert at 30 s; lag ⇒ clients fail-open to L0+L3 |

---

## 10. Reliability: failure behaviours

| Failure | Behaviour |
|---|---|
| Control plane down | L1 is disabled, L0 + L3 continue, and no skip is lost to the outage |
| Map missing or corrupt / signature invalid | Full run, an alert, and a map rebuild is scheduled |
| Probe crash mid-run | That test is marked `untraced`: it always runs next time, and the run itself is unaffected |
| Journal write failure | Buffered locally and retried idempotently (by ULID). Certificates are still emitted to CAS. |
| Rollup lag | Treated as stale, so selection widens |
| Verifier rejects a certificate | The test is forced to run and an incident is opened |

---

## 11. Verification strategy ("testing the tester")

1. **Conformance corpus.** 20+ pinned OSS repos (Python and TypeScript) with full-run ground truth for each commit. The replay harness computes skip rate, escapes, latency and map size. This is the release gate: **0 escapes**.
2. **Mutation differential.** Inject mutants (mutmut, Stryker) on changed lines. Every mutant killed by run-all must be killed by the Skipcert selection.
3. **Property-based tests** on `sc-ladder`: monotonic widening (I3), certificate validity (I1), and determinism of decisions (same inputs give byte-identical certificates).
4. **Verifier hardening.** Fuzzing (cargo-fuzz) on certificate parsing, plus an independent re-implementation check from the specification in Python.
5. **Probe accuracy suite.** Synthetic tests exercising every effect class: file, env, network, clock, subprocess, dynamic import, C extension, fixture scopes and monkeypatch. Each one asserts the expected footprint.
6. **Stress (E6).** 25x PR-rate replay with injected lag. Assert widening and bounded latency.
7. **Dogfooding.** Skipcert's own CI uses Skipcert from M2.

---

## 12. Interfaces

| Interface | Operations |
|---|---|
| **CLI** | `record`, `select`, `run`, `verify`, `explain <test>`, `impact --diff/--lock-delta`, `fetch-map`, `replay-history`, `doctor` |
| **pytest plugin** | `--skipcert=record\|select\|advisory`, markers `@skipcert.never_skip`, `@skipcert.declares(network="stub")` |
| **MCP** (stdio, offline) | `tests_for(diff?)`, `why(test)`, `impact(paths\|packages)`, `flaky(test)` |
| **REST** (control plane) | `/v1/outcomes:batchGet`, `/v1/maps/{repo}/{base}`, `/v1/impact`, `/v1/escapes`, `/v1/runs` |
| **Batch API** | `/v1/outcomes:batchGet` takes N keys and is idempotent (designed now, used later by RL and mutation) |
| **Impact API** | Returns tests plus reason codes plus confidence. `tests_for` is a projection of it. |

---

## 13. Extension points (keeping Next and Later unblocked)

- **Probe SDK:** a capture protocol (Cap'n Proto or FlatBuffers events) that any language probe emits, so Ruby, Go, Java and syscall shims can be added without core changes.
- **Key extension registry:** typed extensions are registered with a resolver, for example `db.migrations` hashing a migrations directory plus the schema dump.
- **Change resolvers:** per ecosystem (pip/uv/poetry, npm/pnpm/yarn), plus later cargo, maven, etc.
- **Policy rules:** declarative YAML compiled to a rule set, versioned and hashed.
- **Evidence exporters:** SOX/PCI/DORA change-to-evidence reports, a requirement-traceability join, and later a tool-qualification kit built from the conformance suite.
- **Adapters:** Nx, Pants and Turborepo map package-level graphs onto stable test IDs.

---

## 14. Observability

- **Metrics:** skip rate by certificate type, minutes saved, fail-closed rate by rule, escapes, map staleness, probe overhead, rollup lag, flake rates.
- **Tracing:** OpenTelemetry spans for CLI and server, with run_id propagation.
- **Per-run artifact:** `skipcert-report.json`, the human and agent summary that drives the PR check.

---

## 15. Build plan (engineering milestones)

| Milestone | Deliverables | Exit gate |
|---|---|---|
| **M0 Foundations** (2 wks) | Workspace, ADRs, `sc-model` v1 schemas, `sc-hash`, CI with conformance skeleton, coding standards | Schemas reviewed and frozen at v1 |
| **M1 Core + Python** (to 31 Jan) | Python probe, `sc-graph` (bidirectional), `sc-change` (git + AST + uv/pip lock resolver), `sc-policy` R001–R012, `sc-ladder` L0, `sc-cert` (Ed25519), `verifier`, CLI, `replay-history`, conformance corpus of 5 repos | **E1:** 0 escapes, ≥2x less test time than the target-graph baseline |
| **M2 CI integration** (Feb) | GitHub Action, CAS backends (CI cache, S3), journal, local audit, determinism gate, flaky signalling, dogfooding | **E6:** stress passes |
| **M3 Outcome index** (Feb–Mar) | Control plane (axum + Postgres), rollups, trusted/untrusted namespaces, L1, typed key envelope, OIDC signing | **E4:** duplicate rate measured; L1 has 0 escapes |
| **M4 Design partners** (Mar) | GitHub App, dashboard v0, escape log, shadow mode | **E2:** 0 escapes over 2–4 weeks with partners |
| **M5 Agents + TS** (Apr) | Node probe (Jest/Vitest), npm/pnpm resolver, MCP server (offline), `fetch-map`, platform recipes, merge-queue scheduler v0 | Inner-loop p95 ≤ 1 s; TS conformance at 0 escapes |

---

## 16. Architecture decision records (initial)

| ADR | Decision | Reason |
|---|---|---|
| 001 | Rust core, thin language probes | Speed, one binary, memory safety, embeddable in Python and Node |
| 002 | Footprint graph is bidirectional, function-level, with ExtPackage nodes, from v1 | Serves dependency bumps, impact, mutation and adapters without a schema rewrite |
| 003 | Outcome key is a typed, versioned envelope; unknown extension means not reusable | Sound L1 across future test types |
| 004 | Certificates only from trusted CI identities; sandbox is advisory | The agent controls its sandbox, so this is the threat model |
| 005 | Journal + stateless rollups; staleness widens selection | The Anthropic failure mode |
| 006 | Independent verifier with a minimal trusted base | The product's claim must be checkable without trusting the engine |
| 007 | ML only orders tests and never gates them | Probabilistic misses contradict the product promise |
| 008 | Separate retention classes for working vs evidence data | Regulated use without bloating working storage |
| 009 | L2 witnesses are excluded from the MVP and gated by E3 | Complexity is justified only by measured headroom |
| 010 | Offline-first selection and opportunistic control-plane calls | Agent sandboxes and air-gapped sites |
