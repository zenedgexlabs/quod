//! `quod-verify`: independent verification of skip certificates and Verdicts.
//!
//! ```text
//! quod-verify --certs <certs.jsonl> --map <map.qmap>
//!                 [--trusted-key ed25519:<hex>]... [--policy-hash <hex>] [--allow-unsigned]
//! quod-verify --verdict <verdict.json> [--map <map.qmap>] [--certs <certs.jsonl>]
//!                 [--trusted-key ed25519:<hex>]...
//! ```
//!
//! Exit status: 0 = all certificates valid, 1 = violations found,
//! 2 = usage or input error.

use std::fs;
use std::process::ExitCode;

use quod_graph::FootprintGraph;
use quod_hash::Digest;
use quod_model::Certificate;
use quod_verify::{VerdictCheck, VerifyConfig, verify, verify_verdict};

const USAGE: &str = "usage: quod-verify --certs <certs.jsonl> --map <map.qmap> \
[--trusted-key ed25519:<hex>]... [--policy-hash <hex>] [--allow-unsigned]\n\
       quod-verify --verdict <verdict.json> [--map <map.qmap>] [--certs <certs.jsonl>] \
[--trusted-key ed25519:<hex>]...";

fn run() -> Result<bool, String> {
    let mut args = std::env::args().skip(1);
    let (mut certs_path, mut map_path, mut verdict_path) = (None, None, None);
    let mut cfg = VerifyConfig::default();
    while let Some(a) = args.next() {
        let mut value = || {
            args.next()
                .ok_or_else(|| format!("{a} needs a value\n{USAGE}"))
        };
        match a.as_str() {
            "--certs" => certs_path = Some(value()?),
            "--map" => map_path = Some(value()?),
            "--verdict" => verdict_path = Some(value()?),
            "--trusted-key" => cfg.trusted_keys.push(value()?),
            "--policy-hash" => {
                cfg.expected_policy = Some(Digest::from_hex(&value()?).map_err(|e| e.to_string())?)
            }
            "--allow-unsigned" => cfg.allow_unsigned = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(true);
            }
            other => return Err(format!("unknown argument {other:?}\n{USAGE}")),
        }
    }
    let map = match &map_path {
        Some(p) => {
            let bytes = fs::read(p).map_err(|e| format!("reading {p}: {e}"))?;
            Some(FootprintGraph::decode(&bytes).map_err(|e| format!("{p}: {e}"))?)
        }
        None => None,
    };
    let certs = match &certs_path {
        Some(p) => Some(read_certs(p)?),
        None => None,
    };

    if let Some(vp) = verdict_path {
        let text = fs::read_to_string(&vp).map_err(|e| format!("reading {vp}: {e}"))?;
        let report = verify_verdict(
            &text,
            &VerdictCheck {
                cfg: &cfg,
                map: map.as_ref().map(|(g, r)| (g, *r)),
                certs: certs.as_deref(),
            },
        );
        for v in &report.violations {
            println!("FAIL {}: {}", v.test, v.reason);
        }
        println!(
            "Verdict {vp}: {} violation(s){}{}",
            report.violations.len(),
            if map.is_some() { ", map checked" } else { "" },
            if certs.is_some() {
                ", certificates checked"
            } else {
                ""
            }
        );
        return Ok(report.ok());
    }

    let (Some(certs), Some((graph, root))) = (certs, map) else {
        return Err(USAGE.into());
    };
    let report = verify(&certs, &graph, root, &cfg);
    for v in &report.violations {
        println!("FAIL #{} {}: {}", v.index, v.test, v.reason);
    }
    println!(
        "{} certificate(s) checked, {} violation(s), map {}",
        report.checked,
        report.violations.len(),
        root
    );
    Ok(report.ok())
}

fn read_certs(path: &str) -> Result<Vec<Certificate>, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("reading {path}: {e}"))?;
    let mut certs = Vec::new();
    for (i, line) in text
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
    {
        let c: Certificate =
            serde_json::from_str(line).map_err(|e| format!("{path}:{}: {e}", i + 1))?;
        certs.push(c);
    }
    Ok(certs)
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(2)
        }
    }
}
