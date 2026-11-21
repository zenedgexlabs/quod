//! Certificate schema v1 (hash-chained JSON Lines).
//!
//! A certificate's `self` hash is computed over the canonical CBOR encoding
//! of its body, under [`Domain::Certificate`]. The signature covers that hash.
//! This module checks structure and integrity only. Whether the *decision* was
//! correct is checked by the independent verifier, which re-derives it from
//! the pinned inputs (ADR-006).

use serde::{Deserialize, Serialize};

use sc_hash::{Digest, Domain, hash_value};

use crate::error::ModelError;
use crate::ids::StableTestId;
use crate::outcome::OutcomeKey;
use crate::version::CERT_VERSION;

/// What a certificate asserts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CertKind {
    /// Licenses skipping a test.
    Skip,
    /// Reachability or impact context. It can never license a skip.
    Evidence,
}

/// Why a skip is sound.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipType {
    /// L0: the change is disjoint from the test's footprint and root scope.
    Disjoint,
    /// L1: an identical outcome key exists in the trusted namespace.
    Dedup,
    /// L2: boundary witnesses replayed equal (gated, ADR-009).
    Witness,
}

/// Everything needed to re-derive the decision bit for bit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedInputs {
    /// Repository identifier.
    pub repo: String,
    /// Base commit SHA (merge base).
    pub base: String,
    /// Head commit SHA.
    pub head: String,
    /// Hash of the working tree when it differs from `head` (dirty tree).
    pub tree: Option<Digest>,
    /// Merkle root of the footprint map used.
    pub map: Digest,
    /// Hash of the policy rule set used.
    pub policy: Digest,
    /// Engine version string.
    pub engine: String,
    /// Hash of the lockfile delta, if dependencies changed.
    pub lock_delta: Option<Digest>,
}

/// The evidence behind the decision.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    /// Changed graph nodes, as canonical node references.
    pub changed_nodes: Vec<String>,
    /// Changed nodes found in the test's footprint. Must be empty for a
    /// `disjoint` skip.
    pub intersection: Vec<String>,
    /// The reused outcome, for a `dedup` skip.
    pub outcome_ref: Option<OutcomeKey>,
    /// Policy rule IDs that were evaluated and passed.
    pub rules_passed: Vec<String>,
}

/// Optional links to change records and requirements (regulated use).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Traceability {
    /// Change request or ticket ID.
    pub change_request: Option<String>,
    /// Requirement IDs covered by the test.
    pub requirements: Vec<String>,
    /// Approver identity.
    pub approver: Option<String>,
}

/// Detached signature over the certificate's `self` hash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    /// Algorithm: `ed25519`, `sigstore` or `kms`.
    pub alg: String,
    /// Key or identity reference.
    pub key_id: String,
    /// Encoded signature value.
    pub value: String,
    /// Optional RFC 3161 timestamp token.
    pub tsa: Option<String>,
}

/// The signed body of a certificate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertificateBody {
    /// Schema version.
    pub v: u32,
    /// Skip or evidence.
    pub kind: CertKind,
    /// Present exactly when `kind` is `skip`.
    pub skip_type: Option<SkipType>,
    /// The test this certificate is about.
    pub test: StableTestId,
    /// Human-readable test name (not part of identity).
    pub test_name: String,
    /// Pinned inputs.
    pub inputs: PinnedInputs,
    /// Evidence.
    pub evidence: Evidence,
    /// Traceability links.
    pub trace: Traceability,
    /// True if produced outside a trusted CI identity. Advisory output is
    /// never written to the shared outcome index or accepted as proof
    /// (ADR-004).
    pub advisory: bool,
    /// `self` hash of the previous certificate in this run.
    pub prev: Option<Digest>,
}

/// A sealed certificate: body, integrity hash, optional signature.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Certificate {
    /// The body.
    #[serde(flatten)]
    pub body: CertificateBody,
    /// Hash of the body.
    #[serde(rename = "self")]
    pub self_hash: Digest,
    /// Signature over `self_hash`, if signed.
    pub sig: Option<Signature>,
}

impl CertificateBody {
    /// Integrity hash of the body.
    pub fn digest(&self) -> Result<Digest, ModelError> {
        Ok(hash_value(Domain::Certificate, self)?)
    }

    fn check_structure(&self) -> Result<(), ModelError> {
        if self.v != CERT_VERSION {
            return Err(ModelError::Invalid(format!(
                "unsupported certificate version {}",
                self.v
            )));
        }
        match (self.kind, self.skip_type) {
            (CertKind::Skip, None) => {
                return Err(ModelError::Invalid(
                    "skip certificate without skip_type".into(),
                ));
            }
            (CertKind::Evidence, Some(_)) => {
                return Err(ModelError::Invalid(
                    "evidence certificate with skip_type".into(),
                ));
            }
            _ => {}
        }
        match self.skip_type {
            Some(SkipType::Disjoint) if !self.evidence.intersection.is_empty() => Err(
                ModelError::Invalid("disjoint skip with non-empty intersection".into()),
            ),
            Some(SkipType::Dedup) if self.evidence.outcome_ref.is_none() => {
                Err(ModelError::Invalid("dedup skip without outcome_ref".into()))
            }
            _ => Ok(()),
        }
    }
}

impl Certificate {
    /// Validates structure and computes the `self` hash. The result is unsigned.
    pub fn seal(body: CertificateBody) -> Result<Self, ModelError> {
        body.check_structure()?;
        let self_hash = body.digest()?;
        Ok(Self {
            body,
            self_hash,
            sig: None,
        })
    }

    /// Re-checks structure and that `self` matches the body.
    pub fn check_integrity(&self) -> Result<(), ModelError> {
        self.body.check_structure()?;
        if self.body.digest()? != self.self_hash {
            return Err(ModelError::Invalid("self hash does not match body".into()));
        }
        Ok(())
    }

    /// True if this certificate may license skipping its test: a valid,
    /// non-advisory skip certificate. Signature checks are done by the
    /// verifier.
    pub fn licenses_skip(&self) -> bool {
        self.body.kind == CertKind::Skip && !self.body.advisory && self.check_integrity().is_ok()
    }
}

/// Checks integrity of every certificate and the `prev` chain of one run.
pub fn verify_chain(certs: &[Certificate]) -> Result<(), ModelError> {
    let mut prev: Option<Digest> = None;
    for (i, cert) in certs.iter().enumerate() {
        cert.check_integrity()?;
        if cert.body.prev != prev {
            return Err(ModelError::Invalid(format!(
                "chain broken at certificate {i}"
            )));
        }
        prev = Some(cert.self_hash);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_hash::hash_domain;

    fn body(prev: Option<Digest>) -> CertificateBody {
        CertificateBody {
            v: CERT_VERSION,
            kind: CertKind::Skip,
            skip_type: Some(SkipType::Disjoint),
            test: StableTestId::derive("tests/t.py", "test_a", ""),
            test_name: "tests/t.py::test_a".into(),
            inputs: PinnedInputs {
                repo: "acme/app".into(),
                base: "a1b2".into(),
                head: "c3d4".into(),
                tree: None,
                map: hash_domain(Domain::MapSection, b"map"),
                policy: hash_domain(Domain::Policy, b"policy"),
                engine: "0.1.0".into(),
                lock_delta: None,
            },
            evidence: Evidence {
                changed_nodes: vec!["file:src/billing.py".into()],
                intersection: vec![],
                outcome_ref: None,
                rules_passed: vec!["R001".into(), "R002".into()],
            },
            trace: Traceability::default(),
            advisory: false,
            prev,
        }
    }

    #[test]
    fn seal_and_check() {
        let cert = Certificate::seal(body(None)).unwrap();
        cert.check_integrity().unwrap();
        assert!(cert.licenses_skip());
    }

    #[test]
    fn tampering_is_detected() {
        let mut cert = Certificate::seal(body(None)).unwrap();
        cert.body.evidence.changed_nodes.clear();
        assert!(cert.check_integrity().is_err());
        assert!(!cert.licenses_skip());
    }

    #[test]
    fn structural_rules_enforced() {
        let mut b = body(None);
        b.skip_type = None;
        assert!(Certificate::seal(b).is_err());

        let mut b = body(None);
        b.evidence.intersection = vec!["file:src/billing.py".into()];
        assert!(
            Certificate::seal(b).is_err(),
            "disjoint skip must have an empty intersection"
        );

        let mut b = body(None);
        b.skip_type = Some(SkipType::Dedup);
        assert!(
            Certificate::seal(b).is_err(),
            "dedup skip needs an outcome_ref"
        );

        let mut b = body(None);
        b.kind = CertKind::Evidence;
        assert!(
            Certificate::seal(b).is_err(),
            "evidence must not carry a skip_type"
        );
    }

    #[test]
    fn advisory_never_licenses_skip() {
        let mut b = body(None);
        b.advisory = true;
        let cert = Certificate::seal(b).unwrap();
        assert!(!cert.licenses_skip());
    }

    #[test]
    fn chain_verification() {
        let c1 = Certificate::seal(body(None)).unwrap();
        let c2 = Certificate::seal(body(Some(c1.self_hash))).unwrap();
        verify_chain(&[c1.clone(), c2.clone()]).unwrap();
        assert!(verify_chain(&[c2, c1]).is_err());
    }

    #[test]
    fn json_round_trip_preserves_integrity() {
        let cert = Certificate::seal(body(None)).unwrap();
        let json = serde_json::to_string(&cert).unwrap();
        assert!(json.contains("\"self\":"));
        let back: Certificate = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cert);
        back.check_integrity().unwrap();
    }
}
