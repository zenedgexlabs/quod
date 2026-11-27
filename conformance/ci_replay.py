"""E9: Quod on GitHub-hosted CI, replaying real history on a public fork.

The fork's `main` is the commit E8 records its map at (G), plus one commit
that swaps the project's workflows for Quod's (conformance/e9/*.yml). That
push records the map on GitHub's runners. Then, for each of the next N real
first-parent commits C, a branch on top of `main` gets C's exact tree (with
Quod's workflow), plus a mutant on odd steps, and a pull request; then C
(without the mutant) lands on `main`, which records the next map, as CI
does after a merge. Every pull request runs two jobs on the same commit:

- shadow:  the full suite, and the tests Quod would skip;
- enforce: only the tests Quod doesn't skip.

Scoring (`--collect`): an escape is a test Quod would skip in shadow mode
that failed in shadow mode's full run. Time is the Quod step's duration in
each job (install, plan, tests, Verdict): what a user's CI pays.

    python ci_replay.py --repo more-itertools --push
    python ci_replay.py --repo more-itertools --collect
"""

from __future__ import annotations

import argparse
import json
import random
import shutil
import sys
import time
import tomllib
from datetime import datetime
from pathlib import Path

import culprit as E7
import replay as R

E9 = R.ROOT / "conformance" / "e9"


def gh(*args: str, check: bool = True):
    return R.sh(["gh", *args], check=check)


def upstream_slug(cfg: dict) -> str:
    return cfg["url"].removeprefix("https://github.com/").removesuffix(".git")


def workflow_for(cfg: dict) -> Path:
    return E9 / ("quod-pytest.yml" if cfg.get("runner", "pytest") == "pytest" else "quod-vitest.yml")


def with_quod_workflow(ws: R.Workspace, cfg: dict) -> None:
    """Replaces the project's workflows with Quod's (theirs would run on the
    fork too, and use its minutes)."""
    wf = ws.repo / ".github" / "workflows"
    if wf.exists():
        shutil.rmtree(wf)
    wf.mkdir(parents=True)
    shutil.copy(workflow_for(cfg), wf / "quod.yml")
    ws.git("add", "-A", ".github")


def commit(ws: R.Workspace, message: str) -> str:
    ws.git("-c", "user.name=quod-e9", "-c", "user.email=e9@users.noreply.github.com",
           "commit", "-q", "--allow-empty", "-m", message)
    return ws.git("rev-parse", "HEAD").stdout.strip()


def push(args, cfg: dict, ws: R.Workspace, fork: str, good: str, window: list[str]) -> None:
    if gh("repo", "view", fork, check=False).returncode != 0:
        R.log(f"forking {upstream_slug(cfg)} to {fork}")
        gh("repo", "fork", upstream_slug(cfg), "--clone=false", "--default-branch-only")
        time.sleep(5)
    gh("api", "-X", "PUT", f"repos/{fork}/actions/permissions", "-F", "enabled=true", "-f", "allowed_actions=all")
    url = f"https://github.com/{fork}.git"
    if ws.git("remote", "get-url", "fork", check=False).returncode != 0:
        ws.git("remote", "add", "fork", url)

    ws.checkout(good)
    with_quod_workflow(ws, cfg)
    base = commit(ws, f"E9: Quod workflow on {good[:10]}")
    ws.git("push", "-q", "--force", "fork", f"{base}:refs/heads/main")
    R.log(f"[{fork}] main = {base[:8]} (upstream {good[:8]} + workflow); recording the map")
    wait_for_run(fork, base)

    rng = random.Random(args.seed)
    for k, c in enumerate(window, 1):
        ws.checkout(base)
        # C's exact tree (deletions included), then Quod's workflow again.
        ws.git("read-tree", "-u", "--reset", c)
        with_quod_workflow(ws, cfg)
        desc = ""
        if k % 2 == 1:
            files = E7.mutate_files(ws)
            for _ in range(30):
                path = rng.choice(files)
                sites = E7.sites_for(path)
                if sites:
                    desc = R.apply_mutant(path, rng.choice(sites))
                    break
            ws.git("add", "-A")
        subject = ws.git("log", "-1", "--format=%s", c).stdout.strip()
        sha = commit(ws, f"E9 step {k}: {subject}" + (f" + mutant {desc}" if desc else ""))
        branch = f"e9-{k}"
        ws.git("push", "-q", "--force", "fork", f"{sha}:refs/heads/{branch}")
        body = f"Replays upstream {c} on the map's commit" + (f", plus the mutant `{desc}`." if desc else ".")
        gh("pr", "create", "-R", fork, "--base", "main", "--head", branch,
           "--title", f"E9 step {k}: {subject}"[:120], "--body", body, check=False)
        R.log(f"  step {k} {c[:8]}{' + ' + desc if desc else ''}: {branch} @ {sha[:8]}")

        # As in real CI, the change (without the mutant) lands on main, which
        # records the map the next pull request is compared with. The pull
        # request's merge base stays `base`, whose map is already recorded.
        ws.checkout(base)
        ws.git("read-tree", "-u", "--reset", c)
        with_quod_workflow(ws, cfg)
        base = commit(ws, f"E9 main: {subject}")
        ws.git("push", "-q", "--force", "fork", f"{base}:refs/heads/main")
        R.log(f"    main = {base[:8]} (upstream {c[:8]}); recording the map")
        wait_for_run(fork, base)


def runs_for(fork: str, sha: str) -> list[dict]:
    out = gh("run", "list", "-R", fork, "--commit", sha, "--json", "databaseId,status,conclusion,event",
             check=False).stdout
    return json.loads(out or "[]")


def wait_for_run(fork: str, sha: str, timeout: float = 3600) -> dict:
    t0 = time.monotonic()
    while time.monotonic() - t0 < timeout:
        runs = runs_for(fork, sha)
        if runs and all(r["status"] == "completed" for r in runs):
            return runs[0]
        time.sleep(20)
    raise RuntimeError(f"no completed run for {sha} after {timeout:.0f}s")


def seconds(a: str, b: str) -> float:
    f = "%Y-%m-%dT%H:%M:%SZ"
    return (datetime.strptime(b, f) - datetime.strptime(a, f)).total_seconds()


def collect(args, fork: str) -> dict:
    out_dir = R.ROOT / "conformance" / "results" / f"e9-{args.repo}"
    out_dir.mkdir(parents=True, exist_ok=True)
    prs = json.loads(gh("pr", "list", "-R", fork, "--state", "all", "--limit", "100",
                        "--json", "number,headRefName,headRefOid,title").stdout)
    steps = []
    for pr in sorted(prs, key=lambda p: int(p["headRefName"].split("-")[1]) if p["headRefName"].startswith("e9-") else 0):
        if not pr["headRefName"].startswith("e9-"):
            continue
        k = int(pr["headRefName"].split("-")[1])
        run = wait_for_run(fork, pr["headRefOid"])
        rid = str(run["databaseId"])
        d = out_dir / f"step-{k}"
        if d.exists():
            shutil.rmtree(d)
        gh("run", "download", rid, "-R", fork, "-D", str(d), check=False)
        jobs = json.loads(gh("run", "view", rid, "-R", fork, "--json", "jobs").stdout)["jobs"]
        times = {}
        for j in jobs:
            for st in j.get("steps", []):
                if "quod" in st["name"].lower() and st.get("startedAt") and st.get("completedAt"):
                    times[j["name"]] = seconds(st["startedAt"], st["completedAt"])
        def results(name: str) -> dict:
            f = d / name / "results.jsonl"
            if not f.exists():
                return {}
            return {r["name"]: r["outcome"] for r in map(json.loads, f.read_text(encoding="utf-8").splitlines())
                    if r.get("type") == "result"}
        shadow = results("quod-verdict-shadow")
        enforce = results("quod-verdict")
        skip_file = d / "quod-verdict-shadow" / "skip.txt"
        would_skip = {l.strip() for l in skip_file.read_text(encoding="utf-8").splitlines()
                      if l.strip() and not l.startswith("#")} if skip_file.exists() else set()
        failing = {n for n, o in shadow.items() if o == "failed"}
        s = {
            "step": k,
            "pr": pr["number"],
            "title": pr["title"],
            "run": rid,
            "conclusion": run["conclusion"],
            "tests": len(shadow),
            "would_skip": len(would_skip),
            "ran_enforce": len(enforce),
            "failing": len(failing),
            "caught_enforce": len(failing & {n for n, o in enforce.items() if o == "failed"}),
            "escapes": sorted(failing & would_skip),
            "time_shadow": times.get("shadow"),
            "time_enforce": times.get("enforce"),
            "has_artifacts": bool(shadow) and skip_file.exists(),
        }
        steps.append(s)
        R.log(f"  step {k}: {s['tests'] - s['would_skip']}/{s['tests']} obligated, enforce ran {s['ran_enforce']}, "
              f"failing {s['failing']}, escapes {len(s['escapes'])}, quod step {s['time_enforce']}s vs {s['time_shadow']}s")
    scored = [s for s in steps if s["has_artifacts"]]
    summary = {
        "repo": args.repo,
        "fork": fork,
        "steps": len(steps),
        "scored_steps": len(scored),
        "escapes": sum(len(s["escapes"]) for s in scored),
        "failing_tests": sum(s["failing"] for s in scored),
        "tests_fraction": round(sum(s["tests"] - s["would_skip"] for s in scored) / max(1, sum(s["tests"] for s in scored)), 3),
        "time_fraction": round(sum(s["time_enforce"] or 0 for s in scored) / max(1e-9, sum(s["time_shadow"] or 0 for s in scored)), 3),
    }
    out = R.ROOT / "conformance" / "results" / f"e9-{args.repo}.json"
    out.write_text(json.dumps({"summary": summary, "steps": steps}, indent=2), encoding="utf-8")
    print(json.dumps(summary, indent=2))
    return summary


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--repo", required=True)
    ap.add_argument("--window", type=int, default=10)
    ap.add_argument("--seed", type=int, default=5)
    ap.add_argument("--owner", default="zenedgexlabs")
    ap.add_argument("--python", default="3.13")
    ap.add_argument("--work", type=Path, default=R.ROOT / "conformance" / ".work")
    ap.add_argument("--push", action="store_true")
    ap.add_argument("--collect", action="store_true")
    args = ap.parse_args()

    cfg = tomllib.loads((R.ROOT / "conformance" / "repos.toml").read_text(encoding="utf-8"))["repos"][args.repo]
    fork = f"{args.owner}/{upstream_slug(cfg).split('/')[1]}"
    if args.push:
        ws = R.setup(args.repo, cfg, args.work / args.repo, args.python)
        branch = cfg["branch"]
        ws.git("fetch", "-q", "origin", branch, check=False)
        commits = ws.git("log", "--first-parent", "--format=%H", f"-n{args.window + 1}",
                         f"origin/{branch}").stdout.split()[::-1]
        push(args, cfg, ws, fork, commits[0], commits[1:])
    if args.collect:
        summary = collect(args, fork)
        return 0 if summary["escapes"] == 0 else 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
