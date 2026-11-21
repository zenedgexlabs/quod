//! Node and test vocabulary of the footprint graph (`.scmap` v1).
//!
//! The binary layout (string table, node table, forward and inverse CSR) lives
//! in `sc-graph`. This module defines only the types it stores, so the graph is
//! bidirectional and package-aware from v1 on (ADR-002).

use serde::{Deserialize, Serialize};

use sc_hash::Digest;

/// Kinds of entities a test can observe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// A file in the repository.
    File,
    /// A function or method inside a repository file (AST-hashed).
    Function,
    /// An imported module.
    Module,
    /// An external package (name, version, distribution hash).
    ExtPackage,
    /// A file inside an external package.
    ExtFile,
    /// An environment variable that was read.
    EnvVar,
    /// A network endpoint that was contacted.
    NetEndpoint,
    /// A typed external resource (database schema, container image, contract, ...).
    Resource,
    /// A test node.
    Test,
}

/// Package ecosystems with a lockfile resolver.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ecosystem {
    /// Python packages (pip, uv, poetry).
    Pypi,
    /// JavaScript packages (npm, pnpm, yarn).
    Npm,
}

/// An external package as recorded in a footprint.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ExtPackage {
    /// Package ecosystem.
    pub ecosystem: Ecosystem,
    /// Normalized package name.
    pub name: String,
    /// Exact installed version.
    pub version: String,
    /// Hash of the installed distribution (wheel RECORD or npm integrity).
    pub dist_hash: Digest,
    /// True if the package ships native code. A native package changing is
    /// fail-closed for every test that touched it (rule R006).
    pub native: bool,
}

/// Fixture scope in which an observation happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestScope {
    /// Outside any test: conftest, plugins, interpreter startup. These are
    /// global dependencies.
    Root,
    /// Session-scoped fixtures.
    Session,
    /// Module-scoped fixtures.
    Module,
    /// The test body and its function-scoped fixtures.
    Test,
}

/// Per-test behaviour flags observed by the probe (bitset).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TestFlags(pub u32);

impl TestFlags {
    /// Observed nondeterminism (A/A disagreement or flake history).
    pub const NONDETERMINISTIC: Self = Self(1 << 0);
    /// Never skip (policy or `@skipcert.never_skip`).
    pub const MANDATORY: Self = Self(1 << 1);
    /// Contacted the network without a declared stub.
    pub const NETWORK: Self = Self(1 << 2);
    /// Read the wall clock without a fixed time.
    pub const CLOCK: Self = Self(1 << 3);
    /// Used randomness without a seed.
    pub const RANDOM: Self = Self(1 << 4);
    /// Spawned a subprocess.
    pub const SUBPROCESS: Self = Self(1 << 5);
    /// Executed native extension code the probe can't see into.
    pub const C_EXT: Self = Self(1 << 6);
    /// Performed a dynamic import or eval with non-constant input.
    pub const DYNAMIC_IMPORT: Self = Self(1 << 7);
    /// The probe crashed or was disabled, so the footprint is incomplete.
    pub const UNTRACED: Self = Self(1 << 8);

    /// True if every bit of `other` is set.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Sets the bits of `other`.
    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    /// True if any flag makes the footprint unsafe to rely on for skipping.
    pub const fn blocks_skip(self) -> bool {
        let blocking = Self::NONDETERMINISTIC.0
            | Self::MANDATORY.0
            | Self::NETWORK.0
            | Self::DYNAMIC_IMPORT.0
            | Self::UNTRACED.0;
        self.0 & blocking != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_combine_and_block() {
        let mut f = TestFlags::default();
        assert!(!f.blocks_skip());
        f.insert(TestFlags::CLOCK);
        assert!(f.contains(TestFlags::CLOCK));
        assert!(
            !f.blocks_skip(),
            "clock alone is handled by rule R010, not a hard block"
        );
        f.insert(TestFlags::UNTRACED);
        assert!(f.blocks_skip());
    }
}
