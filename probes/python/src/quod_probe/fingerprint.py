"""Environment and invocation fingerprints (ADR 012).

The *environment* fingerprint is a flat ``key -> value`` map of everything
outside the repository that can change test outcomes without going through
code the probe traces: the interpreter, system libraries, environment
variables read at C level, and every installed distribution. ``quod
select`` compares it with the one recorded in the map. Any difference outside
``dist.*`` runs everything (R018). ``dist.*`` differences become changed
package nodes.

The *invocation* fingerprint is the pytest command line and ini
configuration, minus Quod's own options. The plugin checks it at session
start against the digest in the skip file.

Environment variable values are stored only as hashes (``env_value_hash``).
"""

from __future__ import annotations

import hashlib
import json
import locale
import os
import platform
import sys
import time
import warnings

from .packages import SITE_PSEUDO_PACKAGE, PackageIndex

_HASH_PREFIX = b"quod v1 env value\0"
_ABSENT = b"absent"

# Variables the interpreter and C libraries read without going through
# ``os.environ``, so per-test tracking can't see them.
_PROCESS_ENV_PREFIXES = ("PYTHON", "LC_")
_PROCESS_ENV_NAMES = frozenset({"TZ", "LANG", "LANGUAGE"})
# pytest reads these before any plugin is loaded.
_PYTEST_ENV_NAMES = ("PYTEST_ADDOPTS", "PYTEST_PLUGINS", "PYTEST_DISABLE_PLUGIN_AUTOLOAD", "PYTEST_DEBUG")

QUOD_OPTIONS = ("--quod-record", "--quod-skip", "--quod-root", "--quod-results")

# Variables the test runner sets in its own process (pytest, pytest-xdist).
# They aren't inputs: their values follow from the installed distributions,
# which ``dist.*`` covers, and they are absent where ``quod select``
# fingerprints the environment. Reads of them aren't recorded, and ``env:*``
# leaves them out.
RUNNER_ENV_NAMES = frozenset(
    {
        "PYTEST_VERSION",
        "PYTEST_CURRENT_TEST",
        "PYTEST_XDIST_WORKER",
        "PYTEST_XDIST_WORKER_COUNT",
        "PYTEST_XDIST_TESTRUNUID",
    }
)


def env_key(name: str) -> str:
    """The canonical spelling of an environment variable name (upper case
    on Windows, where names are case-insensitive)."""
    return name.upper() if os.name == "nt" else name


def env_value_hash(value: str | None) -> str:
    """Hash of an environment variable's value, or of its absence."""
    data = _ABSENT if value is None else b"present\0" + value.encode("utf-8", "surrogateescape")
    return hashlib.sha256(_HASH_PREFIX + data).hexdigest()


# CI bookkeeping (ADR 021): per-run identifiers and runner-private paths
# that differ on every CI run. Left out of ``env:*`` only, the observation
# that code iterated the whole environment; reading one by name is still
# recorded. A test whose outcome depended on them through iteration would
# differ from run to run on the same commit.
CI_BOOKKEEPING_NAMES = frozenset(
    {
        # GitHub Actions
        "GITHUB_ACTION",
        "GITHUB_ACTION_PATH",
        "GITHUB_ACTION_REF",
        "GITHUB_ACTION_REPOSITORY",
        "GITHUB_ACTOR",
        "GITHUB_ACTOR_ID",
        "GITHUB_ARTIFACTS",
        "GITHUB_ARTIFACTS_LIST",
        "GITHUB_BASE_REF",
        "GITHUB_ENV",
        "GITHUB_EVENT_NAME",
        "GITHUB_EVENT_PATH",
        "GITHUB_HEAD_REF",
        "GITHUB_JOB",
        "GITHUB_OUTPUT",
        "GITHUB_PATH",
        "GITHUB_REF",
        "GITHUB_REF_NAME",
        "GITHUB_REF_PROTECTED",
        "GITHUB_REF_TYPE",
        "GITHUB_RUN_ATTEMPT",
        "GITHUB_RUN_ID",
        "GITHUB_RUN_NUMBER",
        "GITHUB_SHA",
        "GITHUB_STATE",
        "GITHUB_STEP_SUMMARY",
        "GITHUB_TRIGGERING_ACTOR",
        "GITHUB_WORKFLOW",
        "GITHUB_WORKFLOW_REF",
        "GITHUB_WORKFLOW_SHA",
        "RUNNER_NAME",
        "RUNNER_TRACKING_ID",
        # systemd, which starts the runner
        "INVOCATION_ID",
        "JOURNAL_STREAM",
        "SYSTEMD_EXEC_PID",
    }
)
CI_BOOKKEEPING_PREFIXES = ("ACTIONS_",)


def ci_bookkeeping(key: str) -> bool:
    return key in CI_BOOKKEEPING_NAMES or key.startswith(CI_BOOKKEEPING_PREFIXES)


def environ_hash(environ: dict[str, str] | None = None) -> str:
    """Hash of the whole environment (the ``env:*`` node), minus the
    variables the test runner sets itself, Quod's own, and CI bookkeeping."""
    env = os.environ if environ is None else environ
    items = sorted(
        (key, v)
        for key, v in ((env_key(k), v) for k, v in env.items())
        if key not in RUNNER_ENV_NAMES and not key.startswith("QUOD_") and not ci_bookkeeping(key)
    )
    return hashlib.sha256(_HASH_PREFIX + json.dumps(items).encode("utf-8", "surrogateescape")).hexdigest()


def digest(fp: dict[str, str]) -> str:
    """Canonical digest of a fingerprint map."""
    return hashlib.sha256(json.dumps(fp, sort_keys=True, separators=(",", ":")).encode("utf-8")).hexdigest()


def _safe(f) -> str:
    try:
        v = f()
    except Exception as e:  # noqa: BLE001 - record the failure itself as the value
        return f"<error {type(e).__name__}>"
    return "" if v is None else str(v)


def _system() -> dict[str, str]:
    out = {
        "system.os": platform.system(),
        # Not the kernel release (ADR 012 amendment): it varies across a CI
        # provider's own fleet, and the OS, libc and libraries are here.
        "system.libc": _safe(lambda: " ".join(platform.libc_ver())),
        "system.tzname": _safe(lambda: ",".join(time.tzname)),
        "system.timezone_offset": _safe(lambda: time.timezone),
        "system.locale": _safe(lambda: ",".join(str(x) for x in locale.getlocale())),
        "system.preferred_encoding": _safe(lambda: locale.getpreferredencoding(False)),
        "system.fs_encoding": sys.getfilesystemencoding(),
        "system.cpu_count": _safe(os.cpu_count),
    }
    container = os.environ.get("QUOD_CONTAINER_DIGEST")
    if container:
        out["system.container"] = container

    def lib(key: str, f) -> None:
        out[f"system.{key}"] = _safe(f)

    lib("openssl", lambda: __import__("ssl").OPENSSL_VERSION)
    lib("sqlite", lambda: __import__("sqlite3").sqlite_version)
    lib("zlib", lambda: __import__("zlib").ZLIB_RUNTIME_VERSION)
    lib("expat", lambda: __import__("pyexpat").EXPAT_VERSION)
    return out


def _interpreter() -> dict[str, str]:
    flags = sys.flags
    out = {
        "interpreter.implementation": sys.implementation.name,
        "interpreter.version": sys.version,
        "interpreter.cache_tag": str(sys.implementation.cache_tag),
        "interpreter.platform": sys.platform,
        "interpreter.machine": platform.machine(),
        "interpreter.byteorder": sys.byteorder,
        "interpreter.gil": _safe(lambda: sys._is_gil_enabled()) if hasattr(sys, "_is_gil_enabled") else "1",
    }
    for name in (
        "optimize",
        "dev_mode",
        "utf8_mode",
        "hash_randomization",
        "safe_path",
        "isolated",
        "warn_default_encoding",
        "ignore_environment",
        "bytes_warning",
        "int_max_str_digits",
    ):
        out[f"interpreter.flags.{name}"] = str(getattr(flags, name, ""))
    out["interpreter.warnoptions"] = json.dumps(sys.warnoptions)
    out["interpreter.xoptions"] = json.dumps(sorted(f"{k}={v}" for k, v in sys._xoptions.items()))
    return out


def _process_env() -> dict[str, str]:
    out = {}
    for k, v in os.environ.items():
        key = env_key(k)
        if key.startswith(_PROCESS_ENV_PREFIXES) or key in _PROCESS_ENV_NAMES:
            out[f"process_env.{key}"] = env_value_hash(v)
    return out


def _distributions(index: PackageIndex) -> dict[str, str]:
    out = {}
    for info in index.distributions():
        out[f"dist.{info.name}"] = f"{info.version} {info.dist_hash}"
    out[f"dist.{SITE_PSEUDO_PACKAGE}"] = index.site_hash()
    return out


def environment(index: PackageIndex | None = None) -> dict[str, str]:
    """The environment fingerprint of this process."""
    fp: dict[str, str] = {}
    fp.update(_interpreter())
    fp.update(_system())
    fp.update(_process_env())
    fp.update(_distributions(index or PackageIndex()))
    return fp


def strip_quod_args(args) -> list[str]:
    """Command-line arguments without Quod's own options."""
    out: list[str] = []
    skip_next = False
    for a in args:
        a = str(a)
        if skip_next:
            skip_next = False
            continue
        if a in QUOD_OPTIONS:
            skip_next = True
            continue
        if a.startswith(tuple(o + "=" for o in QUOD_OPTIONS)):
            continue
        out.append(a)
    return out


def invocation(config, root: str | None = None) -> dict[str, str]:
    """The invocation fingerprint of a pytest ``Config`` (early or full).

    With the repository ``root``, it includes the directory pytest was
    started from, relative to it: rootdir and conftest discovery depend on
    it, and it is what an agent needs to run the same invocation again.
    """
    out = {"invocation.args": json.dumps(strip_quod_args(config.invocation_params.args))}
    if root is not None:
        try:
            rel = os.path.relpath(str(config.invocation_params.dir), root).replace(os.sep, "/")
        except ValueError:  # another drive
            rel = str(config.invocation_params.dir)
        out["invocation.dir"] = rel
    inipath = getattr(config, "inipath", None)
    rootpath = getattr(config, "rootpath", None)
    if inipath is not None and rootpath is not None:
        try:
            out["invocation.inifile"] = os.path.relpath(str(inipath), str(rootpath)).replace(os.sep, "/")
        except ValueError:
            out["invocation.inifile"] = str(inipath)
    else:
        out["invocation.inifile"] = ""
    # ``Config.inicfg`` is deprecated (pytest 9) and warns, which suites
    # running with ``-W error`` turn into an error.
    with warnings.catch_warnings():
        warnings.simplefilter("ignore")
        inicfg = getattr(config, "inicfg", None)
    if inicfg is None:
        inicfg = getattr(config, "_inicfg", None) or {}
    for k in sorted(inicfg):
        v = inicfg[k]
        v = getattr(v, "value", v)  # pytest >= 8.4 wraps values
        out[f"invocation.ini.{k}"] = json.dumps(v, sort_keys=True, default=str)
    for name in _PYTEST_ENV_NAMES:
        out[f"invocation.env.{name}"] = env_value_hash(os.environ.get(name))
    return out
