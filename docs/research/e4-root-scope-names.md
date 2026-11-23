# E4: name-level root-scope precision

The question was whether R017 can be narrowed for repos whose conftest imports
the whole package (click, jinja). The idea: a module- or class-level edit only
changes the names it binds, so only tests that read those names would need to
re-run.

## Method

[`conformance/experiments/e4_root_names.py`](../../conformance/experiments/e4_root_names.py)
replays the root-hit history steps offline from stored E1 artefacts.

1. **Changed bindings.** It diffs each changed module's top-level and
   class-level statements per bound name, using the ADR 011 rules (an
   undecorated `def` that exists on both sides is function-level).
   `if TYPE_CHECKING` blocks are ignored. It closes over module-level data flow
   and over `from M import X` copies across modules. Changes the interpreter
   acts on implicitly make a step **opaque**: special methods,
   `__init_subclass__`, class headers, `__all__`, module `__getattr__`, and
   non-binding statements.
2. **Names a test may read.** These are the identifiers of every repository
   function the test executed, plus its test module's top-level code.
   Root-scope functions that read a changed name affect everyone.
3. **Two bounds.**
   - *Optimistic* trusts those names.
   - *Guarded* also treats a test as reading everything when its executed
     repository code is reflective (`getattr`, `vars`, `__dict__`, `dir`,
     `inspect`, `copy`, `pickle`, field helpers).

   Neither sees stdlib or package code, so both overstate the gain.

## Result (2026-11-23)

| repo | root-hit steps | opaque | time run on those steps: R017 / optimistic / guarded |
|---|---|---|---|
| click | 8 | 4 | 1.00 / 0.73 / **0.97** |
| jinja | 7 | 2 | 1.00 / 0.59 / **0.98** |

Half the steps are opaque because they change special methods,
`__init_subclass__`, class headers or module `__getattr__`. On the rest, the
optimistic bound saves 27–41%, but it isn't sound: nearly every test executes
repository code that reads attributes reflectively, so the guarded bound saves
2–3%. Real gains would need *runtime* attribute-read observation:

- a module `__class__` swap for module attributes;
- metaclass and instance `__getattribute__` instrumentation for classes.

That costs a large slowdown and adds semantic risk, and it is capped by the
optimistic bound. Even then, click's overall history time would drop at most
to about the file baseline.

## Decision

**Don't build it.** R017 stays as is. Click and jinja are reported as the
known worst case for sound selection: a package imported by its own
conftest, with frequent class- and module-level edits. Revisit only if a
design partner's repository is dominated by this pattern. The cheaper
alternative to test then is recording the root scope *per test module*
(lazy conftest imports) rather than analysing names.
