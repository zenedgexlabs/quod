//! Certificate signing and serialization.
//!
//! A signature covers `SIG_CONTEXT || self_hash`. The `self` hash already
//! commits to the whole body, including `prev`, so signing every certificate
//! also signs the chain. Keys are Ed25519. A key is identified as
//! `ed25519:<hex public key>`, so a verifier configured with trusted public
//! keys can check a certificate with no key registry.
//!
//! Sigstore keyless and KMS backends implement the same [`Signer`] trait later.

use std::io::{BufRead, Write};

use ed25519_dalek::{Signer as _, SigningKey, VerifyingKey};
use sc_model::{Certificate, Signature};

/// Domain-separation prefix for certificate signatures (format v1).
pub const SIG_CONTEXT: &[u8] = b"skipcert v1 certificate signature\0";

/// Signature algorithm identifier for Ed25519.
pub const ALG_ED25519: &str = "ed25519";

/// Errors from signing and I/O.
#[derive(Debug, thiserror::Error)]
pub enum CertError {
    /// Key material is malformed.
    #[error("invalid key: {0}")]
    Key(String),
    /// The OS random source failed.
    #[error("random source failed: {0}")]
    Random(String),
    /// Reading or writing failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A JSON line could not be (de)serialized.
    #[error("certificate line {line}: {message}")]
    Json {
        /// 1-based line number.
        line: usize,
        /// Error message.
        message: String,
    },
}

/// Lowercase hex encoding.
pub fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

/// Decodes lowercase or uppercase hex.
pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 || !s.is_ascii() {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// The message a certificate signature covers.
pub fn signing_message(cert: &Certificate) -> Vec<u8> {
    let mut m = Vec::with_capacity(SIG_CONTEXT.len() + 32);
    m.extend_from_slice(SIG_CONTEXT);
    m.extend_from_slice(cert.self_hash.as_bytes());
    m
}

/// Something that can sign certificates.
pub trait Signer {
    /// Algorithm identifier.
    fn alg(&self) -> &str;
    /// Key identifier recorded in the signature.
    fn key_id(&self) -> String;
    /// Signs `message`, returning the encoded signature value.
    fn sign_message(&self, message: &[u8]) -> String;

    /// Signs one certificate in place. Integrity must already hold.
    fn sign(&self, cert: &mut Certificate) {
        let value = self.sign_message(&signing_message(cert));
        cert.sig = Some(Signature {
            alg: self.alg().to_owned(),
            key_id: self.key_id(),
            value,
            tsa: None,
        });
    }

    /// Signs a chain of certificates in place.
    fn sign_all(&self, certs: &mut [Certificate]) {
        for c in certs {
            self.sign(c);
        }
    }
}

/// An Ed25519 signing key.
pub struct Ed25519Signer {
    key: SigningKey,
}

impl Ed25519Signer {
    /// Generates a new key from the OS random source.
    pub fn generate() -> Result<Self, CertError> {
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).map_err(|e| CertError::Random(e.to_string()))?;
        Ok(Self {
            key: SigningKey::from_bytes(&seed),
        })
    }

    /// Loads a key from its 32-byte seed in hex.
    pub fn from_seed_hex(hex: &str) -> Result<Self, CertError> {
        let bytes = from_hex(hex.trim()).ok_or_else(|| CertError::Key("seed is not hex".into()))?;
        let seed: [u8; 32] = bytes
            .try_into()
            .map_err(|_| CertError::Key("seed must be 32 bytes".into()))?;
        Ok(Self {
            key: SigningKey::from_bytes(&seed),
        })
    }

    /// The seed in hex. This is secret material: store it like a password.
    pub fn seed_hex(&self) -> String {
        to_hex(self.key.as_bytes())
    }

    /// The public key in hex.
    pub fn public_hex(&self) -> String {
        to_hex(self.key.verifying_key().as_bytes())
    }
}

impl Signer for Ed25519Signer {
    fn alg(&self) -> &str {
        ALG_ED25519
    }

    fn key_id(&self) -> String {
        format!("{ALG_ED25519}:{}", self.public_hex())
    }

    fn sign_message(&self, message: &[u8]) -> String {
        to_hex(&self.key.sign(message).to_bytes())
    }
}

/// Parses an `ed25519:<hex>` key ID into a verifying key.
pub fn parse_key_id(key_id: &str) -> Option<VerifyingKey> {
    let hex = key_id.strip_prefix("ed25519:")?;
    let bytes: [u8; 32] = from_hex(hex)?.try_into().ok()?;
    VerifyingKey::from_bytes(&bytes).ok()
}

/// Writes certificates as JSON Lines.
pub fn write_jsonl<W: Write>(certs: &[Certificate], mut w: W) -> Result<(), CertError> {
    for (i, c) in certs.iter().enumerate() {
        let line = serde_json::to_string(c).map_err(|e| CertError::Json {
            line: i + 1,
            message: e.to_string(),
        })?;
        w.write_all(line.as_bytes())?;
        w.write_all(b"\n")?;
    }
    Ok(())
}

/// Reads certificates from JSON Lines. Blank lines are ignored.
pub fn read_jsonl<R: BufRead>(r: R) -> Result<Vec<Certificate>, CertError> {
    let mut out = Vec::new();
    for (i, line) in r.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        out.push(serde_json::from_str(&line).map_err(|e| CertError::Json {
            line: i + 1,
            message: e.to_string(),
        })?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::Verifier;
    use sc_hash::{Domain, hash_domain};
    use sc_model::{
        CertKind, CertificateBody, Evidence, PinnedInputs, SkipType, StableTestId, Traceability,
        version::CERT_VERSION,
    };

    fn cert() -> Certificate {
        Certificate::seal(CertificateBody {
            v: CERT_VERSION,
            kind: CertKind::Skip,
            skip_type: Some(SkipType::Disjoint),
            test: StableTestId::derive("t.py", "test_a", ""),
            test_name: "t.py::test_a".into(),
            inputs: PinnedInputs {
                repo: "r".into(),
                base: "b".into(),
                head: "h".into(),
                tree: None,
                map: hash_domain(Domain::MapSection, b"m"),
                policy: hash_domain(Domain::Policy, b"p"),
                engine: "0.1.0".into(),
                lock_delta: None,
                environment: None,
            },
            evidence: Evidence::default(),
            trace: Traceability::default(),
            advisory: false,
            prev: None,
        })
        .unwrap()
    }

    #[test]
    fn hex_round_trip() {
        assert_eq!(
            from_hex(&to_hex(&[0, 1, 254, 255])).unwrap(),
            vec![0, 1, 254, 255]
        );
        assert!(from_hex("abc").is_none());
        assert!(from_hex("zz").is_none());
    }

    #[test]
    fn sign_and_verify() {
        let signer = Ed25519Signer::generate().unwrap();
        let mut c = cert();
        signer.sign(&mut c);
        let sig = c.sig.as_ref().unwrap();
        let vk = parse_key_id(&sig.key_id).unwrap();
        let bytes: [u8; 64] = from_hex(&sig.value).unwrap().try_into().unwrap();
        let s = ed25519_dalek::Signature::from_bytes(&bytes);
        assert!(vk.verify(&signing_message(&c), &s).is_ok());
    }

    #[test]
    fn seed_round_trip_keeps_identity() {
        let a = Ed25519Signer::generate().unwrap();
        let b = Ed25519Signer::from_seed_hex(&a.seed_hex()).unwrap();
        assert_eq!(a.key_id(), b.key_id());
        assert!(Ed25519Signer::from_seed_hex("00").is_err());
    }

    #[test]
    fn jsonl_round_trip() {
        let signer = Ed25519Signer::generate().unwrap();
        let mut certs = vec![cert(), cert()];
        signer.sign_all(&mut certs);
        let mut buf = Vec::new();
        write_jsonl(&certs, &mut buf).unwrap();
        let back = read_jsonl(&buf[..]).unwrap();
        assert_eq!(back, certs);
        assert!(read_jsonl(&b"{not json}\n"[..]).is_err());
    }
}
