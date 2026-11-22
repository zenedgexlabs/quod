"""Attribution of installed files to distributions (packages)."""

from __future__ import annotations

import hashlib
import importlib.metadata as md
import os
import re
import site
import sysconfig
from dataclasses import dataclass

_NATIVE_SUFFIXES = (".so", ".pyd", ".dll", ".dylib")


def normalize_name(name: str) -> str:
    """PEP 503 normalisation. Must match the Rust `sc_change::lock::normalize_name`."""
    return re.sub(r"[-_.]+", "-", name).lower()


@dataclass(frozen=True)
class PackageInfo:
    name: str
    version: str
    native: bool
    dist_hash: str  # sha256 hex of the distribution's RECORD (or file list)


SITE_PSEUDO_PACKAGE = "__site__"
"""Package name for installed files no distribution claims (for example
``_virtualenv.py``). Any lockfile change also changes this node, so tests
that touched such files are re-run on dependency changes."""


class PackageIndex:
    """Maps absolute file paths to the distribution that installed them.

    The index is built lazily on first lookup, from every distribution's RECORD.
    """

    def __init__(self) -> None:
        self._by_path: dict[str, PackageInfo] | None = None
        self._infos: list[PackageInfo] = []
        self._unattributed = PackageInfo(SITE_PSEUDO_PACKAGE, "", False, "")

    def _build(self) -> dict[str, PackageInfo]:
        index: dict[str, PackageInfo] = {}
        infos: dict[str, PackageInfo] = {}
        for dist in md.distributions():
            try:
                name = dist.metadata["Name"]
            except Exception:  # noqa: BLE001 - broken metadata must not break the run
                continue
            if not name:
                continue
            files = list(dist.files or [])
            record = dist.read_text("RECORD") or "\n".join(sorted(str(f) for f in files))
            info = PackageInfo(
                name=normalize_name(name),
                version=dist.version or "",
                native=any(str(f).endswith(_NATIVE_SUFFIXES) for f in files),
                dist_hash=hashlib.sha256(record.encode("utf-8", "surrogateescape")).hexdigest(),
            )
            # The first distribution on sys.path wins, as for imports.
            infos.setdefault(info.name, info)
            for f in files:
                try:
                    p = os.path.normcase(os.path.realpath(dist.locate_file(f)))
                except Exception:  # noqa: BLE001
                    continue
                index.setdefault(p, info)
        self._infos = sorted(infos.values(), key=lambda i: i.name)
        return index

    def distributions(self) -> list[PackageInfo]:
        """Every installed distribution, sorted by name."""
        if self._by_path is None:
            self._by_path = self._build()
        return list(self._infos)

    def site_hash(self) -> str:
        """Content hash of the installed files no distribution claims
        (``.pth`` files, ``sitecustomize.py``, virtualenv helpers), which is
        the identity of the ``__site__`` pseudo-package. Bytecode caches are
        ignored."""
        if self._by_path is None:
            self._by_path = self._build()
        claimed = self._by_path
        h = hashlib.sha256()
        for d in site_dirs():
            for dirpath, dirnames, filenames in os.walk(d):
                dirnames[:] = sorted(n for n in dirnames if n != "__pycache__")
                for name in sorted(filenames):
                    if name.endswith((".pyc", ".pyo")):
                        continue
                    full = os.path.join(dirpath, name)
                    if os.path.normcase(os.path.realpath(full)) in claimed:
                        continue
                    rel = os.path.relpath(full, d).replace(os.sep, "/")
                    if ".dist-info/" in rel or ".egg-info/" in rel:
                        continue  # metadata is covered by the distribution's own hash
                    try:
                        with open(full, "rb") as f:
                            data = f.read()
                    except OSError:
                        data = b"<unreadable>"
                    h.update(rel.encode("utf-8", "surrogateescape") + b"\0")
                    h.update(hashlib.sha256(data).digest())
        return h.hexdigest()

    def lookup(self, norm_path: str) -> PackageInfo:
        """Returns the owning package, or the ``__site__`` pseudo-package."""
        if self._by_path is None:
            self._by_path = self._build()
        return self._by_path.get(norm_path, self._unattributed)


def site_dirs() -> list[str]:
    """Installation directories (site-packages), deduplicated and sorted."""
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
    out: dict[str, str] = {}
    for d in dirs:
        if d and os.path.isdir(d):
            out.setdefault(os.path.normcase(os.path.realpath(d)), d)
    return [out[k] for k in sorted(out)]
