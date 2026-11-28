//! Format versions and magic numbers. Changing any of these is a migration.

/// Magic bytes at the start of a `.qmap` footprint graph file.
pub const MAP_MAGIC: &[u8; 5] = b"QUODM";
/// Footprint graph format version.
pub const MAP_FORMAT_VERSION: u32 = 1;
/// Outcome-key envelope version.
pub const OUTCOME_KEY_VERSION: u32 = 1;
/// Certificate schema version.
pub const CERT_VERSION: u32 = 1;
/// Journal event schema version.
pub const JOURNAL_VERSION: u32 = 1;

/// Probe capability: signature and annotation introspection
/// (`inspect.signature`, `typing.get_type_hints`, `inspect.get_annotations`)
/// is recorded as a use of the introspected function. Python header
/// splitting is sound only for maps whose captures all declare it.
pub const CAP_PY_INTROSPECTION: &str = "py.introspection";

/// Probe capability: every function of a Python source file that runs while
/// recording is recorded as a function node, whatever else the test observes
/// in that file. A Python file with no function nodes then means no recorded
/// test executed any of its functions, so a change confined to one of them
/// affects none (rather than the whole file). ADR 022.
pub const CAP_PY_FUNCTIONS: &str = "py.functions";

/// Probe capability: captures carry environment and invocation fingerprints
/// and per-test environment variable reads, which turns on environment mode
/// in `quod select --python` (ADR 012).
pub const CAP_PY_ENVIRONMENT: &str = "py.environment";

/// Probe capability: the JavaScript probe's environment fingerprint (Node
/// runtime, platform, installed packages as `dist.npm:<name>`) and
/// `process.env` reads (ADR 012 applied to Node).
pub const CAP_JS_ENVIRONMENT: &str = "js.environment";

/// Probe capability: every test file was transpiled file by file without
/// type checking (Vitest outside typecheck mode), so tsconfig options that
/// only affect type checking can't change outcomes.
pub const CAP_JS_TRANSPILE_ONLY: &str = "js.transpile-only";
