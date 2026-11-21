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
