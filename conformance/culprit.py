"""E7: Culprit attribution on real history.

For each trial:

1. Record a map at a real commit G (the last green commit).
2. Build a synthetic first-parent branch on G that replays the next W real
   commits' trees in order. A mutant goes in as its own commit at a random
   position p, and every later commit carries it, so only the mutant
   commit introduces it.
3. Run the suite at the mutated end and at the clean end (the same real
   commit, no mutant). Tests that fail only with the mutant, and fail alone
   three times, were broken by the mutant commit M.
4. Run `quod culprit` (map at G, good = G, bad = mutated end) on them.

Soundness gate: M is a suspect for every such test, unless the answer is
`unknown`. Precision: how often M is named alone (`culprit`), and the mean
number of suspects.

Usage: culprit.py --repo NAME [--trials 6] [--window 5] [--seed 11]
"""

from __future__ import annotations

import argparse
import json
import os
import random
import re
import sys
import tomllib
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import replay as R  # noqa: E402


def commit_tree(ws: R.Workspace, parent: str, msg: str) -> str:
    """Commits the working tree on top of `parent` without moving HEAD's branch."""
    ws.git("add", "-A")
    tree = ws.git("write-tree").stdout.strip()
    env = {**os.environ, "GIT_AUTHOR_NAME": "e7", "GIT_AUTHOR_EMAIL": "e7@example.com",
           "GIT_COMMITTER_NAME": "e7", "GIT_COMMITTER_EMAIL": "e7@example.com"}
    out = R.sh(["git", "-C", ws.repo, "commit-tree", tree, "-p", parent, "-m", msg], env=env)
    return out.stdout.strip()


def mutate_files(ws: R.Workspace) -> list[Path]:
    return sorted({p for pat in ws.cfg["mutate"] for p in ws.repo.glob(pat) if p.is_file()
                   and not re.search(r"\.(test|spec|test-d|bench)\.|\.d\.ts$", p.name)})


def sites_for(path: Path) -> list:
    text = path.read_text(encoding="utf-8")
    js = path.suffix in (".ts", ".tsx", ".js", ".mjs", ".cjs", ".mts")
    return R.js_mutation_sites(text) if js else R.mutation_sites(text)


def reapply(path: Path, site, expect: str) -> bool:
    """Applies the mutant at `site` if the original text is still there."""
    if not path.exists():
        return False
    line, a, b, _ = site
    lines = path.read_text(encoding="utf-8").splitlines(keepends=True)
    if line - 1 >= len(lines) or lines[line - 1][a:b] != expect:
        return False
    R.apply_mutant(path, site)
    return True


def trial(ws: R.Workspace, good: str, window: list[str], rng: random.Random, t: int) -> dict | None:
    R.log(f"[{ws.name}] trial {t}: good {good[:8]}, {len(window)} commits")
    ws.checkout(good)
    good_tests, _, cap = R.run_suite(ws, f"c{t}-good")
    map_path = R.build_map(ws, cap, f"c{t}")

    # The mutant site is chosen on the tree where it goes in.
    p = rng.randrange(len(window))
    ws.checkout(window[p])
    files = mutate_files(ws)
    site = path = None
    for _ in range(30):
        path = rng.choice(files)
        sites = sites_for(path)
        if sites:
            site = rng.choice(sites)
            break
    if site is None:
        return None
    line, a, b, _ = site
    original = path.read_text(encoding="utf-8").splitlines(keepends=True)[line - 1][a:b]
    rel = path.relative_to(ws.repo)

    parent, mutant_commit, desc = good, None, ""
    for j, c in enumerate(window):
        ws.checkout(c)
        if j > p and not reapply(ws.repo / rel, site, original):
            R.log("  trial dropped: a later commit changed the mutated line")
            return None
        parent = commit_tree(ws, parent, f"replay {c[:8]}")
        if j == p:
            desc = R.apply_mutant(ws.repo / rel, site)
            parent = mutant_commit = commit_tree(ws, parent, f"mutant {desc}")
    bad = parent

    ws.checkout(window[-1])
    clean_tests, _, _ = R.run_suite(ws, f"c{t}-clean")
    ws.checkout(bad)
    bad_tests, _, _ = R.run_suite(ws, f"c{t}-bad")
    candidates = R.failing(bad_tests) - R.failing(clean_tests) - R.failing(good_tests)
    broken = sorted(n for n in candidates if n in good_tests and R.fails_alone(ws, n))
    R.log(f"  mutant {desc} at {p + 1}/{len(window)}: {len(broken)} test(s) broken")
    if not broken:
        return {"trial": t, "mutant": desc, "position": p, "broken": 0, "tests": []}

    env_args: list = []
    if ws.runner == "pytest":
        env_args = ["--python", ws.python]
    elif ws.runner in R.JS_RUNNERS:
        cmd = ["node", R.JS_PROBE, "fingerprint", "--root", ws.repo, "--dir", ws.test_dir]
        env_args = [x for c in cmd for x in ("--fingerprint-cmd", c)]
    out = R.sh([R.QUOD, "--repo-dir", ws.repo, "--repo-id", ws.name, "culprit", "--map", map_path,
                "--good", good, "--bad", bad, *[x for n in broken for x in ("--failed", n)], *env_args, "--json"])
    report = json.loads(out.stdout)
    tests = []
    for f in report["findings"]:
        suspects = [s["commit"] for s in f["suspects"]]
        tests.append({
            "test": f["test"],
            "outcome": f["outcome"],
            "suspects": len(suspects),
            "mutant_suspected": mutant_commit in suspects,
            "miss": f["outcome"] != "unknown" and mutant_commit not in suspects,
        })
    misses = sum(x["miss"] for x in tests)
    named = sum(x["outcome"] == "culprit" and x["mutant_suspected"] for x in tests)
    R.log(f"  culprit: {named}/{len(tests)} named alone, {misses} miss(es)")
    return {"trial": t, "mutant": desc, "position": p, "commits": len(window) + 1, "broken": len(broken), "tests": tests}


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--repo", required=True)
    ap.add_argument("--trials", type=int, default=6)
    ap.add_argument("--window", type=int, default=5)
    ap.add_argument("--seed", type=int, default=11)
    ap.add_argument("--python", default="3.13")
    ap.add_argument("--work", type=Path, default=R.ROOT / "conformance" / ".work")
    args = ap.parse_args()

    cfg = tomllib.loads((R.ROOT / "conformance" / "repos.toml").read_text(encoding="utf-8"))["repos"][args.repo]
    os.environ.update({k: str(v) for k, v in cfg.get("env", {}).items()})
    R.ENV_MODE = True
    ws = R.setup(args.repo, cfg, args.work / args.repo, args.python)
    branch = cfg["branch"]
    ws.git("fetch", "-q", "origin", branch, check=False)
    need = args.trials + args.window
    commits = ws.git("log", "--first-parent", "--format=%H", f"-n{need}", f"origin/{branch}").stdout.split()[::-1]
    rng = random.Random(args.seed)
    trials = []
    for t in range(args.trials):
        good, window = commits[t], commits[t + 1 : t + 1 + args.window]
        if len(window) < args.window:
            break
        try:
            r = trial(ws, good, window, rng, t + 1)
        except Exception as e:  # noqa: BLE001 - record and continue
            R.log(f"  trial skipped: {str(e).splitlines()[0]}")
            continue
        if r is not None:
            trials.append(r)
    tests = [x for tr in trials for x in tr["tests"]]
    decided = [x for x in tests if x["outcome"] != "unknown"]
    summary = {
        "repo": args.repo,
        "trials": len(trials),
        "trials_with_broken_tests": sum(1 for tr in trials if tr["broken"]),
        "broken_tests": len(tests),
        "misses": sum(x["miss"] for x in tests),
        "unknown": len(tests) - len(decided),
        "named_alone": sum(x["outcome"] == "culprit" and x["mutant_suspected"] for x in tests),
        "mean_suspects": round(sum(x["suspects"] for x in decided) / len(decided), 2) if decided else None,
    }
    out = R.ROOT / "conformance" / "results" / f"e7-{args.repo}.json"
    out.write_text(json.dumps({"summary": summary, "trials": trials}, indent=2), encoding="utf-8")
    print(json.dumps(summary, indent=2))
    return 0 if summary["misses"] == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
