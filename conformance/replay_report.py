"""Renders an E9 replay's results (results/e9-<repo>.json) as a page for the
project's maintainers: what Quod would have skipped on their real history,
whether it missed anything, and links to every pull request and run on the
fork.

    python replay_report.py --repo networkx > ../docs/replays/networkx.md
"""

from __future__ import annotations

import argparse
import json
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def pct(a: float, b: float) -> str:
    return f"{100 * a / b:.0f}%" if b else "-"


def render(repo: str, data: dict, cfg: dict) -> str:
    s = data["summary"]
    fork = s["fork"]
    upstream = cfg["url"].removeprefix("https://github.com/").removesuffix(".git")
    scored = [x for x in data["steps"] if x["has_artifacts"]]
    tests = sum(x["tests"] for x in scored)
    run = sum(x["tests"] - x["would_skip"] for x in scored)
    t_shadow = sum(x["time_shadow"] or 0 for x in scored)
    t_enforce = sum(x["time_enforce"] or 0 for x in scored)
    failing = sum(x["failing"] for x in scored)
    caught = sum(x["caught_enforce"] for x in scored)
    escapes = sum(len(x["escapes"]) for x in scored)

    out = [
        f"# Quod on {upstream}: a replay of {len(data['steps'])} real commits",
        "",
        f"We replayed recent commits of [{upstream}](https://github.com/{upstream}) on a fork "
        f"([{fork}](https://github.com/{fork}/pulls?q=is%3Apr)), with "
        "[Quod](https://github.com/zenedgexlabs/quod) in CI. Every pull request ran "
        "two jobs on the same commit: the full suite (shadow), and only the tests Quod "
        "doesn't skip (enforce). Every other pull request also has an injected bug, to "
        "check that Quod still runs the tests that catch it.",
        "",
        "| | |",
        "|---|---|",
        f"| Pull requests scored | {len(scored)} of {len(data['steps'])} |",
        f"| Tests Quod ran | {run:,} of {tests:,} ({pct(run, tests)}) |",
        f"| Test step time, enforce vs full | {t_enforce:.0f} s vs {t_shadow:.0f} s ({pct(t_enforce, t_shadow)}) |",
        f"| Failing tests in the full run | {failing} |",
        f"| ...also failed in the enforce run | {caught} |",
        f"| **Missed failures (escapes)** | **{escapes}** |",
        "",
        "An escape is a test that failed in the full run but that Quod would have "
        "skipped. Times are the Quod step in each job (install, plan, tests, Verdict), "
        "as reported by GitHub in whole seconds.",
        "",
        "## Per pull request",
        "",
        "| # | Change | Injected bug | Tests run | Failing | Escapes | Enforce | Full |",
        "|---|---|---|---|---|---|---|---|",
    ]
    for x in data["steps"]:
        title = x["title"].split(": ", 1)[-1].replace("|", "\\|")
        if len(title) > 70:
            title = title[:67] + "..."
        link = f"[{x['step']}](https://github.com/{fork}/pull/{x['pr']})"
        bug = "yes" if x["step"] % 2 == 1 else ""  # ci_replay.py mutates odd steps
        if not x["has_artifacts"]:
            out.append(f"| {link} | {title} | {bug} | not scored (the run timed out or was cancelled) | | | | |")
            continue
        ran = x["tests"] - x["would_skip"]
        out.append(
            f"| {link} | {title} | {bug} | {ran:,} of {x['tests']:,} | {x['failing']} | {len(x['escapes'])} | "
            f"{x['time_enforce'] or 0:.0f} s | {x['time_shadow'] or 0:.0f} s |"
        )
    out += [
        "",
        "## Trying it",
        "",
        "Shadow mode changes nothing about your CI: it still runs every test, and adds",
        "a job summary saying which tests Quod would have skipped and why. See the",
        "[Action](https://github.com/marketplace/actions/quod-verdict) and",
        "[supported versions and known limits](https://github.com/zenedgexlabs/quod/blob/main/docs/limits.md).",
        "",
        f"Reproduce: `python conformance/ci_replay.py --repo {repo} --push` then `--collect`.",
        "",
    ]
    return "\n".join(out)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--repo", required=True)
    args = ap.parse_args()
    data = json.loads((ROOT / "conformance" / "results" / f"e9-{args.repo}.json").read_text(encoding="utf-8"))
    cfg = tomllib.loads((ROOT / "conformance" / "repos.toml").read_text(encoding="utf-8"))["repos"][args.repo]
    sys.stdout.write(render(args.repo, data, cfg))
    return 0


if __name__ == "__main__":
    sys.exit(main())
