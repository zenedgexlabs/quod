# Quod on pylint-dev/pylint: a replay of 10 real commits

We replayed recent commits of [pylint-dev/pylint](https://github.com/pylint-dev/pylint) on a fork ([zenedgexlabs/pylint](https://github.com/zenedgexlabs/pylint/pulls?q=is%3Apr)), with [Quod](https://github.com/zenedgexlabs/quod) in CI. Every pull request ran two jobs on the same commit: the full suite (shadow), and only the tests Quod doesn't skip (enforce). Every other pull request also has an injected bug, to check that Quod still runs the tests that catch it.

| | |
|---|---|
| Pull requests scored | 10 of 10 |
| Tests Quod ran | 26,464 of 27,228 (97%) |
| ...of which pytest itself skips (skip and xfail markers) | 2,846 |
| Test step time, enforce vs full | 1525 s vs 1414 s (108%) |
| ...on the 5 commits without an injected bug | 702 s vs 731 s (96%) |
| Failing tests in the full run | 2 |
| ...also failed in the enforce run | 2 |
| **Missed failures (escapes)** | **0** |

An escape is a test that failed in the full run but that Quod would have skipped. Times are the Quod step in each job (install, plan, tests, Verdict), as reported by GitHub in whole seconds.

Tests that pytest skips (a skip or xfail marker, a missing optional dependency) never pass while Quod records them, so Quod can't prove what they depend on and always keeps them. They are counted as run here, but pytest skips them at once, so they cost almost no time.

## Per pull request

| # | Change | Injected bug | Tests run (pytest-skipped) | Failing | Escapes | Enforce | Full |
|---|---|---|---|---|---|---|---|
| [1](https://github.com/zenedgexlabs/pylint/pull/1) | Check that only_required_for_messages names registered messages | yes | 2,709 of 2,709 (283) | 0 | 0 | 144 s | 152 s |
| [2](https://github.com/zenedgexlabs/pylint/pull/2) | Run the design checker's function visit and leave under the same me... |  | 2,710 of 2,710 (283) | 0 | 0 | 151 s | 129 s |
| [3](https://github.com/zenedgexlabs/pylint/pull/3) | Benchmark pylint's startup in CodSpeed simulation mode (#11853) | yes | 2,711 of 2,711 (283) | 0 | 0 | 146 s | 123 s |
| [4](https://github.com/zenedgexlabs/pylint/pull/4) | Replace the pandas example of unnecessary-lambda with a map example... |  | 2,459 of 2,711 (283) | 0 | 0 | 128 s | 140 s |
| [5](https://github.com/zenedgexlabs/pylint/pull/5) | Fix Windows initializer path classification (#11615) | yes | 2,727 of 2,727 (285) | 1 | 0 | 186 s | 162 s |
| [6](https://github.com/zenedgexlabs/pylint/pull/6) | Improve arguments-differ message when parameter totals match (#11132) |  | 2,728 of 2,728 (285) | 0 | 0 | 181 s | 158 s |
| [7](https://github.com/zenedgexlabs/pylint/pull/7) | Fix a crash when the output stream cannot encode a message (#11672) | yes | 2,733 of 2,733 (286) | 1 | 0 | 155 s | 126 s |
| [8](https://github.com/zenedgexlabs/pylint/pull/8) | [pre-commit.ci] pre-commit autoupdate (#11868) |  | 2,477 of 2,733 (286) | 0 | 0 | 119 s | 154 s |
| [9](https://github.com/zenedgexlabs/pylint/pull/9) | [primer] Query for PR number without using an artifact (#11877) | yes | 2,733 of 2,733 (286) | 0 | 0 | 192 s | 120 s |
| [10](https://github.com/zenedgexlabs/pylint/pull/10) | [primer] Unzip single files from artifacts in comment workflow |  | 2,477 of 2,733 (286) | 0 | 0 | 123 s | 150 s |

## Trying it

Shadow mode changes nothing about your CI: it still runs every test, and adds
a job summary saying which tests Quod would have skipped and why. See the
[Action](https://github.com/marketplace/actions/quod-verdict) and
[supported versions and known limits](https://github.com/zenedgexlabs/quod/blob/main/docs/limits.md).

Reproduce: `python conformance/ci_replay.py --repo pylint --push` then `--collect`.
