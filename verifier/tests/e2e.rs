//! End to end: ladder → sign → verify, plus attacks the verifier must catch.

use quod_cert::{Ed25519Signer, Signer};
use quod_change::{ChangeSet, FileChange, FileDetail, FileStatus, SourceDelta};
use quod_graph::{FootprintGraph, GraphBuilder, MapMeta};
use quod_hash::{Digest, Domain, hash_domain};
use quod_ladder::{SelectInput, select};
use quod_model::{Certificate, NodeKind, StableTestId, TestFlags, names};
use quod_policy::{EvalContext, Policy};
use quod_verify::{VerifyConfig, verify};

fn graph() -> (FootprintGraph, Digest) {
    let mut b = GraphBuilder::new(MapMeta {
        repo: "acme/app".into(),
        ..MapMeta::default()
    });
    let h = |s: &str| Some(hash_domain(Domain::Blob, s.as_bytes()));
    let billing = b.node(NodeKind::File, "src/billing.py", h("b"), None);
    let charge = b.node(
        NodeKind::Function,
        &names::function("src/billing.py", "charge"),
        h("c"),
        Some(billing),
    );
    let util = b.node(NodeKind::File, "src/util.py", h("u"), None);
    let t1 = b.test(
        StableTestId::derive("tests/test_pay.py", "test_pay", ""),
        "tests/test_pay.py::test_pay",
        TestFlags::default(),
        5,
    );
    b.observe(t1, billing);
    b.observe(t1, charge);
    let t2 = b.test(
        StableTestId::derive("tests/test_util.py", "test_util", ""),
        "tests/test_util.py::test_util",
        TestFlags::default(),
        5,
    );
    b.observe(t2, util);
    let t3 = b.test(
        StableTestId::derive("tests/test_more.py", "test_more", ""),
        "tests/test_more.py::test_more",
        TestFlags::default(),
        5,
    );
    b.observe(t3, util);
    let g = b.build();
    let (_, root) = g.encode().unwrap();
    (g, root)
}

/// Changes `charge`, so test_util and test_more are skipped.
fn signed_run(
    signer: &Ed25519Signer,
    advisory: bool,
) -> (FootprintGraph, Digest, Policy, Vec<Certificate>) {
    let (g, root) = graph();
    let policy = Policy::default_policy();
    let cs = ChangeSet {
        base: "base".into(),
        head: "head".into(),
        tree: None,
        files: vec![FileDetail {
            change: FileChange {
                path: "src/billing.py".into(),
                old_path: None,
                status: FileStatus::Modified,
            },
            source: Some(SourceDelta {
                module_changed: false,
                entities: vec!["charge".into()],
                parse_error: false,
                removed: vec![],
                skip_markers: (0, 0),
            }),
            lock: None,
            config: None,
        }],
    };
    let ev = policy.evaluate(
        &cs,
        &EvalContext {
            map_commits_behind: Some(0),
            environment: None,
            transpile_only: false,
        },
    );
    let sel = select(&SelectInput {
        repo: "acme/app",
        engine: "0.1.0",
        graph: &g,
        map_digest: root,
        policy: &policy,
        evaluation: &ev,
        change: &cs,
        advisory,
        environment: None,
    })
    .unwrap();
    let mut certs: Vec<Certificate> = sel.skipped.into_iter().map(|(_, c)| c).collect();
    assert_eq!(certs.len(), 2);
    signer.sign_all(&mut certs);
    (g, root, policy, certs)
}

fn cfg(signer: &Ed25519Signer) -> VerifyConfig {
    VerifyConfig {
        trusted_keys: vec![signer.key_id()],
        ..VerifyConfig::default()
    }
}

fn reasons(r: &quod_verify::Report) -> String {
    r.violations
        .iter()
        .map(|v| v.reason.as_str())
        .collect::<Vec<_>>()
        .join(" | ")
}

#[test]
fn honest_run_verifies() {
    let s = Ed25519Signer::generate().unwrap();
    let (g, root, policy, certs) = signed_run(&s, false);
    let mut c = cfg(&s);
    c.expected_policy = Some(policy.hash());
    let r = verify(&certs, &g, root, &c);
    assert!(r.ok(), "{}", reasons(&r));
    assert_eq!(r.checked, 2);
}

#[test]
fn tampered_evidence_fails_integrity() {
    let s = Ed25519Signer::generate().unwrap();
    let (g, root, _, mut certs) = signed_run(&s, false);
    certs[0].body.evidence.changed_nodes.clear();
    let r = verify(&certs, &g, root, &cfg(&s));
    assert!(reasons(&r).contains("integrity"), "{}", reasons(&r));
}

#[test]
fn untrusted_or_missing_signatures_fail() {
    let s = Ed25519Signer::generate().unwrap();
    let other = Ed25519Signer::generate().unwrap();
    let (g, root, _, mut certs) = signed_run(&s, false);
    assert!(reasons(&verify(&certs, &g, root, &cfg(&other))).contains("not trusted"));

    certs[0].sig = None;
    let r = verify(&certs, &g, root, &cfg(&s));
    assert!(reasons(&r).contains("unsigned"));
    let lax = VerifyConfig {
        allow_unsigned: true,
        ..cfg(&s)
    };
    assert!(verify(&certs, &g, root, &lax).ok());
}

#[test]
fn signature_from_trusted_key_over_other_content_fails() {
    let s = Ed25519Signer::generate().unwrap();
    let (g, root, _, mut certs) = signed_run(&s, false);
    let stolen = certs[1].sig.clone();
    certs[0].sig = stolen;
    assert!(reasons(&verify(&certs, &g, root, &cfg(&s))).contains("does not verify"));
}

#[test]
fn advisory_certificates_are_rejected() {
    let s = Ed25519Signer::generate().unwrap();
    let (g, root, _, certs) = signed_run(&s, true);
    assert!(reasons(&verify(&certs, &g, root, &cfg(&s))).contains("advisory"));
}

#[test]
fn wrong_map_or_policy_fails_binding() {
    let s = Ed25519Signer::generate().unwrap();
    let (g, _root, _, certs) = signed_run(&s, false);
    let other_root = hash_domain(Domain::MapSection, b"another map");
    assert!(reasons(&verify(&certs, &g, other_root, &cfg(&s))).contains("binding"));

    let (g, root, _, certs) = signed_run(&s, false);
    let mut c = cfg(&s);
    c.expected_policy = Some(hash_domain(Domain::Policy, b"other policy"));
    assert!(reasons(&verify(&certs, &g, root, &c)).contains("binding: policy"));
}

#[test]
fn reordered_chain_fails() {
    let s = Ed25519Signer::generate().unwrap();
    let (g, root, _, mut certs) = signed_run(&s, false);
    certs.swap(0, 1);
    assert!(reasons(&verify(&certs, &g, root, &cfg(&s))).contains("chain"));
}

/// The case the verifier exists for: an engine bug (or a compromised signer
/// key used carelessly) produces a well-formed, correctly signed certificate
/// whose *decision* is wrong. The verifier must still reject it.
#[test]
fn correctly_signed_wrong_decision_is_caught() {
    let s = Ed25519Signer::generate().unwrap();
    let (g, root, _, certs) = signed_run(&s, false);

    let mut body = certs[0].body.clone();
    // Forge a skip for test_util while claiming src/util.py changed.
    body.evidence.changed_nodes = vec!["file:src/util.py".into()];
    body.prev = None;
    let mut forged = Certificate::seal(body).unwrap();
    s.sign(&mut forged);
    let r = verify(&[forged], &g, root, &cfg(&s));
    assert!(
        reasons(&r).contains("in the test's footprint"),
        "{}",
        reasons(&r)
    );
}

#[test]
fn skip_for_blocked_or_unknown_test_is_caught() {
    let s = Ed25519Signer::generate().unwrap();
    let (_, _, _, certs) = signed_run(&s, false);

    // Same tests, but the map now records test_util as using the network.
    let mut b = GraphBuilder::new(MapMeta {
        repo: "acme/app".into(),
        ..MapMeta::default()
    });
    let util = b.node(NodeKind::File, "src/util.py", None, None);
    let t = b.test(
        StableTestId::derive("tests/test_util.py", "test_util", ""),
        "tests/test_util.py::test_util",
        TestFlags::NETWORK,
        5,
    );
    b.observe(t, util);
    let g2 = b.build();
    let (_, root2) = g2.encode().unwrap();

    let mut body = certs
        .iter()
        .find(|c| c.body.test_name.contains("test_util"))
        .unwrap()
        .body
        .clone();
    body.inputs.map = root2;
    body.prev = None;
    let mut c = Certificate::seal(body).unwrap();
    s.sign(&mut c);
    assert!(reasons(&verify(&[c], &g2, root2, &cfg(&s))).contains("network"));

    let mut body = certs
        .iter()
        .find(|c| c.body.test_name.contains("test_more"))
        .unwrap()
        .body
        .clone();
    body.inputs.map = root2;
    body.prev = None;
    let mut c = Certificate::seal(body).unwrap();
    s.sign(&mut c);
    assert!(reasons(&verify(&[c], &g2, root2, &cfg(&s))).contains("not in the map"));
}
