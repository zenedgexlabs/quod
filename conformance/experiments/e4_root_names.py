"""Experiment E4: how much could name-level root-scope analysis save?

R017 runs every test when a changed node was observed in the root scope. For
repos whose conftest imports the package (click, jinja), any module-level or
class-level edit is a root hit. But such an edit only changes the *bindings*
it (re)defines: module globals and class attributes. A test can observe a
changed binding only by reading that name, or through Python code that
reads it on the test's behalf.

This script estimates, offline, from a replay's stored artefacts.

**Changed bindings.** For each root-hit step, it diffs every changed source
module's top-level and class-level statements, per bound name:

- An undecorated `def` whose statement exists on both sides changes no
  binding. Its header and body only change the function object, which
  callers execute and introspectors are recorded for (ADR 011). Added,
  removed and decorated defs do change the binding.
- `if TYPE_CHECKING:` blocks never run and are ignored. Other `if`, `try`
  and `with` blocks contribute the bindings inside them. Their condition
  counts as read by each of those bindings.
- Module-level data flow is closed over: a statement reading a changed name
  of its module re-binds its own names. `from M import X` copies across
  modules: if `M.X` changed, the importer's binding changed.
- The change is **opaque** (R017 stays) when any of these changes, or is
  added or removed: a non-binding statement (bare call, loop, attribute
  store), a class header (bases, keywords, decorators), `__all__`, a star
  import, a module `__getattr__`, or an implicitly invoked special method.

**Names a test may read.** These are the identifiers (names, attribute names,
identifier-like string constants) of every repository function the test
executed, plus the module-level code of every file it imported. A root-scope
*function* (conftest hooks and fixtures run at session start) that reads a
changed name affects every test.

**Two bounds.**

- *Optimistic* ignores reflective access.
- *Guarded* treats a test as reading everything when its executed
  repository code is reflective: `getattr`, `vars`, `__dict__`, `dir`,
  `inspect`, `copy`, `pickle`, or dataclass/attrs field helpers.

Neither bound sees code outside the repository (stdlib, packages), so both
are upper bounds on the gain. Usage:

    python conformance/experiments/e4_root_names.py click
"""

from __future__ import annotations

import ast
import json
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]

REFLECTIVE = {
    "getattr", "setattr", "hasattr", "delattr", "vars", "__dict__", "dir", "globals", "locals",
    "getmembers", "attrgetter", "asdict", "astuple", "fields", "deepcopy", "dumps", "loads",
    "__getattribute__", "getattr_static",
}
# Special names the interpreter looks up implicitly (no identifier in the
# caller's code). Adding, removing or rebinding one (other than redefining an
# existing def) is opaque.
IMPLICIT = {
    "__init_subclass__", "__class_getitem__", "__set_name__", "__getattr__", "__getattribute__",
    "__setattr__", "__delattr__", "__get__", "__set__", "__delete__", "__eq__", "__ne__", "__lt__",
    "__le__", "__gt__", "__ge__", "__hash__", "__bool__", "__len__", "__iter__", "__next__",
    "__contains__", "__getitem__", "__setitem__", "__delitem__", "__call__", "__enter__", "__exit__",
    "__aenter__", "__aexit__", "__repr__", "__str__", "__format__", "__new__", "__init__", "__del__",
    "__copy__", "__deepcopy__", "__reduce__", "__reduce_ex__", "__getstate__", "__setstate__",
    "__slots__", "__all__", "__mro_entries__", "__instancecheck__", "__subclasscheck__", "__missing__",
    "__add__", "__sub__", "__mul__", "__truediv__", "__or__", "__and__", "__index__", "__int__",
    "__float__", "__await__", "__aiter__", "__anext__", "__fspath__", "__post_init__",
}


def git(repo: Path, *args: str) -> subprocess.CompletedProcess:
    return subprocess.run(["git", "-C", str(repo), *args], capture_output=True, text=True,
                          encoding="utf-8", errors="replace")


def show(repo: Path, rev: str, path: str) -> str | None:
    r = git(repo, "show", f"{rev}:{path}")
    return r.stdout if r.returncode == 0 else None


def idents(node: ast.AST) -> set[str]:
    out: set[str] = set()
    for n in ast.walk(node):
        if isinstance(n, ast.Name):
            out.add(n.id)
        elif isinstance(n, ast.Attribute):
            out.add(n.attr)
        elif isinstance(n, ast.Constant) and isinstance(n.value, str) and n.value.isidentifier():
            out.add(n.value)
        elif isinstance(n, ast.alias):
            out.add((n.asname or n.name).split(".")[0])
            out.add(n.name.split(".")[-1])
    return out


def is_type_checking(test: ast.expr) -> bool:
    return (isinstance(test, ast.Name) and test.id == "TYPE_CHECKING") or (
        isinstance(test, ast.Attribute) and test.attr == "TYPE_CHECKING")


class Scope:
    """Bindings of one module or class body."""

    def __init__(self) -> None:
        self.binds: dict[str, list[str]] = {}   # name -> statement keys, in order
        self.reads: dict[str, set[str]] = {}    # name -> identifiers its statements read
        self.defs: dict[str, int] = {}          # undecorated defs: name -> count
        self.opaque: list[str] = []             # non-binding statements
        self.imports: list[tuple[str, int, str, str]] = []  # (module, level, name, alias)
        self.classes: dict[str, ast.ClassDef] = {}
        self.star = False

    def add(self, name: str, key: str, reads: set[str]) -> None:
        self.binds.setdefault(name, []).append(key)
        self.reads.setdefault(name, set()).update(reads)


def collect(body: list[ast.stmt], sc: Scope, cond: set[str] | None = None) -> None:
    cond = cond or set()
    for st in body:
        if isinstance(st, (ast.FunctionDef, ast.AsyncFunctionDef)):
            if st.decorator_list:
                key = "".join(ast.dump(d) for d in st.decorator_list) + ast.dump(st.args)
                sc.add(st.name, "decorated " + key, idents(ast.Module(body=[], type_ignores=[])) | cond
                       | set().union(*(idents(d) for d in st.decorator_list)))
            else:
                sc.defs[st.name] = sc.defs.get(st.name, 0) + 1
                sc.add(st.name, "def", cond)
        elif isinstance(st, ast.ClassDef):
            key = "".join(ast.dump(x) for x in st.bases + st.keywords + st.decorator_list)
            sc.add(st.name, "class " + key, cond | set().union(set(), *(idents(x) for x in st.bases + st.decorator_list)))
            sc.classes[st.name] = st
        elif isinstance(st, ast.Import):
            for a in st.names:
                sc.add((a.asname or a.name).split(".")[0], ast.dump(st), cond)
        elif isinstance(st, ast.ImportFrom):
            if any(a.name == "*" for a in st.names):
                sc.star = True
                continue
            for a in st.names:
                sc.add(a.asname or a.name, ast.dump(a) + (st.module or "") + str(st.level), cond)
                sc.imports.append((st.module or "", st.level, a.name, a.asname or a.name))
        elif isinstance(st, (ast.Assign, ast.AnnAssign, ast.AugAssign)):
            targets = st.targets if isinstance(st, ast.Assign) else [st.target]
            names: set[str] = set()
            plain = True
            for t in targets:
                for n in ast.walk(t):
                    if isinstance(n, ast.Name):
                        names.add(n.id)
                    elif not isinstance(n, (ast.Tuple, ast.List, ast.Store, ast.Starred, ast.Load)):
                        plain = False
            if not plain or (st.value is None and isinstance(st, ast.AnnAssign) and False):
                sc.opaque.append(ast.dump(st))
                continue
            for n in names:
                sc.add(n, ast.dump(st), idents(st) | cond)
        elif isinstance(st, ast.Expr) and isinstance(st.value, ast.Constant) and isinstance(st.value.value, str):
            sc.add("__doc__", ast.dump(st), set())
        elif isinstance(st, ast.If):
            if is_type_checking(st.test):
                continue
            c = cond | idents(st.test)
            collect(st.body, sc, c)
            collect(st.orelse, sc, c)
        elif isinstance(st, ast.Try):
            collect(st.body, sc, cond)
            for h in st.handlers:
                collect(h.body, sc, cond | (idents(h.type) if h.type else set()))
            collect(st.orelse, sc, cond)
            collect(st.finalbody, sc, cond)
        elif isinstance(st, ast.Pass):
            continue
        else:
            sc.opaque.append(ast.dump(st))


def module_name(path: str) -> str:
    p = path[:-3] if path.endswith(".py") else path
    parts = p.split("/")
    if parts[0] == "src":
        parts = parts[1:]
    if parts[-1] == "__init__":
        parts = parts[:-1]
    return ".".join(parts)


def resolve(importer: str, is_pkg: bool, module: str, level: int) -> str:
    if level == 0:
        return module
    base = importer.split(".")
    if not is_pkg:
        base = base[:-1]
    if level > 1:
        base = base[: len(base) - (level - 1)]
    return ".".join(base + ([module] if module else []))


def analyse(repo: Path, base: str, head: str, root_files: list[str]):
    """Returns (changed {(module, name)}, changed attribute names, opaque reason)."""
    paths = [p for p in git(repo, "diff", "--name-only", base, head, "--", "*.py").stdout.split()
             if not p.startswith(("tests/", "docs/", "examples/", "scripts/"))]
    changed: set[tuple[str, str]] = set()
    attrs: set[str] = set()
    scopes_b: dict[str, Scope] = {}
    for path in paths:
        old, new = show(repo, base, path), show(repo, head, path)
        if old is None or new is None:
            return None, None, f"{path}: added or removed"
        try:
            a, b = ast.parse(old), ast.parse(new)
        except SyntaxError:
            return None, None, f"{path}: syntax error"
        mod = module_name(path)

        def compare(ba: list, bb: list, where: str, is_class: bool):
            sa, sb = Scope(), Scope()
            collect(ba, sa)
            collect(bb, sb)
            if sorted(sa.opaque) != sorted(sb.opaque):
                return f"non-binding statement changed in {where}"
            if sa.star != sb.star:
                return f"star import changed in {where}"
            local: set[str] = set()
            for n in set(sa.binds) | set(sb.binds):
                if sa.binds.get(n) == sb.binds.get(n):
                    continue
                both_defs = (n in sa.defs and n in sb.defs and sa.binds[n] == ["def"] * len(sa.binds[n])
                             and sb.binds[n] == ["def"] * len(sb.binds[n]))
                if both_defs:
                    continue
                if n in IMPLICIT or n == "__all__" or (not is_class and n == "__getattr__"):
                    return f"{n} changed in {where}"
                local.add(n)
            grew = True
            while grew:  # data flow within the scope
                grew = False
                for n, rd in sb.reads.items():
                    if n not in local and rd & local:
                        local.add(n)
                        grew = True
            for n in local:
                if is_class:
                    attrs.add(n)
                else:
                    changed.add((mod, n))
            for name in set(sa.classes) & set(sb.classes):
                if name in local:
                    return f"class header {name} changed in {where}"
                r = compare(sa.classes[name].body, sb.classes[name].body, f"class {name}", True)
                if r:
                    return r
            if not is_class:
                scopes_b[mod] = sb
            return None

        reason = compare(a.body, b.body, mod, False)
        if reason:
            return None, None, f"{path}: {reason}"

    # Import closure across all repository modules at head.
    all_paths = [p for p in git(repo, "ls-tree", "-r", "--name-only", head).stdout.split()
                 if p.endswith(".py") and not p.startswith(("tests/", "docs/", "examples/", "scripts/"))]
    imports = []
    for p in all_paths:
        src = show(repo, head, p)
        try:
            tree = ast.parse(src or "")
        except SyntaxError:
            continue
        sc = Scope()
        collect(tree.body, sc)
        m = module_name(p)
        for module, level, name, alias in sc.imports:
            imports.append((m, resolve(m, p.endswith("__init__.py"), module, level), name, alias))
    grew = True
    while grew:
        grew = False
        for importer, source, name, alias in imports:
            if (source, name) in changed and (importer, alias) not in changed:
                changed.add((importer, alias))
                grew = True
    return changed, attrs, None


def qual_index(src: str) -> dict[str, ast.AST]:
    out: dict[str, ast.AST] = {}

    def walk(body, prefix):
        for st in body:
            if isinstance(st, (ast.FunctionDef, ast.AsyncFunctionDef)):
                out.setdefault(prefix + st.name, st)
                walk(st.body, prefix + st.name + ".<locals>.")
            elif isinstance(st, ast.ClassDef):
                out.setdefault(prefix + st.name, st)
                walk(st.body, prefix + st.name + ".")
            elif isinstance(st, ast.If):
                walk(st.body + st.orelse, prefix)
            elif isinstance(st, ast.Try):
                walk(st.body + [x for h in st.handlers for x in h.body] + st.orelse + st.finalbody, prefix)
            elif isinstance(st, (ast.With, ast.For, ast.While)):
                walk(st.body, prefix)

    try:
        walk(ast.parse(src).body, "")
    except SyntaxError:
        pass
    return out


def module_level_idents(src: str) -> set[str]:
    try:
        tree = ast.parse(src)
    except SyntaxError:
        return set()
    out = set()
    for st in tree.body:
        if isinstance(st, (ast.FunctionDef, ast.AsyncFunctionDef)):
            out.add(st.name)
            for d in st.decorator_list + st.args.defaults:
                out |= idents(d)
        elif isinstance(st, ast.ClassDef):
            out.add(st.name)
            for x in st.bases + st.decorator_list:
                out |= idents(x)
            for inner in st.body:
                if not isinstance(inner, (ast.FunctionDef, ast.AsyncFunctionDef)):
                    out |= idents(inner)
        else:
            out |= idents(st)
    return out


def main(repo_name: str) -> None:
    work = ROOT / "conformance" / ".work" / repo_name
    repo, art = work / "repo", work / "artifacts"
    tot = {"steps": 0, "opaque": 0, "all": 0, "opt": 0, "guard": 0}
    for h in range(1, 21):
        rp = art / f"h{h}.report.json"
        if not rp.exists():
            continue
        rep = json.loads(rp.read_text(encoding="utf-8"))
        recs = [json.loads(line) for line in (art / f"h{h}-base.jsonl").read_text(encoding="utf-8").splitlines()]
        root = next(r for r in recs if r["type"] == "root")
        tests = [r for r in recs if r["type"] == "test"]
        root_files = set(root["files"])
        root_fns = {f"{a}::{b}" for a, b in root["functions"]}
        if not any((c.startswith("file:") and c[5:] in root_files) or (c.startswith("fn:") and c[3:] in root_fns)
                   for c in rep["changed"]):
            continue
        tot["steps"] += 1
        all_ms = sum(t["duration_ms"] for t in tests)
        tot["all"] += all_ms
        base, head = rep["base"], rep["head"]
        changed, attrs, reason = analyse(repo, base, head, root["files"])
        if reason:
            tot["opaque"] += 1
            tot["opt"] += all_ms
            tot["guard"] += all_ms
            print(f"h{h:<2} {head[:8]} OPAQUE: {reason}")
            continue
        names = {n for _, n in changed} | attrs

        srcs: dict[str, str] = {}
        idx: dict[str, dict] = {}

        def src(p):
            if p not in srcs:
                srcs[p] = show(repo, base, p) or ""
            return srcs[p]

        def fn_idents(p, q):
            if p not in idx:
                idx[p] = qual_index(src(p))
            n = idx[p].get(q)
            if n is None:
                return None
            if isinstance(n, ast.ClassDef):
                return idents(ast.Module(body=[s for s in n.body if not isinstance(s, (ast.FunctionDef, ast.AsyncFunctionDef))], type_ignores=[]))
            return idents(n)

        root_reads = set()
        for a, b in root["functions"]:
            root_reads |= fn_idents(a, b) or set()
        if root_reads & names:
            tot["opt"] += all_ms
            tot["guard"] += all_ms
            print(f"h{h:<2} {head[:8]} root-scope functions read {sorted(root_reads & names)[:5]}")
            continue
        opt = guard = opt_ms = guard_ms = 0
        for t in tests:
            seen: set[str] = set()
            unknown = False
            for p, q in t["functions"]:
                got = fn_idents(p, q)
                if got is None:
                    unknown = True
                else:
                    seen |= got
            for f in t["files"]:
                if f.endswith(".py") and f.startswith("tests"):
                    seen |= module_level_idents(src(f))
            hit = bool(seen & names) or unknown or t["outcome"] != "passed"
            if hit:
                opt += 1
                opt_ms += t["duration_ms"]
            if hit or seen & REFLECTIVE:
                guard += 1
                guard_ms += t["duration_ms"]
        tot["opt"] += opt_ms
        tot["guard"] += guard_ms
        shown = sorted(names)
        print(f"h{h:<2} {head[:8]} changed {shown[:5]}{'...' if len(shown) > 5 else ''}: "
              f"run optimistic {opt}/{len(tests)}, guarded {guard}/{len(tests)}")
    a = tot["all"] or 1
    print(f"\n{repo_name}: {tot['steps']} root-hit steps, {tot['opaque']} opaque. Time run on those steps: "
          f"R017 1.00, optimistic {tot['opt'] / a:.2f}, guarded {tot['guard'] / a:.2f}")


if __name__ == "__main__":
    main(sys.argv[1])
