# Architecture decision records

Each ADR records one decision that is expensive to reverse. To change an
accepted ADR, add a new one that supersedes it. Full context is in
[`../architecture.md`](../architecture.md).

| ADR | Status | Decision | Why |
|---|---|---|---|
| [001](0001-rust-core.md) | Accepted | Rust core, thin language probes | Speed, one static binary, memory safety, embeddable in Python and Node |
| [002](0002-bidirectional-graph.md) | Accepted | The footprint graph is bidirectional, function-level, with external-package nodes, from v1 | Dependency bumps, impact API, mutation testing and monorepo adapters without rewriting stored maps |
| [003](0003-typed-outcome-key.md) | Accepted | The outcome key is a typed, versioned envelope; an unknown extension means not reusable | Sound L1 dedup across future test types (LLM evals, DB, containers, contracts) |
| [004](0004-trust-zones.md) | Accepted | Only trusted CI identities mint certificates; sandbox and local output is advisory | Agents control their own sandboxes; a malicious PR must not be able to forge a skip |
| [005](0005-journal.md) | Accepted | Append-only journal and stateless rollups; staleness widens selection | The failure mode of a stateful single-writer selector at 25x load |
| [006](0006-independent-verifier.md) | Accepted | Independent verifier with a minimal trusted base | The product's claim must be checkable without trusting the engine |
| [007](0007-ml-orders-only.md) | Accepted | ML only orders tests and never gates them | Probabilistic misses contradict the product promise |
| [008](0008-retention-classes.md) | Accepted | Separate retention for working and evidence data | Regulated use without bloating working storage |
| [009](0009-l2-gated.md) | Accepted | L2 value witnesses excluded from the MVP and gated by experiment E3 | Complexity is justified only by measured headroom |
| [010](0010-offline-first.md) | Accepted | Offline-first selection; control-plane calls are opportunistic | Agent sandboxes without network, and air-gapped sites |
| [011](0011-def-header-splitting.md) | Accepted | Split `def` headers by what the enclosing scope evaluates; gated on the probe's `py.introspection` capability | Method-header edits made whole classes root-scope changes (click, E1 run 3) |
| [012](0012-environment-fingerprint.md) | Accepted | Environment fingerprinting replaces path-based environment rules (R001, R003, most of R002, lockfiles); new R018 | Path rules were both imprecise (most CI/tox edits change nothing) and incomplete (runner images, env vars) |
| [013](0013-language-adapters.md) | Accepted | Any language: probes report executed lines, Quod maps them to entities with the same tree-sitter extractor that diffs changes; JS/TS, Go, Rust, Java specs; static languages need the build closure | One bespoke probe per language doesn't scale, and probe/diff naming must never disagree |
| [014](0014-module-state-widening.md) | Accepted | A test that touches a file with module- or class-level mutable state observes every entity of that file | date-fns: a memoised result was read by 90 tests that never ran the function computing it |
| [015](0015-verdict.md) | Accepted | The Verdict (in-toto Statement in a DSSE envelope, pinned changed set) is the product primitive; lenses are pure functions | Users buy trust ("can this merge, and why believe it?"), not selection alone |
| [016](0016-flake-proof.md) | Accepted | A failure is cleared (F001) only when its footprint is complete and untouched, no global rule fired, and its file didn't change | A wrong "not your fault" merges a regression |
| [017](0017-advisory-verdicts.md) | Accepted | Agents get advisory Verdicts over MCP; only trusted CI seals proof; unmet obligations block | Agents control their sandboxes and are documented gaming tests |
| [018](0018-culprit.md) | Accepted | `quod culprit` names the commit(s) that changed what a failing test executes; one means a revert-sufficient culprit, none means not code; drifts and global rules withhold claims | Red main is the top CI pain; footprints answer it without bisecting |
| [019](0019-import-closures.md) | Accepted | A file change obligates every test that ran its functions; the Python probe gives cached imports the imported module's closure | A module body runs once, in the first importer's scope: a changed constant skipped a test that read it |
| [020](0020-additive-definitions.md) | Proposed | A new undecorated top-level `def` under an unused name changes only the module's namespace, observed by failed lookups and enumeration (probe `py.namespace`) | E8b attrs: adding a function made every test re-run |
| [021](0021-environment-enumeration.md) | Accepted | Bulk reads of the environment are one `env:*` observation, and `env:*` leaves out per-run CI bookkeeping variables (reading one by name is still recorded) | E9 networkx: a prefix scan of `os.environ` at import made every CI run look changed |
| [022](0022-function-granular-python-maps.md) | Accepted | Python maps declare `py.functions`: a changed function no recorded test executed affects nothing, even in a file with no function nodes (no file fallback) | E9 networkx: body edits in untested functions became root-scope changes because `conftest.py` imports every module |
| [023](0023-transparent-import-tracking.md) | Accepted | The probe replaces nothing the program can see: imports are tracked through `_find_and_load`, bytecode and monitoring events; remaining replacements keep the stdlib's names | E9 pylint: dill pickling and astroid's live builtins broke while recording |
| [024](0024-incremental-maps.md) | Accepted | `quod record --incremental`: record only the tests the plan runs and keep certified-skipped footprints; the Action does it by default on pytest, with a full recording every 25 runs | Recording the whole suite on every merge was Quod's own biggest cost |

The linked per-ADR files will be written out as each decision is exercised in
code. Until then, this table and `architecture.md` are authoritative.
