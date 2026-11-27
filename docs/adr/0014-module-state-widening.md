# ADR 014: Widen tests that touch files with module-level state

Status: Accepted

## Context

E1 on date-fns (JS) found 90 dependency escapes from one mutant, the first
real soundness failure since run 1. `tzOffset` memoises results in a
module-level object:

```ts
const offsetCache: Record<string, number> = {};
export function tzOffset(tz, date) {
  // ...
  if (offsetStr in offsetCache) return offsetCache[offsetStr]!;
  return calcOffset(offsetStr, offsetStr.split(":"));
}
function calcOffset(cacheStr, values) { return (offsetCache[cacheStr] = /* ... */); }
```

Recording runs tests one after another in one process. The first test that
sees an offset runs `calcOffset`. Every later test reads the cached value
and never runs `calcOffset`, so `calcOffset` isn't in those tests'
footprints. A change to `calcOffset` then skipped 90 tests that do depend on
it: run alone, each computes the value itself and fails.

This is the cross-test-state assumption the Python probe documents. The
Python probe handles `functools` caches, but hand-written memo tables are
idiomatic in both languages, so the assumption can't be left to the audit.

## Decision

**Module-state widening.** `quod_change::lang::LineMap::module_state` marks a
source file *stateful* when its top level, or a class body, declares
mutable state:

| language | stateful when |
|---|---|
| JS/TS | top-level `let`/`var`; `const` bound to an object, array or `new` expression; `static` class fields with such values |
| Python | module- or class-level assignments of dict/list/set displays or comprehensions, or `dict()`/`set()`/`defaultdict()`-style calls; any `global` statement |
| Go | package-level `var` |
| Rust | `static` items, `thread_local!` |
| Java | `static` fields other than `final` primitives and strings |

At ingest, a test (or the root scope) that touched a stateful file through
any observation (an executed function, module-level code, lines) also
observes **every entity of that file and the file node**. A cached result
can then be stale only through code in the same file, and every change to
that file re-runs the test.

## Consequences

- **Sound for same-file memoisation**, the dominant idiom: a cache next to
  the functions that fill it.
- **Precision cost** is confined to stateful files: tests that touch them
  lose function granularity *within those files only*. Stateless files stay
  function-precise.
- **Still assumed (documented, audited):**
  - state shared *across* files, such as a cache in one module filled with
    values computed by a function in another, which a later test reads
    without running that function;
  - stateful values created by calls the detector doesn't recognise (for
    example `const cache = createCache()` in JS).
- A regression test reproduces the date-fns escape
  (`memoised_module_state_widens_to_the_whole_file`).

## Amendment (2026-11-25): dunder declarations are not state

E7 on more-itertools showed tests "executing" every function of `more.py`.
The cause was `__all__ = [...]`: a list display at module level, which
nearly every Python package module has, so the widening fired almost
everywhere. Assignments to dunder names (`__all__`, `__slots__`,
`__match_args__`, ...) are declarations that the language and tools read,
not state that code mutates between calls, so they no longer mark a file
stateful. Other module-level tables (for example `_perfect_tests = [...]`)
still do.

## Amendment (2026-11-25): a test's own file is exempt

Test modules often hold module-level tables (parameters, fixtures data),
which marked the whole test file stateful: every test in it then observed
every other test class, and a commit adding a test anywhere in the file
implicated them all (E7, more-itertools). State that one test leaves for
another inside a test module is order dependence, already a separate
category (victims), not a dependency of the test on code it didn't run.
The widening now skips the test's own file; helper modules it imports are
still widened.

## Amendment (2026-11-25): constant tables of immutable literals

A Python module-level `NAME = [...]` / `{...}` whose elements are all
immutable literals (strings, numbers, booleans, `None`, tuples of them)
is not state if every use of NAME in the file is read-only:

- an index read (not an assignment or `del` target);
- the iterable of a loop or comprehension;
- a comparison or condition;
- an argument to a pure builtin (`len`, `sorted`, ...);
- a non-mutating method call (`get`, `items`, ...);
- a rebinding.

Any other use keeps the file stateful: passing the table positionally or
by keyword, returning or aliasing it, a mutating method, or `global`.
Immutable elements mean that iterating over the table can't hand out
anything mutable. Mutation from other files stays the documented
cross-file assumption.

## Amendment (2026-11-27): numbers and read-only defaults

Found by the demo repository: `RATES = {"EUR": Decimal("0.92"), ...}`,
used as `def convert(amount, to, rates=RATES)`, made `currency.py`
stateful. The package's `__init__` imports it, so every test was credited
with every function in it, and a change to `convert` ran the whole suite.

Two more forms are read-only tables:

- **Immutable numbers.** An element may be `Decimal(...)` or
  `Fraction(...)` with literal arguments, when the file imports the name
  from the standard library (`from decimal import Decimal`, or
  `import decimal` and `decimal.Decimal`). An alias (`as D`), an import
  from anywhere else, or a module-level rebinding of the name doesn't
  count.
- **Defaults that are only read.** `def f(x=NAME)` hands NAME to every
  call that omits `x`: the classic mutable-default problem. It counts as a
  read-only use only if every use of `x` in `f`'s body is read-only by the
  same rules (index reads, iteration, comparisons, pure builtins,
  non-mutating methods). Returning `x`, passing it on or mutating it keeps
  the file stateful. Defaults of defaults are followed two levels deep.

Mutation from other files stays the documented cross-file assumption.
