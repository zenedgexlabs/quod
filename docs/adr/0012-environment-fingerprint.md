# ADR 012: Environment fingerprinting replaces path-based environment rules

Status: Accepted (engine 0.1, python-probe 0.3)

## Context

E1 run 3 found zero escapes, but most history time on attrs, jinja and
itsdangerous went to policy-forced full runs:

| repo | policy full runs | share of suite time |
|---|---|---|
| attrs | 5 | 0.13 |
| jinja | 8 | 0.35 |
| itsdangerous | 7 | 0.30 |

Nearly all of them are R001 or R002 firing on *files that could change the
test environment*:

- workflow action bumps;
- `tox.ini` and `tool.tox`;
- `tool.uv` and `dependency-groups`;
- `project.version` and `requires-python`.

These rules have two problems:

1. **Imprecise.** Most such edits leave the environment the tests run in
   unchanged.
2. **Incomplete.** The environment changes without any file changing: a runner
   image update, a different `PYTHONHASHSEED`, a new OpenSSL, a CI variable a
   test reads. Path rules never see these.

The environment is the thing that matters, and it can be observed directly.

## Decision

### 1. Fingerprint (probe 0.3, capability `py.environment`)

`quod_probe.fingerprint` produces a flat, sorted map of `key → value`
strings. It is also available as `python -m quod_probe fingerprint`.

| group | keys |
|---|---|
| `interpreter.*` | implementation, full version, cache tag, `sys.platform`, machine, byte order, and `sys.flags` (optimize, dev_mode, utf8_mode, hash_randomization, safe_path, isolated, warn_default_encoding) |
| `system.*` | OS release, libc, OpenSSL, SQLite, zlib, expat, `time.tzname`, locale and preferred encoding, filesystem encoding, CPU count, and container digest (`QUOD_CONTAINER_DIGEST` when set) |
| `process_env.*` | `PYTHON*`, `TZ`, `LANG`, `LANGUAGE` and `LC_*`, which the interpreter and C libraries read without going through `os.environ` |
| `dist.<name>` | `version` plus `dist_hash` (RECORD hash) for every installed distribution, plus `__site__` (unclaimed installed files) |

The pytest *invocation* is fingerprinted separately as `invocation.*`: the
arguments without `--quod-*`, the ini file path relative to the root, and
the raw ini values. The record header stores both fingerprints.

### 2. Per-test environment variable reads

The probe patches `os._Environ` (`__getitem__`, `__contains__`, `get` through
`__getitem__`, and `__iter__`/`keys`/`items`/`copy`).

- **Read of one key.** It records `env:NAME` with the value hash
  (sha256 of the value, or of an "absent" marker) for the current collector.
- **Iteration.** It records `env:*` with the hash of the whole environment.
  The test then re-runs whenever anything in the environment differs.
- **Keys the process wrote itself** (fixtures, `monkeypatch`, CliRunner) are
  in-process state, not inputs, so later reads of them aren't recorded.

Root-scope reads go to the root collector as usual. A changed value
there triggers R017.

### 3. Select in environment mode

`quod select --python <exe>` runs
`<exe> -m quod_probe fingerprint --env NAME...` for every `env:` node in
the map and compares the result with the map's fingerprint. Environment mode
is on only when the map's captures all declare `py.environment` and
`--python` is given. Otherwise the path-based rules apply unchanged
(fail-closed fallback).

In environment mode:

| rule | path mode (today) | environment mode |
|---|---|---|
| R001 CI configuration | global on test workflows and CI files | **off**. Its effects reach tests only through the environment, the invocation, or files, which are all observed. |
| R003 runtime pin files | global | **off**, replaced by `interpreter.*` |
| R002 root configuration | test-relevant sections | only pytest sections: `tool.pytest*`, `[pytest]`, `[tool:pytest]`. `conftest.py` and jest/vitest stay. `noxfile.py`, tox and uv sections are off. |
| lockfile deltas | inferred package nodes plus `__site__` | **replaced** by the exact `dist.*` difference. Lockfile edits alone change nothing. |
| **R018** (new, global) | none | `interpreter.*`, `system.*` or `process_env.*` differs |
| `env:` nodes | none | per-test changed nodes, by value hash |

### 4. Binding and runtime enforcement

- **Certificates.** `PinnedInputs` gains an optional `environment`: the
  map's environment binding, a hash of the recorded environment and
  invocation fingerprints. The target environment matched it, apart from
  changed distributions and variables. It is omitted in path mode, so
  path-mode certificates hash exactly as before and `CERT_VERSION` stays 1.
  The verifier checks the binding against the map.
- **Skip file.** It gains header lines `# quod-environment <digest>` and
  `# quod-invocation <digest>`, plus `# quod-env NAME HASH` for the
  `env:` nodes of skipped tests. At session start, the pytest plugin
  recomputes all three. On any mismatch it **ignores the skip list**, runs
  everything and prints why. This closes the gap where select and pytest run
  in different processes, CI steps or interpreters, which is why the
  invocation check lives in the plugin.
- **Generated files.** Observed repository files that git ignores (generated
  sources, in-place built extensions) are re-hashed at select time. A
  difference is a changed node. Path mode doesn't do this today.

## Assumptions (documented, audited)

- **C-level system queries outside the fingerprint aren't observed.** Examples
  are `os.cpu_count` beyond the recorded value, `/proc` and `/etc` reads, and
  hardware. Tests that open such files go through the audit hook. Container
  digests pin the rest when available.
- **Value hashes.** Environment values are stored as unsalted sha256. Low-entropy
  secret values could be brute-forced from a map. Maps are trusted-zone
  artefacts, and `env_hash = "presence"` can be set in `quod.toml` to
  record presence only (re-run on any change).

## Consequences

- **Expected on run 3's history:** policy-forced full runs drop from 5, 8 and
  7 (attrs, jinja, itsdangerous) to the steps that change pytest sections,
  conftest or real dependencies. This has to be measured by E1 run 5. The
  harness passes `--python` and runs every step in one venv, so dependency
  bumps it doesn't install are genuinely no change.
- **Version bumps stay full runs.** `dist.<project>` changes, and the root
  scope observes all distribution metadata (ADR 011).
- **New moving part.** `select` needs the target interpreter. The default CI
  recipe already has it installed before tests run.

## Addendum: Node (`js.environment`, 2026-11-24)

The JavaScript probe implements the same contract:

- **Fingerprint keys.** `interpreter.*` covers Node, its bundled libraries
  (`process.versions`) and `execArgv`. `system.*` covers the OS release, time
  zone, locale and CPU count. `process_env.*` covers `NODE_*`, `UV_*`, `TZ`,
  `LANG` and `LC_*`. `dist.npm:<name>` covers every installed copy's version
  and `package.json` hash, under every `node_modules` from the test directory
  up to the filesystem root, including pnpm's `.pnpm` store.
- **Per-test variable reads.** `process.env` reads are recorded through a
  Proxy (enumeration is `env:*`). Writes, including `vi.stubEnv`, are
  in-process state, not inputs.
- **Where fingerprints come from.** The CLI computes them before Vitest starts,
  because Vitest sets `NODE_ENV` itself. `quod-vitest fingerprint`
  serves `select --fingerprint-cmd`, and `quod-vitest run` checks the
  skip-file header in that same context. On any mismatch it runs every test.
- **Generic select option.** `select --fingerprint-cmd ARG...` (one flag per
  argument) runs any probe's fingerprint command. `--python` is shorthand
  for the Python probe's.
- **npm lockfiles** (2026-11-26). With a `js.environment` map, npm lockfile
  edits change nothing by themselves, as Python ones don't: the installed
  packages are compared directly (`EnvironmentDelta::npm_observed`). A map
  with only a Python fingerprint still counts them.
