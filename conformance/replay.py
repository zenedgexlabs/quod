#!/usr/bin/env python3
"""Skipcert conformance replay (experiment E1).

For one repository from ``repos.toml`` this harness measures:

History replay
    For consecutive first-parent commits (base, next):
    1. full suite at ``base`` with the probe → footprint map and base outcomes;
    2. check out ``next``; ``skipcert select`` (signed) and ``skipcert-verify``;
    3. full suite at ``next`` → ground truth (failures re-run to drop flakes);
    4. **escape** = a test Skipcert skipped that passed at ``base`` and really
       fails at ``next``.

Mutation testing
    At the newest base, inject single-token mutants into function bodies
    (comparison flips, constant changes, True/False swaps). Each killed
    mutant's failing tests must all be selected. A skipped killing test is an
    escape.

Both report test time selected versus the full suite and versus an
Ekstazi-style file-level baseline computed from the same footprints. The
E1 gate is **0 escapes** and **≥2x less test time than the baseline**.

Usage::

    uv run conformance/replay.py --repo click --commits 20 --mutants 10

Only the Python standard library is used. ``uv`` and ``git`` must be on PATH.
"""

from __future__ import annotations

import argparse
import ast
import fnmatch
import io
import json
import os
import random
import shutil
import subprocess
import sys
import time
import tokenize
import tomllib
from dataclasses import asdict, dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
EXE = ".exe" if os.name == "nt" else ""
SKIPCERT = ROOT / "target" / "release" / f"skipcert{EXE}"
VERIFY = ROOT / "target" / "release" / f"skipcert-verify{EXE}"


def sh(cmd: list, cwd: Path | None = None, check: bool = True, env: dict | None = None) -> subprocess.CompletedProcess:
    res = subprocess.run([str(c) for c in cmd], cwd=cwd, capture_output=True, text=True, env=env)
    if check and res.returncode != 0:
        raise RuntimeError(f"command failed ({res.returncode}): {' '.join(map(str, cmd))}\n{res.stdout[-3000:]}\n{res.stderr[-3000:]}")
    return res


def log(msg: str) -> None:
    print(f"[{time.strftime('%H:%M:%S')}] {msg}", flush=True)


# Workspace -----------------------------------------------------------------


@dataclass
class Workspace:
    name: str
    cfg: dict
    work: Path
    repo: Path
    python: Path
    key: Path
    key_id: str
    artifacts: Path

    def git(self, *args: str, check: bool = True) -> subprocess.CompletedProcess:
        return sh(["git", "-c", "core.autocrlf=false", "-C", self.repo, *args], check=check)

    def checkout(self, rev: str) -> None:
        self.git("checkout", "-q", "-f", "--detach", rev)
        self.git("clean", "-fdq")  # untracked only; keeps ignored build artifacts


def setup(name: str, cfg: dict, work: Path, python_version: str) -> Workspace:
    work.mkdir(parents=True, exist_ok=True)
    repo = work / "repo"
    if not repo.exists():
        log(f"cloning {cfg['url']}")
        sh(["git", "clone", "-q", "--filter=blob:none", "-b", cfg["branch"], cfg["url"], repo])
    venv = work / "venv"
    if not venv.exists():
        log(f"creating venv (python {python_version})")
        sh(["uv", "venv", "-q", "--python", python_version, venv])
    py = venv / ("Scripts/python.exe" if os.name == "nt" else "bin/python")
    sh(["uv", "pip", "install", "-q", "--python", py, "--reinstall-package", "skipcert-probe", ROOT / "probes" / "python", "pytest"])
    sh(["uv", "pip", "install", "-q", "--python", py, *cfg["install"]], cwd=repo)

    if not SKIPCERT.exists() or not VERIFY.exists():
        log("building skipcert (release)")
        sh(["cargo", "build", "-q", "--release", "-p", "sc-cli", "-p", "skipcert-verify"], cwd=ROOT)
    key = work / "replay-key.seed"
    if key.exists():
        key.unlink()
    out = sh([SKIPCERT, "keygen", "--out", key]).stdout
    key_id = next(line.split(": ", 1)[1] for line in out.splitlines() if line.startswith("trusted key id"))
    artifacts = work / "artifacts"
    artifacts.mkdir(exist_ok=True)
    return Workspace(name, cfg, work, repo, py, key, key_id, artifacts)


# Suite runs ------------------------------------------------------------------


def read_capture(path: Path) -> tuple[dict, list[dict]]:
    tests, recs = {}, []
    for f in sorted(path.parent.glob(path.name + "*")):
        for line in f.read_text(encoding="utf-8").splitlines():
            r = json.loads(line)
            recs.append(r)
            if r["type"] == "test":
                tests[r["name"]] = r
    return tests, recs


def run_suite(ws: Workspace, tag: str, extra: list[str] | None = None) -> tuple[dict, list[dict], Path]:
    """Runs the full suite with the probe recording. Returns (tests by name, records, capture path)."""
    cap = ws.artifacts / f"{tag}.jsonl"
    for old in ws.artifacts.glob(cap.name + "*"):
        old.unlink()
    args = [ws.python, "-m", "pytest", *ws.cfg["pytest"], f"--skipcert-record={cap}", f"--skipcert-root={ws.repo}", *(extra or [])]
    sh(args, cwd=ws.repo, check=False)
    if not cap.exists():
        raise RuntimeError(f"no capture produced for {tag}; the suite may not have started")
    tests, recs = read_capture(cap)
    return tests, recs, cap


def nonflaky_failures(ws: Workspace, failing: set[str]) -> set[str]:
    """Re-runs each failing test twice; a test that passes either time is flaky."""
    real = set()
    for name in sorted(failing):
        passes = 0
        for _ in range(2):
            r = sh([ws.python, "-m", "pytest", "-q", "-p", "no:cacheprovider", name], cwd=ws.repo, check=False)
            passes += r.returncode == 0
        if passes == 0:
            real.add(name)
    return real


def failing(tests: dict) -> set[str]:
    return {n for n, t in tests.items() if t["outcome"] == "failed"}


# Selection -----------------------------------------------------------------


def build_map(ws: Workspace, cap: Path, tag: str) -> Path:
    out = ws.artifacts / f"{tag}.scmap"
    caps = sorted(cap.parent.glob(cap.name + "*"))
    sh([SKIPCERT, "--repo-dir", ws.repo, "--repo-id", ws.name, "ingest", *[a for c in caps for a in ("--capture", c)], "--out", out])
    return out


# Environment mode (ADR 012): select fingerprints the replay venv. Every step
# runs in that one venv, so the comparison is exact for the harness.
ENV_MODE = True


def select(ws: Workspace, map_path: Path, tag: str) -> dict:
    skip, certs, report = (ws.artifacts / f"{tag}.{s}" for s in ("skip.txt", "certs.jsonl", "report.json"))
    env_args = ["--python", ws.python] if ENV_MODE else []
    sh([SKIPCERT, "--repo-dir", ws.repo, "--repo-id", ws.name, "select", "--map", map_path, "--key", ws.key,
        "--skip-out", skip, "--certs-out", certs, "--report-out", report, *env_args])
    v = sh([VERIFY, "--certs", certs, "--map", map_path, "--trusted-key", ws.key_id], check=False)
    rep = json.loads(report.read_text(encoding="utf-8"))
    rep["verifier_ok"] = v.returncode == 0
    rep["verifier_output"] = v.stdout.strip().splitlines()[-1] if v.stdout.strip() else v.stderr.strip()
    rep["skip_names"] = [
        line for line in skip.read_text(encoding="utf-8").splitlines() if line and not line.startswith("#")
    ]
    return rep


def changed_files(ws: Workspace, base: str) -> set[str]:
    out = ws.git("diff", "--name-only", base).stdout.split()
    out += ws.git("ls-files", "--others", "--exclude-standard").stdout.split()
    return set(out)


def file_level_skips(base_tests: dict, changed: set[str]) -> set[str]:
    """Ekstazi-style baseline: skip a test iff none of the files it touched changed."""
    skips = set()
    for name, t in base_tests.items():
        files = set(t["files"]) | {p for p, _ in t["functions"]}
        if t["outcome"] == "passed" and not (files & changed):
            skips.add(name)
    return skips


# Metrics -----------------------------------------------------------------


@dataclass
class Step:
    kind: str
    base: str
    head: str
    tests_total: int
    total_ms: int
    skipcert_skipped: int
    skipcert_run_ms: int
    baseline_skipped: int
    baseline_run_ms: int
    escapes: list[str]
    baseline_escapes: list[str]
    newly_failing: list[str]
    global_triggers: list[str]
    verifier_ok: bool
    note: str = ""
    # File-level baseline that also obeys Skipcert's global policy triggers
    # (runs everything whenever Skipcert's policy does): the fair comparison.
    policy_baseline_skipped: int = 0
    policy_baseline_run_ms: int = 0
    # Escapes split by cause: a dependency escape fails when run alone (Skipcert
    # missed a real dependency); a victim passes alone and fails only through
    # state other failing tests left behind.
    dependency_escapes: list[str] = field(default_factory=list)
    victim_escapes: list[str] = field(default_factory=list)
    # True if the change caused regressions and *none* of them was selected:
    # the bad change would have merged green.
    change_escape: bool = False
    extra: dict = field(default_factory=dict)


def fails_alone(ws: "Workspace", name: str) -> bool:
    r = sh([ws.python, "-m", "pytest", "-q", "-p", "no:cacheprovider", name], cwd=ws.repo, check=False)
    return r.returncode != 0


def measure(kind, base, head, base_tests, head_tests, rep, base_changed, real_fail, ws, note="") -> Step:
    """Computes metrics. Must be called with the head state (commit or mutant)
    still in the working tree, because escapes are re-run in isolation."""
    skipped = set(rep["skip_names"])
    baseline_skip = file_level_skips(base_tests, base_changed)
    policy_baseline_skip = set() if rep["global"] else baseline_skip
    dur = {n: t["duration_ms"] for n, t in head_tests.items()}
    total = sum(dur.values())
    passed_at_base = {n for n, t in base_tests.items() if t["outcome"] == "passed"}
    regressions = real_fail & passed_at_base
    escapes = sorted(skipped & regressions)
    dependency = [n for n in escapes if fails_alone(ws, n)]
    return Step(
        kind=kind,
        base=base,
        head=head,
        tests_total=len(head_tests),
        total_ms=total,
        skipcert_skipped=len(skipped & set(head_tests)),
        skipcert_run_ms=total - sum(dur.get(n, 0) for n in skipped),
        baseline_skipped=len(baseline_skip & set(head_tests)),
        baseline_run_ms=total - sum(dur.get(n, 0) for n in baseline_skip),
        policy_baseline_skipped=len(policy_baseline_skip & set(head_tests)),
        policy_baseline_run_ms=total - sum(dur.get(n, 0) for n in policy_baseline_skip),
        escapes=escapes,
        dependency_escapes=dependency,
        victim_escapes=[n for n in escapes if n not in dependency],
        change_escape=bool(regressions) and regressions <= skipped,
        baseline_escapes=sorted(baseline_skip & regressions),
        newly_failing=sorted(regressions),
        global_triggers=rep["global"],
        verifier_ok=rep["verifier_ok"],
        note=note,
    )


# Mutants -----------------------------------------------------------------

_SWAPS = {"==": "!=", "!=": "==", "<": ">=", ">": "<=", "<=": ">", ">=": "<", "+": "-", "-": "+", "True": "False", "False": "True"}


def mutation_sites(source: str) -> list[tuple[int, int, int, str]]:
    """(line, col_start, col_end, replacement) for tokens inside function bodies."""
    tree = ast.parse(source)
    body_lines: set[int] = set()
    for node in ast.walk(tree):
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)) and node.body:
            first = node.body[0]
            start = first.lineno
            if isinstance(first, ast.Expr) and isinstance(getattr(first, "value", None), ast.Constant) and isinstance(first.value.value, str):
                start = (first.end_lineno or first.lineno) + 1  # skip docstrings
            body_lines.update(range(start, (node.end_lineno or start) + 1))
    sites = []
    for tok in tokenize.generate_tokens(io.StringIO(source).readline):
        line, col = tok.start
        if line not in body_lines or tok.start[0] != tok.end[0]:
            continue
        if tok.type == tokenize.OP and tok.string in _SWAPS:
            sites.append((line, col, tok.end[1], _SWAPS[tok.string]))
        elif tok.type == tokenize.NAME and tok.string in ("True", "False"):
            sites.append((line, col, tok.end[1], _SWAPS[tok.string]))
        elif tok.type == tokenize.NUMBER and tok.string.isdigit():
            sites.append((line, col, tok.end[1], str(int(tok.string) + 1)))
    return sites


def apply_mutant(path: Path, site: tuple[int, int, int, str]) -> str:
    text = path.read_text(encoding="utf-8")
    lines = text.splitlines(keepends=True)
    line, a, b, rep = site
    original = lines[line - 1]
    lines[line - 1] = original[:a] + rep + original[b:]
    path.write_text("".join(lines), encoding="utf-8", newline="")
    return f"{path.name}:{line}: {original[a:b]!r} -> {rep!r}"


# Driver -----------------------------------------------------------------


def replay(ws: Workspace, n_commits: int, n_mutants: int, seed: int) -> list[Step]:
    branch = ws.cfg["branch"]
    ws.git("fetch", "-q", "origin", branch, check=False)
    commits = ws.git("log", "--first-parent", "--format=%H", f"-n{n_commits + 1}", f"origin/{branch}").stdout.split()[::-1]
    steps: list[Step] = []

    for i in range(1, len(commits)):
        base, head = commits[i - 1], commits[i]
        log(f"[{ws.name}] history {i}/{len(commits) - 1}: {base[:8]} -> {head[:8]}")
        try:
            ws.checkout(base)
            base_tests, _, cap = run_suite(ws, f"h{i}-base")
            map_path = build_map(ws, cap, f"h{i}")
            ws.checkout(head)
            rep = select(ws, map_path, f"h{i}")
            changed = changed_files(ws, base)
            head_tests, _, _ = run_suite(ws, f"h{i}-head")
            real = nonflaky_failures(ws, failing(head_tests) - failing(base_tests))
            steps.append(measure("history", base, head, base_tests, head_tests, rep, changed, real, ws))
        except Exception as e:  # noqa: BLE001 - record and continue (e.g. a commit that doesn't build)
            log(f"  step skipped: {str(e).splitlines()[0]}")
            continue
        s = steps[-1]
        log(f"  skipcert ran {s.tests_total - s.skipcert_skipped}/{s.tests_total} tests, escapes={len(s.dependency_escapes)} dep + {len(s.victim_escapes)} victim, verifier={'ok' if s.verifier_ok else 'FAIL'}")

    if n_mutants:
        rng = random.Random(seed)
        base = commits[-1]
        ws.checkout(base)
        base_tests, _, cap = run_suite(ws, "m-base")
        map_path = build_map(ws, cap, "m")
        files = sorted({p for pat in ws.cfg["mutate"] for p in ws.repo.glob(pat) if p.is_file()})
        attempts = 0
        made = 0
        while made < n_mutants and attempts < n_mutants * 10:
            attempts += 1
            path = rng.choice(files)
            sites = mutation_sites(path.read_text(encoding="utf-8"))
            if not sites:
                continue
            desc = apply_mutant(path, rng.choice(sites))
            made += 1
            log(f"[{ws.name}] mutant {made}/{n_mutants}: {desc}")
            try:
                rep = select(ws, map_path, f"m{made}")
                changed = changed_files(ws, base)
                head_tests, _, _ = run_suite(ws, f"m{made}-head")
                real = failing(head_tests) - failing(base_tests)
                step = measure("mutant", base, f"mutant{made}", base_tests, head_tests, rep, changed, real, ws, note=desc)
                step.extra["killed"] = bool(real)
                steps.append(step)
                log(f"  killed={bool(real)} ran {step.tests_total - step.skipcert_skipped}/{step.tests_total}, escapes={len(step.dependency_escapes)} dep + {len(step.victim_escapes)} victim, change_escape={step.change_escape}")
            finally:
                ws.checkout(base)
    return steps


def summarize(name: str, steps: list[Step]) -> dict:
    hist = [s for s in steps if s.kind == "history"]
    muts = [s for s in steps if s.kind == "mutant"]

    def agg(ss):
        total = sum(s.total_ms for s in ss) or 1
        sk = sum(s.skipcert_run_ms for s in ss)
        bl = sum(s.baseline_run_ms for s in ss)
        pbl = sum(s.policy_baseline_run_ms for s in ss)
        return {
            "steps": len(ss),
            "escapes": sum(len(s.escapes) for s in ss),
            "dependency_escapes": sum(len(s.dependency_escapes) for s in ss),
            "victim_escapes": sum(len(s.victim_escapes) for s in ss),
            "change_escapes": sum(s.change_escape for s in ss),
            "baseline_escapes": sum(len(s.baseline_escapes) for s in ss),
            "skipcert_time_fraction": round(sk / total, 4),
            "baseline_time_fraction": round(bl / total, 4),
            "policy_baseline_time_fraction": round(pbl / total, 4),
            "advantage_vs_baseline": round(bl / sk, 2) if sk else None,
            "advantage_vs_policy_baseline": round(pbl / sk, 2) if sk else None,
            "verifier_failures": sum(not s.verifier_ok for s in ss),
            "full_run_steps": sum(bool(s.global_triggers) for s in ss),
        }

    out = {"repo": name, "history": agg(hist), "mutants": agg(muts)}
    out["mutants"]["killed"] = sum(bool(s.extra.get("killed")) for s in muts)
    both = (out["history"], out["mutants"])
    adv = out["history"]["advantage_vs_baseline"]
    padv = out["history"]["advantage_vs_policy_baseline"]
    out["gate"] = {
        # Soundness: no missed dependency and no bad change merged green.
        # Order-dependent victims are reported but don't fail the gate.
        "zero_dependency_escapes": sum(a["dependency_escapes"] for a in both) == 0,
        "zero_change_escapes": sum(a["change_escapes"] for a in both) == 0,
        "verifier_clean": sum(a["verifier_failures"] for a in both) == 0,
        "two_x_vs_baseline": adv is not None and adv >= 2.0,
        "two_x_vs_policy_baseline": padv is not None and padv >= 2.0,
    }
    return out


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--repo", required=True, help="name in repos.toml")
    ap.add_argument("--commits", type=int, default=20)
    ap.add_argument("--mutants", type=int, default=10)
    ap.add_argument("--python", default="3.13")
    ap.add_argument("--seed", type=int, default=7)
    ap.add_argument("--work", type=Path, default=ROOT / "conformance" / ".work")
    ap.add_argument("--path-mode", action="store_true", help="select without --python (path-based rules)")
    args = ap.parse_args()

    cfg = tomllib.loads((ROOT / "conformance" / "repos.toml").read_text(encoding="utf-8"))["repos"][args.repo]
    for tool in ("git", "uv"):
        if shutil.which(tool) is None:
            print(f"{tool} is required on PATH", file=sys.stderr)
            return 2
    global ENV_MODE
    ENV_MODE = not args.path_mode
    ws = setup(args.repo, cfg, args.work / args.repo, args.python)
    steps = replay(ws, args.commits, args.mutants, args.seed)
    summary = summarize(args.repo, steps)
    summary["mode"] = "environment" if ENV_MODE else "path"
    results = ROOT / "conformance" / "results"
    results.mkdir(exist_ok=True)
    (results / f"{args.repo}.json").write_text(
        json.dumps({"summary": summary, "steps": [asdict(s) for s in steps]}, indent=2), encoding="utf-8"
    )
    print(json.dumps(summary, indent=2))
    g = summary["gate"]
    return 0 if g["zero_dependency_escapes"] and g["zero_change_escapes"] and g["verifier_clean"] else 1


if __name__ == "__main__":
    sys.exit(main())
