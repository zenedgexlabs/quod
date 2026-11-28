# Quod on networkx/networkx: a replay of 10 real commits

We replayed recent commits of [networkx/networkx](https://github.com/networkx/networkx) on a fork ([zenedgexlabs/networkx](https://github.com/zenedgexlabs/networkx/pulls?q=is%3Apr)), with [Quod](https://github.com/zenedgexlabs/quod) in CI. Every pull request ran two jobs on the same commit: the full suite (shadow), and only the tests Quod doesn't skip (enforce). Every other pull request also has an injected bug, to check that Quod still runs the tests that catch it.

| | |
|---|---|
| Pull requests scored | 10 of 10 |
| Tests Quod ran | 43,608 of 92,413 (47%) |
| ...of which pytest itself skips (skip and xfail markers) | 10,860 |
| Test step time, enforce vs full | 552 s vs 1028 s (54%) |
| Failing tests in the full run | 8 |
| ...also failed in the enforce run | 8 |
| **Missed failures (escapes)** | **0** |

An escape is a test that failed in the full run but that Quod would have skipped. Times are the Quod step in each job (install, plan, tests, Verdict), as reported by GitHub in whole seconds.

Tests that pytest skips (a skip or xfail marker, a missing optional dependency) never pass while Quod records them, so Quod can't prove what they depend on and always keeps them. They are counted as run here, but pytest skips them at once, so they cost almost no time.

## Per pull request

| # | Change | Injected bug | Tests run (pytest-skipped) | Failing | Escapes | Enforce | Full |
|---|---|---|---|---|---|---|---|
| [1](https://github.com/zenedgexlabs/networkx/pull/1) | Cache osmnx responses in github actions, like in circleci builds (#... | yes | 9,237 of 9,237 (1,086) | 0 | 0 | 131 s | 110 s |
| [2](https://github.com/zenedgexlabs/networkx/pull/2) | Speed up chain_decomposition and bridges 2.5x to 4x by dropping the... |  | 1,133 of 9,247 (1,086) | 0 | 0 | 28 s | 106 s |
| [3](https://github.com/zenedgexlabs/networkx/pull/3) | TST: Make Infomap C++ comparison tests robust to ragged hierarchies... | yes | 1,154 of 9,247 (1,086) | 3 | 0 | 26 s | 103 s |
| [4](https://github.com/zenedgexlabs/networkx/pull/4) | Bugfix kosaraku (#9021) |  | 1,100 of 9,248 (1,086) | 0 | 0 | 22 s | 110 s |
| [5](https://github.com/zenedgexlabs/networkx/pull/5) | TST: Unseed chains test. (#9028) | yes | 9,239 of 9,239 (1,086) | 2 | 0 | 81 s | 127 s |
| [6](https://github.com/zenedgexlabs/networkx/pull/6) | Update milestone-merged-prs.yaml (#9038) |  | 1,086 of 9,239 (1,086) | 0 | 0 | 31 s | 103 s |
| [7](https://github.com/zenedgexlabs/networkx/pull/7) | ENH: Consistent node location in bipartite_layout (#8982) (#8984) | yes | 9,239 of 9,239 (1,086) | 1 | 0 | 85 s | 101 s |
| [8](https://github.com/zenedgexlabs/networkx/pull/8) | Add zizmor pre-commit hook configuration (#9033) |  | 1,086 of 9,239 (1,086) | 0 | 0 | 26 s | 80 s |
| [9](https://github.com/zenedgexlabs/networkx/pull/9) | Bump prefix-dev/setup-pixi from 0.11.2 to 0.11.3 in the actions gro... | yes | 1,095 of 9,239 (1,086) | 2 | 0 | 22 s | 81 s |
| [10](https://github.com/zenedgexlabs/networkx/pull/10) | TST,CONF: Prevent process oversubsciption. (#9043) |  | 9,239 of 9,239 (1,086) | 0 | 0 | 100 s | 107 s |

## Trying it

Shadow mode changes nothing about your CI: it still runs every test, and adds
a job summary saying which tests Quod would have skipped and why. See the
[Action](https://github.com/marketplace/actions/quod-verdict) and
[supported versions and known limits](https://github.com/zenedgexlabs/quod/blob/main/docs/limits.md).

Reproduce: `python conformance/ci_replay.py --repo networkx --push` then `--collect`.
