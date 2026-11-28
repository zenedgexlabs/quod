"""``python -m quod_probe fingerprint [--env NAME]...``

Prints this environment's fingerprint as JSON, for ``quod select
--python``: ``{"environment": {...}, "env": {NAME: hash}}``. ``*`` hashes the
whole environment.
"""

from __future__ import annotations

import argparse
import json
import os
import sys

from . import PROBE_ID, fingerprint


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(prog="python -m quod_probe")
    sub = ap.add_subparsers(dest="cmd", required=True)
    fp = sub.add_parser("fingerprint", help="print the environment fingerprint")
    fp.add_argument("--env", action="append", default=[], metavar="NAME")
    args = ap.parse_args(argv)
    if args.cmd == "fingerprint":
        if "*" in args.env:
            fingerprint.debug_environ("select")
        env = {
            n: fingerprint.environ_hash() if n == "*" else fingerprint.env_value_hash(os.environ.get(n))
            for n in args.env
        }
        environment = fingerprint.environment()
        json.dump(
            {
                "probe": PROBE_ID,
                "environment": environment,
                # What the plugin re-checks before skipping: this environment.
                "environment_digest": fingerprint.digest(environment),
                "env": env,
            },
            sys.stdout,
            sort_keys=True,
        )
        sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
