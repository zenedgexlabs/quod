//! Format versions and magic numbers. Changing any of these is a migration.

/// Magic bytes at the start of a `.scmap` footprint graph file.
pub const MAP_MAGIC: &[u8; 5] = b"SCMAP";
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

/// Probe capability: captures carry environment and invocation fingerprints
/// and per-test environment variable reads, which turns on environment mode
/// in `skipcert select --python` (ADR 012).
pub const CAP_PY_ENVIRONMENT: &str = "py.environment";
