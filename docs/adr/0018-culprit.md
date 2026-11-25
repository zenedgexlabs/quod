# ADR 018: Culprit names a commit only with footprint evidence

Status: Accepted

## Context

A red main branch is users' most acute CI pain. CircleCI's 2026 data puts
main-branch success at 70.8%, with about 72 minutes to recover. Bisecting
re-runs the suite O(log n) times. Quod already records what each test
executes, so it can answer "which commit broke this test?" directly.

## Decision

`quod culprit --map M --good G --bad B --failed T…` (also an MCP tool)
evaluates every first-parent commit between G and B against its parent,
and intersects the change with each failing test's footprint plus the root
scope. The argument:

- **The footprint is what the test executed at G.** The map was recorded at
  G, or every *drift* left the footprint untouched. Drifts are the change
  from the map's commit to G and, in environment mode, how the failing
  run's environment differs from the recorded one.
- **A commit outside the footprint can't change the test alone.** Without
  the commits that do touch it (set S), the test executes exactly G's code
  and passes. So any code-caused failure needs a commit in S.

| Outcome | When | Claim |
|---|---|---|
| `culprit` | \|S\| = 1 | Reverting it alone restores G's behaviour for everything the test executes |
| `suspects` | \|S\| > 1 | The failure needs at least one of them |
| `not_code` | S = ∅ | No code change explains it: flaky or environment (consistent with ADR 016) |
| `unknown` | test not in map, incomplete footprint, a drift touched it or fired a global rule | None |

- **Global rules don't rule anything out.** A commit that fires one can't be
  excluded and is always a suspect.
- **Environment mode** (`--python`, `--fingerprint-cmd`) evaluates commits
  as if the environment were unchanged. CI, workflow and lockfile edits then
  stop being suspects, and the environment's real difference is a drift.
- **Culprit naming is evidence, not action.** Each suspect lists the
  footprint nodes it changed. Auto-revert, if ever offered, is opt-in policy
  (as the v5 proposal said).

## Consequences

- **E7 measures it:**
  - **Soundness:** the mutant commit must be a suspect for every test it
    breaks.
  - **Precision:** how often the mutant is named alone.
  - **Results:** see [`../benchmarks/e7.md`](../benchmarks/e7.md).
- **Interactions.** A commit outside the footprint can still *contribute*,
  for example by adding a function that a suspect later calls. The claim is
  about necessity and revert-sufficiency, not blame for every line.
- **Commit-versioned maps** (the remaining part of the v5 proposal) would
  make the drift check unnecessary. The map at G could be fetched directly.
  This is deferred: recording on every merge to main gives the same result
  today.
