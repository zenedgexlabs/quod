# ADR 019: Cached imports carry the imported module's closure

Status: Accepted

## Context

A module's body runs once per process, in the scope that imports it first.
A minimal repository showed the consequence:

```python
# src/util.py
CONST = 1
def f():
    return CONST

# tests/test_a.py               # tests/test_b.py
import util                     import util
def test_a():                   def test_b():
    assert util.f() == 1            assert util.f() == 1
```

`util`'s body ran while `test_a.py` was collected, so only `test_a`
recorded `src/util.py`. `test_b` recorded `util.f` but not the file.
Changing `CONST = 1` to `CONST = 2` changes only module-level code, and
the file node's only observer was `test_a`. Quod skipped `test_b`, which
fails at the new commit: a false green.

E1 and E8 didn't catch it: most of their module-level mutants are in
modules that a conftest imports (root scope, R017), or are observed in
the first test module to import them. Nothing guarantees either.

## Decision

Two layers, both conservative:

1. **Engine (all maps).** A change to a file node also obligates every
   test that observed a function of that file (`quod-ladder`,
   `resolve_changes`). A function's globals (Python) or closure (JS) are
   its module's namespace, so running any of its code depends on the
   module's top level, wherever that top level ran. Maps recorded by older
   probes get this protection too.

2. **Python probe: import closures.** The probe wraps
   `builtins.__import__` and `importlib.import_module`
   (`quod_probe/imports.py`):
   - a call that may load modules pushes a fresh collector. Its
     observations become the *closure* of every module that finished
     loading during the call;
   - every import, cached or not, adds the module it names (its parents
     for `import a.b.c`, and the submodules in `fromlist`) and that
     module's closure to the current scope;
   - each scope records a given cached import only once. Libraries import
     inside hot functions: 261,000 cached imports in one attrs run.

   This covers what layer 1 can't: a test that reads `util.CONST` without
   calling anything in `util`, and modules whose values come from
   another module's import (`from helper import BASE`).

## Consequences

- Footprints grow by each test module's import closure: on attrs, from
  1.2 to 14.3 files per test. Selection on attrs is unchanged, because
  `conftest.py` imports `attr`, so module-level changes there already ran
  everything (R017).
- Recording overhead on attrs: about 5% (10.2 s → 10.8 s).
- A module-level edit now also runs the tests that called the module's
  functions. A per-name analysis (which module bindings a function reads)
  could narrow this later.
- Assumptions, documented in the probe:
  - modules reached without an import (`sys.modules[name]`, or an object
    handed over from elsewhere) are covered only by layer 1;
  - modules loaded by direct `exec_module` (pytest's `importlib` import
    mode) record their own file but no closure.
- The JS probe needs the same analysis. ESM modules are evaluated once
  per worker; Vitest isolates per test file, but tests within one file
  share one evaluation. Layer 1 covers functions; reading an exported
  binding without calling anything is the remaining gap.
