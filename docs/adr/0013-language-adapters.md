# ADR 013: Any language through a line-coverage contract and tree-sitter entities

Status: Accepted (M1: core)

## Context

Everything except the probe and the Python source diff is already language
neutral:

- the footprint graph;
- policy and the ladder;
- certificates and the verifier;
- environment binding.

Supporting another language one bespoke probe at a time doesn't scale. Each
probe would also have to reproduce the engine's entity names exactly; Python
does this with `co_qualname` (ADR 011).

## Decision

### 1. Probes report lines, not names (capture protocol v1, additive)

A test (or root) record may carry `lines: [[path, [[start, end], ...]], ...]`:
the 1-based, inclusive source line ranges the test executed. At ingest,
Quod maps each line to the innermost entity that contains it. It uses
**the same tree-sitter extractor `quod-change` uses to diff changes**, at the
recorded commit. Probe and diff therefore can't disagree on names, and a
probe needs nothing but per-test line coverage, which every mainstream
runtime has:

- V8 precise coverage;
- JaCoCo;
- Go coverage counters;
- LLVM source-based coverage;
- Ruby `Coverage`;
- coverlet.

A covered line outside every definition is module-level execution, which
observes the file node. The Python probe keeps reporting `functions`. Both
forms may appear in the same record.

### 2. Language registry with declarative entity specs

`quod_change::lang` maps file extensions to a `Language`. For each language,
a spec gives:

- **definition node kinds**, how each is named, and whether it contains
  methods: a class, impl block or type;
- **body field**: what only runs when the entity runs. The header stays in
  the enclosing stream, as in Python without header splitting;
- **skip markers**, for the test-integrity rule (R011);
- **`static`**: whether module-level declarations can affect behaviour
  without executing any line. This is true of compiled languages
  (constants inlined at compile time, types, generics, macros) and false of
  dynamic ones.

Entity naming is internal (`Outer.inner`, with same-named definitions
combined, as in ADR 011). Only `quod-change` and ingest use it.

| language | extensions | entities | static |
|---|---|---|---|
| Python | `.py` | existing `python.rs` (ADR 011) | no |
| JavaScript | `.js` `.mjs` `.cjs` `.jsx` | functions, generators, classes, methods, `const f = () => …` / `function` expressions | no |
| TypeScript | `.ts` `.mts` `.cts`, `.tsx` | as JavaScript, plus abstract methods | no |
| Go | `.go` | functions, methods (`Recv.Name`) | **yes** |
| Rust | `.rs` | functions, `impl`/`trait` blocks, `mod`s | **yes** |
| Java | `.java` | classes, interfaces, enums, records, methods, constructors | **yes** |
| anything else | any | none: the file is one node | yes |

### 3. What a probe must record (the contract)

- **`lines`:** every line the test executed, including in shared setup.
  Observations made outside any single test (module evaluation, `beforeAll`,
  shared fixtures) must be attributed to every test they serve. This is the
  Python probe's collector rule, restated.
- **`files` for static languages:** every source file compiled into the
  test's unit (the test binary's or module's static dependency closure, from
  the build tool). A module-level change there (a constant, type, macro or
  import) changes the file node. With the closure in `files`, every test that
  could see the change re-runs, even though no line of it executed.
- **`files` for dynamic languages:** files whose module-level code ran.
  Ingest also derives these from `lines` outside definitions.
- **Flags:** `network`, `subprocess`, `untraced` and so on, as today.
  Anything the probe can't attribute is `untraced`, which fails closed (R012).
- **Header:** `language`, the runtime and test-runner identity, and the
  environment fingerprint where the adapter has one (ADR 012). Without a
  fingerprint, path mode applies.

### 4. Fail-closed defaults for languages without a spec

A file in an unknown language is a single file node, and any change to it is
a change to that node. Lockfiles of unknown ecosystems keep firing R005 or
R006 globally until an ecosystem parser exists.

## Plan

| milestone | scope |
|---|---|
| **M1 (this ADR)** | protocol `lines`; language registry; generic tree-sitter extractor for JS, TS/TSX, Go, Rust and Java; line→entity ingest; skip markers; tests |
| M2 | JS/TS probe: Vitest and Jest with per-test V8 precise coverage via the inspector and source-map remapping; npm/pnpm/yarn lockfile deltas; Node environment fingerprint; E1 on JS repos |
| M3 | Go adapter: per-test counters via `runtime/coverage` in `TestMain`; `go list -deps -test` closure; `go.sum` deltas |
| M4 | JVM adapter: JUnit 5 listener plus JaCoCo agent reset per test; Gradle and Maven classpath closure |
| M5 | Generic importer: per-test LCOV (`TN:` blocks) or Cobertura, for any other runner |

## Assumptions

- **Compiled languages.** Soundness for static languages depends on the
  adapter's `files` closure, which comes from the build tool. Without it,
  module-level changes in a static language would be missed. An adapter that
  can't produce the closure must flag tests `untraced`.
- **TypeScript type-only changes.** Interfaces and type aliases are erased at
  runtime but count as module-level changes. That is conservative.
- **Line-coverage granularity.** A function whose definition and body share
  one line maps to that function. Module-level execution of such a file is
  still recorded through the module-level lines a module evaluation always
  executes, and dynamic-language probes also report the file in `files`.
