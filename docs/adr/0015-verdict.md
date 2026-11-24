# ADR 015: The Verdict is the product primitive

Status: Accepted

## Context

Research on user pain ranked CI cost 9th of 10. What users feel is about
trust and recovery:

- main is red and slow to recover;
- CI can't keep up with agent volume;
- green checks aren't trusted;
- flaky tests;
- tools that skip tests silently;
- agents weakening tests.

Selection alone is the easiest part to copy, and incumbents already hold
per-test coverage (see `architecture-v5-verdict.md`). The question users ask
is "can this change merge, and why should I believe it?"

## Decision

1. **One Verdict per change.** It records:
   - the pinned inputs (map, policy, engine, environment);
   - the changed node set;
   - what the change obligated and what ran;
   - every lens finding;
   - the skip certificates, referenced by chain head and count.

   Status is derived, never asserted:
   - `blocked` if any finding has severity `block`;
   - else `fail` if a failure was not cleared by Flake-proof;
   - else `pass`.
2. **Lenses are pure functions** over map + change + policy evaluation (+
   results). The same inputs give the same findings. The rules implemented so
   far:

   | Rule | Lens | Meaning |
   |---|---|---|
   | U001 | Untested-change | Changed function that no recorded test executes |
   | I001 | Integrity | Test edited together with code it executes |
   | R011 | Integrity | Test removed, test file deleted, or skip markers added |
   | F001 | Flake-proof | Failure this change did not cause (ADR 016) |
   | O001 | Obligation | Obligated test missing from the results. Blocks, unless its file changed |
   | O002 | Obligation | Obligated test skipped by the runner |

3. **Format: an in-toto Statement v1**, predicate type
   `https://quodhq.com/verdict/v1`, subject `gitCommit` = head. It is
   **sealed in a DSSE envelope** (Ed25519 over the DSSE PAE), so existing
   attestation tooling can store and route it.
4. **The Verdict pins its changed set.** With the map alone, a verifier can
   then re-check every Flake-proof claim, without trusting the engine or
   needing the certificates.
5. **Independent verification** (`quod-verify --verdict`) re-implements
   base64, DSSE and every check, as ADR 006 requires.

## Consequences

- New capabilities are new lenses over the same graph, not new data stores.
- The predicate URI is embedded in every Verdict. It must be final before
  anything is published.
- Untested-change claims are relative to the base map. A test added by the
  same change may cover the entity, and the finding says so when test files
  changed.
