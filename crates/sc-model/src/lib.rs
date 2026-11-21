//! Skipcert v1 schemas.
//!
//! This crate holds the persisted and wire formats only. Decision logic lives
//! in other crates, so the independent verifier can depend on this crate
//! without pulling in the engine. Every format carries a version, and a
//! version change is an explicit migration (ADR-002, ADR-003).

pub mod cert;
pub mod error;
pub mod graph;
pub mod ids;
pub mod journal;
pub mod names;
pub mod outcome;
pub mod version;

pub use cert::{
    CertKind, Certificate, CertificateBody, Evidence, PinnedInputs, Signature, SkipType,
    Traceability, verify_chain,
};
pub use error::ModelError;
pub use graph::{Ecosystem, ExtPackage, NodeKind, TestFlags, TestScope};
pub use ids::StableTestId;
pub use journal::{Event, EventKind, Namespace, SelectReason};
pub use outcome::{Determinism, KeyExtension, OutcomeKey, OutcomeKeyBase, OutcomeKeyEnvelope};
pub use sc_hash::Digest;
