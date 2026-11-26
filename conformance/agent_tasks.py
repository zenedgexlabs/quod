"""E8b: a real coding agent (Claude Code, headless) on real tasks, with and
without Quod's MCP server.

Each task is a real commit: the agent starts at its parent with the
commit message as the task. The commit's own test changes stay hidden
and are applied afterwards to score the implementation. Each task runs
twice from the same state:

- quod: the Quod MCP server is attached (map recorded at the parent, the
  test environment given as QUOD_PYTHON). The prompt doesn't mention it:
  the agent learns about it from the server's own instructions.
- quod-guided: the same, plus the instructions section `quod mcp install`
  writes into CLAUDE.md.
- baseline: no MCP server.

The agent runs in a scratch clone (`.work/<repo>-agent`) with only
file tools, the test interpreter's `-m pytest` and, in the quod
condition, the Quod tools. Everything else is denied.

Scored per run: did it use tests_for, how many tests and seconds its
pytest runs took (from their summaries), whether the full suite passes on
its result, whether the commit's hidden tests pass, and a "false green":
its last pytest run passed while the full suite on its result fails.

Usage: agent_tasks.py --repo attrs --tasks f53fc54 [...] [--conditions quod baseline]
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import time
import tomllib
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import replay as R  # noqa: E402

CLAUDE = Path(os.environ.get("QUOD_E8B_CLAUDE", ""))
SUMMARY = re.compile(r"=*\s*((?:\d+ \w+(?:, )?)+) in ([\d.]+)s")

PROMPT = """You are working in a Python repository (the `attrs` library). Implement the change described below.

Task:
{task}

Write or update tests for your change, and make sure the tests pass before you finish.
The test environment's interpreter is {python} (run tests with `{python} -m pytest ...` from the repository root).
Don't commit. When you're done, reply with a one-line summary."""


def claude_bin() -> Path:
    if CLAUDE.is_file():
        return CLAUDE
    ext = Path.home() / ".vscode" / "extensions"
    found = sorted(ext.glob("anthropic.claude-code-*/resources/native-binary/claude*"))
    if not found:
        raise SystemExit("Claude Code binary not found; set QUOD_E8B_CLAUDE")
    return found[-1]


def task_text(ws: R.Workspace, commit: str) -> str:
    msg = ws.git("log", "-1", "--format=%B", commit).stdout
    keep = [l for l in msg.splitlines() if not re.match(r"^(Co-authored-by|Signed-off-by):", l, re.I)]
    return "\n".join(keep).strip()


def test_files(ws: R.Workspace, commit: str) -> list[str]:
    names = ws.git("show", "--name-only", "--format=", commit).stdout.split()
    return [n for n in names if n.startswith("tests/")]


def run_agent(ws: R.Workspace, prompt: str, mcp: Path | None, transcript: Path, timeout: float) -> int:
    py = ws.python.as_posix()
    allowed = ["Read", "Edit", "Write", "Glob", "Grep", "TodoWrite", f"Bash({py} -m pytest:*)",
               f"PowerShell({py} -m pytest:*)", f"PowerShell(& {py} -m pytest:*)"]
    if mcp:
        allowed.append("mcp__quod")
    cmd = [claude_bin(), "-p", prompt, "--output-format", "stream-json", "--verbose",
           "--max-turns", "80", "--allowedTools", ",".join(allowed),
           "--disallowedTools", "WebFetch,WebSearch,Task"]
    if mcp:
        cmd += ["--mcp-config", mcp, "--strict-mcp-config"]
    else:
        cmd += ["--strict-mcp-config", "--mcp-config", '{"mcpServers": {}}']
    res = R.sh(cmd, cwd=ws.repo, check=False, timeout=timeout)
    transcript.write_text(res.stdout + ("\n" + res.stderr if res.stderr else ""), encoding="utf-8")
    return res.returncode


def parse(transcript: Path) -> dict:
    """Tool use and pytest summaries from a stream-json transcript."""
    tools: list[str] = []
    pytest_runs: list[dict] = []
    pending: dict[str, str] = {}
    result_text, cost, turns = "", None, None
    for line in transcript.read_text(encoding="utf-8").splitlines():
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue
        if ev.get("type") == "result":
            result_text, cost, turns = ev.get("result", ""), ev.get("total_cost_usd"), ev.get("num_turns")
        msg = ev.get("message")
        if not isinstance(msg, dict):
            continue
        for block in msg.get("content") or []:
            if not isinstance(block, dict):
                continue
            if block.get("type") == "tool_use":
                tools.append(block["name"])
                if block["name"] in ("Bash", "PowerShell"):
                    pending[block["id"]] = block.get("input", {}).get("command", "")
            elif block.get("type") == "tool_result" and block.get("tool_use_id") in pending:
                content = block.get("content")
                text = content if isinstance(content, str) else " ".join(
                    c.get("text", "") for c in content or [] if isinstance(c, dict))
                m = None
                for m in SUMMARY.finditer(text):
                    pass
                counts = {}
                if m:
                    for part in m.group(1).split(", "):
                        n, kind = part.split(" ", 1)
                        counts[kind] = int(n)
                pytest_runs.append({
                    "command": pending.pop(block["tool_use_id"])[:300],
                    "counts": counts,
                    "seconds": float(m.group(2)) if m else None,
                    "denied": "permission" in text.lower() and not m,
                })
    return {
        "tools": tools,
        "tests_for_calls": tools.count("mcp__quod__tests_for"),
        "quod_calls": sum(t.startswith("mcp__quod__") for t in tools),
        "pytest_runs": pytest_runs,
        "tests_run": sum(sum(v for k, v in r["counts"].items() if k in ("passed", "failed", "error", "errors"))
                         for r in pytest_runs),
        "test_seconds": round(sum(r["seconds"] or 0 for r in pytest_runs), 2),
        "result": result_text[-500:],
        "cost_usd": cost,
        "turns": turns,
    }


def score(ws: R.Workspace, commit: str, tag: str) -> dict:
    """Full suite on the agent's result, then with the commit's hidden tests."""
    diff = ws.git("diff", "--shortstat", "HEAD").stdout.strip()
    own, _, _ = R.run_suite(ws, f"{tag}-own")
    own_fail = sorted(R.failing(own))
    hidden_files = test_files(ws, commit)
    if hidden_files:
        # Partial clone: a lazy blob fetch can fail once; blobs are also
        # warmed before the agent starts.
        if ws.git("checkout", commit, "--", *hidden_files, check=False).returncode:
            ws.git("checkout", commit, "--", *hidden_files)
    hidden, _, _ = R.run_suite(ws, f"{tag}-hidden")
    hidden_fail = sorted(R.failing(hidden))
    return {
        "diff": diff,
        "full_suite_tests": len(own),
        "full_suite_failures": own_fail,
        "hidden_tests_failures": hidden_fail,
        "correct": bool(hidden) and not hidden_fail,
    }


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--repo", default="attrs")
    ap.add_argument("--tasks", nargs="+", required=True)
    ap.add_argument("--conditions", nargs="+", default=["quod-guided", "baseline"])
    ap.add_argument("--timeout", type=float, default=1800)
    ap.add_argument("--python", default="3.13")
    ap.add_argument("--score-only", action="store_true",
                    help="score the working tree and saved transcript of the one task/condition given")
    ap.add_argument("--work", type=Path, default=R.ROOT / "conformance" / ".work")
    args = ap.parse_args()

    cfg = tomllib.loads((R.ROOT / "conformance" / "repos.toml").read_text(encoding="utf-8"))["repos"][args.repo]
    ws = R.setup(f"{args.repo}-agent", cfg, args.work / f"{args.repo}-agent", args.python)
    ws.git("fetch", "-q", "origin", cfg["branch"], check=False)
    out_path = R.ROOT / "conformance" / "results" / f"e8b-{args.repo}.json"
    runs = json.loads(out_path.read_text(encoding="utf-8"))["runs"] if out_path.exists() else []

    for short in args.tasks:
        commit = ws.git("rev-parse", short).stdout.strip()
        parent = f"{commit}^"
        task = task_text(ws, commit)
        for f in test_files(ws, commit):
            ws.git("cat-file", "-p", f"{commit}:{f}", check=False)  # fetch hidden test blobs now
        R.log(f"[{ws.name}] task {short}: {task.splitlines()[0]}")
        # A fresh map at the parent, where the agent starts: what `quod map
        # pull` gives an agent on an up-to-date branch.
        map_path = ws.artifacts / f"e8b-{short}-map.qmap"
        if not (args.score_only and map_path.exists()):
            ws.checkout(parent)
            _, _, cap = R.run_suite(ws, f"e8b-{short}-map")
            map_path = R.build_map(ws, cap, f"e8b-{short}-map")
        installed = Path(ws.git("rev-parse", "--git-common-dir").stdout.strip())
        if not installed.is_absolute():
            installed = ws.repo / installed
        (installed / "quod").mkdir(exist_ok=True)
        (installed / "quod" / "map.qmap").write_bytes(map_path.read_bytes())
        mcp = ws.artifacts / "mcp.json"
        mcp.write_text(json.dumps({"mcpServers": {"quod": {
            "command": str(R.QUOD), "args": ["--repo-dir", str(ws.repo), "mcp"],
            "env": {"QUOD_PYTHON": ws.python.as_posix()},
        }}}), encoding="utf-8")

        for cond in args.conditions:
            tag = f"e8b-{short}-{cond}"
            transcript = ws.artifacts / f"{tag}.jsonl"
            t0 = time.monotonic()
            if args.score_only:
                code = 0
            else:
                ws.checkout(parent)
                if cond == "quod-guided":
                    # What a team gets from `quod mcp install`: the
                    # instructions section (the server comes from --mcp-config).
                    R.sh([R.QUOD, "--repo-dir", ws.repo, "mcp", "install", "--client", "claude-code", "--no-pull"])
                    (ws.repo / ".mcp.json").unlink()
                R.log(f"  {cond}: agent running")
                try:
                    code = run_agent(ws, PROMPT.format(task=task, python=ws.python.as_posix()),
                                     mcp if cond.startswith("quod") else None, transcript, args.timeout)
                except R.SuiteTimeout:
                    code = -1
            wall = round(time.monotonic() - t0, 1)
            info = parse(transcript)
            sc = score(ws, commit, tag)
            last = next((r for r in reversed(info["pytest_runs"]) if r["counts"]), None)
            last_green = bool(last) and not last["counts"].get("failed") and not last["counts"].get("error") \
                and not last["counts"].get("errors")
            run = {
                "task": short, "condition": cond, "exit": code, "wall_seconds": wall,
                **{k: info[k] for k in ("tests_for_calls", "quod_calls", "tests_run", "test_seconds",
                                        "cost_usd", "turns", "result")},
                "pytest_runs": len(info["pytest_runs"]),
                "denied_commands": [r["command"] for r in info["pytest_runs"] if r["denied"]],
                **sc,
                "false_green": last_green and bool(sc["full_suite_failures"]),
            }
            R.log(f"  {cond}: {wall}s, tests_for x{run['tests_for_calls']}, pytest x{run['pytest_runs']} "
                  f"({run['tests_run']} tests, {run['test_seconds']}s), full suite failures "
                  f"{len(sc['full_suite_failures'])}, hidden failures {len(sc['hidden_tests_failures'])}, "
                  f"false green {run['false_green']}, cost {run['cost_usd']}")
            runs = [r for r in runs if (r["task"], r["condition"]) != (short, cond)] + [run]
            out_path.write_text(json.dumps({"runs": runs}, indent=2), encoding="utf-8")
    return 0


if __name__ == "__main__":
    sys.exit(main())
