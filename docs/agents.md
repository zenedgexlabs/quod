# Quod for coding agents (MCP)

Quod's MCP server tells a coding agent which tests its change can affect,
and gives it the command that runs exactly those. Answers are advisory:
the agent controls its sandbox, so nothing it reports can license a CI
skip. CI recomputes everything and issues the signed Verdict.

## Setup (once per repository)

1. **CI publishes maps.** Run the Quod GitHub Action in `record` mode on
   the default branch. Every green run uploads its footprint map as the
   artifact `quod-map-<sha>` (kept 30 days; `map-retention-days`).
2. **Register the server** with your agent, from the repository root:

   ```sh
   quod mcp install --client claude-code   # .mcp.json
   quod mcp install --client cursor        # .cursor/mcp.json
   quod mcp install --client vscode        # .vscode/mcp.json
   ```

   Other servers in those files are kept. Commit the file so the whole
   team gets it. `quod` must be on the agent's PATH (or pass
   `--command <path>`).
3. **Log in to GitHub** where the agent runs: `gh auth login`. On start
   the server pulls the newest green map published for an ancestor of
   HEAD (`--no-pull` at install turns that off; `quod map pull` does it by
   hand). The map is kept in `.git/quod/`, never in the working tree.

## The loop

| Step | Tool | What the agent gets |
|---|---|---|
| After an edit | `tests_for` | The obligated tests with reasons, changed code no test runs, and `run`: a `cwd` and `argv` that run exactly those tests plus any new ones |
| Run tests | (its shell) | Runs `run.argv` in `run.cwd` instead of the whole suite |
| Before finishing | `check` | An advisory Verdict from the run's results file: obligations met, failures this change did not cause, untested changes |
| A test fails that the change shouldn't affect | `culprit` | Which commit between the last green one and HEAD broke it, or `not_code` (flaky or environment) |
| One test's decision | `why` | Run or skip, and why |
| Answers look too conservative | `status` | Map commit and how far behind HEAD, runner, environment mode and why it's off |

## When everything runs

That is always the safe answer, and the response says why:

- **No map yet** (`run_everything: true`): CI hasn't published one for an
  ancestor of HEAD, or `gh` isn't logged in.
- **The environment differs from CI's** (rule R018, listed in `fallback`):
  the map was recorded on another OS, interpreter or package set. Run the
  agent in an environment like CI's (the same container image) to skip
  tests.
- **A global rule fired** (`fallback`): for example a test-runner config or
  the policy changed.

## Merge gate for agent changes

`check` is advisory. The merge gate is the signed Verdict from CI, and for
pull requests from agent logins (`agent-authors` in the Action) it is
stricter: a change that passes by weakening its own tests (editing a test
together with the code it guards, removing tests or adding skip markers,
skipping an obligated test) is **blocked**, not sent for review. `check`
reports the same findings, so the agent can fix them before it opens the
pull request.

## Environment mode

The server finds the test environment itself: for pytest, `$QUOD_PYTHON`
or a `.venv` in the directory the tests were recorded from (or the
repository root); for Vitest and Jest, `$QUOD_JS` or
`node_modules/quod-js`. Without one, answers use path mode, which is
sound but runs more tests. `status` says which mode is on.
