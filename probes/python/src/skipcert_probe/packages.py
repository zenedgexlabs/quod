"""Attribution of installed files to distributions (packages)."""

from __future__ import annotations

import hashlib
import importlib.metadata as md
import os
import re
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
        self._unattributed = PackageInfo(SITE_PSEUDO_PACKAGE, "", False, "")

    def _build(self) -> dict[str, PackageInfo]:
        index: dict[str, PackageInfo] = {}
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
            for f in files:
                try:
                    p = os.path.normcase(os.path.realpath(dist.locate_file(f)))
                except Exception:  # noqa: BLE001
                    continue
                index.setdefault(p, info)
        return index

    def lookup(self, norm_path: str) -> PackageInfo:
        """Returns the owning package, or the ``__site__`` pseudo-package."""
        if self._by_path is None:
            self._by_path = self._build()
        return self._by_path.get(norm_path, self._unattributed)
