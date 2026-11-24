# Quod architecture v5: the Verdict platform

*2026-11-24. Builds on [architecture.md](architecture.md) (v4). v4 remains authoritative for the engine internals: the footprint graph format, certificates, the policy catalog, the trust zones and the performance budgets. v5 changes what the product is built around.*

*Inputs:*
- *`reports/CI and test selection pain points.md`;*
- *`reports/Quod number one architecture.md`;*
- *`reports/Quod defensible agent moats.md`;*
- *E1 benchmark results (0 dependency escapes on 11 repos).*

---

## 0. The shift in one paragraph

v4 is built around **selection**: which tests can be skipped. Research shows that selection on its own is the easiest part to copy and the weakest pain to sell; CI cost ranked 9th of 10. The pains users actually feel are about trust and recovery:

- main is red and slow to recover;
- CI can't keep up with agent volume;
- green checks aren't trusted;
- flaky tests;
- tools that skip tests silently;
- agents weakening tests.

They all reduce to one question: **"Can this change merge, and why should I believe it?"**

v5 makes the answer to that question, the **Verdict**, the product primitive:

- **One evidence graph.** Every trusted run feeds it.
- **Many analyses** ("lenses") read it.
- **One signed Verdict per change** combines what the lenses find.
- **Every surface** shows the same Verdict: the PR check, the merge queue, the agent tool, the dashboard and the audit ledger.

---

## 1. Design principles

| # | Principle | Consequence |
|---|---|---|
| P1 | **Proof over prediction** | Every claim in a Verdict is backed by evidence the open verifier can re-check. ML and LLMs may *order*, *prefetch* and *explain*, but never *decide* (extends v4 ADR 007). |
| P2 | **Fail closed, visibly** | Uncertainty widens to "run". The Verdict states *why* it widened, so a fallback is explained rather than looking like a broken skip. |
| P3 | **One graph, many lenses** | Each new capability is an analysis over the same evidence graph and journal, never a new data silo. This is what makes the product hard to copy piecemeal. |
| P4 | **Overlay, never replace** | Quod runs inside existing CI jobs and agent sandboxes. It owns no runners and does not ask teams to migrate (the Earthly lesson). |
| P5 | **The agent is an untrusted author** | Agents get fast advisory answers. Only trusted CI mints certificates. Tests and policy sit outside the agent's write authority. |
| P6 | **Offline-first, source stays home** | The critical path needs no network. The control plane sees hashes and metrics only (v4 driver 7). |
| P7 | **Earn trust continuously** | Shadow mode first, an audited enforce mode, and a published escape record. Trust is a metric on the dashboard, not a sales claim. |

---

## 2. Personas and pain coverage

| Persona | Top pains | What they need from Quod |
|---|---|---|
| **Developer** (PR author) | Waiting on CI; flaky failures blamed on them; opaque skips | A fast Verdict, "this failure isn't yours", and `why(test)` |
| **Coding agent** (Claude Code, Cursor, Codex, Devin, Copilot) | Running too many or the wrong tests; no trustworthy signal | Sub-second `tests_for`, an incremental check, and a clear list of obligations |
| **Reviewer** | Review overload; distrust of green checks on agent PRs | One card answering what was proven, what's untested and whether tests were tampered with |
| **Platform / DevEx lead** (buyer) | Red main, agent volume, flaky tests, CI cost | Safe enforcement, culprit finding, flaky-test control and trust metrics |
| **On-call / build cop** | Main broken, 72-minute recovery | The culprit commit in seconds and a ready revert |
| **Dependency bot owner** (Renovate/Dependabot) | Every version bump runs everything | Impact per bump and a policy for auto-merging |
| **Auditor / compliance** | Evidence of what was tested, per change | An exportable, independently verifiable ledger |

Pain-to-flow coverage (detail in §3). Numbers refer to the ranked pains in the research report.

| Pain (rank) | Flows |
|---|---|
| 1. Red main, slow recovery | F5 culprit, F7 merge queue |
| 2. Agent volume | F2 inner loop, F3 agent fleet, F7 reuse |
| 3. Distrust of green checks | F1/F4 Verdict card, untested-change lens |
| 4. Flaky tests | F6 flake proof |
| 5. Silent false negatives | Fail-closed policy, F9 trust ramp, F11 escape audit |
| 6. Agents weakening tests | Integrity lens in F1/F3/F4 |
| 7. Full suite still required at merge | F9 shadow → enforce, F7 certified merge |
| 8. Opaque selection | `why` in every surface |
| 9. CI cost | Savings metrics as a by-product (F9) |
| 10. Audit evidence | F10 ledger |

---

## 3. User flows

Each flow lists its **trigger**, **steps**, the **lenses** it uses (§4.3) and its **latency budget**.

### F1. Developer opens a PR

**Trigger:** a push to a PR branch.

1. The GitHub Action (or Buildkite plugin) runs `quod verdict --stage plan`. It:
   - fetches the signed map for the merge-base;
   - resolves the change: functions, packages, environment, config;
   - runs the **Select**, **Integrity** and **Untested-change** lenses.
2. The CI job runs only the *obligated* tests, the ones the change requires.
3. `quod verdict --stage seal` collects the results. It applies the **Flake-proof** lens to any failures, then mints the signed Verdict.
4. A PR check posts the **Verdict card** (§6.1).

**Lenses:** Select, Integrity, Untested-change, Flake-proof.
**Budget:** planning ≤ 2 s; sealing ≤ 1 s, plus the tests themselves.

### F2. Agent inner loop (advisory, offline)

**Trigger:** an agent edits code in its sandbox.

1. At sandbox setup, `quod fetch-map` bakes the map into the snapshot. Sandbox snapshots and forks keep it, so every forked candidate starts warm.
2. After each edit, the agent calls MCP `tests_for()`. The in-process engine diffs the dirty tree and returns:
   - the obligated tests, ordered by likelihood of failure;
   - known-flaky annotations;
   - untested changed functions ("you changed `parse()`; no test executes it");
   - integrity warnings ("you modified a test that guards your change").
3. The agent runs those tests and calls `check()`. That returns an **advisory Verdict**, unsigned and marked `advisory: true`.
4. When the agent opens a PR, flow F1 recomputes everything authoritatively. The advisory result never counts as proof.

**Lenses:** Select, Untested-change, Integrity, Flake-proof (history only).
**Budget:** `tests_for` ≤ 1 s at p95, with no network.

### F3. Agent fleet / best-of-N candidates

**Trigger:** an orchestrator spawns N agents or N candidate patches for one task.

1. Every candidate forks from the same snapshot, so they all share one map.
2. Each candidate's tests are keyed by **outcome key** (v4 §5.2): the hash of everything the test observed.
   - Two candidates that leave a test's inputs identical produce the same key, so the test runs once. This is the **Reuse** lens.
   - Inside a sandbox, reuse comes from a local outcome cache and stays advisory. In trusted CI it is certified as L1 dedup.
3. The orchestrator ranks candidates by Verdict:
   - first, obligations met and no integrity findings;
   - then, the fewest untested changes.

**Lenses:** Select, Reuse, Integrity, Untested-change.
**Value:** verification cost grows with *distinct* footprint states, not with N.

### F4. Reviewer reads the Verdict

**Trigger:** a reviewer opens the PR.

1. The Verdict card shows four things:
   - **proven** (ran or certified-skipped), with counts;
   - **untested changes**: changed functions that no test executes;
   - **integrity**: deleted tests, new skip markers, weakened assertions, edited fixtures or `conftest.py`, edits to the very tests that guard changed code;
   - **flake notes**.
2. Drill-downs: `why(test)` gives the reason a test ran or was skipped, as a one-line rule plus evidence. "Verify" re-checks every signature in the browser, using the verifier compiled to WASM.
3. A policy can turn findings into required reviews. For example: "an agent PR that edits tests guarding its own change needs human approval".

**Lenses:** read-only over the sealed Verdict.

### F5. Main breaks: culprit finding

**Trigger:** a test fails on main, or in a merge-queue batch.

1. The **Culprit** lens takes the failing tests' forward footprints and intersects them with each candidate commit's changed nodes. The candidates are the commits since the last green run.
2. The outcome depends on how many commits intersect:
   - **exactly one:** name it as the culprit, with the shared nodes as evidence. Optionally open a revert PR and notify its author;
   - **several:** rank them, and bisect only over the commits that intersect;
   - **none:** the failure is not caused by code. Hand it to Flake-proof and the environment delta (F6).
3. Commits whose footprint doesn't intersect are **proven innocent**, so they keep merging.

**Lenses:** Culprit, Flake-proof.
**Budget:** a named culprit in ≤ 10 s from the failure event, against a typical 72-minute recovery today.

### F6. A failure that may be flaky

**Trigger:** a test fails on a PR or on main.

1. The **Flake-proof** lens looks at whether anything in the test's footprint changed (code, packages or environment):
   - **nothing changed:** the failure is *provably not caused by this change*. Label it "not yours: flaky or infrastructure", auto-retry it, and attach the history;
   - **only the environment or external inputs changed:** label it "environment", and name the changed variable or package;
   - **something in the footprint changed:** treat it as a real failure. Never relabel it.
2. Repeated proven-flaky failures **quarantine** the test:
   - it still runs and still reports;
   - it no longer blocks merges, under the customer's policy;
   - it is never eligible for outcome reuse (v4 I4);
   - an owner is notified with the evidence.

**Lenses:** Flake-proof, plus the journal's flake statistics.

### F7. Merge queue: merge as proof

**Trigger:** PRs enter the merge queue.

1. The scheduler batches PRs whose combined footprint impact is **disjoint**: no test is obligated by two PRs in the same batch.
2. Each batch's Verdict is computed on the **exact merged tree**. Tests already proven on identical outcome keys are reused (L1).
3. If a batch fails, the Culprit lens splits it by footprint overlap rather than by blind bisection.
4. Main only ever advances to trees that carry a valid Verdict. **Every commit on main is a sealed proof.**

**Lenses:** Select, Reuse, Culprit.
**Integrations:** the GitHub merge queue `merge_group` event first, then Graphite, Aviator and Mergify.

### F8. Dependency bump

**Trigger:** a Renovate or Dependabot PR.

1. The lockfile resolver turns the bump into package nodes, and then into the files that changed inside the package. The **Impact** lens returns every test that executes code from that package.
2. The Verdict reports how many tests exercise the package, and lists the untested usages: APIs of the package that the repo calls but no test reaches.
3. A policy can auto-merge a patch-level bump when the obligated tests are green and nothing is untested.

**Lenses:** Impact, Select, Untested-change.

### F9. Platform lead: adoption and the trust ramp

**Trigger:** the app is installed on a repo.

1. **Shadow week.** The full suite still runs. Every Verdict shows what it *would* have skipped, and the nightly audit compares that with real results. The dashboard tracks:
   - would-have-saved minutes;
   - shadow escapes (target 0);
   - flaky tests proven;
   - culprits found;
   - integrity findings.
2. **The enforce gate is earned.** Enforcement is offered only after N shadow runs with 0 escapes. The lead turns it on per repo or per path.
3. **Continuous audit.** A sampled full run keeps checking the certificates. The **trust score** is the escape-free streak, the share of skips certified and the fallback rate by rule.

**Lenses:** all of them, plus Audit.

### F10. Auditor: evidence

**Trigger:** an audit period or a sampled change.

1. `quod ledger export --from --to` produces every Verdict for the period: an in-toto bundle per change, plus the pinned inputs and policy versions.
2. The auditor runs the **open verifier** offline. Every signature, every skip certificate and every policy hash is checked, without trusting Quod's service.

### F11. Escape incident

**Trigger:** the audit finds a test that was skipped but fails in a full run.

1. Open an incident. It carries a minimal reproduction: the certificate, the diff, the footprint and the rule set.
2. Immediately widen the affected scope. For example, turn the file or package into a forced-run path.
3. An engineer fixes the root cause (as ADR 014 did for date-fns memoisation) and adds a conformance case. The policy hash bumps.
4. Publish the escape in the public escape log.

---

## 4. System architecture

### 4.1 Layered view

```
┌──────────────────────────────── SURFACES ─────────────────────────────────────┐
│ PR check / Verdict card · merge-queue gate · MCP + CLI (agents) · dashboard   │
│ · ledger export · REST / webhooks (Slack, PagerDuty, orchestrators)           │
└──────────────────────────────────────┬────────────────────────────────────────┘
                                       │ one Verdict schema
┌──────────────────────────── VERDICT COMPILER ─────────────────────────────────┐
│ obligations (plan) → results (run) → findings (lenses) → seal (sign, chain)   │
└──────────────────────────────────────┬────────────────────────────────────────┘
                                       │
┌──────────────────────────────── LENSES (pure functions) ──────────────────────┐
│ Select · Reuse · Integrity · Untested-change · Flake-proof · Culprit · Impact │
│ · Audit                      (inputs: graph + change + journal → findings)    │
└──────────────────────────────────────┬────────────────────────────────────────┘
                                       │
┌──────────────────────────── EVIDENCE GRAPH ───────────────────────────────────┐
│ footprint graph (bidirectional, versioned per commit, structural sharing)     │
│ change resolver (AST entities, lockfile deltas, env/config deltas)            │
│ outcome index (typed keys) · test registry (stable IDs, flake stats, owners)  │
└──────────────────────────────────────┬────────────────────────────────────────┘
                                       │
┌──────────────────────────────── CAPTURE ──────────────────────────────────────┐
│ language probes (py, js/ts → jest, go, jvm) · job probes (lint, typecheck,    │
│ build: file-access tracing) · environment fingerprint · determinism observer  │
└───────────────────────────────────────────────────────────────────────────────┘
  TRUST (cross-cutting): CI OIDC identities · namespaces (trusted/untrusted/advisory)
  · signing (Ed25519 → Sigstore → KMS) · open verifier (native + WASM)
  · journal (append-only)
```

### 4.2 Where each part runs

| Component | Runs in | Why |
|---|---|---|
| Probes, change resolver, lenses, Verdict compiler | **Customer CI / sandbox** (one static binary plus probe packages) | Offline-first; source never leaves |
| Footprint maps, certificates, Verdicts | **Customer storage** (CI cache, S3, GCS), content-addressed | Data residency; no lock-in |
| Outcome index, journal rollups, flake stats, dashboard, audit scheduler | **Control plane** (hosted or self-hosted), which sees hashes only | Cross-branch reuse, history, trust metrics |
| Verifier | **Anywhere**: CLI, a WASM build in the browser, the auditor's laptop | Independent by construction |

### 4.3 Lenses: contracts

Every lens is a **pure function**: `(graph, change, journal_view, policy) → findings[]`. Each finding carries `{kind, subject, evidence, rule, severity}`. Because lenses are deterministic and their inputs are pinned, the verifier can re-derive any finding that a Verdict relies on.

| Lens | Question | Evidence it cites | Status |
|---|---|---|---|
| **Select** | Which tests are obligated, and which are certified-skippable? | changed nodes ∩ forward footprints; rules | Built (v4 L0) |
| **Reuse** | Has this exact test-input state already been proven? | outcome key, the trusted-namespace record | Designed (v4 L1) |
| **Integrity** | Did the change weaken the tests that guard it? | test-file diffs, removed IDs, markers, assertion counts, fixture/config edits, guard overlap | Partly built (R011) |
| **Untested-change** | Which changed entities does no test execute? | changed nodes with an empty inverse set | **New**, cheap (inverse CSR) |
| **Flake-proof** | Can this failure be caused by this change? | failing test's footprint ∩ changed nodes = ∅; environment delta | **New** (extends the v4 DeFlaker check) |
| **Culprit** | Which commit broke this test? | footprint ∩ each candidate commit's changes | **New** |
| **Impact** | What does this package or path reach? | inverse footprint of package/file nodes | Partly built |
| **Audit** | Did any certified skip actually fail? | full-run results joined with certificates | Designed (v4 §6.6) |

### 4.4 The Verdict compiler

The Verdict is compiled in three stages, so agents and CI can stop at any point:

1. **plan:** obligations from Select, Integrity and Untested-change. Needs no test results; this is what `tests_for` returns.
2. **run:** the CI or the agent executes the obligated tests. Quod observes outcomes (and records footprints on trusted runs).
3. **seal:** Flake-proof and Culprit run over the outcomes. The Verdict is then assembled, hash-chained to its certificates and signed (trusted CI only).

---

## 5. Data model additions (v4 schemas unchanged)

### 5.1 The Verdict (in-toto statement, predicate `https://quod.dev/verdict/v1`)

```json
{
  "_type": "https://in-toto.io/Statement/v1",
  "subject": [{ "name": "git+<repo>@<head_tree>", "digest": { "gitTree": "<sha>" } }],
  "predicateType": "https://quod.dev/verdict/v1",
  "predicate": {
    "v": 1,
    "status": "pass | fail | blocked | advisory",
    "change": { "base": "<sha>", "head": "<sha>", "tree": "<hash>", "author_kind": "human | agent", "agent": "claude-code|…|null" },
    "inputs": { "map": "<merkle>", "policy": "<hash>", "engine": "<ver>", "environment": "<digest>" },
    "obligations": { "ran": 37, "passed": 36, "failed": 1, "certified_skips": 1381, "fallback_rules": ["R002"] },
    "findings": [
      { "kind": "untested_change", "subject": "src/billing.py::refund", "severity": "warn" },
      { "kind": "integrity", "rule": "I-GUARD-EDIT", "subject": "tests/test_billing.py::test_refund", "severity": "require_review" },
      { "kind": "flake_proof", "subject": "tests/test_net.py::test_retry", "evidence": "footprint ∩ change = ∅", "severity": "info" }
    ],
    "certificates": { "chain_head": "<hash>", "count": 1381, "location": "<cas uri>" },
    "explain_uri": "<deep link>"
  }
}
```

- The **skip certificates** from v4 §5.3 are unchanged. The Verdict refers to them by the head of their hash chain.
- `status: advisory` is only ever produced in sandboxes. The verifier rejects an advisory Verdict wherever a proof is required.
- We publish this predicate as an open spec with a conformance suite. That is the standards moat.

### 5.2 New journal events

`VerdictSealed`, `UntestedChange`, `FlakeProven(test, evidence)`, `CulpritNamed(test, commit, shared_nodes)`, `Quarantined(test, policy)`, `IntegrityFinding(rule, subject)`.

### 5.3 Graph additions

- **Commit-versioned maps with structural sharing.** Each trusted run patches the map, and the system keeps a persistent version per commit. That lets us answer "the footprint as of commit X" cheaply, which is what Culprit and Audit need.
- **Owner attribution** for tests, from CODEOWNERS plus history, to route quarantine and culprit notifications.

---

## 6. Interfaces

### 6.1 The Verdict card (PR check)

```
Quod Verdict: PASS (enforced) · 37 run · 1,381 proven skipped · 41 s
  ⚠ 1 untested change     src/billing.py::refund: no test executes it
  ⛔ integrity             test_refund was edited and guards refund(): human review required
  ℹ 1 flaky (not yours)   test_retry failed; nothing it depends on changed. Retried and passed.
  ✓ all 1,381 skips certified · verify ↗ · why did X run/skip? ↗
```

### 6.2 Agent API (MCP and CLI, same core)

| Call | Returns | Mode |
|---|---|---|
| `tests_for(diff?)` | obligated tests (ordered), untested changes, integrity warnings, flake notes | offline, advisory |
| `check(results?)` | an advisory Verdict for the current tree | offline, advisory |
| `why(test)` | the rule and evidence for run or skip | offline |
| `impact(paths \| packages)` | the tests reached and the untested usages | offline |
| `culprit(test)` | the commit and the shared nodes (needs history) | control plane |

### 6.3 Events and webhooks

Verdict sealed, culprit named, test quarantined, escape detected. They go to Slack, PagerDuty and agent orchestrators: an orchestrator can subscribe and re-plan when a Verdict blocks.

---

## 7. Futuristic horizon (designed for, built later)

These items follow from the same graph and are the long-term differentiators. Each one keeps P1: nothing probabilistic ever gates a merge.

1. **Speculative verification.** While an agent is still editing, a learned model *prefetches* the likely obligated tests in a warm sandbox, so results are ready by the time it calls `check()`. The model only orders and prefetches; the Verdict still comes from proof.
2. **Verify once, trust everywhere.** Outcome keys are content-addressed, so a proof minted in trusted CI can be reused by any branch, agent or fork whose inputs hash the same. Across an organisation, verification cost tends toward the number of *distinct* states rather than the number of attempts.
3. **Verdicts as agent rewards.** RL and eval harnesses use the sealed Verdict as a reward signal that resists hacking:
   - tests and policy sit outside the agent's write authority;
   - integrity findings zero the reward;
   - untested changes reduce it.
   The batch API from v4 §12 serves this.
4. **The living graph.** The evidence graph becomes an organisation-wide knowledge base: "what exercises this function?", "what would this refactor touch?", "which tests are dead?". Agents query it before they write code, not only after.
5. **Beyond unit tests.** The same footprint and certificate model extends to other jobs:
   - lint, typecheck and build, using file-access tracing with a declared tracer coverage, where incomplete coverage means run;
   - integration and E2E tests, under three named regimes: pinned hermetic service, recorded interactions with periodic refresh, and bounded staleness.
   Every CI job then either runs or carries a proof.
6. **The open verdict standard.** We contribute the predicate to in-toto/OpenSSF (as a test-result v0.2 with skips and reasons, or as a separate predicate), with GitHub, Witness and JFrog policy packs. "Has a valid Verdict" then becomes a supply-chain policy primitive.
7. **LLM explanations from structured evidence.** A model turns findings into plain-language reviewer summaries and fix suggestions, such as "add a test that executes `refund()`". It only renders evidence and never adds claims.

---

## 8. Build sequence

| Release | Flows | New components | Exit gate |
|---|---|---|---|
| **V1 Shadow Verdict** (6–8 wks) | F1 (shadow), F2 (`tests_for`), F4, F6 (labels only), F9 (shadow) | Verdict compiler (plan/seal), Untested-change lens, Integrity lens v1, Flake-proof lens, Verdict card, GitHub Action, MCP server | 3–5 design partners in shadow; 0 shadow escapes; ≥1 proven flake or untested-change finding they act on |
| **V2 Enforce** (months 2–4) | F1 enforce, F3, F5, F6 quarantine, F8 | Culprit lens, commit-versioned maps, control plane (outcome index, journal rollups), L1 reuse, Jest probe | Enforced on ≥2 partners; culprit in ≤10 s on real breakages; first paid conversion |
| **V3 Platform** (months 4–12) | F7, F10, F11 automated | Merge-queue gate, ledger export, WASM verifier, published predicate spec, Go/JVM probes, lint/build tracing (beta) | Merge-queue design partner; spec published; public escape log |

**Kill criteria, carried over from the research:**
- no design partner acts on V1 findings within 6 weeks;
- no partner moves from shadow to enforce;
- a major platform ships signed skip evidence first. In that case, pivot to being the open verifier and standard for others' evidence.

---

## 9. Decisions to record as ADRs

| ADR | Status | Decision |
|---|---|---|
| [015](adr/0015-verdict.md) | Accepted | The Verdict is the product primitive (in-toto Statement, DSSE, pinned changed set); lenses are pure functions over pinned inputs |
| [016](adr/0016-flake-proof.md) | Accepted | Flake-proof clears a failure only when its footprint is complete and untouched; never a footprint-intersecting failure |
| [017](adr/0017-advisory-verdicts.md) | Accepted | Advisory Verdicts (sandbox, MCP) and sealed Verdicts (trusted CI) are separate; the verifier rejects advisory where proof is required |
| 018 | Proposed | Culprit naming is evidence (shared nodes). Auto-revert is opt-in policy, never a default |
| 019 | Proposed | Maps are commit-versioned with structural sharing, to serve Culprit and Audit |

---

## 10. Open questions (validate with design partners)

1. Will reviewers act on integrity and untested-change findings, or treat them as noise? What severity defaults work?
2. Will teams accept "not yours: flaky" relabelling as merge-safe, or only as information?
3. How much history does Culprit need on typical repos? This drives the retention of commit-versioned maps.
4. Which agent platforms allow a custom MCP server or binary in their cloud sandboxes (Codex, Copilot agent, Devin, Cursor)? If they don't, F2 has to run in CI only.
5. What pricing unit maps to the value: per committer, per sealed Verdict, or per agent run?
