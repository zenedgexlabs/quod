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
| `record` | push to the default branch | Runs the suite with the probe and caches the footprint map for that commit. A green commit is remembered; when a push turns main red, the job summary names the commit that broke each failing test (`quod culprit`) |
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
        with: { python-version: '3.13' }  # the probe needs Python 3.12+
      - run: pip install -e .[test]       # your project and its test deps
      - uses: zenedgexlabs/quod/integrations/github-action@v0.1.0-alpha.2
        with:
          mode: ${{ github.event_name == 'push' && 'record' || 'shadow' }}
          test-command: python -m pytest -q
          python: python
          signing-key: ${{ secrets.QUOD_SIGNING_KEY }}
```

The action downloads `quod` and the matching `quod-probe` from the same
release (`quod-version`, default `latest`) and checks the binary's
SHA-256. Pin the action to a release tag.

Generate the signing key once with `quod keygen --out quod.seed`. Store the
seed as the `QUOD_SIGNING_KEY` secret, and publish the printed key ID, so
anyone can verify your Verdicts. Without a key (for example on forks), the
Verdict is advisory.

## Vitest and Jest

```yaml
      - uses: actions/setup-node@v4
        with: { node-version: 22 }
      - run: npm ci
      - uses: zenedgexlabs/quod/integrations/github-action@v0.1.0-alpha.2
        with:
          mode: ${{ github.event_name == 'push' && 'record' || 'shadow' }}
          runner: vitest                  # or jest
          test-command: ''                # extra arguments for the runner
```

`quod-js` is installed into its own directory, so your `node_modules` (npm,
pnpm or yarn) is left as it is.

## What you get per pull request

- The job summary: the Verdict card. In shadow mode it also says how many
  tests Quod would have skipped.
- The `quod-verdict` artifact: the signed Verdict, the plan (every test run
  or skipped, with its reason), the skip list and the test results.

## Trust

- **Signed Verdicts.** Only jobs with the secret (trusted CI) produce signed
  Verdicts. On forks the secret is absent, so the output is advisory.
- **Map writes.** Only `record` runs on the default branch write the map
  cache. Pull requests only read it.
- **Agent authors.** Logins listed in `agent-authors` are recorded as agent
  authors in the Verdict, and their changes are held to a stricter bar: a
  test edited together with the code it guards (I001), tests removed or
  skip markers added (R011), or an obligated test skipped by the runner
  (O002) block the merge instead of asking for review. Set `agent_block`
  in `quod.toml` to change the list (add `U001` to also block
  untested agent changes; `[]` turns it off). The verifier rejects a
  Verdict whose agent findings were lowered.
