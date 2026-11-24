//! Append-only journal events (ADR-005).
//!
//! Events are idempotent by `event_id`. Rollups are pure functions over the
//! journal, so any rollup can be rebuilt from scratch.

use serde::{Deserialize, Serialize};

use quod_hash::Digest;

use crate::ids::StableTestId;
use crate::outcome::OutcomeKey;

/// Trust namespace of the run that produced an event (ADR-004).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "ns", rename_all = "snake_case")]
pub enum Namespace {
    /// Trusted CI identity (protected branches, approved workflows).
    Trusted,
    /// Untrusted runs (forks, unapproved agent PRs). Their outcomes are
    /// written only to this namespace.
    Untrusted {
        /// Fork or source identifier.
        source: String,
    },
}

/// Why a test was selected to run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectReason {
    /// The change intersects the test's footprint.
    Affected,
    /// A fail-closed policy rule fired (rule ID).
    Policy(String),
    /// No usable footprint (new test, untraced, stale map).
    NoFootprint,
    /// Marked mandatory.
    Mandatory,
}

/// Event payloads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventKind {
    /// A selection or record run started.
    RunStarted {
        /// Base commit.
        base: String,
        /// Head commit.
        head: String,
    },
    /// A footprint map was loaded.
    MapUsed {
        /// Merkle root of the map.
        map: Digest,
    },
    /// A test was selected to run.
    TestSelected {
        /// The test.
        test: StableTestId,
        /// Why.
        reason: SelectReason,
    },
    /// A test was skipped under a certificate.
    TestSkipped {
        /// The test.
        test: StableTestId,
        /// `self` hash of the certificate.
        cert: Digest,
    },
    /// A test ran.
    TestRan {
        /// The test.
        test: StableTestId,
        /// Whether it passed.
        passed: bool,
        /// Wall time in milliseconds.
        duration_ms: u64,
        /// Outcome key, if one could be computed.
        outcome_key: Option<OutcomeKey>,
    },
    /// Nondeterminism was observed (A/A disagreement or retry pass).
    FlakeObserved {
        /// The test.
        test: StableTestId,
    },
    /// A test-integrity rule fired (deleted test, added skip/xfail, ...).
    IntegrityViolation {
        /// The test.
        test: StableTestId,
        /// Rule ID.
        rule: String,
    },
    /// The audit found that a skipped test fails: an escape.
    EscapeDetected {
        /// The test.
        test: StableTestId,
        /// The certificate that licensed the skip.
        cert: Digest,
    },
    /// The run finished.
    RunFinished {
        /// Tests selected to run.
        selected: u64,
        /// Tests skipped under certificates.
        skipped: u64,
    },
}

/// One journal record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// Journal schema version.
    pub v: u32,
    /// Globally unique, sortable ID (ULID). Duplicate IDs are ignored.
    pub event_id: String,
    /// Unix time in milliseconds.
    pub ts_unix_ms: i64,
    /// Run identifier.
    pub run_id: String,
    /// Repository identifier.
    pub repo: String,
    /// Trust namespace.
    pub namespace: Namespace,
    /// Payload.
    #[serde(flatten)]
    pub kind: EventKind,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::JOURNAL_VERSION;

    #[test]
    fn event_json_round_trip() {
        let ev = Event {
            v: JOURNAL_VERSION,
            event_id: "01JD0000000000000000000000".into(),
            ts_unix_ms: 1_795_000_000_000,
            run_id: "run-1".into(),
            repo: "acme/app".into(),
            namespace: Namespace::Untrusted {
                source: "fork/alice".into(),
            },
            kind: EventKind::TestSelected {
                test: StableTestId::derive("t.py", "test_x", ""),
                reason: SelectReason::Policy("R002".into()),
            },
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("\"kind\":\"test_selected\""));
        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ev);
    }
}
