//! Outcome keys for L1 dedup: a typed, versioned envelope (ADR-003).
//!
//! Two runs with equal outcome keys observed identical inputs, so a verdict
//! recorded in the trusted namespace can be reused. An envelope with an
//! extension type this build doesn't know is never reusable.

use std::fmt;

use serde::{Deserialize, Serialize};

use sc_hash::{Digest, Domain, canonical_cbor, hash_domain};

use crate::error::ModelError;
use crate::ids::StableTestId;
use crate::version::OUTCOME_KEY_VERSION;

/// The base tuple every outcome key contains.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutcomeKeyBase {
    /// The test.
    pub test: StableTestId,
    /// Hash of the test's own code (rule I5: any change forces a run).
    pub test_code: Digest,
    /// Merkle root of the test's observed footprint (content hashes).
    pub footprint: Digest,
    /// Hash of the environment variables the test read.
    pub env_subset: Digest,
    /// Hash of the interpreter or runtime identity.
    pub interpreter: Digest,
    /// Hash of the resolved dependency set the test observed.
    pub resolved_deps: Digest,
}

/// Typed extensions for test types whose outcome depends on more than code.
///
/// Integer fields stand in for floats (for example `temperature_milli`) to
/// keep encoding canonical.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum KeyExtension {
    /// Database migrations and schema dump.
    #[serde(rename = "db.migrations")]
    DbMigrations {
        /// Hash of the migrations directory plus the schema dump.
        hash: Digest,
    },
    /// Container image used by the test.
    #[serde(rename = "container.digest")]
    ContainerDigest {
        /// Image digest, e.g. `sha256:...`.
        digest: String,
    },
    /// Consumer/provider contract version (Pact and similar).
    #[serde(rename = "contract.version")]
    ContractVersion {
        /// Provider name.
        provider: String,
        /// Provider contract version.
        version: String,
    },
    /// Parameters of an LLM-backed test (evals). Outcomes are recorded
    /// samples, not deterministic verdicts.
    #[serde(rename = "llm.params")]
    LlmParams {
        /// Model identifier.
        model: String,
        /// Hash of the prompt template and inputs.
        prompt: Digest,
        /// Judge model, if any.
        judge: Option<String>,
        /// Hash of the grading rubric, if any.
        rubric: Option<Digest>,
        /// Sampling temperature × 1000.
        temperature_milli: i64,
        /// Sampling seed, if the provider supports one.
        seed: Option<u64>,
    },
    /// Identity of an input data snapshot.
    #[serde(rename = "data.snapshot")]
    DataSnapshot {
        /// Snapshot identifier.
        id: String,
    },
    /// Any extension type this build doesn't recognise. Its presence makes
    /// the key non-reusable.
    #[serde(other)]
    Unknown,
}

/// Whether a reused outcome is a deterministic verdict or a recorded sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Determinism {
    /// Equal inputs imply an equal verdict.
    Deterministic,
    /// The outcome is one sample of a nondeterministic process (e.g. LLM
    /// evals). It may be reused only if policy allows, and is never certified
    /// as `dedup`.
    RecordedSample,
}

/// The full outcome-key envelope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutcomeKeyEnvelope {
    /// Envelope version.
    pub v: u32,
    /// Base tuple.
    pub base: OutcomeKeyBase,
    /// Typed extensions, in any order (they are sorted before hashing).
    pub ext: Vec<KeyExtension>,
}

/// A computed outcome key.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OutcomeKey(pub Digest);

impl fmt::Debug for OutcomeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "OutcomeKey({})", self.0.to_hex())
    }
}

impl OutcomeKeyEnvelope {
    /// Creates a v1 envelope.
    pub fn new(base: OutcomeKeyBase, ext: Vec<KeyExtension>) -> Self {
        Self {
            v: OUTCOME_KEY_VERSION,
            base,
            ext,
        }
    }

    /// Determinism class implied by the extensions.
    pub fn determinism(&self) -> Determinism {
        if self
            .ext
            .iter()
            .any(|e| matches!(e, KeyExtension::LlmParams { .. }))
        {
            Determinism::RecordedSample
        } else {
            Determinism::Deterministic
        }
    }

    /// Computes the key.
    ///
    /// Fails with [`ModelError::NotReusable`] if the version is unsupported or
    /// any extension is unknown. Extensions are sorted by their canonical
    /// encoding, so their order doesn't affect the key.
    pub fn key(&self) -> Result<OutcomeKey, ModelError> {
        if self.v != OUTCOME_KEY_VERSION {
            return Err(ModelError::NotReusable(format!(
                "unsupported envelope version {}",
                self.v
            )));
        }
        if self.ext.iter().any(|e| matches!(e, KeyExtension::Unknown)) {
            return Err(ModelError::NotReusable("unknown key extension".into()));
        }
        let mut encoded_ext = self
            .ext
            .iter()
            .map(canonical_cbor)
            .collect::<Result<Vec<_>, _>>()?;
        encoded_ext.sort();
        encoded_ext.dedup();

        let mut buf = canonical_cbor(&(self.v, &self.base))?;
        buf.extend_from_slice(&(encoded_ext.len() as u64).to_le_bytes());
        for e in &encoded_ext {
            buf.extend_from_slice(&(e.len() as u64).to_le_bytes());
            buf.extend_from_slice(e);
        }
        Ok(OutcomeKey(hash_domain(Domain::OutcomeKey, &buf)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_hash::hash_domain as h;

    fn base() -> OutcomeKeyBase {
        OutcomeKeyBase {
            test: StableTestId::derive("tests/t.py", "test_a", ""),
            test_code: h(Domain::Blob, b"code"),
            footprint: h(Domain::Blob, b"fp"),
            env_subset: h(Domain::Blob, b"env"),
            interpreter: h(Domain::Blob, b"py3.13"),
            resolved_deps: h(Domain::Blob, b"deps"),
        }
    }

    #[test]
    fn key_is_stable_and_input_sensitive() {
        let k1 = OutcomeKeyEnvelope::new(base(), vec![]).key().unwrap();
        let k2 = OutcomeKeyEnvelope::new(base(), vec![]).key().unwrap();
        assert_eq!(k1, k2);

        let mut changed = base();
        changed.footprint = h(Domain::Blob, b"fp2");
        assert_ne!(k1, OutcomeKeyEnvelope::new(changed, vec![]).key().unwrap());
    }

    #[test]
    fn extension_order_does_not_matter() {
        let a = KeyExtension::ContainerDigest {
            digest: "sha256:aa".into(),
        };
        let b = KeyExtension::DataSnapshot {
            id: "snap-1".into(),
        };
        let k1 = OutcomeKeyEnvelope::new(base(), vec![a.clone(), b.clone()])
            .key()
            .unwrap();
        let k2 = OutcomeKeyEnvelope::new(base(), vec![b, a]).key().unwrap();
        assert_eq!(k1, k2);
    }

    #[test]
    fn extensions_change_the_key() {
        let plain = OutcomeKeyEnvelope::new(base(), vec![]).key().unwrap();
        let with = OutcomeKeyEnvelope::new(
            base(),
            vec![KeyExtension::DbMigrations {
                hash: h(Domain::Blob, b"m"),
            }],
        )
        .key()
        .unwrap();
        assert_ne!(plain, with);
    }

    #[test]
    fn unknown_extension_is_not_reusable() {
        let ext: KeyExtension =
            serde_json::from_str(r#"{"type":"gpu.firmware","rev":"7"}"#).unwrap();
        assert_eq!(ext, KeyExtension::Unknown);
        let env = OutcomeKeyEnvelope::new(base(), vec![ext]);
        assert!(matches!(env.key(), Err(ModelError::NotReusable(_))));
    }

    #[test]
    fn unsupported_version_is_not_reusable() {
        let mut env = OutcomeKeyEnvelope::new(base(), vec![]);
        env.v = 2;
        assert!(matches!(env.key(), Err(ModelError::NotReusable(_))));
    }

    #[test]
    fn llm_params_are_recorded_samples() {
        let env = OutcomeKeyEnvelope::new(
            base(),
            vec![KeyExtension::LlmParams {
                model: "m".into(),
                prompt: h(Domain::Blob, b"p"),
                judge: None,
                rubric: None,
                temperature_milli: 0,
                seed: Some(1),
            }],
        );
        assert_eq!(env.determinism(), Determinism::RecordedSample);
        assert_eq!(
            OutcomeKeyEnvelope::new(base(), vec![]).determinism(),
            Determinism::Deterministic
        );
    }
}
