# ADR 024: Incremental maps

Status: Accepted (engine 0.1, quod-action `incremental`, pytest)

## Context

Every push to the default branch recorded the whole suite under the probe,
so Quod's own overhead was one full, instrumented test run per merge. On a
suite that Quod cuts to 20% on pull requests, that overhead decides whether
CI is cheaper at all.

A pull request's plan already proves, test by test, that a skipped test's
footprint is disjoint from the change. The same proof says that test's
recorded footprint still holds at the new commit: nothing it observed
changed.

## Decision

1. `quod record --incremental BASE` plans the change from `BASE`'s commit to
   the working tree (as `quod select --advisory` would, including
   environment mode with `--python`), runs the test command with the skip
   list (`--quod-skip`), and builds the new map from:
   - the run's captures, for every test that ran;
   - `BASE`'s footprint of every test the plan skipped with a certificate;
   - the union of the new and the old root scope (conservative: the root
     only grows until the next full recording);
   - capabilities common to `BASE` and the new captures.
   File nodes are rehashed at the new commit, since another part of a file
   in a skipped test's footprint may have changed. Other carried nodes keep
   what the new run recorded for them, or their old hash.
2. Quod plans by itself rather than accepting a skip list, so a stale or
   mistaken list can't make the map unsound. When the change runs every test
   (a global trigger, R017) or the plan fails, it records every test.
3. The probe's own work in such a run (reading the skip file, fingerprinting
   the environment to check its conditions) goes to a throwaway scope. It
   had leaked into the root scope.
4. The Action records incrementally by default on pytest (`incremental`),
   from the previous map in its cache. It records every test on every 25th
   run (`full-record-every`), so a map never depends on a long chain of
   carried footprints.

## Consequences

- E1 check on more-itertools, three consecutive commits chained: each
  incremental map was identical to a full recording at that commit, in
  every footprint, file hash and the root scope. The runs recorded 15, 1
  and 626 of 771 tests.
- Regression test: `incremental_recording_matches_a_full_recording`
  (the map equals a full ingest at the same commit, hashes included).
- **Assumed (as for skipping):** a test whose footprint didn't change
  behaves as recorded. Carrying its footprint relies on exactly what
  skipping it does. Periodic full recordings re-check that.
- Durations of carried tests are the old ones.
- Vitest and Jest still record every test; the JS runner records and ingests
  separately, and its incremental path is left for later.
