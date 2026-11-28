# ADR 025: Mutation testing scoped to the change

Status: Accepted (advisory; pytest; Python probe 0.4.11, quod-action `mutation`)

## Context

A green test run says the tests passed, not that they check the change. E9
on networkx: an injected bug (`axis=0` to `axis=1` in
`local_and_global_consistency`) failed no test, in the full suite or in
Quod's selection. Nothing escaped, because nothing would have caught it
anyway.

Mutation testing measures that gap: make a small edit, run the tests, see
whether any fails. Over a whole suite it costs one full run per mutant, which
is why few projects use it. The footprint map changes the cost: the tests that
execute a function are its observers in the map, usually a handful.

## Decision

1. `quod mutate --map MAP [--python PY] -- <pytest command>` plans the change
   as `select` does, then, for every function the change touched in a
   non-test Python file:
   - lists mutants of that function's own body with
     `python -m quod_probe mutants FILE QUAL` (comparison and arithmetic
     operators, `and`/`or`, booleans, small integers; docstrings and nested
     definitions left out; every mutant compiles);
   - runs, once without a mutant and then once per mutant, the recorded
     tests that execute the function (`--quod-only`, a new plugin option).
     When the change also edits tests, which aren't in the map yet, it runs
     the plan's tests instead (`--quod-skip`);
   - restores the file after every mutant, whatever happens.
2. A mutant is *killed* when a test fails or the run passes its time limit,
   *survived* when the tests pass. The report lists the survivors with the
   mutated line. When the tests fail without any mutant, the function isn't
   measured.
3. It's advisory: it never fails a job, and it's not part of the Verdict's
   claims. The Action runs it on pull requests with `mutation: true`
   (default off), with a budget (`mutation-max`, default 30), and appends the
   section to the job summary and the pull request comment.

## Consequences

- On the demo, `convert` (3 mutants) and `read_csv` (7) were fully killed by
  their 5 and 4 covering tests, in about 10 seconds each.
- On a function whose test only tries `x = 1`, `2 * x` to `2 // x` and the
  untested `else 0` to `else 1` survive: exactly the gaps.
- Equivalent mutants (edits that don't change behaviour) survive too, and
  can't be told apart automatically. The report says "survived", not "bug".
- Cost is one test run of the covering tests per mutant, plus one baseline
  run per function.
- Not yet: Vitest and Jest; mutating only the changed lines of a function.
