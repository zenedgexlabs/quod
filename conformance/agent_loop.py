"""E8: the agent loop, end to end through the MCP server.

A map is recorded at a real commit G. Then, for each of the next W real
first-parent commits C:

1. The repository is put in the state a coding agent leaves it in: HEAD at
   C's parent, C's change uncommitted in the working tree. In odd steps a
   mutant is added on top (an agent bug). The map stays at G, so it is
   increasingly behind HEAD, like a pulled map for an older ancestor.
2. `tests_for` is called over MCP (`quod mcp`, stdio), as an agent would.
3. The `run` command it returns is executed as-is; it writes its own results file.
4. The full suite runs on the same tree, through the same command with an
   empty skip file (same costs, no recording): the ground truth.

Escape: a test that fails in the full run, fails alone every time, and
the MCP run did not execute. The gate is 0 escapes. Precision: the
fraction of tests and of time the MCP run used.

Usage: agent_loop.py --repo NAME [--window 10] [--seed 5]
"""

from __future__ import annotations

import argparse
import json
import os
import random
import sys
import time
import tomllib
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import culprit as E7  # noqa: E402
import replay as R  # noqa: E402


def mcp_tests_for(ws: R.Workspace, map_path: Path) -> dict:
    """Calls tests_for the way an agent does: one stdio session."""
    msgs = [
        {"jsonrpc": "2.0", "id": 1, "method": "initialize",
         "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "e8", "version": "0"}}},
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
        {"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "tests_for", "arguments": {}}},
    ]
    # How an agent's machine is found: the test venv for pytest, the
    # quod-js CLI for Vitest and Jest.
    env = {**os.environ, **({"QUOD_JS": str(R.JS_PROBE)} if ws.runner in R.JS_RUNNERS
                            else {"QUOD_PYTHON": str(ws.python)})}
    out = R.sh([R.QUOD, "--repo-dir", ws.repo, "--repo-id", ws.name, "mcp", "--map", map_path],
               cwd=ws.repo, env=env, input="".join(json.dumps(m) + "\n" for m in msgs))
    reply = json.loads(out.stdout.splitlines()[-1])
    res = reply["result"]
    if res.get("isError"):
        raise RuntimeError(res["content"][0]["text"])
    return json.loads(res["content"][0]["text"])


def run_returned(ws: R.Workspace, plan: dict, tag: str, timeout: float) -> tuple[dict[str, str], float, int]:
    """Runs the returned command; outcomes by test name, wall time, exit code."""
    run = plan["run"]
    argv = list(run["argv"])
    # The returned command writes its own results file (pytest
    # --quod-results=PATH, quod-js --results PATH): read it from there.
    results = None
    for i, a in enumerate(argv):
        if a.startswith("--quod-results="):
            results = Path(a.split("=", 1)[1])
        elif a == "--results" and i + 1 < len(argv):
            results = Path(argv[i + 1])
    if results is None:
        raise RuntimeError(f"the returned command writes no results file: {argv}")
    if results.exists():
        results.unlink()
    t0 = time.monotonic()
    code = R.sh(argv, cwd=ws.repo / run["cwd"], check=False, timeout=timeout).returncode
    elapsed = time.monotonic() - t0
    out = {}
    if results.exists():
        for line in results.read_text(encoding="utf-8").splitlines():
            r = json.loads(line)
            if r.get("type") == "result":
                out[r["name"]] = r["outcome"]
    return out, elapsed, code


def full_run(ws: R.Workspace, plan: dict, tag: str, timeout: float) -> tuple[dict, float]:
    """The ground truth: the same command with an empty skip file, so both
    runs pay the same costs (no recording) and only the skips differ."""
    empty = ws.artifacts / f"{tag}-noskip.txt"
    empty.write_text("", encoding="utf-8")
    results = ws.artifacts / f"{tag}-results.jsonl"
    if results.exists():
        results.unlink()
    argv, it = [], iter(plan["run"]["argv"])
    for a in it:
        if a.startswith("--quod-skip="):
            argv.append(f"--quod-skip={empty}")
        elif a.startswith("--quod-results="):
            argv.append(f"--quod-results={results}")
        elif a in ("--skip", "--results"):
            next(it)
            argv += [a, str(empty if a == "--skip" else results)]
        else:
            argv.append(a)
    t0 = time.monotonic()
    R.sh(argv, cwd=ws.repo / plan["run"]["cwd"], check=False, timeout=timeout)
    elapsed = time.monotonic() - t0
    out = {}
    if results.exists():
        for line in results.read_text(encoding="utf-8").splitlines():
            r = json.loads(line)
            if r.get("type") == "result":
                out[r["name"]] = {"outcome": r["outcome"]}
    return out, elapsed


def step(ws: R.Workspace, map_path: Path, commit: str, k: int, rng: random.Random, mutate: bool,
         timeout: float) -> dict | None:
    ws.checkout(commit)
    ws.git("reset", "-q", "--soft", f"{commit}^")
    desc = ""
    if mutate:
        files = E7.mutate_files(ws)
        for _ in range(30):
            path = rng.choice(files)
            sites = E7.sites_for(path)
            if sites:
                desc = R.apply_mutant(path, rng.choice(sites))
                break
    plan = mcp_tests_for(ws, map_path)
    # A mutant can make the suite loop forever: such a step is dropped.
    try:
        mcp_out, mcp_time, mcp_code = run_returned(ws, plan, f"e8-{k}", timeout)
        full, full_time = full_run(ws, plan, f"e8-{k}-full", timeout)
    except R.SuiteTimeout:
        R.log(f"  step {k} dropped: the suite hangs under {desc or 'this change'}")
        return None
    failing_full = R.failing(full) - ws.flaky
    not_run = {n for n in failing_full if n not in mcp_out}
    escapes = sorted(n for n in not_run if R.fails_alone(ws, n))
    caught = sorted(n for n in failing_full if mcp_out.get(n) == "failed")
    # A test that failed in the full run, ran in the MCP run and passed
    # there: order-dependent or flaky, not a selection miss.
    passed_in_mcp = sorted(n for n in failing_full if mcp_out.get(n) == "passed")
    # The suite didn't even collect (a broken import): the agent must see
    # its own run fail too.
    collection_error = not full
    collection_error_missed = collection_error and mcp_code == 0
    s = {
        "step": k,
        "commit": commit[:10],
        "subject": ws.git("log", "-1", "--format=%s", commit).stdout.strip(),
        "mutant": desc,
        "environment_mode": plan.get("environment_mode"),
        "fallback": plan.get("fallback"),
        "obligated": plan.get("obligated_count"),
        "ran": len(mcp_out),
        "total": len(full),
        "mcp_time": round(mcp_time, 2),
        "full_time": round(full_time, 2),
        "failing_full": len(failing_full),
        "caught": len(caught),
        "escapes": escapes,
        "passed_in_mcp_run": passed_in_mcp,
        "victims_not_run": sorted(not_run - set(escapes)),
        "collection_error": collection_error,
        "collection_error_missed": collection_error_missed,
        "mcp_exit": mcp_code,
    }
    R.log(f"  step {k} {commit[:8]}{' + ' + desc if desc else ''}: ran {s['ran']}/{s['total']} "
          f"({s['mcp_time']}s vs {s['full_time']}s), failing {s['failing_full']}, caught {s['caught']}, "
          f"escapes {len(escapes)}"
          f"{' (suite did not collect; MCP run exit ' + str(mcp_code) + ')' if collection_error else ''}")
    return s


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--repo", required=True)
    ap.add_argument("--window", type=int, default=10)
    ap.add_argument("--seed", type=int, default=5)
    ap.add_argument("--python", default="3.13")
    ap.add_argument("--work", type=Path, default=R.ROOT / "conformance" / ".work")
    args = ap.parse_args()

    cfg = tomllib.loads((R.ROOT / "conformance" / "repos.toml").read_text(encoding="utf-8"))["repos"][args.repo]
    os.environ.update({k: str(v) for k, v in cfg.get("env", {}).items()})
    ws = R.setup(args.repo, cfg, args.work / args.repo, args.python)
    branch = cfg["branch"]
    ws.git("fetch", "-q", "origin", branch, check=False)
    commits = ws.git("log", "--first-parent", "--format=%H", f"-n{args.window + 1}",
                     f"origin/{branch}").stdout.split()[::-1]
    good, window = commits[0], commits[1:]
    R.log(f"[{ws.name}] map at {good[:8]}, {len(window)} agent steps")
    ws.checkout(good)
    t0 = time.monotonic()
    _, _, cap = R.run_suite(ws, "e8-map")
    timeout = max(300.0, 10 * (time.monotonic() - t0))
    map_path = R.build_map(ws, cap, "e8-map")

    rng = random.Random(args.seed)
    steps = []
    for k, c in enumerate(window, 1):
        try:
            s = step(ws, map_path, c, k, rng, mutate=k % 2 == 1, timeout=timeout)
            if s is not None:
                steps.append(s)
        except Exception as e:  # noqa: BLE001 - record and continue
            R.log(f"  step {k} skipped: {str(e).splitlines()[0]}")
    ran = sum(s["ran"] for s in steps)
    total = sum(s["total"] for s in steps)
    summary = {
        "repo": args.repo,
        "steps": len(steps),
        "escapes": sum(len(s["escapes"]) for s in steps) + sum(s["collection_error_missed"] for s in steps),
        "collection_errors": sum(s["collection_error"] for s in steps),
        "failing_tests": sum(s["failing_full"] for s in steps),
        "caught": sum(s["caught"] for s in steps),
        "tests_fraction": round(ran / total, 3) if total else None,
        "time_fraction": round(sum(s["mcp_time"] for s in steps) / max(1e-9, sum(s["full_time"] for s in steps)), 3),
        "environment_mode_steps": sum(bool(s["environment_mode"]) for s in steps),
        "fallback_steps": sum(bool(s["fallback"]) for s in steps),
    }
    out = R.ROOT / "conformance" / "results" / f"e8-{args.repo}.json"
    out.write_text(json.dumps({"summary": summary, "steps": steps}, indent=2), encoding="utf-8")
    print(json.dumps(summary, indent=2))
    return 0 if summary["escapes"] == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
