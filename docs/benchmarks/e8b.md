# E8b: A real coding agent with and without Quod

**Question:** a real agent works on real tasks. Does it use Quod? Does
Quod keep it from finishing on a broken change? And does it run fewer
tests?

**Method** (`conformance/agent_tasks.py`). The agent is Claude Code,
headless (`claude -p`, the bundled binary), on Windows. It works in a
scratch clone of attrs.

Each task is a real commit:

- The agent starts at the commit's parent.
- The commit message is the task.
- The commit's own test changes are hidden. They are applied afterwards
  to score the agent's implementation.

The agent can only:

- read and edit files;
- run `<venv python> -m pytest`;
- call the Quod tools, when Quod is attached.

There are three conditions:

- **quod:** the MCP server is attached, with a map recorded at the parent.
  Nothing else tells the agent about it.
- **quod-guided:** the same, plus the section `quod mcp install` writes
  into `CLAUDE.md`.
- **baseline:** no Quod.

A **false green** is a run where the agent's last pytest run passed but
the full suite on its result fails.

## Results (2026-11-26, attrs, 5 tasks, 11 sessions, about $3.90 of usage)

| task | condition | `tests_for` calls | tests in its pytest runs | full suite on result | hidden tests | false green |
|---|---|---|---|---|---|---|
| f53fc54 evolve dunders | quod | 0 | 1,408 | pass | pass | no |
| | quod-guided | 1 | 1,405 | pass | 1 failure | no |
| | baseline | 0 | 1,404 | pass | pass | no |
| 5aa76a4 `ne` validator | quod-guided | 1 | 1,403 | pass | pass | no |
| | baseline | 0 | 168 | pass | pass | no |
| a572c3a NO_OP on frozen | quod-guided | 1 | 1,378 | pass¹ | pass | no |
| | baseline | 0 | 1,388 | pass | pass | no |
| af9c510 nested `disabled()` | quod-guided | 1 | **164** | pass | pass | no |
| | baseline | 0 | 164 | pass | pass | no |
| 97f8d17 ClassVar forward refs | quod-guided | 1 | 1,392 | pass | pass | no |
| | baseline | 0 | 1,392 | pass | pass | no |

¹ Scored after an interrupted checkout of the hidden tests. Its own test
file had been replaced by the commit's.

## What it shows

1. **An attached server isn't enough; instructions are.** With only the
   server connected, the agent never called Quod, although the tools were
   listed and the server's `initialize` instructions were sent. With the
   five-line `CLAUDE.md` section, it called `tests_for` in 5 of 5 tasks,
   ran the command it returned, and called `check`. `quod mcp install`
   now writes that section.
2. **No false greens in any condition.** Every agent result passed the
   full suite. One guided run failed one hidden test: its fix differed
   from the real commit's. Agents vary, and the run with the server only
   passed the same task.
3. **attrs is hard for selection, and Quod paid for soundness.** In 4 of
   5 tasks the change touched module-level code in modules every test
   imports. Two examples:
   - a function attrs runs while building its own classes at import time;
   - a new top-level function plus an `__all__` edit.

   Quod then obligates the whole suite (rule R017 or module-level
   widening), which is sound. On `5aa76a4`, the baseline agent ran only
   `test_validators.py` (168 tests) and happened to be right; Quod could
   not prove that the other tests were unaffected. On `af9c510`, Quod gave
   164 tests, the same set the agent chose by hand, but with proof.
4. **Speed isn't the point on attrs.** Its full suite takes about 7 s, so
   the agent's wall time is dominated by the model. The guided sessions
   cost about $0.13 more each, for the extra tool calls and output.
   Selection pays off on slow suites (E1, E8). On fast ones what Quod adds
   is assurance: an explicit, checkable reason for every test skipped.

## Follow-ups

- **Compact `tests_for`.** When everything must run, it now says so with
  the reasons instead of listing tests.
- **Additive definitions.** A new top-level `def`/`class` under an unused
  name can only affect star-importers and code that enumerates the
  module's namespace. Exempting it soundly needs probe support for
  namespace enumeration and an ADR.
- **Re-run on a repository with a slow suite,** and in a CI-like Linux
  sandbox.
