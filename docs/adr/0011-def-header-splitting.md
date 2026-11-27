# ADR 011: Split `def` headers by what the enclosing scope evaluates

Status: Accepted (engine 0.1, python-probe 0.2)

## Context

E1 run 3 on click: 15 of 20 history steps ran the full suite. Ten were R017
(root scope), and not policy triggers. Click's conftest imports the whole package,
so every class body runs in the root scope. Any change to a class entity is
therefore global.

Most of those class-entity changes were edits to method *headers*: a new
keyword parameter with a constant default (`context_settings=None`), or a
return annotation changing to `-> t.Self` in modules with
`from __future__ import annotations`. `quod-change` put the whole header in the
enclosing stream, because decorators, defaults and annotations run in the
enclosing scope.

That is too coarse. The enclosing scope evaluates only some of the header:

| Header part | Evaluated by the enclosing scope? |
|---|---|
| decorators | yes, and they receive the function object |
| default with side effects possible (call, attribute, subscript, operator) | yes |
| default free of side effects (literal, plain name, display of those) | evaluated, but the only effect is the value stored on the function |
| annotation, eager | yes |
| annotation, lazy (`from __future__ import annotations`) | no, stored as a string |
| parameter names | no, part of the function's code object |

## Decision

With `DiffOptions::split_headers`, an **undecorated** `def` contributes to the
enclosing stream only:

- its binding (name, `async`);
- type parameters;
- defaults that aren't free of side effects;
- annotations, unless they're lazy.

The full header is always in the function's own stream. A decorated `def`
keeps its whole header in the enclosing stream.

What moves out of the enclosing stream changes only the function object. Two
kinds of code observe it:

1. **Callers.** They run the function, so `PY_START` records it.
2. **Introspectors.** These are `inspect.signature`, `getfullargspec`,
   `typing.get_type_hints` and `inspect.get_annotations`, for example to build
   a CLI or an API schema. Probe 0.2 records these calls as a use of the
   introspected function (and of the `functools.wraps` chain, or the class's
   `__init__`, `__new__` and metaclass `__call__`). It declares this as the
   `py.introspection` capability.

A map's capabilities are the intersection over its captures.
`quod select` splits headers only for maps with `py.introspection`. Older
maps get the previous, conservative analysis.

## Assumptions (documented, audited)

- **Direct dunder reads aren't recorded.** Reading `f.__defaults__`,
  `f.__annotations__` or `f.__code__` without calling `f` or using one of the
  patched functions isn't recorded.
- **Captured references.** `typing.get_type_hints` and `inspect.get_annotations`
  are patched as module attributes. References bound before recording starts
  aren't covered. `inspect.signature` and `getfullargspec` are covered through
  `inspect._signature_from_callable`.
- **Name lookups have no side effects.** Evaluating a plain-name default doesn't
  run code. The exception is a class body under a metaclass with a custom
  `__prepare__` mapping, which isn't modelled.

## Not done, and why

The remaining click R017 hits are real class or module semantics changes:

- methods added or removed;
- an `__init_subclass__` added;
- class docstrings changed;
- module constants removed.

Code that reads a class attribute or `__doc__` runs none of the class's code,
so narrowing these needs attribute-level observation. One candidate is
member-name reachability through executed code objects' `co_names`, but
dynamic `getattr` is a wildcard in practice.

A version bump in `pyproject.toml` stays a full run. pytest's plugin discovery
reads every installed distribution's metadata in the root scope. The
`uv.lock` change also marks the `__site__` pseudo-package, which the root scope
observes.

## Consequences

- **Offline estimate on click history** (stored captures re-ingested with the
  capability, which estimates precision only, not soundness): one more step
  skips 1,117 of 2,651 tests. The skipped-time fraction goes from 28.9% to
  29.6%.
- **Soundness** must be re-measured by E1 with probe 0.2.
- New tests: `quod-change` split rules (`split_headers_*`), probe introspection
  capture (`test_introspection_counts_as_use`), and the CLI capability gate
  (`header_split_requires_the_introspection_capability`).

## Addendum: plain names can raise (2026-11-27)

The table above counted a plain name as a default free of side effects.
Looking one up can raise `NameError`: `def f(b=SENTINEL)` with `SENTINEL`
unbound when the `def` runs (a typo, or a name bound later in the module)
fails the module's import. Every importer observes that failure, but the
change was attributed to `f` alone, so importers that never call `f`
were skipped. Only builtin names (`int`, `object`, ...) are now free of
side effects; any other name stays in the enclosing scope's stream. That
costs precision only when such a default changes: an unchanged `=SENTINEL`
hashes the same on both sides.
