"""Import closures: what a cached import depends on.

A module's body runs once per process, in the scope that imported it first.
Every later ``import`` of it is a ``sys.modules`` lookup that runs none of its
code, so without this tracker a test whose module imported ``util`` second
observes nothing of ``util``'s module-level code. If that code changes (a
constant, a table built at import, an ``import`` of a helper), the test reads
the new value but isn't selected.

The tracker wraps ``builtins.__import__`` (every ``import`` statement) and
``importlib.import_module``:

- while a call loads modules, a fresh collector records everything observed,
  and becomes the *closure* of each module that finished loading during the
  call (an over-approximation when one call loads several modules);
- every call, cached or not, adds each module it names (its file or
  package) and that module's closure to the current scope (test, fixture,
  test-module collection or root). Modules loaded before recording started
  have no closure; the root scope already covers them.

``sys.modules[name]`` lookups and objects passed in from elsewhere aren't
imports and aren't recorded here. Code they run is still recorded, and
``quod-ladder`` counts a test that ran any function of a file as depending
on the file's module-level code.
"""

from __future__ import annotations

import builtins
import importlib
import importlib.util
import sys
from types import ModuleType
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from .capture import Collector, Recorder


class ImportTracker:
    def __init__(self, recorder: "Recorder") -> None:
        self._rec = recorder
        self.closures: dict[str, "Collector"] = {}
        self._known: set[str] = set()
        self._orig_import = None
        self._orig_import_module = None
        from .capture import Collector

        self._collector = Collector

    # Install --------------------------------------------------------------

    def install(self) -> None:
        if self._orig_import is not None:
            return
        self._known = set(sys.modules)
        orig_import = builtins.__import__
        orig_import_module = importlib.import_module
        self._orig_import, self._orig_import_module = orig_import, orig_import_module
        track = self._call

        def __import__(name, globals=None, locals=None, fromlist=(), level=0):  # noqa: A002 - builtin signature
            # The probe's own imports (also from inside monitoring callbacks)
            # aren't the test's.
            if globals is not None and str(globals.get("__name__", "")).startswith("quod_probe"):
                return orig_import(name, globals, locals, fromlist, level)
            try:
                absolute = _absolute(name, globals, level)
            except Exception:  # noqa: BLE001 - let the real import raise
                return orig_import(name, globals, locals, fromlist, level)
            return track(
                absolute,
                fromlist,
                lambda: orig_import(name, globals, locals, fromlist, level),
            )

        def import_module(name, package=None):
            try:
                absolute = importlib.util.resolve_name(name, package) if name.startswith(".") else name
            except Exception:  # noqa: BLE001
                return orig_import_module(name, package)
            return track(absolute, (), lambda: orig_import_module(name, package))

        __import__.__doc__ = orig_import.__doc__
        import_module.__doc__ = orig_import_module.__doc__
        import_module.__wrapped__ = orig_import_module
        builtins.__import__ = __import__
        importlib.import_module = import_module

    def uninstall(self) -> None:
        if self._orig_import is None:
            return
        builtins.__import__ = self._orig_import
        importlib.import_module = self._orig_import_module
        self._orig_import = self._orig_import_module = None

    # Tracking -------------------------------------------------------------

    def _call(self, absolute: str, fromlist, call):
        rec = self._rec
        closure = None
        if not _cached(absolute, fromlist):
            # This call may run module bodies: record them (fresh, so code
            # that already ran in this scope is recorded again).
            closure = self._collector()
            rec.push(closure, fresh=True)
        try:
            module = call()
        finally:
            if closure is not None:
                rec.pop(closure)
        try:
            if closure is not None:
                self._assign(closure)
                self._use(_names(module, absolute, fromlist))
                return module
            # Cached: what it adds is fixed, so each scope records it once.
            # Libraries import inside hot functions (hundreds of thousands
            # of cached imports in one suite).
            key = (absolute, tuple(fromlist) if fromlist else ())
            targets = rec._targets()
            if all(key in t.imports_seen for t in targets):
                return module
            self._use(_names(module, absolute, fromlist))
            for t in targets:
                t.imports_seen.add(key)
        except Exception:  # noqa: BLE001 - a probe bug must not break the test run
            rec._mark_untraced()
        return module

    def _assign(self, closure: "Collector") -> None:
        """Gives the call's observations to every module that finished
        loading during it. Modules still initialising (a parent package
        whose body is running this import) get theirs from the outer call."""
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

    def _use(self, names) -> None:
        rec = self._rec
        with rec._lock:
            targets = rec._targets()
            for name in names:
                # The module's own file or package, always: a module loaded
                # outside these wrappers (pytest's importlib mode) has no
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


def _cached(absolute: str, fromlist) -> bool:
    """True if the call can't load anything: the module is in ``sys.modules``
    and every ``fromlist`` name is already an attribute of it."""
    m = sys.modules.get(absolute)
    if m is None:
        return False
    for item in fromlist or ():
        if item == "*" or not isinstance(item, str) or not hasattr(m, item):
            return False
    return True


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

