//! Independent verification of Quod skip certificates (ADR-006).
//!
//! Given a run's certificates, the footprint map they cite, and a set of
//! trusted public keys, [`verify`] re-checks every skip without using the
//! engine's decision code:
//!
//! 1. integrity: the `self` hash matches the body, and the `prev` chain is
//!    unbroken;
//! 2. authority: a valid Ed25519 signature by a trusted key, and the
//!    certificate isn't advisory;
//! 3. binding: the certificate cites this exact map (and, optionally, an
//!    expected policy hash), and in environment mode the map's recorded
//!    environment;
//! 4. decision: the test is in the map, has no flag that forbids skipping,
//!    and none of the changed nodes in the evidence is in the test's
//!    footprint or in the root scope.
//!
//! v1 checks that the evidence is *consistent*. It does not check that the
//! list of changed nodes is *complete* (that needs the repository to
//! re-derive the change). Full re-derivation is a later verifier mode.

use std::collections::{HashMap, HashSet};

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use quod_graph::FootprintGraph;
use quod_hash::Digest;
use quod_model::{CertKind, Certificate, NodeKind, SkipType, TestFlags};

/// Signature context. Deliberately duplicated from the signer so that a
/// change on either side breaks verification loudly.
const SIG_CONTEXT: &[u8] = b"quod v1 certificate signature\0";

/// Flags that forbid any skip, re-stated independently of the policy crate.
const BLOCKING: [(TestFlags, &str); 5] = [
    (TestFlags::MANDATORY, "test is mandatory"),
    (
        TestFlags::UNTRACED,
        "test footprint is incomplete (untraced)",
    ),
    (TestFlags::DYNAMIC_IMPORT, "test performs dynamic imports"),
    (TestFlags::NETWORK, "test uses the network"),
    (TestFlags::SUBPROCESS, "test spawns subprocesses"),
];

/// Verification settings.
#[derive(Clone, Debug, Default)]
pub struct VerifyConfig {
    /// Trusted signer key IDs (`ed25519:<hex public key>`).
    pub trusted_keys: Vec<String>,
    /// Accept unsigned certificates. For local inspection only. Never
    /// enable this when certificates license skips in CI.
    pub allow_unsigned: bool,
    /// If set, every certificate must cite this policy hash.
    pub expected_policy: Option<Digest>,
}

/// One failed check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    /// Index of the certificate in the input.
    pub index: usize,
    /// Test name from the certificate.
    pub test: String,
    /// What failed.
    pub reason: String,
}

/// Verification result.
#[derive(Clone, Debug, Default)]
pub struct Report {
    /// Certificates examined.
    pub checked: usize,
    /// Failures, in input order.
    pub violations: Vec<Violation>,
}

impl Report {
    /// True if every certificate passed.
    pub fn ok(&self) -> bool {
        self.violations.is_empty()
    }
}

fn from_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 || !s.is_ascii() {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

fn verifying_key(key_id: &str) -> Option<VerifyingKey> {
    let bytes: [u8; 32] = from_hex(key_id.strip_prefix("ed25519:")?)?
        .try_into()
        .ok()?;
    VerifyingKey::from_bytes(&bytes).ok()
}

/// Node kind for a ref tag, re-stated independently of `quod-graph::kind_tag`.
fn kind_for_tag(tag: &str) -> Option<NodeKind> {
    Some(match tag {
        "file" => NodeKind::File,
        "fn" => NodeKind::Function,
        "mod" => NodeKind::Module,
        "pkg" => NodeKind::ExtPackage,
        "pkgfile" => NodeKind::ExtFile,
        "env" => NodeKind::EnvVar,
        "net" => NodeKind::NetEndpoint,
        "res" => NodeKind::Resource,
        "test" => NodeKind::Test,
        _ => return None,
    })
}

fn check_signature(cert: &Certificate, cfg: &VerifyConfig) -> Result<(), String> {
    let Some(sig) = &cert.sig else {
        return if cfg.allow_unsigned {
            Ok(())
        } else {
            Err("certificate is unsigned".into())
        };
    };
    if sig.alg != "ed25519" {
        return Err(format!("unsupported signature algorithm {:?}", sig.alg));
    }
    if !cfg.trusted_keys.iter().any(|k| k == &sig.key_id) {
        return Err(format!("signer {} is not trusted", sig.key_id));
    }
    let vk = verifying_key(&sig.key_id).ok_or("malformed key id")?;
    let bytes: [u8; 64] = from_hex(&sig.value)
        .and_then(|b| b.try_into().ok())
        .ok_or("malformed signature value")?;
    let mut msg = SIG_CONTEXT.to_vec();
    msg.extend_from_slice(cert.self_hash.as_bytes());
    vk.verify(&msg, &Signature::from_bytes(&bytes))
        .map_err(|_| "signature does not verify".to_string())
}

/// Verifies a run's certificates against the map they cite.
pub fn verify(
    certs: &[Certificate],
    graph: &FootprintGraph,
    map_root: Digest,
    cfg: &VerifyConfig,
) -> Report {
    let mut report = Report {
        checked: certs.len(),
        violations: Vec::new(),
    };
    let root_set: HashSet<_> = graph.root_nodes().collect();
    let mut footprints: HashMap<u32, HashSet<quod_graph::NodeId>> = HashMap::new();
    let mut prev: Option<Digest> = None;

    for (index, cert) in certs.iter().enumerate() {
        let mut fail = |reason: String| {
            report.violations.push(Violation {
                index,
                test: cert.body.test_name.clone(),
                reason,
            });
        };
        let b = &cert.body;

        if let Err(e) = cert.check_integrity() {
            fail(format!("integrity: {e}"));
        }
        if b.prev != prev {
            fail("chain: prev does not match the preceding certificate".into());
        }
        prev = Some(cert.self_hash);

        if b.kind != CertKind::Skip {
            fail("not a skip certificate".into());
            continue;
        }
        if b.skip_type != Some(SkipType::Disjoint) {
            fail(format!(
                "skip type {:?} is not supported by verifier v1",
                b.skip_type
            ));
            continue;
        }
        if b.advisory {
            fail("advisory certificate cannot license a skip".into());
        }
        if let Err(e) = check_signature(cert, cfg) {
            fail(format!("signature: {e}"));
        }
        if b.inputs.map != map_root {
            fail(format!(
                "binding: certificate cites map {} but verifying against {}",
                b.inputs.map, map_root
            ));
        }
        if let Some(env) = b.inputs.environment {
            // Environment mode (ADR 012): the skip assumed the target
            // environment matched the one this map was recorded in.
            if graph.meta().environment_binding() != Some(env) {
                fail(format!(
                    "binding: environment {env} is not the map's recorded environment"
                ));
            }
        }
        if let Some(expected) = cfg.expected_policy {
            if b.inputs.policy != expected {
                fail(format!(
                    "binding: policy {} is not the expected {}",
                    b.inputs.policy, expected
                ));
            }
        }

        let Some(t) = graph.find_test(&b.test) else {
            fail("decision: test is not in the map".into());
            continue;
        };
        if graph.test_name(t) != b.test_name {
            fail(format!(
                "decision: test name differs from map ({})",
                graph.test_name(t)
            ));
        }
        let flags = graph.test(t).flags;
        for (flag, why) in BLOCKING {
            if flags.contains(flag) {
                fail(format!("decision: {why}"));
            }
        }
        if !b.evidence.intersection.is_empty() {
            fail("decision: evidence intersection is not empty".into());
        }

        let fp = footprints
            .entry(t.0)
            .or_insert_with(|| graph.footprint(t).collect());
        for r in &b.evidence.changed_nodes {
            let Some((tag, name)) = r.split_once(':') else {
                fail(format!("decision: malformed node ref {r:?}"));
                continue;
            };
            let Some(kind) = kind_for_tag(tag) else {
                fail(format!("decision: unknown node kind in ref {r:?}"));
                continue;
            };
            if let Some(n) = graph.find_node(kind, name) {
                if fp.contains(&n) {
                    fail(format!(
                        "decision: changed node {r} is in the test's footprint"
                    ));
                }
                if root_set.contains(&n) {
                    fail(format!("decision: changed node {r} is in the root scope"));
                }
            }
        }
    }
    report
}
