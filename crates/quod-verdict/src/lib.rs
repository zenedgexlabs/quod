//! Quod Verdict (architecture v5).
//!
//! - [`lens`]: pure analyses over the evidence graph (Untested-change,
//!   Integrity, Flake-proof).
//! - [`verdict`]: compiles results and findings into a Verdict, wraps it in
//!   an in-toto Statement and seals it in a signed DSSE envelope.
//! - [`card`]: the Markdown card for PR checks.

pub mod b64;
pub mod card;
pub mod lens;
pub mod verdict;

pub use lens::{
    CommitEval, CulpritFinding, CulpritOutcome, Finding, Kind, LensInput, Severity, Suspect,
};
pub use verdict::{
    AuthorKind, CertRef, ChangeRef, CompileInput, Envelope, Inputs, Outcome, Statement, Status,
    TestResult, Verdict, compile, open, seal, statement, verify,
};
