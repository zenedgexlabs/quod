# ADR 017: Agents get advisory Verdicts; only trusted CI seals proof

Status: Accepted (extends ADR 004)

## Context

Coding agents need fast answers while they edit: which tests this edit
obligates, why a test runs, whether their results look sound. An agent
controls its sandbox: the files, the map copy and the results. Research
documents agents editing and deleting tests to make them pass. Anything
computed in the sandbox is therefore the agent's claim, not evidence.

## Decision

- **`quod mcp`** (MCP over stdio) exposes `tests_for`, `why` and `check`.
  It always runs selection with `advisory: true` and no key. Responses
  say they are advisory.
- **Advisory and sealed Verdicts are distinct.** An advisory Verdict is
  never signed, even if a key is available. `quod-verify` and
  `quod_verdict::verify` reject advisory Verdicts wherever proof is
  required.
- **Only jobs holding the signing secret mint signed certificates and
  Verdicts**, which in the GitHub Action means trusted CI. Fork PRs lack the
  secret and get advisory output.
- **Obligations are checked against results.** An obligated test missing
  from the results blocks the Verdict (O001). So an agent can't pass by not
  running a test.

## Consequences

- The inner loop stays offline and fast. Trust comes only from CI, which
  recomputes everything from the pinned inputs.
- A sandbox that also holds the CI signing key breaks the model. The Action
  documentation keeps the key in CI secrets only.
