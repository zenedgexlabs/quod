# Supported versions and known limits

## Supported

| | Versions | Notes |
|---|---|---|
| OS | Linux x86_64, macOS (arm64, x86_64), Windows x86_64 | Release binaries for each |
| Python | 3.12 and later | The probe uses `sys.monitoring` |
| pytest | 8 and later | pytest-xdist is supported |
| Node | 22 and later; 22.12+ for Jest | |
| Vitest | 5 | Projects (`test.projects`), `isolate: false` |
| Jest | 30 | |
| CI | GitHub Actions | Other CI systems can run the CLI directly ([Quickstart](../README.md#quickstart-python--pytest)) |

## When Quod runs every test

These are deliberate. When Quod can't prove a skip is safe, it runs the
test and the Verdict says why (the rule ID).

- **No map yet, or a stale one.** The first pull requests after you add
  the workflow run everything until a push to `main` has recorded a map
  (R007).
- **Configuration that affects every test**: test-runner config,
  `conftest.py`, CI workflows, runtime pins, build inputs, Quod's own
  policy (R001 to R005, R015).
- **Dependency changes Quod can't diff** (R006): an unparseable lockfile,
  or a manifest without a delta parser. `uv.lock`, `poetry.lock`,
  `pylock.toml`, `requirements*.txt`, `package-lock.json` and
  `pnpm-lock.yaml` are diffed per package; `yarn.lock` is not yet.
- **A different environment** (R018, environment mode): another OS,
  interpreter or set of installed packages than the map was recorded in.
- **Changed code that ran during start-up** (R017): conftest, plugins, or
  the root of a test session.

## Tests that always run

- New tests and tests without a complete footprint (R012).
- Tests that start subprocesses (R013): child processes aren't traced.
- Tests that use the network without a declared stub (R009).
- Tests that import modules dynamically from non-constant input (R008).
- Tests marked never-skip in the policy (R014).
- Changed test code (invariant I5).

## Where precision is lower

These are sound, but run more than strictly needed.

- **Module-level changes** (a new import, a changed constant) run every
  test that imported the module or ran any of its functions (ADR 019).
- **Modules that keep state** (ADR 014): a module-level dict, list or set
  that the module could change (anything but a read-only table of
  literals) makes every test that touches the module depend on every
  function in it. When a package's `__init__` imports that module, that's
  every test. For example, `RATES = {"EUR": Decimal("0.92")}` passed as a
  default argument counts as state.
- **Vitest with `isolate: false`** credits each test file with every
  module its worker loaded.
- **Adding a function to a Python module** counts as a module change.
  For JavaScript and TypeScript it doesn't, unless the module's namespace
  is observed (ADR 020; the Python part is proposed).
- **Fast suites.** Under about 10 seconds, start-up and collection
  dominate. Quod still gives a certificate for every skip, but saves
  little time.

## Not supported yet

- Other runners (unittest without pytest, Mocha, node:test), and other
  languages' test runners. The change parser covers Go, Rust and Java, but
  no probe records their tests yet.
- Coverage of code run in child processes or worker threads that the
  runner doesn't own.
- CI systems other than GitHub Actions through a packaged integration.
