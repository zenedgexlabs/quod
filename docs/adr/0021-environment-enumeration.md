# ADR 021: Enumerating the environment, and CI bookkeeping

Status: Accepted (probes: Python 0.4.3, JS 0.2.1; Action step settings as `QUOD_*` from quod-action v0.1.0-alpha.6)

## Context

E9 on networkx: every pull request ran all 9,239 tests. At import,
`networkx/utils/backends.py` reads its settings from every variable with a
prefix:

```python
priorities = {k[26:].lower(): v for k, v in os.environ.items()
              if k.startswith("NETWORKX_BACKEND_PRIORITY_")}
```

The probe recorded two things for that line, both in the root scope:

- `env:*`, the hash of the whole environment, because the code iterated
  it;
- a read of every variable by name, because `items()` reads each key
  through `__getitem__`.

On GitHub Actions the environment differs on every run: `GITHUB_RUN_ID`,
`GITHUB_SHA`, `GITHUB_REF`, the paths of the step's output files,
`ACTIONS_ORCHESTRATION_ID`, systemd's `INVOCATION_ID`. The Action also
leaked its own step variables (`CMD`, `RUNNER`) into the test process. So
the root scope's footprint changed on every run, R017 fired, and nothing
was ever skipped. Any project whose start-up enumerates the environment
(configuration from prefixed variables is common) would see the same.

## Decision

1. **A bulk read is one observation.** `os.environ.items()`, `.values()`
   and `.copy()` record `env:*` only, not a read of every key, and return
   a snapshot (as `copy()` always did). `env:*` already covers every
   variable. Code that iterates the keys and then reads some by name
   (`for k in os.environ: if k.startswith(P): os.environ[k]`) still
   records those names, which is more precise. `dict(os.environ)` still
   reads key by key; that's conservative.

2. **`env:*` leaves out CI bookkeeping.** The hash behind `env:*` skips
   per-run identifiers and runner-private paths:
   `fingerprint.CI_BOOKKEEPING_NAMES` (GitHub's `GITHUB_RUN_ID`,
   `GITHUB_SHA`, `GITHUB_REF`, `GITHUB_EVENT_NAME`, `GITHUB_OUTPUT`, ...,
   `RUNNER_NAME`, `RUNNER_TRACKING_ID`, systemd's `INVOCATION_ID`,
   `JOURNAL_STREAM`, `SYSTEMD_EXEC_PID`) and every `ACTIONS_*` variable.
   The JS probe uses the same list. Reading one of these by name is still
   recorded as `env:NAME`: only the enumeration ignores them.

3. **Quod's own variables aren't inputs.** Reads of `QUOD` (the binary the
   Action passes) and `QUOD_*` aren't recorded, and `env:*` leaves them
   out. The Action passes its step settings as `QUOD_*`.

4. **Shell bookkeeping.** `_` (bash sets it to the path of the command it
   runs: quod in the Plan step, python in the test step), `OLDPWD` and
   `SHLVL` are left out of `env:*` too (found on networkx, after 1-3).

5. **The runner image.** GitHub rotates image versions and machine types:
   across nine jobs of one replay, `ImageVersion`, `GRADLE_HOME`,
   `HCA_CLOUD_PROVIDER` and `SGX_AESM_ADDR` differed. The variables that
   describe the image (its version, host settings, and where the tools it
   ships live: `JAVA_HOME_*`, `ANDROID_*`, `DOTNET_*`, `GOROOT_*`, browser
   drivers, ...) are left out of `env:*`. `PATH`, `HOME`, `LANG`,
   `LD_LIBRARY_PATH`, `CI` and the interpreter's locations are not: a test
   can depend on them through enumeration. A test that runs one of the
   image's tools does so in a subprocess, and always runs (R013).

## Why this is sound enough

The exclusion is an assumption, and it is documented as one: a test whose
outcome depends on a CI bookkeeping variable that it only reaches by
enumerating the environment is not covered. Such a test would already
behave differently from one run to the next on the same commit, since
these values change every run, so its outcome isn't a function of the
change. Code that reads one of them by name, the way real code checks
`GITHUB_EVENT_NAME`, is still tracked. Tests that start subprocesses,
which may hand the environment on, always run (R013).

## Consequences

- networkx's start-up records `env:*` plus the dozen variables it reads by
  name, instead of every variable.
- Other variables that differ between the recording job and a pull
  request job still widen correctly: a changed `env:*` means every test
  that enumerated the environment runs.
- New CI providers need their own bookkeeping names added to both lists.
