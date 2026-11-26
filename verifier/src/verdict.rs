//! Independent verification of Quod Verdicts (architecture v5, ADR-021).
//!
//! Re-implemented from the specification. Nothing here calls the engine's
//! Verdict code: the envelope is read as plain JSON, and base64, the DSSE
//! encoding and every decision check are restated. Given a sealed Verdict,
//! trusted keys and, optionally, the map and skip certificates, it checks:
//!
//! 1. authority: a DSSE signature by a trusted key over the payload; the
//!    statement and predicate types; not advisory;
//! 2. subject: the attested commit is the Verdict's head;
//! 3. status: re-derived from obligations and findings (a blocking finding
//!    blocks; a failure not cleared by Flake-proof fails);
//! 4. Flake-proof claims (F001): none when a global rule widened the run;
//!    with the map, each cleared test is in the map with a complete
//!    footprint, and no changed node is in its footprint or the root scope;
//! 5. certificates: count and chain head match, each cites the Verdict's
//!    map, policy, base, head and changed set, and each verifies (via
//!    [`crate::verify`]).

use std::collections::{BTreeSet, HashSet};

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use quod_graph::FootprintGraph;
use quod_hash::Digest;
use quod_model::{Certificate, NodeKind, TestFlags};
use serde_json::Value;

use crate::{Report, VerifyConfig, Violation, kind_for_tag};

const PAYLOAD_TYPE: &str = "application/vnd.in-toto+json";
const STATEMENT_TYPE: &str = "https://in-toto.io/Statement/v1";
const PREDICATE_TYPE: &str = "https://quodhq.com/verdict/v1";

/// Footprint flags that rule out a Flake-proof claim, restated.
const INCOMPLETE: [TestFlags; 4] = [
    TestFlags::UNTRACED,
    TestFlags::DYNAMIC_IMPORT,
    TestFlags::NETWORK,
    TestFlags::SUBPROCESS,
];

fn b64_decode(s: &str) -> Option<Vec<u8>> {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let s = s.as_bytes();
    if s.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let quads = s.len() / 4;
    for (i, q) in s.chunks(4).enumerate() {
        let pad = q.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && i + 1 != quads) {
            return None;
        }
        let mut n = 0u32;
        for (j, &c) in q.iter().enumerate() {
            let v = if j >= 4 - pad {
                0
            } else {
                A.iter().position(|&a| a == c)? as u32
            };
            n = (n << 6) | v;
        }
        let b = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&b[..3 - pad]);
    }
    Some(out)
}

fn pae(payload_type: &str, body: &[u8]) -> Vec<u8> {
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

fn key(key_id: &str) -> Option<VerifyingKey> {
    let hex = key_id.strip_prefix("ed25519:")?;
    if hex.len() != 64 {
        return None;
    }
    let mut b = [0u8; 32];
    for (i, byte) in b.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    VerifyingKey::from_bytes(&b).ok()
}

/// What to check a Verdict against.
pub struct VerdictCheck<'a> {
    /// Trusted keys (and `allow_unsigned`, which also admits advisory
    /// Verdicts for local inspection).
    pub cfg: &'a VerifyConfig,
    /// The map and its Merkle root, to re-check bindings and F001 claims.
    pub map: Option<(&'a FootprintGraph, Digest)>,
    /// The skip certificates the Verdict cites.
    pub certs: Option<&'a [Certificate]>,
}

struct Ctx {
    report: Report,
}

impl Ctx {
    fn fail(&mut self, subject: &str, reason: impl Into<String>) {
        self.report.violations.push(Violation {
            index: 0,
            test: subject.to_owned(),
            reason: reason.into(),
        });
    }
}

fn s<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    path.iter().try_fold(v, |v, k| v.get(*k))?.as_str()
}

fn n(v: &Value, path: &[&str]) -> Option<u64> {
    path.iter().try_fold(v, |v, k| v.get(*k))?.as_u64()
}

/// Verifies a sealed Verdict (the DSSE envelope's JSON text).
pub fn verify_verdict(envelope_json: &str, check: &VerdictCheck<'_>) -> Report {
    let mut cx = Ctx {
        report: Report {
            checked: 1,
            violations: Vec::new(),
        },
    };
    let env: Value = match serde_json::from_str(envelope_json) {
        Ok(v) => v,
        Err(e) => {
            cx.fail("envelope", format!("not JSON: {e}"));
            return cx.report;
        }
    };

    // 1. Authority.
    if s(&env, &["payloadType"]) != Some(PAYLOAD_TYPE) {
        cx.fail("envelope", "payload type is not an in-toto statement");
    }
    let Some(body) = s(&env, &["payload"]).and_then(b64_decode) else {
        cx.fail("envelope", "payload is missing or not base64");
        return cx.report;
    };
    let msg = pae(PAYLOAD_TYPE, &body);
    let sigs = env
        .get("signatures")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let signed = sigs.iter().any(|sg| {
        let (Some(kid), Some(sig)) = (s(sg, &["keyid"]), s(sg, &["sig"])) else {
            return false;
        };
        if !check.cfg.trusted_keys.iter().any(|k| k == kid) {
            return false;
        }
        let (Some(k), Some(raw)) = (key(kid), b64_decode(sig)) else {
            return false;
        };
        let Ok(bytes) = <[u8; 64]>::try_from(raw.as_slice()) else {
            return false;
        };
        k.verify(&msg, &Signature::from_bytes(&bytes)).is_ok()
    });
    if !signed && !check.cfg.allow_unsigned {
        cx.fail("envelope", "no valid signature by a trusted key");
    }
    let st: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            cx.fail("statement", format!("not JSON: {e}"));
            return cx.report;
        }
    };
    if s(&st, &["_type"]) != Some(STATEMENT_TYPE) {
        cx.fail("statement", "not an in-toto v1 statement");
    }
    if s(&st, &["predicateType"]) != Some(PREDICATE_TYPE) {
        cx.fail("statement", "predicate is not a Quod Verdict v1");
        return cx.report;
    }
    let p = &st["predicate"];
    if p.get("advisory").and_then(Value::as_bool) != Some(false) && !check.cfg.allow_unsigned {
        cx.fail("verdict", "advisory Verdicts are not proof");
    }

    // 2. Subject.
    let head = s(p, &["change", "head"]).unwrap_or_default().to_owned();
    let subject_ok = st["subject"].as_array().is_some_and(|subs| {
        subs.iter()
            .any(|x| s(x, &["digest", "gitCommit"]) == Some(head.as_str()))
    });
    if head.is_empty() || !subject_ok {
        cx.fail("subject", "attested commit is not the Verdict's head");
    }

    // 3. Status.
    let findings = p["findings"].as_array().cloned().unwrap_or_default();
    let rule = |f: &Value| s(f, &["rule"]).unwrap_or_default().to_owned();
    let blocks = findings
        .iter()
        .any(|f| s(f, &["severity"]) == Some("block"));
    let f001: Vec<String> = findings
        .iter()
        .filter(|f| rule(f) == "F001")
        .filter_map(|f| s(f, &["subject"]).map(str::to_owned))
        .collect();
    let failed = n(p, &["obligations", "failed"]).unwrap_or(0);
    let cleared = n(p, &["obligations", "failed_not_caused"]).unwrap_or(0);
    if cleared != f001.len() as u64 || cleared > failed {
        cx.fail(
            "status",
            "failures counted as not caused don't match the F001 findings",
        );
    }
    let derived = if blocks {
        "blocked"
    } else if failed > cleared {
        "fail"
    } else {
        "pass"
    };
    if s(p, &["status"]) != Some(derived) {
        cx.fail(
            "status",
            format!(
                "status {:?} but findings and results give {derived:?}",
                s(p, &["status"])
            ),
        );
    }

    // 3b. Agent-authored changes: every finding under a rule the Verdict
    // says blocks for agents must block (it can't be quietly lowered).
    let agent_block: Vec<String> = p["agent_block"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let agent = s(p, &["change", "author_kind"]) == Some("agent");
    if agent && p.get("agent_block").is_none() {
        cx.fail(
            "agent",
            "an agent-authored Verdict must state the rules that block for agents",
        );
    }
    if !agent && p.get("agent_block").is_some() {
        cx.fail("agent", "agent_block on a Verdict not authored by an agent");
    }
    for f in &findings {
        if agent_block.contains(&rule(f)) && s(f, &["severity"]) != Some("block") {
            cx.fail(
                "agent",
                format!(
                    "{} finding on {:?} must block for an agent-authored change",
                    rule(f),
                    s(f, &["subject"])
                ),
            );
        }
    }

    // 4. Flake-proof claims.
    let fallback = p["obligations"]["fallback_rules"]
        .as_array()
        .is_some_and(|a| !a.is_empty());
    if fallback && !f001.is_empty() {
        cx.fail(
            "F001",
            "failures cleared although a global rule widened the run",
        );
    }
    let changed: Vec<String> = p["changed"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    if let Some((graph, root)) = check.map {
        if s(p, &["inputs", "map"]) != Some(root.to_hex().as_str()) {
            cx.fail("binding", format!("Verdict does not cite map {root}"));
        }
        let mut nodes = Vec::new();
        for r in &changed {
            let Some((tag, name)) = r.split_once(':') else {
                cx.fail("changed", format!("malformed node ref {r:?}"));
                continue;
            };
            match kind_for_tag(tag) {
                Some(kind) => {
                    if let Some(node) = graph.find_node(kind, name) {
                        nodes.push(node);
                    }
                }
                None => cx.fail("changed", format!("unknown node kind in {r:?}")),
            }
        }
        let root_set: HashSet<_> = graph.root_nodes().collect();
        if nodes.iter().any(|n| root_set.contains(n)) && !f001.is_empty() {
            cx.fail("F001", "a changed node is in the root scope");
        }
        for name in &f001 {
            let t = (0..graph.test_count() as u32)
                .map(quod_graph::TestIdx)
                .find(|t| graph.test_name(*t) == name);
            let Some(t) = t else {
                cx.fail(name, "F001: test is not in the map");
                continue;
            };
            let flags = graph.test(t).flags;
            if INCOMPLETE.iter().any(|f| flags.contains(*f)) {
                cx.fail(name, "F001: test footprint is incomplete");
            }
            let fp: HashSet<_> = graph.footprint(t).collect();
            if let Some(hit) = nodes.iter().find(|n| fp.contains(n)) {
                cx.fail(
                    name,
                    format!(
                        "F001: changed node {} is in the test's footprint",
                        graph.node_ref(*hit)
                    ),
                );
            }
            // The test's own file changing rules out a claim even when the
            // map knows the file only through other nodes.
            let file = name.split("::").next().unwrap_or(name);
            if changed.iter().any(|r| {
                r.split_once(':').is_some_and(|(tag, n)| {
                    kind_for_tag(tag).is_some_and(|k| {
                        (k == NodeKind::File && n == file)
                            || (k == NodeKind::Function && n.starts_with(&format!("{file}::")))
                    })
                })
            }) {
                cx.fail(name, "F001: the test's own file changed");
            }
        }
    }

    // 5. Certificates.
    if let Some(certs) = check.certs {
        if n(p, &["certificates", "count"]) != Some(certs.len() as u64) {
            cx.fail(
                "certificates",
                "count differs from the certificates provided",
            );
        }
        let head_hash = certs
            .last()
            .map(|c| c.self_hash.to_hex())
            .unwrap_or_default();
        if s(p, &["certificates", "chain_head"]) != Some(head_hash.as_str()) {
            cx.fail("certificates", "chain head differs");
        }
        let want: BTreeSet<&str> = changed.iter().map(String::as_str).collect();
        for c in certs {
            let i = &c.body.inputs;
            let same = s(p, &["inputs", "map"]) == Some(i.map.to_hex().as_str())
                && s(p, &["inputs", "policy"]) == Some(i.policy.to_hex().as_str())
                && s(p, &["change", "base"]) == Some(i.base.as_str())
                && head == i.head;
            let got: BTreeSet<&str> = c
                .body
                .evidence
                .changed_nodes
                .iter()
                .map(String::as_str)
                .collect();
            if !same || got != want {
                cx.fail(
                    &c.body.test_name,
                    "certificate inputs or changed set differ from the Verdict's",
                );
            }
        }
        if let Some((graph, root)) = check.map {
            let r = crate::verify(certs, graph, root, check.cfg);
            cx.report.violations.extend(r.violations);
        }
    }
    cx.report
}
