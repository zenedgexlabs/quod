"""pytest plugin.

Options:

``--skipcert-record PATH``
    Record per-test footprints to a capture file (JSON Lines, protocol v1).
    Under pytest-xdist each worker writes ``PATH.<worker id>``.
``--skipcert-skip PATH``
    Deselect the tests named in PATH (one test name per line), as produced
    by ``skipcert select``. Every test not listed runs.
``--skipcert-root DIR``
    Repository root (default: ``git rev-parse --show-toplevel``, else rootdir).

Capture protocol v1 (one JSON object per line):

- ``header``: probe ID, protocol version, capabilities, Python version, repository root.
- ``package``: name, version, native, dist_hash. Emitted once per package,
  before its first use.
- ``test``: name, path, qual, params, outcome, duration_ms, files,
  functions (``[path, qualname]``), packages, flags.
- ``root``: root-scope files, functions and packages (conftest, plugins,
  session start). Emitted last.

Attribution rules (what makes skips sound):

- a test's footprint is everything observed during its setup, call and
  teardown, **plus** the observations of every non-function-scoped fixture
  it uses, **plus** the observations made while importing its module (and
  enclosing packages) during collection;
- shared collectors re-enable monitoring when pushed, so code that already
  ran earlier in the same test isn't missed.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
from typing import IO

import pytest

from . import CAPABILITIES, CAPTURE_VERSION, PROBE_ID
from .capture import Collector, Recorder

_STATE: "_State | None" = None


class _State:
    def __init__(self, recorder: Recorder, out: IO[str]) -> None:
        self.recorder = recorder
        self.out = out
        self.modules: dict[str, Collector] = {}
        self.fixtures: dict[int, Collector] = {}
        self.results: dict[str, list] = {}
        self.emitted_packages: set[str] = set()

    def emit(self, obj: dict) -> None:
        self.out.write(json.dumps(obj, sort_keys=True, separators=(",", ":")) + "\n")

    def emit_new_packages(self, names: set[str]) -> None:
        for name in sorted(names - self.emitted_packages):
            info = self.recorder.seen_packages.get(name)
            if info is None:
                continue
            self.emit(
                {
                    "type": "package",
                    "name": info.name,
                    "version": info.version,
                    "native": info.native,
                    "dist_hash": info.dist_hash,
                }
            )
            self.emitted_packages.add(name)


def pytest_addoption(parser: pytest.Parser) -> None:
    g = parser.getgroup("skipcert")
    g.addoption("--skipcert-record", metavar="PATH", default=None, help="record per-test footprints to PATH")
    g.addoption("--skipcert-skip", metavar="PATH", default=None, help="deselect the tests named in PATH")
    g.addoption("--skipcert-root", metavar="DIR", default=None, help="repository root (default: git toplevel)")


def _repo_root(start: str, override: str | None) -> str:
    if override:
        return os.path.abspath(override)
    try:
        out = subprocess.run(
            ["git", "rev-parse", "--show-toplevel"], cwd=start, capture_output=True, text=True, check=True
        )
        return os.path.abspath(out.stdout.strip())
    except (OSError, subprocess.CalledProcessError):
        return os.path.abspath(start)


def item_name(item: pytest.Item, repo_root: str) -> tuple[str, str, str, str]:
    """Returns (name, path, qual, params) using repository-relative paths.

    ``name`` is ``path::qual`` plus params, e.g. ``tests/test_a.py::TestC::test_m[1]``.
    It must match how the Rust side derives stable test IDs.
    """
    rel = os.path.relpath(os.path.realpath(str(item.path)), os.path.realpath(repo_root)).replace(os.sep, "/")
    suffix = item.nodeid.split("::", 1)[1] if "::" in item.nodeid else item.name
    qual, params = suffix, ""
    last = suffix.rsplit("::", 1)[-1]
    if last.endswith("]") and "[" in last:
        cut = len(suffix) - (len(last) - last.index("["))
        qual, params = suffix[:cut], suffix[cut:]
    return f"{rel}::{qual}{params}", rel, qual, params


# Lifecycle ----------------------------------------------------------------


@pytest.hookimpl(tryfirst=True)
def pytest_load_initial_conftests(early_config: pytest.Config, parser: pytest.Parser, args: list[str]) -> None:
    """Starts recording *before* initial conftests are imported, so their
    imports land in the root scope."""
    global _STATE
    ns = early_config.known_args_namespace
    path = getattr(ns, "skipcert_record", None)
    if not path or _STATE is not None:
        return
    worker = os.environ.get("PYTEST_XDIST_WORKER")
    if worker:
        path = f"{path}.{worker}"
    root = _repo_root(str(early_config.invocation_params.dir), getattr(ns, "skipcert_root", None))
    out = open(path, "w", encoding="utf-8", newline="\n")  # noqa: SIM115 - closed at session end
    recorder = Recorder(root)
    _STATE = _State(recorder, out)
    _STATE.emit(
        {
            "type": "header",
            "v": CAPTURE_VERSION,
            "probe": PROBE_ID,
            "capabilities": list(CAPABILITIES),
            "python": sys.version.split()[0],
            "implementation": sys.implementation.name,
            "repo_root": root.replace(os.sep, "/"),
        }
    )
    recorder.start()
    # Repository modules imported before recording started belong to the root scope.
    for mod in list(sys.modules.values()):
        f = getattr(mod, "__file__", None)
        if f:
            cls = recorder.classify_path(f)
            if cls is not None and cls[0] == "file":
                recorder.root.files.add(cls[1])


@pytest.hookimpl(trylast=True)
def pytest_sessionfinish(session: pytest.Session) -> None:
    global _STATE
    st = _STATE
    if st is None:
        return
    st.recorder.stop()
    root = st.recorder.root
    st.emit_new_packages(root.packages)
    st.emit(
        {
            "type": "root",
            "files": sorted(root.files),
            "functions": sorted([list(f) for f in root.functions]),
            "packages": sorted(root.packages),
        }
    )
    st.out.close()
    _STATE = None


# Attribution -------------------------------------------------------------


@pytest.hookimpl(wrapper=True)
def pytest_make_collect_report(collector: pytest.Collector):
    st = _STATE
    if st is None or not isinstance(collector, (pytest.Module, pytest.Package)):
        return (yield)
    c = st.modules.setdefault(collector.nodeid, Collector())
    st.recorder.push(c, fresh=True)
    try:
        return (yield)
    finally:
        st.recorder.pop(c)


@pytest.hookimpl(wrapper=True)
def pytest_fixture_setup(fixturedef, request):
    st = _STATE
    if st is None or fixturedef.scope == "function":
        return (yield)
    c = st.fixtures.setdefault(id(fixturedef), Collector())
    st.recorder.push(c, fresh=True)
    try:
        return (yield)
    finally:
        st.recorder.pop(c)


@pytest.hookimpl(wrapper=True)
def pytest_runtest_protocol(item: pytest.Item, nextitem):
    st = _STATE
    if st is None:
        return (yield)
    st.recorder.boundary()
    own = Collector()
    st.recorder.push(own)
    try:
        return (yield)
    finally:
        st.recorder.pop(own)
        _emit_test(st, item, own)


def _emit_test(st: _State, item: pytest.Item, own: Collector) -> None:
    total = Collector()
    total.merge(own)
    for node in item.listchain():
        m = st.modules.get(node.nodeid)
        if m is not None:
            total.merge(m)
    info = getattr(item, "_fixtureinfo", None)
    if info is None:
        if st.fixtures:
            total.flags.add("untraced")  # can't tell which shared fixtures apply
    else:
        for defs in info.name2fixturedefs.values():
            for d in defs:
                f = st.fixtures.get(id(d))
                if f is not None:
                    total.merge(f)
    name, path, qual, params = item_name(item, st.recorder.repo_root_real)
    outcome, duration = st.results.pop(item.nodeid, ["unknown", 0.0])
    st.emit_new_packages(total.packages)
    st.emit(
        {
            "type": "test",
            "name": name,
            "path": path,
            "qual": qual,
            "params": params,
            "outcome": outcome,
            "duration_ms": int(duration * 1000),
            "files": sorted(total.files),
            "functions": sorted([list(f) for f in total.functions]),
            "packages": sorted(total.packages),
            "flags": sorted(total.flags),
        }
    )


def pytest_runtest_logreport(report: pytest.TestReport) -> None:
    st = _STATE
    if st is None:
        return
    entry = st.results.setdefault(report.nodeid, ["passed", 0.0])
    entry[1] += report.duration
    if report.failed:
        entry[0] = "failed"
    elif report.skipped and entry[0] == "passed":
        entry[0] = "skipped"


# Selection ----------------------------------------------------------------


def pytest_collection_modifyitems(session: pytest.Session, config: pytest.Config, items: list[pytest.Item]) -> None:
    path = config.getoption("skipcert_skip")
    if not path:
        return
    with open(path, encoding="utf-8") as f:
        skip = {line.strip() for line in f if line.strip()}
    root = _repo_root(str(config.invocation_params.dir), config.getoption("skipcert_root"))
    keep, drop = [], []
    for item in items:
        (drop if item_name(item, root)[0] in skip else keep).append(item)
    if drop:
        config.hook.pytest_deselected(items=drop)
        items[:] = keep
