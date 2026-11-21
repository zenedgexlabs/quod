"""Probe tests: run real pytest sessions in a temporary project and check the capture.

One recorded session is shared by all capture tests (session-scoped fixture),
so the suite starts only two inner pytest processes.
"""

from __future__ import annotations

import json
import subprocess
import sys
import textwrap

import pytest

from skipcert_probe.capture import entity_qualname
from skipcert_probe.packages import SITE_PSEUDO_PACKAGE, normalize_name

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


def _pytest(d, *args: str) -> subprocess.CompletedProcess:
    return subprocess.run(
        [sys.executable, "-m", "pytest", "-p", "no:cacheprovider", f"--skipcert-root={d}", *args],
        cwd=d,
        capture_output=True,
        text=True,
    )


@pytest.fixture(scope="session")
def project_dir(tmp_path_factory):
    d = tmp_path_factory.mktemp("proj")
    (d / "app").mkdir()
    (d / "app" / "__init__.py").write_text("")
    (d / "app" / "billing.py").write_text(textwrap.dedent(APP_BILLING))
    (d / "app" / "util.py").write_text(textwrap.dedent(APP_UTIL))
    (d / "tests").mkdir()
    (d / "tests" / "test_pay.py").write_text(textwrap.dedent(TEST_PAY))
    (d / "tests" / "test_sys.py").write_text(textwrap.dedent(TEST_SYS))
    (d / "conftest.py").write_text("import app.util\n")
    return d


@pytest.fixture(scope="session")
def captured(project_dir):
    out = project_dir / "capture.jsonl"
    res = _pytest(project_dir, f"--skipcert-record={out}")
    assert res.returncode == 0, res.stdout + res.stderr
    recs = [json.loads(line) for line in out.read_text(encoding="utf-8").splitlines()]
    tests = {r["name"]: r for r in recs if r["type"] == "test"}
    root = next(r for r in recs if r["type"] == "root")
    return res, recs, tests, root


def fns(rec: dict) -> set[tuple[str, str]]:
    return {tuple(f) for f in rec["functions"]}


def test_header_and_names(captured):
    res, recs, tests, _ = captured
    assert "8 passed" in res.stdout
    assert recs[0]["type"] == "header" and recs[0]["v"] == 1
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
    res = _pytest(project_dir, f"--skipcert-skip={skip}")
    assert res.returncode == 0, res.stdout + res.stderr
    assert "6 passed, 2 deselected" in res.stdout


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
