# Quod on networkx/networkx: a replay of 10 real commits

We replayed recent commits of [networkx/networkx](https://github.com/networkx/networkx) on a fork ([zenedgexlabs/networkx](https://github.com/zenedgexlabs/networkx/pulls?q=is%3Apr)), with [Quod](https://github.com/zenedgexlabs/quod) in CI. Every pull request ran two jobs on the same commit: the full suite (shadow), and only the tests Quod doesn't skip (enforce). Every other pull request also has an injected bug, to check that Quod still runs the tests that catch it.

| | |
|---|---|
| Pull requests scored | 10 of 10 |
| Tests Quod ran | 19,524 of 102,703 (19%) |
| ...of which pytest itself skips (skip and xfail markers) | 8,160 |
| Test step time, enforce vs full | 374 s vs 1321 s (28%) |
| ...on the 5 commits without an injected bug | 225 s vs 633 s (36%) |
| Failing tests in the full run | 8 |
| ...also failed in the enforce run | 8 |
| **Missed failures (escapes)** | **0** |

An escape is a test that failed in the full run but that Quod would have skipped. Times are the Quod step in each job (install, plan, tests, Verdict), as reported by GitHub in whole seconds.

Tests that pytest skips (a skip or xfail marker, a missing optional dependency) never pass while Quod records them, so Quod can't prove what they depend on and always keeps them. They are counted as run here, but pytest skips them at once, so they cost almost no time.

## Per pull request

| # | Change | Injected bug | Tests run (pytest-skipped) | Failing | Escapes | Enforce | Full |
|---|---|---|---|---|---|---|---|
| [1](https://github.com/zenedgexlabs/networkx/pull/1) | Cache osmnx responses in github actions, like in circleci builds (#... | yes | 1,014 of 10,264 (816) | 0 | 0 | 36 s | 130 s |
| [2](https://github.com/zenedgexlabs/networkx/pull/2) | Speed up chain_decomposition and bridges 2.5x to 4x by dropping the... |  | 1,055 of 10,274 (816) | 0 | 0 | 30 s | 112 s |
| [3](https://github.com/zenedgexlabs/networkx/pull/3) | TST: Make Infomap C++ comparison tests robust to ragged hierarchies... | yes | 1,015 of 10,274 (816) | 3 | 0 | 28 s | 159 s |
| [4](https://github.com/zenedgexlabs/networkx/pull/4) | Bugfix kosaraku (#9021) |  | 1,022 of 10,275 (816) | 0 | 0 | 28 s | 119 s |
| [5](https://github.com/zenedgexlabs/networkx/pull/5) | TST: Unseed chains test. (#9028) | yes | 1,019 of 10,266 (816) | 2 | 0 | 31 s | 153 s |
| [6](https://github.com/zenedgexlabs/networkx/pull/6) | Update milestone-merged-prs.yaml (#9038) |  | 1,008 of 10,266 (816) | 0 | 0 | 29 s | 103 s |
| [7](https://github.com/zenedgexlabs/networkx/pull/7) | ENH: Consistent node location in bipartite_layout (#8982) (#8984) | yes | 1,095 of 10,271 (816) | 1 | 0 | 28 s | 137 s |
| [8](https://github.com/zenedgexlabs/networkx/pull/8) | Add zizmor pre-commit hook configuration (#9033) |  | 1,008 of 10,271 (816) | 0 | 0 | 27 s | 149 s |
| [9](https://github.com/zenedgexlabs/networkx/pull/9) | Bump prefix-dev/setup-pixi from 0.11.2 to 0.11.3 in the actions gro... | yes | 1,017 of 10,271 (816) | 2 | 0 | 26 s | 109 s |
| [10](https://github.com/zenedgexlabs/networkx/pull/10) | TST,CONF: Prevent process oversubsciption. (#9043) |  | 10,271 of 10,271 (816) | 0 | 0 | 111 s | 150 s |

## Trying it

Shadow mode changes nothing about your CI: it still runs every test, and adds
a job summary saying which tests Quod would have skipped and why. See the
[Action](https://github.com/marketplace/actions/quod-verdict) and
[supported versions and known limits](https://github.com/zenedgexlabs/quod/blob/main/docs/limits.md).

Reproduce: `python conformance/ci_replay.py --repo networkx --push` then `--collect`.
