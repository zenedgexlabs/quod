# Quod on unjs/ufo: a replay of 10 real commits

We replayed recent commits of [unjs/ufo](https://github.com/unjs/ufo) on a fork ([zenedgexlabs/ufo](https://github.com/zenedgexlabs/ufo/pulls?q=is%3Apr)), with [Quod](https://github.com/zenedgexlabs/quod) in CI. Every pull request ran two jobs on the same commit: the full suite (shadow), and only the tests Quod doesn't skip (enforce). Every other pull request also has an injected bug, to check that Quod still runs the tests that catch it.

| | |
|---|---|
| Pull requests scored | 10 of 10 |
| Tests Quod ran | 1,535 of 4,830 (32%) |
| ...of which pytest itself skips (skip and xfail markers) | 0 |
| Test step time, enforce vs full | 57 s vs 73 s (78%) |
| Failing tests in the full run | 41 |
| ...also failed in the enforce run | 41 |
| **Missed failures (escapes)** | **0** |

An escape is a test that failed in the full run but that Quod would have skipped. Times are the Quod step in each job (install, plan, tests, Verdict), as reported by GitHub in whole seconds.

Tests that pytest skips (a skip or xfail marker, a missing optional dependency) never pass while Quod records them, so Quod can't prove what they depend on and always keeps them. They are counted as run here, but pytest skips them at once, so they cost almost no time.

## Per pull request

| # | Change | Injected bug | Tests run (pytest-skipped) | Failing | Escapes | Enforce | Full |
|---|---|---|---|---|---|---|---|
| [1](https://github.com/zenedgexlabs/ufo/pull/1) | chore(deps): update all non-major dependencies (#312) | yes | 475 of 475 | 33 | 0 | 6 s | 7 s |
| [2](https://github.com/zenedgexlabs/ufo/pull/2) | fix(withBase, withoutBase): prevent false prefix matches (#313) |  | 32 of 483 | 0 | 0 | 6 s | 7 s |
| [3](https://github.com/zenedgexlabs/ufo/pull/3) | chore: update deps | yes | 483 of 483 | 2 | 0 | 6 s | 8 s |
| [4](https://github.com/zenedgexlabs/ufo/pull/4) | chore(release): v1.6.3 |  | 0 of 483 | 0 | 0 | 4 s | 6 s |
| [5](https://github.com/zenedgexlabs/ufo/pull/5) | chore(deps): update autofix-ci/action digest to 7a166d7 (#316) | yes | 12 of 483 | 6 | 0 | 6 s | 6 s |
| [6](https://github.com/zenedgexlabs/ufo/pull/6) | chore: update deps |  | 483 of 483 | 0 | 0 | 6 s | 7 s |
| [7](https://github.com/zenedgexlabs/ufo/pull/7) | chore(deps): update pnpm to v10.33.2 (#315) | yes | 12 of 483 | 0 | 0 | 5 s | 5 s |
| [8](https://github.com/zenedgexlabs/ufo/pull/8) | chore(deps): update codecov/codecov-action action to v6 (#325) |  | 0 of 483 | 0 | 0 | 5 s | 9 s |
| [9](https://github.com/zenedgexlabs/ufo/pull/9) | fix(withoutBase): collapse leading slashes (#335) | yes | 38 of 487 | 0 | 0 | 7 s | 12 s |
| [10](https://github.com/zenedgexlabs/ufo/pull/10) | chore(release): v1.6.4 |  | 0 of 487 | 0 | 0 | 6 s | 6 s |

## Trying it

Shadow mode changes nothing about your CI: it still runs every test, and adds
a job summary saying which tests Quod would have skipped and why. See the
[Action](https://github.com/marketplace/actions/quod-verdict) and
[supported versions and known limits](https://github.com/zenedgexlabs/quod/blob/main/docs/limits.md).

Reproduce: `python conformance/ci_replay.py --repo ufo --push` then `--collect`.
