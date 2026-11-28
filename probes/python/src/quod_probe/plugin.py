"""pytest plugin.

Options:

``--quod-record=PATH``
    Record per-test footprints to a capture file (JSON Lines, protocol v1).
    Under pytest-xdist each worker writes ``PATH.<worker id>``.
``--quod-skip=PATH``
    Deselect the tests named in PATH (one test name per line), as produced
    by ``quod select``. Every test not listed runs. Files listed as
    ``# quod-skip-file PATH`` (every recorded test in them is skipped) aren't
    collected at all. Header lines
    (``# quod-environment D``, ``# quod-invocation D``,
    ``# quod-env NAME H``) are checked against this process first. On
    any mismatch nothing is deselected (ADR 012).
``--quod-results=PATH``
    Write each test's outcome to PATH (JSON Lines: ``{"type": "result",
    "name", "outcome", "duration_ms"}``), for ``quod verdict``. Under
    pytest-xdist only the controller writes it.
``--quod-root=DIR``
    Repository root (default: ``git rev-parse --show-toplevel``, else rootdir).

Capture protocol v1 (one JSON object per line):

- ``header``: probe ID, protocol version, capabilities, Python version,
  repository root, environment and invocation fingerprints.
- ``package``: name, version, native, dist_hash. Emitted once per package,
  before its first use.
- ``test``: name, path, qual, params, outcome, duration_ms, files,
  functions (``[path, qualname]``), packages, flags, env (``[name, hash]``).
- ``root``: root-scope files, functions, packages and env (conftest, plugins,
  session start). Emitted last.

Attribution rules (what makes skips sound):

- a test's footprint is everything observed during its setup, call and
  teardown, **plus** the observations of every non-function-scoped fixture
  it uses, **plus** the observations made while importing its module (and
  enclosing packages) during collection;
- every import, including a cached one, adds the imported module and its
  import closure (what loading it observed) to the current scope (ADR 019);
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

from . import CAPABILITIES, CAPTURE_VERSION, PROBE_ID, fingerprint
from .capture import Collector, Recorder

_STATE: "_State | None" = None

# The environment as it was when pytest loaded the plugin, before any
# conftest or plugin code ran: what recording and ``quod select`` see. A
# conftest that sets a variable (networkx: ``OMP_NUM_THREADS``) must not make
# the skip file's checks fail.
_OUTER_ENV: dict[str, str] = dict(os.environ)


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
    g = parser.getgroup("quod")
    g.addoption("--quod-record", metavar="PATH", default=None, help="record per-test footprints to PATH")
    g.addoption("--quod-skip", metavar="PATH", default=None, help="deselect the tests named in PATH")
    g.addoption("--quod-root", metavar="DIR", default=None, help="repository root (default: git toplevel)")
    g.addoption("--quod-results", metavar="PATH", default=None, help="write per-test outcomes to PATH (JSON Lines)")


class ResultsWriter:
    """Collects per-test outcomes from reports and writes them at session end.

    Names are derived from node IDs so the xdist controller, which never sees
    the items, produces the same names as the recorder (``item_name``).
    """

    def __init__(self, path: str, rootdir: str, repo_root: str) -> None:
        self.path = path
        self.rootdir = os.path.realpath(rootdir)
        self.repo_root = os.path.realpath(repo_root)
        self.results: dict[str, list] = {}

    def name(self, nodeid: str) -> str:
        file, _, rest = nodeid.partition("::")
        rel = os.path.relpath(os.path.realpath(os.path.join(self.rootdir, file)), self.repo_root)
        rel = rel.replace(os.sep, "/")
        return f"{rel}::{rest}" if rest else rel

    def pytest_runtest_logreport(self, report: pytest.TestReport) -> None:
        entry = self.results.setdefault(report.nodeid, ["passed", 0.0])
        entry[1] += report.duration
        if report.failed:
            entry[0] = "failed"
        elif report.skipped and entry[0] == "passed":
            entry[0] = "skipped"

    @pytest.hookimpl(trylast=True)
    def pytest_sessionfinish(self, session: pytest.Session) -> None:
        with open(self.path, "w", encoding="utf-8") as f:
            for nodeid, (outcome, duration) in sorted(self.results.items()):
                rec = {"type": "result", "name": self.name(nodeid), "outcome": outcome,
                       "duration_ms": round(duration * 1000, 3)}
                f.write(json.dumps(rec, sort_keys=True))
                f.write("\n")


def pytest_configure(config: pytest.Config) -> None:
    path = config.getoption("quod_results", None)
    if path and not hasattr(config, "workerinput"):
        root = _repo_root(str(config.rootpath), config.getoption("quod_root"))
        config.pluginmanager.register(ResultsWriter(os.path.abspath(path), str(config.rootpath), root), "quod-results")


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
    path = getattr(ns, "quod_record", None)
    if not path or _STATE is not None:
        return
    worker = os.environ.get("PYTEST_XDIST_WORKER")
    if worker:
        path = f"{path}.{worker}"
    root = _repo_root(str(early_config.invocation_params.dir), getattr(ns, "quod_root", None))
    out = open(path, "w", encoding="utf-8", newline="\n")  # noqa: SIM115 - closed at session end
    recorder = Recorder(root)
    _STATE = _State(recorder, out)
    capabilities = list(CAPABILITIES)
    try:
        env_fp = fingerprint.environment(recorder.packages)
        inv_fp = fingerprint.invocation(early_config, root)
        fingerprints = {
            "environment": env_fp,
            "invocation": inv_fp,
            "environment_digest": fingerprint.digest(env_fp),
            "invocation_digest": fingerprint.digest(inv_fp),
        }
    except Exception as e:  # noqa: BLE001 - a probe bug must not break the test run
        # Without fingerprints the map falls back to path mode.
        sys.stderr.write(f"quod: no environment fingerprint ({type(e).__name__}: {e})\n")
        capabilities.remove("py.environment")
        fingerprints = {}
    _STATE.emit(
        {
            "type": "header",
            "v": CAPTURE_VERSION,
            "probe": PROBE_ID,
            "capabilities": capabilities,
            "python": sys.version.split()[0],
            "implementation": sys.implementation.name,
            "repo_root": root.replace(os.sep, "/"),
            **fingerprints,
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
            "env": sorted([list(e) for e in root.env.items()]),
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
            "env": sorted([list(e) for e in total.env.items()]),
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


_NO_PLAN = object()


def _skip_plan(config: pytest.Config) -> tuple[set[str], set[str], str] | None:
    """The skip file's test names and whole test files, once its conditions
    are confirmed in this process; ``None`` (skip nothing) otherwise."""
    cached = getattr(config, "_quod_skip_plan", _NO_PLAN)
    if cached is not _NO_PLAN:
        return cached
    plan = None
    path = config.getoption("quod_skip")
    if path:
        names: set[str] = set()
        files: set[str] = set()
        checks: list[tuple[str, ...]] = []
        # Reading the skip file and checking its conditions (which
        # fingerprints the environment) is the probe's own work. While
        # recording (an incremental run, ADR 024), what it reads goes to a
        # throwaway scope, not the root.
        st = _STATE
        scratch = None
        if st is not None:
            from .capture import Collector

            scratch = Collector()
            st.recorder.push(scratch)
        try:
            with open(path, encoding="utf-8") as f:
                for line in f:
                    line = line.strip()
                    if line.startswith("# quod-skip-file "):
                        files.add(line[len("# quod-skip-file "):])
                    elif line.startswith("# quod-"):
                        checks.append(tuple(line[len("# quod-"):].split()))
                    elif line and not line.startswith("#"):
                        names.add(line)
            problem = _check_skip_file(config, checks)
        except Exception as e:  # noqa: BLE001 - can't confirm the conditions: don't skip
            problem = f"could not check the skip file ({type(e).__name__}: {e})"
        finally:
            if st is not None and scratch is not None:
                st.recorder.pop(scratch)
                # Code first run during the check is disabled for the scope it
                # ran in; re-enable it so the root still records it.
                sys.monitoring.restart_events()
        if problem:
            spaced = _space_separated_quod_options(config.invocation_params.args)
            if spaced:
                problem += (
                    f". Pass {spaced[0]}=PATH, not {spaced[0]} PATH: pytest chooses its rootdir and ini"
                    " file before Quod's options are known, and reads a separate PATH as a test path"
                )
            sys.stderr.write(f"quod: not skipping any tests: {problem}\n")
        else:
            root = _repo_root(str(config.invocation_params.dir), config.getoption("quod_root"))
            plan = (names, files, root)
            config._quod_skip_files = ({f.rsplit("/", 1)[-1] for f in files}, os.path.realpath(root))
    config._quod_skip_plan = plan
    return plan


def pytest_ignore_collect(collection_path, config: pytest.Config):
    """Test files whose every recorded test is skipped aren't collected at
    all: importing them is the slow part of a small run."""
    plan = _skip_plan(config)
    if not plan or not plan[1]:
        return None
    basenames, real_root = config._quod_skip_files
    if collection_path.name not in basenames:
        return None
    try:
        # The same spelling as test names (``item_name``).
        rel = os.path.relpath(os.path.realpath(str(collection_path)), real_root).replace(os.sep, "/")
    except ValueError:  # another drive
        return None
    return True if rel in plan[1] else None


def pytest_collection_modifyitems(session: pytest.Session, config: pytest.Config, items: list[pytest.Item]) -> None:
    plan = _skip_plan(config)
    if not plan:
        return
    skip, _, root = plan
    keep, drop = [], []
    for item in items:
        (drop if item_name(item, root)[0] in skip else keep).append(item)
    if drop:
        config.hook.pytest_deselected(items=drop)
        items[:] = keep


def _space_separated_quod_options(args) -> list[str]:
    """Quod options given as ``--quod-x PATH`` rather than ``--quod-x=PATH``."""
    return [str(a) for a in args if str(a) in fingerprint.QUOD_OPTIONS]


def _check_skip_file(config: pytest.Config, checks: list[tuple[str, ...]]) -> str | None:
    """Re-checks the conditions the certificates assumed, in this process.
    Returns the first mismatch, or ``None``."""
    env_fp: dict[str, str] | None = None
    for c in checks:
        kind, args = c[0], c[1:]
        if kind == "environment" and len(args) == 1:
            if env_fp is None:
                env_fp = fingerprint.environment(environ=_OUTER_ENV)
            if fingerprint.digest(env_fp) != args[0]:
                return "the environment differs from the one the map was recorded in"
        elif kind == "invocation" and len(args) == 1:
            root = _repo_root(str(config.invocation_params.dir), config.getoption("quod_root"))
            if fingerprint.digest(fingerprint.invocation(config, root)) != args[0]:
                return "the pytest invocation or ini configuration differs from the recorded one"
        elif kind == "env" and len(args) == 2:
            name, expected = args
            actual = (
                fingerprint.environ_hash(_OUTER_ENV)
                if name == "*"
                else fingerprint.env_value_hash(_OUTER_ENV.get(fingerprint.env_key(name), _OUTER_ENV.get(name)))
            )
            if actual != expected:
                return f"environment variable {name} differs"
        else:
            return f"unknown skip-file check: {' '.join(c)}"
    return None
