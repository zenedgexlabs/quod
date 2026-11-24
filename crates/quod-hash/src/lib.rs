//! Content hashing for Quod.
//!
//! Every hash in Quod is a BLAKE3 digest computed under an explicit
//! [`Domain`], so a digest of one kind of object can never collide with a
//! digest of another kind. Structured values are hashed through
//! [`canonical_cbor`], which is deterministic as long as maps use ordered
//! containers (`BTreeMap`) and floats are avoided.

use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Length of a digest in bytes.
pub const DIGEST_LEN: usize = 32;

const HEX: &[u8; 16] = b"0123456789abcdef";

/// Errors produced by hashing and canonical encoding.
#[derive(Debug, thiserror::Error)]
pub enum HashError {
    /// Reading the input failed.
    #[error("i/o error while hashing: {0}")]
    Io(#[from] io::Error),
    /// A value could not be canonically encoded.
    #[error("canonical encoding failed: {0}")]
    Encode(String),
    /// A hex string was not a valid 32-byte digest.
    #[error("invalid digest hex: {0}")]
    InvalidHex(String),
}

/// A 32-byte BLAKE3 digest.
///
/// Serialized as a lowercase hex string in human-readable formats (JSON) and
/// as raw bytes in binary formats (CBOR).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest([u8; DIGEST_LEN]);

impl Digest {
    /// Wraps raw digest bytes.
    pub const fn from_bytes(bytes: [u8; DIGEST_LEN]) -> Self {
        Self(bytes)
    }

    /// Returns the raw digest bytes.
    pub const fn as_bytes(&self) -> &[u8; DIGEST_LEN] {
        &self.0
    }

    /// Lowercase hex encoding (64 characters).
    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(DIGEST_LEN * 2);
        for b in self.0 {
            s.push(HEX[(b >> 4) as usize] as char);
            s.push(HEX[(b & 0x0f) as usize] as char);
        }
        s
    }

    /// Parses a 64-character hex string.
    pub fn from_hex(s: &str) -> Result<Self, HashError> {
        if s.len() != DIGEST_LEN * 2 || !s.is_ascii() {
            return Err(HashError::InvalidHex(s.to_owned()));
        }
        let mut out = [0u8; DIGEST_LEN];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
                .map_err(|_| HashError::InvalidHex(s.to_owned()))?;
        }
        Ok(Self(out))
    }

    fn from_slice(bytes: &[u8]) -> Option<Self> {
        <[u8; DIGEST_LEN]>::try_from(bytes).ok().map(Self)
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Digest({})", self.to_hex())
    }
}

impl Serialize for Digest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if serializer.is_human_readable() {
            serializer.serialize_str(&self.to_hex())
        } else {
            serializer.serialize_bytes(&self.0)
        }
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct DigestVisitor;

        impl Visitor<'_> for DigestVisitor {
            type Value = Digest;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a 32-byte digest as 64 hex characters or raw bytes")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Digest, E> {
                Digest::from_hex(v).map_err(E::custom)
            }

            fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<Digest, E> {
                Digest::from_slice(v).ok_or_else(|| E::invalid_length(v.len(), &self))
            }
        }

        deserializer.deserialize_any(DigestVisitor)
    }
}

/// Hash domains. Each one maps to a distinct BLAKE3 key-derivation context,
/// which gives domain separation between object kinds.
///
/// The context strings are part of the v1 on-disk format: changing one
/// changes every stored digest of that kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Domain {
    /// Raw file or blob contents.
    Blob,
    /// Stable test identifiers.
    StableTestId,
    /// Outcome-key envelopes (L1 dedup).
    OutcomeKey,
    /// Certificate bodies.
    Certificate,
    /// Merkle leaves.
    MerkleLeaf,
    /// Merkle interior nodes.
    MerkleNode,
    /// Footprint map sections.
    MapSection,
    /// Policy rule sets.
    Policy,
    /// Source-entity (function/module) AST hashes.
    Entity,
}

impl Domain {
    /// The BLAKE3 key-derivation context for this domain.
    pub const fn context(self) -> &'static str {
        match self {
            Domain::Blob => "quod v1 blob",
            Domain::StableTestId => "quod v1 stable-test-id",
            Domain::OutcomeKey => "quod v1 outcome-key",
            Domain::Certificate => "quod v1 certificate",
            Domain::MerkleLeaf => "quod v1 merkle-leaf",
            Domain::MerkleNode => "quod v1 merkle-node",
            Domain::MapSection => "quod v1 map-section",
            Domain::Policy => "quod v1 policy",
            Domain::Entity => "quod v1 entity",
        }
    }
}

/// Hashes `data` under `domain`.
pub fn hash_domain(domain: Domain, data: &[u8]) -> Digest {
    let mut hasher = blake3::Hasher::new_derive_key(domain.context());
    hasher.update(data);
    Digest(*hasher.finalize().as_bytes())
}

/// Streams `reader` into a digest under `domain`.
pub fn hash_reader<R: Read>(domain: Domain, mut reader: R) -> io::Result<Digest> {
    let mut hasher = blake3::Hasher::new_derive_key(domain.context());
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(Digest(*hasher.finalize().as_bytes()))
}

/// Hashes a file's contents under [`Domain::Blob`].
pub fn hash_file(path: impl AsRef<Path>) -> io::Result<Digest> {
    hash_reader(Domain::Blob, File::open(path)?)
}

/// Deterministic CBOR encoding of `value`.
///
/// Determinism relies on two rules that callers must follow: maps are
/// `BTreeMap` (ordered keys), and no floating-point fields are used.
pub fn canonical_cbor<T: Serialize>(value: &T) -> Result<Vec<u8>, HashError> {
    let mut out = Vec::new();
    ciborium::into_writer(value, &mut out).map_err(|e| HashError::Encode(e.to_string()))?;
    Ok(out)
}

/// Canonically encodes `value` and hashes it under `domain`.
pub fn hash_value<T: Serialize>(domain: Domain, value: &T) -> Result<Digest, HashError> {
    Ok(hash_domain(domain, &canonical_cbor(value)?))
}

/// Merkle root over an ordered list of digests.
///
/// Leaves and interior nodes use separate domains, which prevents
/// second-preimage attacks between levels. An odd node at the end of a level
/// is promoted unchanged. The root of an empty list is the node-domain hash of
/// the empty string.
pub fn merkle_root(leaves: &[Digest]) -> Digest {
    if leaves.is_empty() {
        return hash_domain(Domain::MerkleNode, &[]);
    }
    let mut level: Vec<Digest> = leaves
        .iter()
        .map(|d| hash_domain(Domain::MerkleLeaf, d.as_bytes()))
        .collect();
    while level.len() > 1 {
        let mut next = Vec::with_capacity(level.len().div_ceil(2));
        for pair in level.chunks(2) {
            if let [left, right] = pair {
                let mut buf = [0u8; DIGEST_LEN * 2];
                buf[..DIGEST_LEN].copy_from_slice(left.as_bytes());
                buf[DIGEST_LEN..].copy_from_slice(right.as_bytes());
                next.push(hash_domain(Domain::MerkleNode, &buf));
            } else {
                next.push(pair[0]);
            }
        }
        level = next;
    }
    level[0]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn hex_round_trip() {
        let d = hash_domain(Domain::Blob, b"hello");
        assert_eq!(Digest::from_hex(&d.to_hex()).unwrap(), d);
        assert_eq!(d.to_hex().len(), 64);
    }

    #[test]
    fn bad_hex_rejected() {
        assert!(Digest::from_hex("abc").is_err());
        assert!(Digest::from_hex(&"zz".repeat(32)).is_err());
    }

    #[test]
    fn domains_are_separated() {
        assert_ne!(
            hash_domain(Domain::Blob, b"x"),
            hash_domain(Domain::Certificate, b"x")
        );
    }

    #[test]
    fn reader_matches_one_shot() {
        let data = vec![7u8; 200_000];
        let streamed = hash_reader(Domain::Blob, &data[..]).unwrap();
        assert_eq!(streamed, hash_domain(Domain::Blob, &data));
    }

    #[test]
    fn merkle_is_order_sensitive_and_stable() {
        let a = hash_domain(Domain::Blob, b"a");
        let b = hash_domain(Domain::Blob, b"b");
        let c = hash_domain(Domain::Blob, b"c");
        assert_ne!(merkle_root(&[a, b]), merkle_root(&[b, a]));
        assert_eq!(merkle_root(&[a, b, c]), merkle_root(&[a, b, c]));
        assert_ne!(merkle_root(&[a, b, c]), merkle_root(&[a, b]));
        assert_eq!(
            merkle_root(&[a]),
            hash_domain(Domain::MerkleLeaf, a.as_bytes())
        );
        assert_eq!(merkle_root(&[]), hash_domain(Domain::MerkleNode, &[]));
    }

    #[test]
    fn canonical_cbor_is_deterministic_for_ordered_maps() {
        let mut m1 = BTreeMap::new();
        m1.insert("b", 2u32);
        m1.insert("a", 1u32);
        let mut m2 = BTreeMap::new();
        m2.insert("a", 1u32);
        m2.insert("b", 2u32);
        assert_eq!(canonical_cbor(&m1).unwrap(), canonical_cbor(&m2).unwrap());
    }

    #[test]
    fn digest_serde_json_and_cbor() {
        let d = hash_domain(Domain::Blob, b"serde");
        let json = serde_json::to_string(&d).unwrap();
        assert_eq!(json, format!("\"{}\"", d.to_hex()));
        assert_eq!(serde_json::from_str::<Digest>(&json).unwrap(), d);

        let cbor = canonical_cbor(&d).unwrap();
        let back: Digest = ciborium::from_reader(&cbor[..]).unwrap();
        assert_eq!(back, d);
    }
}
