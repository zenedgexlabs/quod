# ADR 020: New top-level functions affect only namespace observers

Status: JavaScript/TypeScript part accepted and implemented; Python part proposed

## Context

E8b on attrs: in 4 of 5 agent tasks, Quod obligated the whole suite. One
cause was a new top-level `def`. Today, adding a definition to a module is
a module-level change (the module's stream gains a binding), and with
ADR 019 every test that imported the module, or ran any of its functions,
re-runs. For a package's `__init__.py`, that is every test.

What a new `def name` at module level actually does when the module runs:
it binds one more name in the module's namespace. Nothing that ran at the
base commit could have called the new function, because it didn't exist.
So the tests that can tell the difference are the ones that observed the
**absence** of the name or the **set** of names:

| Observation at base | Example | Affected by a new name? |
|---|---|---|
| failed lookup of that name | `hasattr(mod, "f")`, `getattr(mod, "f", None)`, `try: from mod import f` | yes |
| enumeration of the namespace | `dir(mod)`, `vars(mod)`, `mod.__dict__`, `from mod import *` without `__all__`, `inspect.getmembers` | yes |
| global lookup in the module's own code that fell through to builtins | `f` shadowing a builtin used in `mod` | yes |
| module `__getattr__` (PEP 562) answering for `f` | lazy attributes | yes |
| anything else | calling other functions, reading other names | no |

## Proposal

### Engine (`quod-change`, gated on a new probe capability `py.namespace`)

A top-level `def` added at head counts as a *namespace* change, not a
module change, when all of these hold:

1. it is directly in the module body (not under `if`/`try`/`with`),
   undecorated, and its evaluated header (ADR 011) is empty: every default
   is a literal or builtin, annotations are lazy or absent, no type
   parameters;
2. the name is not a builtin, not a dunder, and doesn't occur as a word
   anywhere in the base version of the file (covers code in the module
   that looked the name up, including in strings passed to `eval` or used
   as forward references);
3. the base version defines no module-level `__getattr__` or `__dir__`;
4. removing exactly these `def` statements from the head stream gives the
   base module stream (no other module-level change).

Then the delta is `module_changed = false`, the new function is a new
entity (no observers), and the reserved entity `<namespace>` of the file
changes. Classes are excluded at first: a class body runs at import, and
base classes can run `__init_subclass__` or a metaclass.

`__all__` edits (the other half of the E8b case) can follow the same
pattern: a literal list or tuple of strings, with `__all__` occurring only
in that assignment, changes `<namespace>` instead of the module.

### Probe (`py.namespace`)

The probe swaps the class of every repository module to a subclass of
`types.ModuleType` (allowed since Python 3.5; only when the type is exactly
`ModuleType`) with:

- `__getattr__`: called only when normal lookup fails, so it records
  failed lookups (`hasattr`, `getattr` with a default, `from m import x`)
  at no cost to successful ones, then raises `AttributeError`. Dunder
  names are ignored (tools probe `__path__`, `__test__`, ...);
- a `__dict__` data descriptor: records enumeration (`vars`, `dir` and
  star-imports read `__dict__` through the type), then returns the real
  dictionary;

and records both as an observation of `(file, "<namespace>")` in the
current scope. The swap happens before the module body runs (on the
module's first `PY_START`), so lookups during circular imports are seen.
A module that replaces itself in `sys.modules`, or has a custom class,
isn't swapped and gets no exemption: the probe lists the files it tracked,
and the engine exempts only those.

Root-scope observations (pytest enumerating conftest and plugin
namespaces) turn a namespace change into R017, as for any root node.

### JavaScript

E8 on luxon (2026-11-27) hit the same case: the only change to
`src/impl/util.js` was a new `export function snapFloatingPoint`, and since
`util.js` is imported everywhere, all 1,190 tests ran on every step.
JavaScript makes the exemption simpler than Python:

- function declarations are hoisted, so adding one runs no code when the
  module is evaluated;
- an ES module can't import a name that doesn't exist (a link-time
  error), so there is no failed-lookup case to observe;
- a new export is visible only to code that enumerates or dynamically
  indexes the module namespace: `import * as ns` with `Object.keys(ns)` or
  `ns[name]`, `export * from`, dynamic `import()`, and CommonJS interop
  (`require`).

Implemented (`quod-change`):

- `lang::diff` collects top-level `function`/`function*` declarations,
  plain or `export`ed but not `export default`, whose name doesn't occur
  anywhere in the base file (code, comments or strings). It removes them
  from the head source and re-analyses: only if the module stream then
  equals the base's is the change additive (`module_changed = false`,
  `added` lists the names). A removal that changes how the remaining code
  parses never matches, so it is never exempt.
- `compute` then runs one `git grep` over the head (commit, or working
  tree with untracked files) for lines that could observe a module's set
  of names: `import * as`, `export *`, `import()`, `require()`,
  `.mock(`, `requireActual`/`importActual`. A matching line names the
  module if one of its string literals ends in the module's stem (or its
  directory, for `index` files). A line with no readable literal (a
  dynamic specifier, a template with substitutions, a statement split
  over lines) counts as naming every module. Any match makes the change a
  module change again.

## Remaining assumptions

- The namespace reached without going through the module object: a
  function's `__globals__`, a frame's `f_globals`, `globals()` in another
  module's code. Condition 2 covers the module's own code. Libraries that
  enumerate another module's globals this way (some pickling and
  serialization tools) aren't covered and must be listed in the probe's
  assumptions.
- C extensions that read a module's dictionary with `PyModule_GetDict`.

## Validation plan

- Unit tests for each condition, including each hazard in the table as a
  must-not-exempt case.
- Conformance: the E8b attrs tasks that added a function (`af9c510`,
  `5aa76a4`) and E1 additive mutants (add an unused function to a module)
  must select fewer tests with 0 escapes, compared with a full run.
