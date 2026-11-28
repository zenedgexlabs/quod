# ADR 023: The probe replaces nothing the program can see

Status: Accepted (Python probe 0.4.9)

## Context

E9 on pylint: two of pylint's 2,425 tests failed while recording, and only
then.

- `test_worker_initialize_reregisters_custom_plugins` pickles the linter
  with dill, which pickles module dictionaries and the functions in them.
  dill pickles a function by reference only if `module.qualname` names that
  very object, and otherwise by value, with its closure. The probe had
  replaced `builtins.__import__` (ADR 019) and `functools.lru_cache`
  (`CacheTracker`) with Python functions defined in `quod_probe`, so the
  pickle reached the original builtin `__import__` (`it's not the same
  object as builtins.__import__`) and an `lru_cache` closure that didn't
  load.
- `test_functional[deprecated_module__import__]` checks calls to
  `__import__("mypackage")`. astroid builds its model of `builtins` from the
  live interpreter, so it saw a Python function where the builtin should be,
  and pylint's check found nothing to flag.

A probe that changes what the program observes can change test outcomes, and
a recording that fails tests marks them untraced. Worse, a test that passes
only because of the probe would be skipped later on a footprint recorded
under different behaviour.

## Decision

1. **`builtins.__import__` and `importlib.import_module` aren't replaced.**
   Import tracking (ADR 019) uses hooks the program doesn't see:
   - *Loads.* CPython calls `importlib._bootstrap._find_and_load` for every
     import that loads a module, and looks it up by name at each call. The
     probe wraps that private function. The closure semantics are unchanged:
     what a load observes becomes the closure of each module that finished
     loading during it.
   - *Uses.* When a code object first runs in a scope (`PY_START`), its
     `IMPORT_NAME` instructions are read from its bytecode, with their
     level and fromlist, and each named module and its closure are added to
     the scope. An import in a branch that doesn't run is counted too,
     which is conservative. When the bytecode doesn't show an import's
     level, the scope is marked untraced (fails closed).
   - *Calls.* `importlib.import_module` and `importlib.__import__` are seen
     as they start (their `PY_START` stays enabled), with their arguments.
     Explicit `__import__(name)` calls are seen through `CALL` events,
     enabled only in code objects whose names include `__import__`.
2. **Replacements that remain stand in under the original's name.**
   `functools.lru_cache`, `functools.cache` and `functools.cached_property`
   are still replaced (`CacheTracker` must see every cache created), but
   their `__module__` and `__qualname__` are the stdlib's, so pickling by
   reference resolves to them.
3. A regression test runs, under recording, the checks pylint's tests
   depend on (`test_recording_leaves_the_import_builtins_alone`): the
   builtin is a builtin, and the import entry points and functools
   decorators pickle and load as themselves.

## Consequences

- pylint's suite records with the same outcomes as without the probe.
- Import uses are found statically per code object rather than per call.
  That is at least as complete (every `import` statement in code that ran
  counts) and costs less: one bytecode scan per code object, cached.
- `importlib.__import__` is now tracked; it wasn't before.
- **Assumed (documented):** an explicit `__import__` call is absolute. A
  `CALL` event shows only the first argument, so a relative
  `__import__(name, globals(), locals(), [], 1)` of an already loaded
  module is recorded under the wrong name.
- **Still visible:** the `functools` replacements are Python objects, so
  `inspect` sees Python functions instead of the stdlib's. Pickling and
  calling behave as before.
