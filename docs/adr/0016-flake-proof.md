# ADR 016: Flake-proof clears a failure only when its footprint is untouched

Status: Accepted

## Context

Flaky tests cost at least 2.5% of developer time (ICST 2024). Developers
re-run failures, or quarantine tests, on gut feeling. Quod knows each test's
recorded footprint, so it can sometimes *prove* that a change could not have
caused a failure. A wrong "not your fault" label lets a real regression
merge, so the claim needs strict rules.

## Decision

A failed test gets F001 ("this change did not cause the failure") only if
all of these hold:

1. no global rule fired (the run was not widened to everything);
2. no changed node is in the root scope;
3. the test is in the map, and its footprint is complete: not untraced, no
   dynamic import, no network, no subprocess;
4. the test's own file did not change;
5. no changed node is in the test's footprint.

The claim's wording depends on whether the environment was fingerprinted
(ADR 012):

- **with environment mode:** "nothing this test depends on changed";
- **without:** "this change's code did not cause it".

Flake-proof **never** relabels a failure whose footprint the change touches.
F001 only adds a finding. A cleared failure stops counting towards `fail`,
but it stays visible on the card.

## Consequences

- In enforce mode, a cleared failure does not fail the job. Quarantine
  policy (repeated F001 for one test) is a later decision.
- The independent verifier re-checks rules 1, 3, 4 and 5 against the map
  and the Verdict's pinned changed set, and rule 2 via the map's root scope.
