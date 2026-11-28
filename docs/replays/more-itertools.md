# Quod on more-itertools/more-itertools: a replay of 10 real commits

We replayed recent commits of [more-itertools/more-itertools](https://github.com/more-itertools/more-itertools) on a fork ([zenedgexlabs/more-itertools](https://github.com/zenedgexlabs/more-itertools/pulls?q=is%3Apr)), with [Quod](https://github.com/zenedgexlabs/quod) in CI. Every pull request ran two jobs on the same commit: the full suite (shadow), and only the tests Quod doesn't skip (enforce). Every other pull request also has an injected bug, to check that Quod still runs the tests that catch it.

| | |
|---|---|
| Pull requests scored | 9 of 10 |
| Tests Quod ran | 2,090 of 6,940 (30%) |
| ...of which pytest itself skips (skip and xfail markers) | 0 |
| Test step time, enforce vs full | 931 s vs 976 s (95%) |
| ...on the 5 commits without an injected bug | 50 s vs 149 s (34%) |
| Failing tests in the full run | 24 |
| ...also failed in the enforce run | 24 |
| **Missed failures (escapes)** | **0** |

An escape is a test that failed in the full run but that Quod would have skipped. Times are the Quod step in each job (install, plan, tests, Verdict), as reported by GitHub in whole seconds.

Tests that pytest skips (a skip or xfail marker, a missing optional dependency) never pass while Quod records them, so Quod can't prove what they depend on and always keeps them. They are counted as run here, but pytest skips them at once, so they cost almost no time.

## Per pull request

| # | Change | Injected bug | Tests run (pytest-skipped) | Failing | Escapes | Enforce | Full |
|---|---|---|---|---|---|---|---|
| [1](https://github.com/zenedgexlabs/more-itertools/pull/1) | Merge pull request #1342 from feiiiiii5/more-itertools-C-211 | yes | 769 of 769 | 6 | 0 | 848 s | 753 s |
| [2](https://github.com/zenedgexlabs/more-itertools/pull/2) | Merge pull request #1345 from chengzhudejing/fix/numeric-range-larg... |  | 626 of 771 | 0 | 0 | 14 s | 16 s |
| [3](https://github.com/zenedgexlabs/more-itertools/pull/3) | Merge pull request #1346 from abhishekcpp007/fix-concurrent-tee-ser... | yes | not scored (the run timed out or was cancelled) | | | | |
| [4](https://github.com/zenedgexlabs/more-itertools/pull/4) | Merge pull request #1349 from Dhairya-007/fix-generator-return-types |  | 0 of 771 | 0 | 0 | 9 s | 23 s |
| [5](https://github.com/zenedgexlabs/more-itertools/pull/5) | Merge pull request #1356 from saadkhantareen/docs/set-partitions-em... | yes | 15 of 771 | 5 | 0 | 9 s | 29 s |
| [6](https://github.com/zenedgexlabs/more-itertools/pull/6) | Merge pull request #1363 from ravikumar021/codex/docstring-exceptio... |  | 15 of 771 | 0 | 0 | 7 s | 18 s |
| [7](https://github.com/zenedgexlabs/more-itertools/pull/7) | Merge pull request #1355 from IAMVENEETH/fix-outer-product-empty-ys | yes | 7 of 771 | 1 | 0 | 15 s | 26 s |
| [8](https://github.com/zenedgexlabs/more-itertools/pull/8) | Merge pull request #1311 from dualfroz/fix/all-unique-falsy-key |  | 627 of 772 | 0 | 0 | 13 s | 72 s |
| [9](https://github.com/zenedgexlabs/more-itertools/pull/9) | Merge pull request #1366 from Tibefox/codex/sample-api-docs | yes | 31 of 772 | 12 | 0 | 9 s | 19 s |
| [10](https://github.com/zenedgexlabs/more-itertools/pull/10) | Merge branch 'mau5trobe-docs/running-focused-tests' |  | 0 of 772 | 0 | 0 | 7 s | 20 s |

## Trying it

Shadow mode changes nothing about your CI: it still runs every test, and adds
a job summary saying which tests Quod would have skipped and why. See the
[Action](https://github.com/marketplace/actions/quod-verdict) and
[supported versions and known limits](https://github.com/zenedgexlabs/quod/blob/main/docs/limits.md).

Reproduce: `python conformance/ci_replay.py --repo more-itertools --push` then `--collect`.
