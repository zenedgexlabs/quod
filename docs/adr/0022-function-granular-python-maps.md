# ADR 022: Python maps are function-granular even where no function ran

Status: Accepted (Python probe 0.4.7, capability `py.functions`)

## Context

E9 on networkx, three of the five pull requests with an injected bug ran
all 9,239 tests. Each bug was a one-token edit inside a function body
(`local_and_global_consistency` in `node_classification.py`, `moral_graph`,
`efficiency`). The replay installed `.[test]` without numpy and scipy, so
the tests of those functions were skipped, and no recorded test executed any
function of those files.

The ladder resolves a changed function in two ways:

- the map has the function node: its observers run;
- the map has no node for it: if the map has function nodes for *other*
  functions of the same file, nobody executed this one and it affects no
  test. If the file has no function nodes at all, the ladder assumed the
  probe had recorded the file only at file granularity, and used the file
  node instead.

That fallback exists for adapters that know files but not functions: build
tool closures (Go, JVM) and per-test LCOV imports (ADR 013). For the Python
probe it is never needed. `sys.monitoring` reports the start of every code
object that runs, and the probe records each one as a function node (or
the file node for module-level code). A Python file with no function nodes
means none of its functions ran.

The fallback still cost precision. networkx's `conftest.py` imports every
module at start-up, so every file node is in the root scope: the fallback
turned a body edit in an untested function into a root-scope change, and
R017 ran everything.

## Decision

1. The Python probe declares the capability `py.functions`: every function
   of a repository file that runs while recording is recorded as a function
   node, whatever else the test observes in that file.
2. When every capture of a map declares `py.functions` (map capabilities are
   the intersection of their captures'), a changed function of a `.py` file
   that the map doesn't have affects no test, even if the file has no
   function nodes. It is still listed in the certificate's changed refs
   (`fn:<file>::<name>`), and the verifier fails a Verdict if that node is
   in a skipped test's footprint or the root scope, as for any changed ref.
3. Without the capability, and for other files (`.pyi` stubs, other
   languages), the file fallback is unchanged.
4. The untested-change lens (U001) applies the same rule: such a function is
   reported as executed by no recorded test, instead of making no claim.

## Consequences

- networkx-style start-up imports no longer turn edits to untested
  functions into run-everything changes.
- Module-level changes are unaffected: a changed decorator, default value,
  class attribute or top-level statement is a module change
  (`quod-change` attributes them to the enclosing scope) and resolves to the
  file node as before.
- Functions that run only at import (called by module-level code) are
  recorded in the root or module scope as function nodes, so they are never
  "missing".
- **Assumed:** the entity names `quod-change` emits match the probe's
  `co_qualname` mapping (lambdas and comprehensions belong to their
  enclosing definition). The ladder already relied on this for every file
  with function nodes; this ADR extends it to files without any.
- Tests that pytest skipped while recording are untraced and always run
  (R012), so a function covered only by skipped tests is still run by them.
- A regression test reproduces the networkx case
  (`unexecuted_function_of_a_root_scope_file_is_not_a_root_scope_change_with_py_functions`).
