//! Errors for schema construction and validation.

use quod_hash::HashError;

/// Errors raised while building or validating v1 objects.
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    /// Hashing or canonical encoding failed.
    #[error(transparent)]
    Hash(#[from] HashError),
    /// The object can't be used as an L1 dedup key (for example, it has an
    /// unknown key extension). Callers must fall back to L0 or run the test.
    #[error("outcome key not reusable: {0}")]
    NotReusable(String),
    /// The object violates a structural rule of the v1 schema.
    #[error("invalid object: {0}")]
    Invalid(String),
}
