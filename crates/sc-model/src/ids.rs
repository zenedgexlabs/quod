//! Stable, project-scoped test identifiers.

use std::fmt;

use sc_hash::{Digest, Domain, hash_domain};
use serde::{Deserialize, Serialize};

/// A test identifier that is stable across machines, operating systems and
/// runs.
///
/// It is derived from the project-relative path (always `/`-separated), the
/// fully qualified test name, and the canonical parametrization string. Each
/// field is length-prefixed, so field boundaries can't be confused.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StableTestId(pub Digest);

impl StableTestId {
    /// Derives the stable ID for a test.
    pub fn derive(project_rel_path: &str, qualified_name: &str, params_canonical: &str) -> Self {
        let path = project_rel_path.replace('\\', "/");
        let mut buf =
            Vec::with_capacity(path.len() + qualified_name.len() + params_canonical.len() + 24);
        for field in [path.as_str(), qualified_name, params_canonical] {
            buf.extend_from_slice(&(field.len() as u64).to_le_bytes());
            buf.extend_from_slice(field.as_bytes());
        }
        Self(hash_domain(Domain::StableTestId, &buf))
    }
}

impl fmt::Display for StableTestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl fmt::Debug for StableTestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "StableTestId({})", self.0.to_hex())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_separators_are_normalized() {
        assert_eq!(
            StableTestId::derive(r"tests\api\test_users.py", "test_create", ""),
            StableTestId::derive("tests/api/test_users.py", "test_create", "")
        );
    }

    #[test]
    fn parametrization_distinguishes_tests() {
        assert_ne!(
            StableTestId::derive("t.py", "test_x", "[1]"),
            StableTestId::derive("t.py", "test_x", "[2]")
        );
    }

    #[test]
    fn field_boundaries_are_unambiguous() {
        assert_ne!(
            StableTestId::derive("a", "bc", ""),
            StableTestId::derive("ab", "c", "")
        );
    }
}
