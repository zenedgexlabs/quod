# Quod Verdict GitHub Action

Every pull request gets a **Verdict card** in the job summary and a signed
Verdict artifact (an in-toto Statement in a DSSE envelope). The Verdict
covers:

- the tests the change obligates, and a certificate for every test skipped;
- failures the change provably did not cause;
- changed code that no test executes;
- tests removed, skipped or edited together with the code they guard.

## Modes

| Mode | Where | What happens |
|---|---|---|
| `record` | push to the default branch | Runs the suite with the probe and caches the footprint map for that commit |
| `shadow` (default) | pull requests | The full suite still runs. The card shows what Quod would have skipped, and the job passes or fails exactly as before |
| `enforce` | pull requests | Certified tests are skipped, and the Verdict decides: failures the change didn't cause don't fail the job, and unmet obligations do |

Start in `shadow`. Switch to `enforce` once the shadow Verdicts have earned
your trust.

## Workflow (pytest)

```yaml
name: tests
on:
  push: { branches: [main] }
  pull_request:

jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with: { fetch-depth: 0 }          # Quod diffs against the map's commit
      - uses: actions/setup-python@v5
        with: { python-version: '3.13' }
      - run: pip install -e . quod-probe  # your deps + the pytest plugin
      - run: |                            # until releases are published
          cargo install --locked --git https://github.com/quodhq/quod quod-cli
      - uses: quodhq/quod/integrations/github-action@main
        with:
          mode: ${{ github.event_name == 'push' && 'record' || 'shadow' }}
          test-command: python -m pytest -q
          signing-key: ${{ secrets.QUOD_SIGNING_KEY }}
          python: python
```

Generate the key once with `quod keygen --out quod.seed`. Store the seed as
the `QUOD_SIGNING_KEY` secret. Publish the printed key ID, so anyone can
verify your Verdicts.

## Vitest

Set `runner: vitest`, set `probe-js` to the quod-js CLI, and put the Vitest
arguments in `test-command`.

## Trust

- **Signed Verdicts.** Only jobs with the secret (trusted CI) produce signed
  Verdicts. On forks the secret is absent, so the output is advisory.
- **Map writes.** Only `record` runs on the default branch write the map
  cache. Pull requests only read it.
- **Agent authors.** Logins listed in `agent-authors` are recorded as agent
  authors in the Verdict.
