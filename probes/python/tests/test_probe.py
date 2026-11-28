"""Probe tests: run real pytest sessions in a temporary project and check the capture.

One recorded session is shared by all capture tests (session-scoped fixture),
so the suite starts only two inner pytest processes.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import textwrap

import pytest

from quod_probe import fingerprint
from quod_probe.capture import entity_qualname
from quod_probe.packages import SITE_PSEUDO_PACKAGE, normalize_name

APP_BILLING = """
from app.util import fmt

RATE = 2

def charge(x):
    return fmt(x * RATE)

def refund(x):
    return -x
"""

APP_UTIL = """
def fmt(v):
    return f"{v}"

def unused():
    return 1
"""

TEST_PAY = """
import pytest
from app.billing import charge

@pytest.fixture(scope="module")
def shared():
    from app.util import unused
    return unused()

def test_charge(shared):
    assert charge(2) == "4"

def test_shared_again(shared):
    assert shared == 1

@pytest.mark.parametrize("n", [1, 2])
def test_param(n):
    assert n

class TestC:
    def test_m(self):
        assert True
"""

APP_PRIMES = """
from functools import cache, cached_property, lru_cache

def helper(n):
    return n % 2

@lru_cache
def cached(n):
    return helper(n)

@cache
def cached2(n):
    return n + 1

class Box:
    @cached_property
    def value(self):
        return helper(3)

BOX = Box()
"""

TEST_CACHE = """
from app.primes import BOX, cached, cached2

def test_warm():
    assert cached(3) == 1 and cached2(1) == 2 and BOX.value == 1

def test_hit():
    assert cached(3) == 1 and cached2(1) == 2 and BOX.value == 1
"""

TEST_SYS = """
import os
import subprocess
import sys

def test_sub():
    subprocess.run([sys.executable, "-c", "pass"], check=True)

def test_list():
    os.listdir("app")

def test_lambda():
    f = lambda: 1
    assert f() == 1
"""

APP_API = """
from functools import wraps

def handler(a, b=1) -> int:
    return a

def deco(f):
    @wraps(f)
    def inner(*args, **kwargs):
        return f(*args, **kwargs)
    return inner

@deco
def wrapped(x: int = 3):
    return x

class Model:
    def __init__(self, x: int = 0):
        self.x = x

    def method(self, y=2):
        return y
"""

TEST_INTRO = """
import functools
import inspect
import typing

from app.api import Model, handler, wrapped

def test_signature():
    assert list(inspect.signature(handler).parameters) == ["a", "b"]

def test_hints():
    assert typing.get_type_hints(handler)["return"] is int

def test_wrapped():
    assert "x" in inspect.signature(wrapped).parameters

def test_class():
    assert "x" in inspect.signature(Model).parameters

def test_bound():
    assert "y" in inspect.signature(object.__new__(Model).method).parameters

def test_partial():
    assert list(inspect.signature(functools.partial(handler, 1)).parameters) == ["b"]
"""

TEST_ENV = """
import os

def test_env_read():
    assert os.environ.get("QPT_A") == "1"
    assert "QPT_MISSING" not in os.environ

def test_env_written(monkeypatch):
    monkeypatch.setenv("QPT_B", "x")
    assert os.environ["QPT_B"] == "x"

def test_env_iter(monkeypatch):
    monkeypatch.setenv("ZZ_QPT_C", "y")
    assert len(dict(os.environ)) > 0

def test_env_items():
    # networkx: settings from every variable with a prefix (ADR 021).
    assert {k: v for k, v in os.environ.items() if k.startswith("QPT_PREFIX_")} == {}

def test_env_runner():
    assert os.environ.get("PYTEST_VERSION")
    assert os.environ.get("PYTEST_CURRENT_TEST")
"""

ENV = {**os.environ, "QPT_A": "1"}
ENV.pop("QPT_MISSING", None)
ENV.pop("QPT_B", None)


def _pytest(d, *args: str) -> subprocess.CompletedProcess:
    return subprocess.run(
        [sys.executable, "-m", "pytest", "-p", "no:cacheprovider", "-W", "error::DeprecationWarning", f"--quod-root={d}", *args],
        cwd=d,
        capture_output=True,
        text=True,
        env=ENV,
    )


@pytest.fixture(scope="session")
def project_dir(tmp_path_factory):
    d = tmp_path_factory.mktemp("proj")
    (d / "app").mkdir()
    (d / "app" / "__init__.py").write_text("")
    (d / "app" / "billing.py").write_text(textwrap.dedent(APP_BILLING))
    (d / "app" / "util.py").write_text(textwrap.dedent(APP_UTIL))
    (d / "app" / "primes.py").write_text(textwrap.dedent(APP_PRIMES))
    (d / "app" / "api.py").write_text(textwrap.dedent(APP_API))
    (d / "tests").mkdir()
    (d / "tests" / "test_pay.py").write_text(textwrap.dedent(TEST_PAY))
    (d / "tests" / "test_sys.py").write_text(textwrap.dedent(TEST_SYS))
    (d / "tests" / "test_cache.py").write_text(textwrap.dedent(TEST_CACHE))
    (d / "tests" / "test_intro.py").write_text(textwrap.dedent(TEST_INTRO))
    (d / "tests" / "test_env.py").write_text(textwrap.dedent(TEST_ENV))
    (d / "conftest.py").write_text("import app.util\n")
    return d


@pytest.fixture(scope="session")
def captured(project_dir):
    out = project_dir / "capture.jsonl"
    res = _pytest(project_dir, f"--quod-record={out}")
    assert res.returncode == 0, res.stdout + res.stderr
    recs = [json.loads(line) for line in out.read_text(encoding="utf-8").splitlines()]
    tests = {r["name"]: r for r in recs if r["type"] == "test"}
    root = next(r for r in recs if r["type"] == "root")
    return res, recs, tests, root


def fns(rec: dict) -> set[tuple[str, str]]:
    return {tuple(f) for f in rec["functions"]}


def test_header_and_names(captured):
    res, recs, tests, _ = captured
    assert "21 passed" in res.stdout
    assert recs[0]["type"] == "header" and recs[0]["v"] == 1
    assert recs[0]["capabilities"] == ["py.environment", "py.introspection"]
    assert recs[-1]["type"] == "root"
    assert set(tests) >= {
        "tests/test_pay.py::test_charge",
        "tests/test_pay.py::test_param[1]",
        "tests/test_pay.py::TestC::test_m",
    }
    p1 = tests["tests/test_pay.py::test_param[1]"]
    assert (p1["path"], p1["qual"], p1["params"]) == ("tests/test_pay.py", "test_param", "[1]")
    assert tests["tests/test_pay.py::test_charge"]["outcome"] == "passed"


def test_function_level_footprint(captured):
    _, _, tests, _ = captured
    t = tests["tests/test_pay.py::test_charge"]
    assert ("app/billing.py", "charge") in fns(t)
    assert ("app/util.py", "fmt") in fns(t)
    assert ("app/billing.py", "refund") not in fns(t)
    # The test module's import (collection) is attributed to its tests.
    assert "tests/test_pay.py" in t["files"]
    assert "app/billing.py" in t["files"]


def test_shared_fixture_is_attributed_to_every_user(captured):
    _, _, tests, _ = captured
    # `shared` (module scope) runs once, during test_charge's setup. The second
    # user must still carry what the fixture executed.
    assert ("app/util.py", "unused") in fns(tests["tests/test_pay.py::test_charge"])
    assert ("app/util.py", "unused") in fns(tests["tests/test_pay.py::test_shared_again"])
    # A test that doesn't use the fixture doesn't get it.
    assert ("app/util.py", "unused") not in fns(tests["tests/test_pay.py::TestC::test_m"])


def test_root_scope_captures_conftest_imports(captured):
    _, _, _, root = captured
    assert "conftest.py" in root["files"]
    assert "app/util.py" in root["files"]


def test_behaviour_flags(captured):
    _, _, tests, _ = captured
    assert "subprocess" in tests["tests/test_sys.py::test_sub"]["flags"]
    assert "dynamic_import" in tests["tests/test_sys.py::test_list"]["flags"]
    assert tests["tests/test_pay.py::test_charge"]["flags"] == [], "imports must not look like discovery"


def test_cache_hits_still_record_the_cached_function(captured):
    """Regression for a conformance escape (more-itertools, lru_cache).

    test_warm fills the caches. test_hit gets cache hits that execute no Python
    code, so without cache clearing its footprint would miss the cached
    functions, and a change to them would wrongly skip it.
    """
    _, _, tests, _ = captured
    hit = fns(tests["tests/test_cache.py::test_hit"])
    for q in ("cached", "cached2", "helper", "Box.value"):
        assert ("app/primes.py", q) in hit, q


def test_lambdas_map_to_enclosing_entity(captured):
    _, _, tests, _ = captured
    f = fns(tests["tests/test_sys.py::test_lambda"])
    assert ("tests/test_sys.py", "test_lambda") in f
    assert not any("<" in q for _, q in f)


def test_packages_are_emitted_before_use(captured):
    _, recs, _, _ = captured
    declared = set()
    for r in recs:
        if r["type"] == "package":
            declared.add(r["name"])
            if r["name"] != SITE_PSEUDO_PACKAGE:
                assert r["name"] == normalize_name(r["name"])
        elif r["type"] in ("test", "root"):
            assert set(r["packages"]) <= declared, r.get("name")


def test_skip_file_deselects_named_tests(project_dir):
    skip = project_dir / "skip.txt"
    skip.write_text("tests/test_pay.py::test_charge\ntests/test_pay.py::test_param[2]\n")
    res = _pytest(project_dir, f"--quod-skip={skip}")
    assert res.returncode == 0, res.stdout + res.stderr
    assert "19 passed, 2 deselected" in res.stdout


@pytest.mark.parametrize(
    ("qual", "expected"),
    [
        ("f", "f"),
        ("C.m", "C.m"),
        ("outer.<locals>.inner", "outer.<locals>.inner"),
        ("f.<locals>.<lambda>", "f"),
        ("C.<lambda>", "C"),
        ("<lambda>", None),
        ("<genexpr>", None),
        ("f.<locals>.<listcomp>", "f"),
    ],
)
def test_entity_qualname(qual, expected):
    assert entity_qualname(qual) == expected


@pytest.mark.parametrize(
    ("test", "entity"),
    [
        ("test_signature", "handler"),
        ("test_hints", "handler"),
        ("test_wrapped", "wrapped"),
        ("test_class", "Model.__init__"),
        ("test_bound", "Model.method"),
        ("test_partial", "handler"),
    ],
)
def test_introspection_counts_as_use(captured, test, entity):
    """A test that inspects a signature without calling the function still
    depends on its header (parameter names, defaults, annotations)."""
    _, _, tests, _ = captured
    assert ("app/api.py", entity) in fns(tests[f"tests/test_intro.py::{test}"])


def test_introspection_is_not_a_global_use(captured):
    _, _, tests, _ = captured
    assert ("app/api.py", "handler") not in fns(tests["tests/test_pay.py::test_charge"])


def env_of(rec: dict) -> dict[str, str]:
    return dict(map(tuple, rec["env"]))


def test_environment_and_invocation_fingerprints(captured):
    _, recs, _, _ = captured
    h = recs[0]
    env = h["environment"]
    assert env["interpreter.version"] == sys.version
    assert "dist.pytest" in env and "dist.__site__" in env
    assert all(k.split(".")[0] in {"interpreter", "system", "process_env", "dist"} for k in env)
    args = json.loads(h["invocation"]["invocation.args"])
    assert not any(a.startswith("--quod") for a in args)
    assert "-p" in args  # the rest of the command line is kept
    assert h["invocation"]["invocation.dir"] == "."  # started at the repository root


def test_fingerprint_cli_matches_the_recorded_environment(captured, project_dir):
    """What ``select`` computes outside pytest matches what was recorded
    inside it, including ``env:*`` after pytest set ``PYTEST_VERSION`` and a
    test wrote a variable before enumerating."""
    _, recs, tests, _ = captured
    out = subprocess.run(
        [sys.executable, "-m", "quod_probe", "fingerprint", "--env", "QPT_A", "--env", "NOPE_X", "--env", "*"],
        cwd=project_dir,
        capture_output=True,
        text=True,
        env=ENV,
        check=True,
    )
    fp = json.loads(out.stdout)
    assert fp["environment"] == recs[0]["environment"]
    assert fp["env"].pop("*") == env_of(tests["tests/test_env.py::test_env_iter"])["*"]
    assert fp["env"] == {"QPT_A": fingerprint.env_value_hash("1"), "NOPE_X": fingerprint.env_value_hash(None)}


def test_environment_reads_are_recorded_per_test(captured):
    _, _, tests, _ = captured
    read = env_of(tests["tests/test_env.py::test_env_read"])
    assert read["QPT_A"] == fingerprint.env_value_hash("1")
    assert read["QPT_MISSING"] == fingerprint.env_value_hash(None)
    assert "*" not in read
    # A key the test wrote itself is in-process state, not an input.
    assert "QPT_B" not in env_of(tests["tests/test_env.py::test_env_written"])
    assert "*" in env_of(tests["tests/test_env.py::test_env_iter"])
    assert "QPT_A" not in env_of(tests["tests/test_pay.py::test_charge"])
    # A bulk read is one whole-environment observation, not one per key.
    items = env_of(tests["tests/test_env.py::test_env_items"])
    assert "*" in items and "QPT_A" not in items and "PATH" not in items
    # Variables pytest sets for itself aren't inputs.
    assert env_of(tests["tests/test_env.py::test_env_runner"]).keys().isdisjoint(fingerprint.RUNNER_ENV_NAMES)


def _skip_file(project_dir, *header: str):
    skip = project_dir / "skip-checked.txt"
    skip.write_text("\n".join([*header, "tests/test_pay.py::test_charge"]) + "\n")
    return skip


def test_skip_file_checks_pass_when_everything_matches(captured, project_dir):
    _, recs, _, _ = captured
    h = recs[0]
    skip = _skip_file(
        project_dir,
        f"# quod-environment {fingerprint.digest(h['environment'])}",
        f"# quod-invocation {fingerprint.digest(h['invocation'])}",
        f"# quod-env QPT_A {fingerprint.env_value_hash('1')}",
    )
    res = _pytest(project_dir, f"--quod-skip={skip}")
    assert "20 passed, 1 deselected" in res.stdout, res.stdout + res.stderr


@pytest.mark.parametrize(
    ("line", "why"),
    [
        ("# quod-environment 00", "environment differs"),
        ("# quod-invocation 00", "invocation"),
        ("# quod-env QPT_A " + fingerprint.env_value_hash("2"), "QPT_A differs"),
        ("# quod-something new", "unknown"),
    ],
)
def test_skip_file_mismatch_runs_everything(project_dir, line, why):
    res = _pytest(project_dir, f"--quod-skip={_skip_file(project_dir, line)}")
    assert "21 passed" in res.stdout and "deselected" not in res.stdout, res.stdout
    assert why in res.stderr


def test_results_file_names_match_recorded_names(captured, project_dir):
    """``quod verdict`` joins results to the map by name, so the results
    file must spell every test exactly as the recorder does."""
    _, _, tests, _ = captured
    out = project_dir / "results.jsonl"
    res = _pytest(project_dir, f"--quod-results={out}")
    assert res.returncode == 0, res.stdout + res.stderr
    results = [json.loads(line) for line in out.read_text(encoding="utf-8").splitlines()]
    assert {r["name"] for r in results} == set(tests)
    assert all(r["type"] == "result" and r["outcome"] in {"passed", "skipped"} for r in results)


def test_ci_bookkeeping_is_not_part_of_the_whole_environment():
    """ADR 021: per-run CI identifiers don't change ``env:*``; other
    variables do."""
    base = {"PATH": "/bin", "GITHUB_ACTIONS": "true", "GITHUB_RUN_ID": "1", "ACTIONS_ORCHESTRATION_ID": "a"}
    same = {**base, "GITHUB_RUN_ID": "2", "ACTIONS_ORCHESTRATION_ID": "b", "GITHUB_SHA": "f00",
            "QUOD": "/tmp/quod", "_": "/usr/bin/python", "OLDPWD": "/",
            "ImageVersion": "20261123.1", "GRADLE_HOME": "/usr/share/gradle-9.2", "JAVA_HOME_21_X64": "/x"}
    assert fingerprint.environ_hash(base) == fingerprint.environ_hash(same)
    assert fingerprint.environ_hash(base) != fingerprint.environ_hash({**base, "GITHUB_ACTIONS": "false"})
    assert fingerprint.environ_hash(base) != fingerprint.environ_hash({**base, "MY_SETTING": "1"})


def test_quod_options_are_stripped_in_both_forms():
    from quod_probe.fingerprint import strip_quod_args

    args = ["-q", "--quod-results", "/tmp/r.jsonl", "--quod-skip=/s", "--quod-root", "/r", "tests"]
    assert strip_quod_args(args) == ["-q", "tests"]


def test_skip_file_outside_the_project_needs_the_equals_form(tmp_path):
    """pytest picks its rootdir and ini file before plugin options are
    parsed, so ``--quod-skip PATH`` makes PATH (an existing file) a test
    path: rootdir moves to PATH's directory and the project's ini file is
    lost. ``--quod-skip=PATH`` doesn't. Found by the GitHub Action, whose
    skip file lives in the runner's temp directory."""
    proj = tmp_path / "proj"
    (proj / "tests").mkdir(parents=True)
    (proj / "pyproject.toml").write_text('[tool.pytest.ini_options]\ntestpaths = ["tests"]\n')
    (proj / "tests" / "test_a.py").write_text("def test_one():\n    pass\n\ndef test_two():\n    pass\n")
    cap = tmp_path / "capture.jsonl"
    res = _pytest(proj, f"--quod-record={cap}")
    assert res.returncode == 0, res.stdout + res.stderr
    header = json.loads(cap.read_text(encoding="utf-8").splitlines()[0])
    elsewhere = tmp_path / "temp"
    elsewhere.mkdir()
    skip = elsewhere / "skip.txt"
    skip.write_text(f"# quod-invocation {fingerprint.digest(header['invocation'])}\ntests/test_a.py::test_one\n")

    res = _pytest(proj, f"--quod-skip={skip}")
    assert "1 passed, 1 deselected" in res.stdout, res.stdout + res.stderr

    res = _pytest(proj, "--quod-skip", str(skip))
    assert "deselected" not in res.stdout, res.stdout
    assert "Pass --quod-skip=PATH, not --quod-skip PATH" in res.stderr, res.stderr


CACHED_PROPS = """
import gc
import sys
import weakref
from functools import cached_property


class Node:
    def __init__(self, child=None):
        self.child = child

    @cached_property
    def depth(self):
        return 0 if self.child is None else self.child.depth + 1


def test_object_is_freed_after_reading_a_cached_property():
    # networkx: views are cached properties, and the graph must be freed.
    gc.disable()
    try:
        n = Node()
        assert n.depth == 0
        ref = weakref.ref(n)
        del n
        assert ref() is None
    finally:
        gc.enable()


def test_deeply_nested_cached_properties():
    # sqlfluff: recursive properties near the recursion limit. Each level
    # takes two frames (``__get__`` and ``depth``); a third would overflow.
    levels = sys.getrecursionlimit() // 2 - 80
    n = None
    for _ in range(levels):
        n = Node(n)
    assert n.depth == levels - 1
"""


def test_recording_doesnt_change_cached_property_behaviour(tmp_path):
    """Found by E9 on networkx and sqlfluff: the probe's cached_property
    held every instance it saw (so objects weren't freed), and added a frame
    per access (so deep recursion hit the limit only while recording)."""
    proj = tmp_path / "proj"
    (proj / "tests").mkdir(parents=True)
    (proj / "tests" / "test_props.py").write_text(textwrap.dedent(CACHED_PROPS))
    plain = _pytest(proj, "tests")
    assert "2 passed" in plain.stdout, plain.stdout + plain.stderr
    rec = _pytest(proj, "tests", f"--quod-record={tmp_path / 'cap.jsonl'}")
    assert "2 passed" in rec.stdout, rec.stdout + rec.stderr


def test_a_conftest_setting_a_variable_doesnt_void_the_skip_file(tmp_path):
    """E9 on networkx: its conftest sets OMP_NUM_THREADS, and the skip
    file's env:* check hashed the environment after that, so nothing was
    skipped. The checks use the environment as pytest started."""
    proj = tmp_path / "proj"
    (proj / "tests").mkdir(parents=True)
    (proj / "conftest.py").write_text("import os\nos.environ['QPT_SET_BY_CONFTEST'] = '1'\n")
    (proj / "tests" / "test_a.py").write_text("def test_one():\n    pass\n\ndef test_two():\n    pass\n")
    skip = tmp_path / "skip.txt"
    skip.write_text(f"# quod-env * {fingerprint.environ_hash(ENV)}\ntests/test_a.py::test_one\n")
    res = _pytest(proj, f"--quod-skip={skip}")
    assert "1 passed, 1 deselected" in res.stdout, res.stdout + res.stderr


def test_real_paths_match_realpath(tmp_path):
    """The per-directory resolution gives exactly ``normcase(realpath())``,
    including for a file that is itself a symlink."""
    from quod_probe.packages import _RealPaths

    (tmp_path / "pkg").mkdir()
    target = tmp_path / "pkg" / "mod.py"
    target.write_text("x = 1\n")
    paths = [target, tmp_path / "pkg" / ".." / "pkg" / "mod.py", tmp_path / "pkg" / "missing.py"]
    try:
        (tmp_path / "pkg" / "link.py").symlink_to(target)
        paths.append(tmp_path / "pkg" / "link.py")
    except OSError:
        pass  # no symlink privilege (Windows without developer mode)
    real = _RealPaths()
    for p in paths:
        assert real(str(p)) == os.path.normcase(os.path.realpath(p)), p


def test_cached_imports_carry_the_import_closure(tmp_path):
    """A module's body runs once, in the first importer's scope. Every later
    importer still depends on it (and on what it imported): a changed
    constant in ``lib/helper.py`` must reach every test that reads
    ``lib.consts.CONST``, however it got the module."""
    d = tmp_path
    (d / "lib").mkdir()
    (d / "lib" / "__init__.py").write_text("")
    (d / "lib" / "helper.py").write_text("BASE = 1\n")
    (d / "lib" / "consts.py").write_text("from lib.helper import BASE\nCONST = BASE + 1\n")
    (d / "lib" / "rel.py").write_text("from . import consts\nVALUE = consts.CONST\n")
    (d / "tests").mkdir()
    files = {
        # Loads lib.consts (and lib.helper) first.
        "test_a.py": "import lib.consts\n\ndef test_a():\n    assert lib.consts.CONST == 2\n",
        # Cached: ``from package import submodule``.
        "test_b.py": "from lib import consts\n\ndef test_b():\n    assert consts.CONST == 2\n",
        # Cached, inside the test.
        "test_c.py": "def test_c():\n    import lib.consts as c\n    assert c.CONST == 2\n",
        # Cached, through importlib.
        "test_d.py": "import importlib\n\ndef test_d():\n    assert importlib.import_module('lib.consts').CONST == 2\n",
        # Imports nothing from lib.
        "test_e.py": "def test_e():\n    assert True\n",
        # New module whose body does a cached relative import.
        "test_f.py": "from lib import rel\n\ndef test_f():\n    assert rel.VALUE == 2\n",
    }
    for name, src in files.items():
        (d / "tests" / name).write_text(src)
    out = d / "capture.jsonl"
    res = _pytest(d, f"--quod-record={out}", "tests")
    assert res.returncode == 0, res.stdout + res.stderr
    tests = {
        r["name"].split("::")[-1]: set(r["files"])
        for r in map(json.loads, out.read_text(encoding="utf-8").splitlines())
        if r["type"] == "test"
    }
    for t in ("test_a", "test_b", "test_c", "test_d", "test_f"):
        assert {"lib/consts.py", "lib/helper.py"} <= tests[t], (t, tests[t])
    assert not any(f.startswith("lib/") for f in tests["test_e"]), tests["test_e"]


def test_skip_file_lists_whole_files_that_are_not_collected(tmp_path):
    """``# quod-skip-file`` files aren't imported at all, but only once the
    skip file's conditions hold; otherwise everything is collected."""
    d = tmp_path
    (d / "tests").mkdir()
    (d / "tests" / "test_a.py").write_text("def test_a():\n    assert True\n")
    # Importing this file fails: collecting it would be a collection error.
    (d / "tests" / "test_b.py").write_text("raise RuntimeError('collected')\n\ndef test_b():\n    pass\n")
    skip = d / "skip.txt"
    skip.write_text("# quod-skip-file tests/test_b.py\ntests/test_b.py::test_b\n")
    res = _pytest(d, f"--quod-skip={skip}", "tests")
    assert res.returncode == 0, res.stdout + res.stderr
    assert "1 passed" in res.stdout

    # A condition that doesn't hold: nothing is skipped, so test_b.py is
    # collected (and fails to import).
    skip.write_text("# quod-env QUOD_TEST_UNSET_VAR 00\n# quod-skip-file tests/test_b.py\ntests/test_b.py::test_b\n")
    res = _pytest(d, f"--quod-skip={skip}", "tests")
    assert res.returncode != 0
    assert "not skipping any tests" in res.stderr
    assert "collected" in res.stdout + res.stderr


def test_runner_state_is_not_recorded(tmp_path):
    """pytest's cache and bytecode are the runner's own state: a fresh CI
    checkout has none, so recording them made every test look changed."""
    d = tmp_path
    (d / "tests").mkdir()
    (d / "tests" / "test_a.py").write_text("def test_a():\n    assert True\n")
    out = d / "capture.jsonl"
    cmd = [sys.executable, "-m", "pytest", "-q", f"--quod-root={d}", "tests"]
    # The first run creates .pytest_cache; the recorded second run reads it.
    subprocess.run(cmd, cwd=d, capture_output=True, text=True, env=ENV, check=True)
    res = subprocess.run([*cmd, f"--quod-record={out}"], cwd=d, capture_output=True, text=True, env=ENV)
    assert res.returncode == 0, res.stdout + res.stderr
    assert (d / ".pytest_cache").is_dir()
    files = set()
    for r in map(json.loads, out.read_text(encoding="utf-8").splitlines()):
        files |= set(r.get("files", []))
    assert not any(".pytest_cache/" in f or "__pycache__/" in f for f in files), files
