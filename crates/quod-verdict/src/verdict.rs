//! The Verdict: one signed answer per change (architecture v5, §5.1).
//!
//! A Verdict is an in-toto Statement (v1) whose predicate records the pinned
//! inputs, what the change obligated, what ran, and every lens finding. It is
//! signed in a DSSE envelope, the standard in-toto signing wrapper, so
//! existing attestation tooling can carry it.

use std::collections::{BTreeMap, HashSet};

use quod_cert::{Signer, from_hex, parse_key_id};
use serde::{Deserialize, Serialize};

use crate::b64;
use crate::lens::{Finding, Kind, O001, O002, Severity};

/// in-toto Statement type.
pub const STATEMENT_TYPE: &str = "https://in-toto.io/Statement/v1";
/// Verdict predicate type. Must be final before anything is published.
pub const PREDICATE_TYPE: &str = "https://quodhq.com/verdict/v1";
/// DSSE payload type for in-toto statements.
pub const PAYLOAD_TYPE: &str = "application/vnd.in-toto+json";

/// Errors.
#[derive(Debug, thiserror::Error)]
pub enum VerdictError {
    /// JSON encoding or decoding failed.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// The envelope is malformed.
    #[error("malformed envelope: {0}")]
    Malformed(String),
    /// No signature verified.
    #[error("signature does not verify")]
    BadSignature,
}

/// How a test ended, as reported by the probe's results file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Passed.
    Passed,
    /// Failed (including errors in setup or teardown).
    Failed,
    /// Skipped by the test runner itself (markers, conditions).
    Skipped,
}

/// One test result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestResult {
    /// Test name (`path::qual[params]`), as in the map.
    pub name: String,
    /// Outcome.
    pub outcome: Outcome,
    /// Duration in milliseconds.
    #[serde(default)]
    pub duration_ms: u64,
}

/// Overall status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Obligations met, no unexplained failure, nothing blocking.
    Pass,
    /// A test failed and the change may have caused it.
    Fail,
    /// A blocking finding (for example, an obligated test didn't run).
    Blocked,
}

/// Who authored the change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorKind {
    /// A person.
    Human,
    /// A coding agent.
    Agent,
    /// Not stated.
    Unknown,
}

/// The change the Verdict is about.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeRef {
    /// Repository ID.
    pub repo: String,
    /// Base commit (the map's).
    pub base: String,
    /// Head commit.
    pub head: String,
    /// Working-tree digest when the tree differs from `head`.
    pub tree: Option<String>,
    /// Author kind.
    pub author_kind: AuthorKind,
    /// Agent name, if an agent authored it.
    pub agent: Option<String>,
}

/// Pinned inputs: enough to re-derive every decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inputs {
    /// Footprint map Merkle root.
    pub map: String,
    /// Policy hash.
    pub policy: String,
    /// Engine version.
    pub engine: String,
    /// Environment digest, when environment mode was on.
    pub environment: Option<String>,
}

/// What the change obligated and what happened.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Obligations {
    /// Tests the change obligated.
    pub obligated: usize,
    /// Tests that ran (obligated or not).
    pub ran: usize,
    /// Passed.
    pub passed: usize,
    /// Failed.
    pub failed: usize,
    /// Failures cleared by Flake-proof (not caused by this change).
    pub failed_not_caused: usize,
    /// Skipped by the test runner.
    pub runner_skipped: usize,
    /// Skips certified by Quod.
    pub certified_skips: usize,
    /// Global rules that widened selection to everything.
    pub fallback_rules: Vec<String>,
}

/// Certificates the Verdict relies on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertRef {
    /// Hash of the last certificate in the chain (empty if none).
    pub chain_head: String,
    /// Number of certificates.
    pub count: usize,
}

/// The Verdict predicate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    /// Predicate version.
    pub v: u32,
    /// Overall status.
    pub status: Status,
    /// Advisory (sandbox or local): never proof.
    pub advisory: bool,
    /// The change.
    pub change: ChangeRef,
    /// Pinned inputs.
    pub inputs: Inputs,
    /// The changed graph nodes, as canonical node references (sorted). With
    /// the map, this lets a verifier re-check every Flake-proof claim.
    pub changed: Vec<String>,
    /// Obligations and results.
    pub obligations: Obligations,
    /// Lens findings, sorted.
    pub findings: Vec<Finding>,
    /// Skip certificates.
    pub certificates: CertRef,
    /// Rules whose findings block because an agent authored the change
    /// (policy `agent_block`); empty for other authors. A verifier checks
    /// that every finding under these rules blocks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_block: Option<Vec<String>>,
}

/// in-toto subject.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subject {
    /// Name.
    pub name: String,
    /// Digests by algorithm.
    pub digest: BTreeMap<String, String>,
}

/// in-toto Statement carrying a Verdict.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Statement {
    /// Statement type.
    #[serde(rename = "_type")]
    pub type_: String,
    /// Subjects.
    pub subject: Vec<Subject>,
    /// Predicate type.
    #[serde(rename = "predicateType")]
    pub predicate_type: String,
    /// The Verdict.
    pub predicate: Verdict,
}

/// Everything the compiler needs besides lens findings.
pub struct CompileInput {
    /// The change.
    pub change: ChangeRef,
    /// Pinned inputs.
    pub inputs: Inputs,
    /// Changed node references.
    pub changed: Vec<String>,
    /// Advisory output.
    pub advisory: bool,
    /// Tests the change obligated (names).
    pub obligated: Vec<String>,
    /// Test files the change touched (an obligated test missing from the
    /// results may have been renamed or removed there).
    pub changed_test_files: HashSet<String>,
    /// Results reported by the probe.
    pub results: Vec<TestResult>,
    /// Certified skips.
    pub certificates: CertRef,
    /// Global rules that fired.
    pub fallback_rules: Vec<String>,
    /// Policy `agent_block`: rules that block for agent-authored changes.
    pub agent_block: Vec<String>,
}

/// Assembles the Verdict from inputs, results and lens findings.
///
/// Status: `Blocked` if any finding blocks; else `Fail` if a test failed and
/// Flake-proof didn't clear it; else `Pass`.
pub fn compile(input: CompileInput, mut findings: Vec<Finding>) -> Verdict {
    let by_name: BTreeMap<&str, &TestResult> =
        input.results.iter().map(|r| (r.name.as_str(), r)).collect();
    for name in &input.obligated {
        let file = name.split("::").next().unwrap_or(name);
        match by_name.get(name.as_str()) {
            None => findings.push(Finding {
                kind: Kind::Obligation,
                // A missing test in a file the change touched may have been
                // renamed; elsewhere, an obligation was simply not met.
                severity: if input.changed_test_files.contains(file) {
                    Severity::Review
                } else {
                    Severity::Block
                },
                rule: O001.into(),
                subject: name.clone(),
                detail: "obligated by this change but did not run".into(),
            }),
            Some(r) if r.outcome == Outcome::Skipped => findings.push(Finding {
                kind: Kind::Obligation,
                severity: Severity::Review,
                rule: O002.into(),
                subject: name.clone(),
                detail: "obligated by this change but skipped by the test runner".into(),
            }),
            Some(_) => {}
        }
    }
    // Agent-authored changes: findings that could hide a change passing by
    // weakening its own tests block instead of asking for review.
    let agent_block: Option<Vec<String>> =
        (input.change.author_kind == AuthorKind::Agent).then(|| {
            let mut r = input.agent_block.clone();
            r.sort();
            r.dedup();
            r
        });
    for f in &mut findings {
        if f.severity < Severity::Block && agent_block.as_ref().is_some_and(|r| r.contains(&f.rule))
        {
            f.severity = Severity::Block;
            f.detail.push_str(" (blocks: agent-authored change)");
        }
    }
    findings.sort();
    findings.dedup();

    let cleared: HashSet<&str> = findings
        .iter()
        .filter(|f| f.kind == Kind::FlakeProof)
        .map(|f| f.subject.as_str())
        .collect();
    let count = |o: Outcome| input.results.iter().filter(|r| r.outcome == o).count();
    let failed: Vec<&str> = input
        .results
        .iter()
        .filter(|r| r.outcome == Outcome::Failed)
        .map(|r| r.name.as_str())
        .collect();
    let failed_not_caused = failed.iter().filter(|n| cleared.contains(*n)).count();
    let status = if findings.iter().any(|f| f.severity == Severity::Block) {
        Status::Blocked
    } else if failed.len() > failed_not_caused {
        Status::Fail
    } else {
        Status::Pass
    };
    Verdict {
        v: 1,
        status,
        advisory: input.advisory,
        change: input.change,
        inputs: input.inputs,
        changed: {
            let mut c = input.changed;
            c.sort();
            c.dedup();
            c
        },
        obligations: Obligations {
            obligated: input.obligated.len(),
            ran: input.results.len() - count(Outcome::Skipped),
            passed: count(Outcome::Passed),
            failed: failed.len(),
            failed_not_caused,
            runner_skipped: count(Outcome::Skipped),
            certified_skips: input.certificates.count,
            fallback_rules: input.fallback_rules,
        },
        findings,
        certificates: input.certificates,
        agent_block,
    }
}

/// Wraps a Verdict in an in-toto Statement about the head commit.
pub fn statement(v: Verdict) -> Statement {
    let mut digest = BTreeMap::new();
    digest.insert("gitCommit".to_owned(), v.change.head.clone());
    if let Some(t) = &v.change.tree {
        digest.insert("quodTree".to_owned(), t.clone());
    }
    Statement {
        type_: STATEMENT_TYPE.into(),
        subject: vec![Subject {
            name: format!("git+{}@{}", v.change.repo, v.change.head),
            digest,
        }],
        predicate_type: PREDICATE_TYPE.into(),
        predicate: v,
    }
}

/// A DSSE signature.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DsseSignature {
    /// Key ID (`ed25519:<hex>`).
    pub keyid: String,
    /// Base64 signature over the PAE.
    pub sig: String,
}

/// A DSSE envelope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    /// Payload type.
    #[serde(rename = "payloadType")]
    pub payload_type: String,
    /// Base64 payload.
    pub payload: String,
    /// Signatures (empty for advisory Verdicts).
    pub signatures: Vec<DsseSignature>,
}

/// DSSE pre-authentication encoding.
pub fn pae(payload_type: &str, body: &[u8]) -> Vec<u8> {
    let mut m = format!(
        "DSSEv1 {} {} {} ",
        payload_type.len(),
        payload_type,
        body.len()
    )
    .into_bytes();
    m.extend_from_slice(body);
    m
}

/// Serialises the statement and signs it. Advisory Verdicts and a missing
/// signer give an unsigned envelope.
pub fn seal(st: &Statement, signer: Option<&dyn Signer>) -> Result<Envelope, VerdictError> {
    let body = serde_json::to_vec(st)?;
    let signatures = match signer {
        Some(s) if !st.predicate.advisory => {
            let sig_hex = s.sign_message(&pae(PAYLOAD_TYPE, &body));
            let raw = from_hex(&sig_hex)
                .ok_or_else(|| VerdictError::Malformed("signer returned non-hex".into()))?;
            vec![DsseSignature {
                keyid: s.key_id(),
                sig: b64::encode(&raw),
            }]
        }
        _ => Vec::new(),
    };
    Ok(Envelope {
        payload_type: PAYLOAD_TYPE.into(),
        payload: b64::encode(&body),
        signatures,
    })
}

/// Decodes an envelope's statement without checking signatures.
pub fn open(env: &Envelope) -> Result<Statement, VerdictError> {
    if env.payload_type != PAYLOAD_TYPE {
        return Err(VerdictError::Malformed(format!(
            "payload type {}",
            env.payload_type
        )));
    }
    let body = b64::decode(&env.payload)
        .ok_or_else(|| VerdictError::Malformed("payload is not base64".into()))?;
    let st: Statement = serde_json::from_slice(&body)?;
    if st.predicate_type != PREDICATE_TYPE {
        return Err(VerdictError::Malformed(format!(
            "predicate type {}",
            st.predicate_type
        )));
    }
    Ok(st)
}

/// Verifies that a signature by `trusted_key` (an `ed25519:<hex>` key ID)
/// covers the payload, and returns the statement. Advisory statements are
/// rejected: they are never proof.
pub fn verify(env: &Envelope, trusted_key: &str) -> Result<Statement, VerdictError> {
    let st = open(env)?;
    if st.predicate.advisory {
        return Err(VerdictError::Malformed(
            "advisory Verdicts are not proof".into(),
        ));
    }
    let key = parse_key_id(trusted_key)
        .ok_or_else(|| VerdictError::Malformed(format!("bad key id {trusted_key}")))?;
    let body = b64::decode(&env.payload)
        .ok_or_else(|| VerdictError::Malformed("payload is not base64".into()))?;
    let msg = pae(&env.payload_type, &body);
    for s in env.signatures.iter().filter(|s| s.keyid == trusted_key) {
        let Some(raw) = b64::decode(&s.sig) else {
            continue;
        };
        let Ok(bytes) = <[u8; 64]>::try_from(raw.as_slice()) else {
            continue;
        };
        let sig = ed25519_dalek::Signature::from_bytes(&bytes);
        if key.verify_strict(&msg, &sig).is_ok() {
            return Ok(st);
        }
    }
    Err(VerdictError::BadSignature)
}

/// Key ID of the first signature, for display.
pub fn signer_of(env: &Envelope) -> Option<&str> {
    env.signatures.first().map(|s| s.keyid.as_str())
}
