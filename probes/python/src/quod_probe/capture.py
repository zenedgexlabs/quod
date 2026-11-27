"""Low-level capture: code execution via sys.monitoring, I/O via audit hooks.

Observations go to every *collector* on the active stack. Collectors are
pushed for a test, for a non-function-scoped fixture's setup, and for a test
module's import during collection. With the stack empty (plugin start,
conftest import, session start), observations go to the *root* collector,
whose changes affect every test (rule R017).

Each code object fires ``PY_START`` once and is then disabled. At every test
boundary, ``sys.monitoring.restart_events()`` re-enables everything, so each
test pays for one callback per distinct function it runs. This is the
technique coverage tools use for per-test contexts.
"""

from __future__ import annotations

import os
import site
import sys
import sysconfig
import threading
from dataclasses import dataclass, field
from types import CodeType, FunctionType, MethodType

from .packages import PackageIndex, PackageInfo

FLAG_NETWORK = "network"
FLAG_SUBPROCESS = "subprocess"
FLAG_DYNAMIC_IMPORT = "dynamic_import"
FLAG_C_EXT = "c_ext"
FLAG_UNTRACED = "untraced"

_SUBPROCESS_EVENTS = frozenset(
    {"subprocess.Popen", "os.system", "os.posix_spawn", "os.spawn", "os.exec", "os.startfile", "pty.spawn"}
)
_LISTING_EVENTS = frozenset({"os.listdir", "os.scandir", "glob.glob", "os.walk"})
_LOOPBACK_PREFIXES = ("127.", "::1", "localhost", "0:0:0:0:0:0:0:1")


@dataclass
class Collector:
    """A set of observations."""

    files: set[str] = field(default_factory=set)
    functions: set[tuple[str, str]] = field(default_factory=set)
    packages: set[str] = field(default_factory=set)
    flags: set[str] = field(default_factory=set)
    # Environment variables read from outside the process: name -> value hash.
    # ``*`` is the whole environment (iteration).
    env: dict[str, str] = field(default_factory=dict)
    # Import closures already merged in (``imports.ImportTracker``), by id.
    closures_merged: set[int] = field(default_factory=set, compare=False, repr=False)
    # Cached imports already recorded: (absolute name, fromlist).
    imports_seen: set[tuple] = field(default_factory=set, compare=False, repr=False)

    def merge(self, other: "Collector") -> None:
        self.files |= other.files
        self.functions |= other.functions
        self.packages |= other.packages
        self.flags |= other.flags
        self.env.update(other.env)


def entity_qualname(co_qualname: str) -> str | None:
    """Maps a runtime ``co_qualname`` to the entity name ``quod-change`` emits.

    Lambdas, comprehensions and generator expressions belong to their
    enclosing def or class. ``None`` means module level.

    >>> entity_qualname("f.<locals>.<lambda>")
    'f'
    >>> entity_qualname("C.m")
    'C.m'
    >>> entity_qualname("<lambda>") is None
    True
    """
    parts = co_qualname.split(".")
    while parts and parts[-1].startswith("<"):
        parts.pop()
        if parts and parts[-1] == "<locals>":
            parts.pop()
    return ".".join(parts) if parts else None


class CacheTracker:
    """Clears ``functools`` caches at every test boundary during recording.

    A cached function hit from an earlier test executes no Python code, so the
    probe can't see that the test depends on it. A change to the function then
    goes unnoticed, which is unsound. Clearing ``lru_cache``/``cache`` wrappers
    and ``cached_property`` values before each test makes every test recompute,
    and therefore record, what it uses.

    ``functools.lru_cache``, ``functools.cache`` and ``functools.cached_property``
    are replaced by registering equivalents before project code is imported
    (the recorder starts before the initial conftests load). Caches created
    earlier, by already-imported third-party or stdlib modules, aren't tracked.
    Their code is covered by package and interpreter identity instead.

    Hand-written memo dictionaries aren't tracked. The probe assumes tests
    don't depend on state left behind by other tests except through these
    caches. The post-merge audit is what catches violations.
    """

    def __init__(self) -> None:
        self._wrappers: list = []
        self._properties: list[tuple[object, str]] = []
        self._orig: tuple | None = None

    def install(self) -> None:
        import functools

        if self._orig is not None:
            return
        orig_lru, orig_cache, orig_cp = functools.lru_cache, functools.cache, functools.cached_property
        self._orig = (orig_lru, orig_cache, orig_cp)
        wrappers, properties = self._wrappers, self._properties

        def lru_cache(maxsize=128, typed=False):
            if callable(maxsize) and not isinstance(maxsize, bool):
                w = orig_lru(maxsize, typed)  # used bare: @lru_cache
                wrappers.append(w)
                return w
            deco = orig_lru(maxsize, typed)

            def register(fn):
                w = deco(fn)
                wrappers.append(w)
                return w

            return register

        def cache(user_function):
            return lru_cache(maxsize=None)(user_function)

        class cached_property(orig_cp):  # noqa: N801 - keeps the stdlib name
            def __get__(self, instance, owner=None):
                value = super().__get__(instance, owner)
                if instance is not None:
                    properties.append((instance, self.attrname))
                return value

        cached_property.__module__ = "functools"
        functools.lru_cache, functools.cache, functools.cached_property = lru_cache, cache, cached_property

    def uninstall(self) -> None:
        import functools

        if self._orig is not None:
            functools.lru_cache, functools.cache, functools.cached_property = self._orig
            self._orig = None

    def clear(self) -> None:
        for w in self._wrappers:
            try:
                w.cache_clear()
            except Exception:  # noqa: BLE001
                pass
        for instance, name in self._properties:
            try:
                instance.__dict__.pop(name, None)
            except Exception:  # noqa: BLE001
                pass
        self._properties.clear()


class IntrospectionTracker:
    """Records signature and annotation introspection as a use of the function.

    Parameter names, constant defaults and lazy annotations of a ``def`` only
    change the function object, so ``quod-change`` attributes them to the
    function itself (header splitting). Code that *calls* the function is
    recorded by ``PY_START``. Code that only *inspects* it (``inspect.signature``
    to build a CLI or an API schema, ``typing.get_type_hints`` for validation)
    runs none of its code, so it is recorded here instead: the introspected
    function counts as used by the current test, fixture or root scope.

    ``inspect._signature_from_callable`` is the one path behind
    ``inspect.signature``, ``Signature.from_callable`` and
    ``inspect.getfullargspec``. It is looked up as a module global at call
    time, so the patch also covers callers that imported ``signature`` before
    recording started. ``typing.get_type_hints`` and ``inspect.get_annotations``
    (also ``annotationlib.get_annotations`` on 3.14) are patched as module
    attributes, so references bound before recording started aren't covered.
    Reading ``__annotations__``, ``__defaults__`` or ``__code__`` directly isn't
    recorded. That is a documented assumption.
    """

    def __init__(self, note) -> None:
        self._note = note
        self._orig: list[tuple[object, str, object]] = []

    def install(self) -> None:
        import inspect
        import typing

        if self._orig:
            return
        note = self._note
        targets = [(inspect, "_signature_from_callable"), (inspect, "get_annotations"), (typing, "get_type_hints")]
        try:
            import annotationlib  # Python 3.14+

            targets.append((annotationlib, "get_annotations"))
        except ImportError:
            pass
        for module, name in targets:
            orig = getattr(module, name, None)
            if orig is None:
                continue

            wrapper = _noting(orig, note)
            wrapper.__name__ = getattr(orig, "__name__", name)
            wrapper.__qualname__ = getattr(orig, "__qualname__", name)
            wrapper.__doc__ = getattr(orig, "__doc__", None)
            wrapper.__wrapped__ = orig
            self._orig.append((module, name, orig))
            setattr(module, name, wrapper)

    def uninstall(self) -> None:
        for module, name, orig in reversed(self._orig):
            setattr(module, name, orig)
        self._orig.clear()


class EnvTracker:
    """Records reads of ``os.environ`` as ``env:NAME`` observations.

    ``os._Environ.__getitem__`` is the one path behind ``os.environ[k]``,
    ``.get``, ``in`` and ``os.getenv``. Iteration (``keys``, ``items``,
    ``copy``, ``dict(os.environ)``, ``len``) depends on the whole environment,
    recorded as ``env:*``: the hash of the environment as it was when
    recording started, the part that comes from outside the process. Writes
    after that are in-process state made by traced code (fixtures,
    ``monkeypatch``), not inputs, so later reads of a written key aren't
    recorded. Writes made before recording started are treated as inputs,
    which is conservative. Variables the runner sets for itself
    (``fingerprint.RUNNER_ENV_NAMES``) are never recorded.

    ``monkeypatch.setenv``/``delenv`` read the old value only to restore it
    later, so the key is marked written before that read.
    """

    def __init__(self, note) -> None:
        self._note = note
        self.written: set[str] = set()
        self._orig: dict[str, object] = {}
        self._mp: list[tuple[type, str, object]] = []

    def install(self) -> None:
        if self._orig:
            return
        from .fingerprint import RUNNER_ENV_NAMES, env_key, env_value_hash, environ_hash

        cls = type(os.environ)
        note, written, environ = self._note, self.written, os.environ
        outer = environ_hash(dict(os.environ))
        orig = {n: cls.__dict__[n] for n in ("__getitem__", "__setitem__", "__delitem__", "__iter__", "__len__")}
        self._orig = orig

        def __getitem__(self, key):
            if self is environ:
                try:
                    k = env_key(key)
                    if k not in written and k not in RUNNER_ENV_NAMES:
                        try:
                            value = orig["__getitem__"](self, key)
                        except KeyError:
                            note(k, env_value_hash(None))
                            raise
                        note(k, env_value_hash(value))
                        return value
                except KeyError:
                    raise
                except Exception:  # noqa: BLE001 - never break the test run
                    pass
            return orig["__getitem__"](self, key)

        def __setitem__(self, key, value):
            if self is environ:
                written.add(env_key(key))
            return orig["__setitem__"](self, key, value)

        def __delitem__(self, key):
            if self is environ:
                written.add(env_key(key))
            return orig["__delitem__"](self, key)

        def whole(self):
            if self is environ:
                note("*", outer)

        def __iter__(self):
            whole(self)
            return orig["__iter__"](self)

        def __len__(self):
            whole(self)
            return orig["__len__"](self)

        try:
            from _pytest.monkeypatch import MonkeyPatch

            for meth in ("setenv", "delenv"):
                orig_mp = MonkeyPatch.__dict__[meth]
                self._mp.append((MonkeyPatch, meth, orig_mp))
                setattr(MonkeyPatch, meth, _marking_written(orig_mp, written, env_key))
        except (ImportError, KeyError):
            pass

        for name, f in (
            ("__getitem__", __getitem__),
            ("__setitem__", __setitem__),
            ("__delitem__", __delitem__),
            ("__iter__", __iter__),
            ("__len__", __len__),
        ):
            setattr(cls, name, f)

    def uninstall(self) -> None:
        cls = type(os.environ)
        for name, f in self._orig.items():
            setattr(cls, name, f)
        self._orig = {}
        for owner, meth, f in reversed(self._mp):
            setattr(owner, meth, f)
        self._mp.clear()


def _marking_written(orig, written: set[str], env_key):
    def wrapper(self, name, *args, **kwargs):
        try:
            written.add(env_key(str(name)))
        except Exception:  # noqa: BLE001
            pass
        return orig(self, name, *args, **kwargs)

    return wrapper


def _noting(orig, note):
    def wrapper(obj, *args, **kwargs):
        note(obj)
        return orig(obj, *args, **kwargs)

    return wrapper


def _introspected_functions(obj) -> list[FunctionType]:
    """The Python functions whose headers introspecting ``obj`` reads.

    Only plain attribute lookups that can't run user code are used: no
    ``getattr`` on arbitrary objects (mocks and proxies answer anything).
    """
    import functools

    out: list[FunctionType] = []
    for _ in range(32):
        if isinstance(obj, MethodType):
            obj = obj.__func__
        elif isinstance(obj, functools.partial):
            obj = obj.func
        elif isinstance(obj, (staticmethod, classmethod)):
            obj = obj.__func__
        else:
            break
    if isinstance(obj, FunctionType):
        out.append(obj)
        # Follow the ``functools.wraps`` chain, as ``inspect.signature`` does.
        seen = {id(obj)}
        w = obj.__dict__.get("__wrapped__")
        while isinstance(w, FunctionType) and id(w) not in seen and len(out) < 32:
            out.append(w)
            seen.add(id(w))
            w = w.__dict__.get("__wrapped__")
    elif isinstance(obj, type):
        # A class's signature comes from ``__new__``/``__init__`` (or a
        # metaclass ``__call__``); its type hints from the class bodies.
        for name in ("__init__", "__new__"):
            for klass in obj.__mro__:
                f = klass.__dict__.get(name)
                if isinstance(f, staticmethod):
                    f = f.__func__
                if f is not None:
                    if isinstance(f, FunctionType):
                        out.append(f)
                    break
        call = type(obj).__dict__.get("__call__")
        if isinstance(call, FunctionType):
            out.append(call)
    return out


def _norm(path: str) -> str:
    return os.path.normcase(os.path.realpath(path))


def _pyc_to_source(path: str) -> str:
    """``pkg/__pycache__/mod.cpython-313.pyc`` → ``pkg/mod.py``."""
    head, tail = os.path.split(path)
    if os.path.basename(head) == "__pycache__" and tail.endswith(".pyc"):
        return os.path.join(os.path.dirname(head), tail.split(".", 1)[0] + ".py")
    return path


class Recorder:
    """Process-wide capture. Only one may be active per process."""

    _active: "Recorder | None" = None
    _audit_installed = False

    def __init__(self, repo_root: str) -> None:
        self.repo_root_real = os.path.realpath(repo_root)
        self.repo_root = os.path.normcase(self.repo_root_real)
        self._repo_prefix = self.repo_root.rstrip(os.sep) + os.sep
        self.packages = PackageIndex()
        self.seen_packages: dict[str, PackageInfo] = {}
        self.root = Collector()
        self.stack: list[Collector] = []
        self._lock = threading.RLock()
        self._file_cache: dict[str, tuple | None] = {}
        self._tool_id: int | None = None
        self._site_prefixes = self._site_dirs()
        self._own_dir = _norm(os.path.dirname(__file__)) + os.sep
        self.caches = CacheTracker()
        self.introspection = IntrospectionTracker(self._on_introspect)
        self.environ = EnvTracker(self._on_env)
        from .imports import ImportTracker

        self.imports = ImportTracker(self)

    @staticmethod
    def _site_dirs() -> tuple[str, ...]:
        dirs: set[str] = set()
        for key in ("purelib", "platlib"):
            p = sysconfig.get_paths().get(key)
            if p:
                dirs.add(p)
        try:
            dirs.update(site.getsitepackages())
        except AttributeError:
            pass
        user = getattr(site, "getusersitepackages", lambda: None)()
        if user:
            dirs.add(user)
        return tuple(sorted({_norm(d).rstrip(os.sep) + os.sep for d in dirs if d}))

    # Classification --------------------------------------------------------

    def classify_path(self, raw: str) -> tuple | None:
        """Returns ``("file", rel)``, ``("pkg", name)`` or ``None`` (ignored)."""
        cached = self._file_cache.get(raw)
        if cached is not None or raw in self._file_cache:
            return cached
        result: tuple | None = None
        if raw and not raw.startswith("<"):
            p = _norm(_pyc_to_source(raw))
            if p.startswith(self._own_dir):
                result = None
            elif any(p.startswith(s) for s in self._site_prefixes):
                info = self.packages.lookup(p)
                self.seen_packages.setdefault(info.name, info)
                result = ("pkg", info.name, info.native)
            elif p.startswith(self._repo_prefix):
                rel = p[len(self._repo_prefix):].replace(os.sep, "/")
                # Keep the repository's own spelling of the path (normcase lowercases on Windows).
                actual = os.path.relpath(os.path.realpath(_pyc_to_source(raw)), self.repo_root_real)
                result = ("file", actual.replace(os.sep, "/") if not actual.startswith("..") else rel)
        self._file_cache[raw] = result
        return result

    def _targets(self) -> list[Collector]:
        return self.stack if self.stack else [self.root]

    # sys.monitoring -------------------------------------------------------

    def _on_py_start(self, code: CodeType, _offset: int):
        self._record_code(code)
        return sys.monitoring.DISABLE

    def _on_introspect(self, obj) -> None:
        try:
            for f in _introspected_functions(obj):
                self._record_code(f.__code__)
        except Exception:  # noqa: BLE001 - a probe bug must not break the test run
            self._mark_untraced()

    def _on_env(self, name: str, value_hash: str) -> None:
        with self._lock:
            for c in self._targets():
                c.env.setdefault(name, value_hash)

    def _record_code(self, code: CodeType) -> None:
        try:
            cls = self.classify_path(code.co_filename)
            if cls is not None:
                with self._lock:
                    targets = self._targets()
                    if cls[0] == "file":
                        q = entity_qualname(code.co_qualname)
                        for c in targets:
                            if q is None:
                                c.files.add(cls[1])
                            else:
                                c.functions.add((cls[1], q))
                    else:
                        for c in targets:
                            c.packages.add(cls[1])
                            if cls[2]:
                                c.flags.add(FLAG_C_EXT)
        except Exception:  # noqa: BLE001 - a probe bug must not break the test run
            self._mark_untraced()

    def _mark_untraced(self) -> None:
        for c in self._targets():
            c.flags.add(FLAG_UNTRACED)

    # Audit hook -----------------------------------------------------------

    @staticmethod
    def _audit_trampoline(event: str, args: tuple) -> None:
        rec = Recorder._active
        if rec is not None:
            rec._on_audit(event, args)

    def _on_audit(self, event: str, args: tuple) -> None:
        try:
            if event == "open":
                path, mode = args[0], args[1]
                if isinstance(path, int) or path is None:
                    return
                if isinstance(mode, str) and any(m in mode for m in "wax") and "+" not in mode:
                    return  # pure writes are outputs, not inputs
                cls = self.classify_path(os.fsdecode(path))
                if cls is not None:
                    with self._lock:
                        for c in self._targets():
                            (c.files if cls[0] == "file" else c.packages).add(cls[1])
            elif event in _SUBPROCESS_EVENTS:
                self._flag(FLAG_SUBPROCESS)
            elif event == "socket.connect":
                addr = args[1] if len(args) > 1 else None
                host = addr[0] if isinstance(addr, tuple) and addr else None
                if isinstance(host, str) and not host.startswith(_LOOPBACK_PREFIXES):
                    self._flag(FLAG_NETWORK)
            elif event in _LISTING_EVENTS:
                target = args[0] if args else None
                if target is None:
                    target = "."
                if isinstance(target, (str, bytes, os.PathLike)):
                    cls = self.classify_path(os.fsdecode(target))
                    if cls is not None and cls[0] == "file" and self._listing_by_repo_code():
                        self._flag(FLAG_DYNAMIC_IMPORT)
        except Exception:  # noqa: BLE001
            self._mark_untraced()

    def _listing_by_repo_code(self) -> bool:
        """True if the nearest non-stdlib caller of a directory listing is
        repository code.

        The import system (``FileFinder``) and libraries such as pytest list
        directories for their own bookkeeping. Imports are already traced
        precisely, so only listings the project's own code performs are
        discovery hazards.
        """
        f = sys._getframe(2)
        for _ in range(40):
            if f is None:
                return False
            filename = f.f_code.co_filename
            if filename.startswith("<frozen importlib"):
                return False
            cls = self.classify_path(filename)
            if cls is not None:
                return cls[0] == "file"
            f = f.f_back
        return False

    def _flag(self, flag: str) -> None:
        # Flags describe tests. Root-scope activity (collection itself lists
        # directories) must not flag anything.
        if self.stack:
            with self._lock:
                for c in self.stack:
                    c.flags.add(flag)

    # Lifecycle ------------------------------------------------------------

    def start(self) -> None:
        if Recorder._active is not None:
            raise RuntimeError("a Quod recorder is already active in this process")
        mon = sys.monitoring
        for tool in (3, 4):
            if mon.get_tool(tool) is None:
                mon.use_tool_id(tool, "quod")
                self._tool_id = tool
                break
        else:
            raise RuntimeError("no free sys.monitoring tool id (3 and 4 are in use)")
        mon.register_callback(self._tool_id, mon.events.PY_START, self._on_py_start)
        mon.set_events(self._tool_id, mon.events.PY_START)
        Recorder._active = self
        self.caches.install()
        self.introspection.install()
        self.environ.install()
        self.imports.install()
        if not Recorder._audit_installed:
            sys.addaudithook(Recorder._audit_trampoline)  # cannot be removed; gated by _active
            Recorder._audit_installed = True

    def stop(self) -> None:
        if self._tool_id is not None:
            mon = sys.monitoring
            mon.set_events(self._tool_id, 0)
            mon.register_callback(self._tool_id, mon.events.PY_START, None)
            mon.free_tool_id(self._tool_id)
            self._tool_id = None
        self.caches.uninstall()
        self.introspection.uninstall()
        self.environ.uninstall()
        self.imports.uninstall()
        Recorder._active = None

    def boundary(self) -> None:
        """Starts a test: clears tracked caches so the test recomputes (and
        records) what it uses, then re-enables all disabled code locations."""
        self.caches.clear()
        sys.monitoring.restart_events()

    def push(self, c: Collector, fresh: bool = False) -> None:
        """Pushes a collector. With ``fresh``, re-enables every code location
        first, so the collector sees code that already ran earlier in this
        test. Shared (fixture and module) collectors need this: their contents
        are later attributed to other tests."""
        if fresh:
            sys.monitoring.restart_events()
        with self._lock:
            self.stack.append(c)

    def pop(self, c: Collector) -> None:
        with self._lock:
            if self.stack and self.stack[-1] is c:
                self.stack.pop()
            elif c in self.stack:
                self.stack.remove(c)
