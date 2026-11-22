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

The linked per-ADR files will be written out as each decision is exercised in
code. Until then, this table and `architecture.md` are authoritative.
