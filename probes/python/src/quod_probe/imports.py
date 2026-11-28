"""Import closures: what a cached import depends on.

A module's body runs once per process, in the scope that imported it first.
Every later ``import`` of it is a ``sys.modules`` lookup that runs none of its
code, so without this tracker a test whose module imported ``util`` second
observes nothing of ``util``'s module-level code. If that code changes (a
constant, a table built at import, an ``import`` of a helper), the test reads
the new value but isn't selected.

Nothing the program can see is replaced (ADR 023): ``builtins.__import__``
and ``importlib.import_module`` stay the interpreter's own objects, so
pickling them by name, and tools that inspect the live builtins (pylint and
astroid), behave as they do without the probe. Instead:

- **Loads.** CPython calls ``importlib._bootstrap._find_and_load`` for every
  import that loads a module, looking it up by name at each call. The tracker
  wraps that private function: while a load runs, a fresh collector records
  everything observed, and becomes the *closure* of each module that
  finished loading during it (an over-approximation when one load loads
  several modules).
- **Uses.** When a code object first runs in a scope (``PY_START``), its
  ``IMPORT_NAME`` instructions are read from its bytecode, and each module it
  names (its file or package) and that module's closure are added to the
  current scope (test, fixture, test-module collection or root). An import
  in a branch that doesn't run counts too, which is conservative. Calls to
  ``importlib.import_module`` and ``importlib.__import__`` are seen when they
  start, with their arguments; explicit ``__import__(name)`` calls through
  ``sys.monitoring`` ``CALL`` events, enabled only in code objects that name
  ``__import__``.

Modules loaded before recording started have no closure; the root scope
already covers them. ``sys.modules[name]`` lookups and objects passed in from
elsewhere aren't imports and aren't recorded here. Code they run is still
recorded, and ``quod-ladder`` counts a test that ran any function of a file
as depending on the file's module-level code.

Assumed (documented): an explicit ``__import__`` call is absolute. Its
``level`` argument isn't visible to a ``CALL`` event, so a relative
``__import__(name, globals(), locals(), [], 1)`` of an already loaded module
is recorded under the wrong name.
"""

from __future__ import annotations

import builtins
import dis
import importlib
import importlib.util
import sys
from types import CodeType, ModuleType
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from .capture import Collector, Recorder

_IMPORT_NAME = dis.opmap["IMPORT_NAME"]
# Instructions that load an import's level and fromlist (3.14 loads small
# ints with LOAD_SMALL_INT).
_CONST_LOADS = frozenset({"LOAD_CONST", "LOAD_SMALL_INT"})
_SKIP = frozenset({"EXTENDED_ARG", "NOP", "CACHE"})
# A code object's imports as (name, level, fromlist), and whether it names
# ``__import__``. ``level`` is None when the bytecode didn't show it.
Scan = tuple[tuple[tuple[str, "int | None", "tuple | None"], ...], bool]
_EMPTY: Scan = ((), False)


def scan_imports(code: CodeType) -> Scan:
    """The ``import`` statements in ``code``'s own bytecode (not its nested
    functions', which have code objects of their own), and whether it names
    ``__import__``."""
    explicit = "__import__" in code.co_names
    if _IMPORT_NAME not in code.co_code[::2]:
        return ((), True) if explicit else _EMPTY
    out = []
    prev: list = []
    for ins in dis.get_instructions(code):
        if ins.opname == "IMPORT_NAME":
            level = fromlist = None
            if len(prev) == 2 and prev[0].opname in _CONST_LOADS and prev[1].opname in _CONST_LOADS:
                level, fromlist = prev[0].argval, prev[1].argval
            ok = isinstance(level, int) and (fromlist is None or isinstance(fromlist, tuple))
            out.append((ins.argval, level if ok else None, fromlist if ok else None))
        if ins.opname not in _SKIP:
            prev = [*prev[-1:], ins]
    return tuple(out), explicit


class ImportTracker:
    def __init__(self, recorder: "Recorder") -> None:
        self._rec = recorder
        self.closures: dict[str, "Collector"] = {}
        self._known: set[str] = set()
        self._orig_find_and_load = None
        self._scans: dict[CodeType, Scan] = {}
        self._entries: dict[CodeType, str] = {}
        self._builtin_import = builtins.__import__
        from .capture import Collector

        self._collector = Collector

    # Install --------------------------------------------------------------

    def install(self) -> None:
        """Installs the load hook and the ``CALL`` callback. The recorder's
        ``PY_START`` callback calls :meth:`on_start`."""
        if self._orig_find_and_load is not None:
            return
        from importlib import _bootstrap

        self._known = set(sys.modules)
        self._builtin_import = builtins.__import__
        orig = _bootstrap._find_and_load
        self._orig_find_and_load = orig
        load = self._load

        def _find_and_load(name, import_):
            return load(name, lambda: orig(name, import_))

        _find_and_load.__wrapped__ = orig
        _find_and_load.__module__ = orig.__module__
        _find_and_load.__qualname__ = orig.__qualname__
        _bootstrap._find_and_load = _find_and_load
        self._entries = {
            importlib.import_module.__code__: "import_module",
            _bootstrap.__import__.__code__: "__import__",
        }
        mon = sys.monitoring
        mon.register_callback(self._rec._tool_id, mon.events.CALL, self._on_call)

    def uninstall(self) -> None:
        if self._orig_find_and_load is None:
            return
        from importlib import _bootstrap

        _bootstrap._find_and_load = self._orig_find_and_load
        self._orig_find_and_load = None
        if self._rec._tool_id is not None:
            mon = sys.monitoring
            mon.register_callback(self._rec._tool_id, mon.events.CALL, None)

    # Loads ----------------------------------------------------------------

    def _load(self, absolute: str, call):
        rec = self._rec
        if absolute in sys.modules or _from_probe(sys._getframe(2)):
            return call()
        # The load runs module bodies: record them (fresh, so code that
        # already ran in this scope is recorded again).
        closure = self._collector()
        rec.push(closure, fresh=True)
        try:
            module = call()
        finally:
            rec.pop(closure)
        try:
            self._assign(closure)
            self._use(_names(module, absolute, ()))
        except Exception:  # noqa: BLE001 - a probe bug must not break the test run
            rec._mark_untraced()
        return module

    def _assign(self, closure: "Collector") -> None:
        """Gives the load's observations to every module that finished
        loading during it. Modules still initialising (a parent package
        whose body is running this import) get theirs from the outer load."""
        with self._rec._lock:
            for name in list(sys.modules):
                if name in self._known:
                    continue
                m = sys.modules.get(name)
                spec = getattr(m, "__spec__", None)
                if getattr(spec, "_initializing", False):
                    continue
                self._known.add(name)
                self.closures[name] = closure

    # Uses -----------------------------------------------------------------

    def on_start(self, code: CodeType, frame) -> bool:
        """Called on ``PY_START`` with the starting frame. Records the uses of
        ``code``'s imports, and returns True to keep ``PY_START`` enabled for
        ``code``: the import entry points are called with different names."""
        rec = self._rec
        try:
            kind = self._entries.get(code)
            if kind is not None:
                self._on_entry(kind, frame)
                return True
            scan = self._scans.get(code)
            if scan is None:
                scan = self._scans[code] = scan_imports(code)
                if scan[1]:
                    mon = sys.monitoring
                    mon.set_local_events(rec._tool_id, code, mon.events.CALL)
            imports = scan[0]
            if not imports or _from_probe(frame):
                return False
            g = frame.f_globals
            for name, level, fromlist in imports:
                if level is None:
                    rec._mark_untraced()  # the bytecode didn't show the import's level
                    continue
                try:
                    absolute = _absolute(name, g, level)
                except Exception:  # noqa: BLE001 - the import itself will raise
                    continue
                self._use_once(absolute, fromlist)
        except Exception:  # noqa: BLE001 - a probe bug must not break the test run
            rec._mark_untraced()
        return False

    def _on_entry(self, kind: str, frame) -> None:
        if _from_probe(frame.f_back):
            return
        loc = frame.f_locals
        name = loc.get("name")
        if not isinstance(name, str):
            return
        if kind == "import_module":
            package = loc.get("package")
            absolute = importlib.util.resolve_name(name, package) if name.startswith(".") else name
            self._use_once(absolute, ())
        else:
            level = loc.get("level", 0)
            absolute = _absolute(name, loc.get("globals"), level if isinstance(level, int) else 0)
            fromlist = loc.get("fromlist") or ()
            self._use_once(absolute, tuple(fromlist) if isinstance(fromlist, (list, tuple)) else ())

    def _on_call(self, code: CodeType, offset: int, callable, arg0):  # noqa: A002 - sys.monitoring's signature
        if callable is not self._builtin_import:
            return sys.monitoring.DISABLE
        try:
            if isinstance(arg0, str) and not _from_probe(sys._getframe(1)):
                self._use_once(arg0, ())
        except Exception:  # noqa: BLE001 - a probe bug must not break the test run
            self._rec._mark_untraced()
        return None

    def _use_once(self, absolute: str, fromlist) -> None:
        # What an import adds is fixed, so each scope records it once:
        # libraries import inside hot functions.
        key = (absolute, tuple(fromlist) if fromlist else ())
        targets = self._rec._targets()
        if all(key in t.imports_seen for t in targets):
            return
        self._use(_names(None, absolute, fromlist))
        for t in targets:
            t.imports_seen.add(key)

    def _use(self, names) -> None:
        rec = self._rec
        with rec._lock:
            targets = rec._targets()
            for name in names:
                # The module's own file or package, always: a module loaded
                # outside these hooks (pytest's importlib mode) has no
                # closure of its own.
                m = sys.modules.get(name)
                f = getattr(m, "__file__", None) if m is not None else None
                cls = rec.classify_path(f) if isinstance(f, str) else None
                if cls is not None:
                    for t in targets:
                        (t.files if cls[0] == "file" else t.packages).add(cls[1])
                c = self.closures.get(name)
                if c is not None:
                    for t in targets:
                        if id(c) not in t.closures_merged:
                            t.closures_merged.add(id(c))
                            t.merge(c)


def _from_probe(frame) -> bool:
    """True if the nearest frame outside the import system is the probe's:
    its own imports (also from inside monitoring callbacks) aren't the
    test's."""
    for _ in range(64):
        if frame is None:
            return False
        if not frame.f_code.co_filename.startswith("<frozen importlib"):
            return str(frame.f_globals.get("__name__", "")).startswith("quod_probe")
        frame = frame.f_back
    return False


def _absolute(name: str, globals, level: int) -> str:
    if level <= 0:
        return name
    package = _package_of(globals)
    if name:
        return importlib.util.resolve_name("." * level + name, package)
    return _parent(package, level)


def _names(module, absolute: str, fromlist) -> list[str]:
    """Absolute names an import used: the module and its parents for
    ``import a.b.c``; plus any submodules named in ``fromlist`` for
    ``from a import b`` (and in ``__all__`` for ``from a import *``)."""
    parts = absolute.split(".")
    names = [".".join(parts[: i + 1]) for i in range(len(parts))]
    if fromlist:
        target = sys.modules.get(absolute, module)
        items = [i for i in fromlist if i != "*"]
        if "*" in fromlist:
            items.extend(getattr(target, "__all__", ()) or ())
        for item in items:
            sub = getattr(target, item, None) if isinstance(item, str) else None
            if isinstance(sub, ModuleType):
                names.append(sub.__name__)
            elif isinstance(item, str) and f"{absolute}.{item}" in sys.modules:
                names.append(f"{absolute}.{item}")
    return names


def _package_of(globals) -> str:
    g = globals or {}
    pkg = g.get("__package__")
    if pkg is None:
        spec = g.get("__spec__")
        pkg = spec.parent if spec is not None else g.get("__name__", "")
        if "__path__" not in g and spec is None:
            pkg = pkg.rpartition(".")[0]
    return pkg or ""


def _parent(package: str, level: int) -> str:
    parts = package.split(".")
    return ".".join(parts[: len(parts) - (level - 1)]) if level > 1 else package
