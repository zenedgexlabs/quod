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
