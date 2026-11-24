//! `quod-verify`: independent certificate verification.
//!
//! ```text
//! quod-verify --certs <certs.jsonl> --map <map.qmap>
//!                 [--trusted-key ed25519:<hex>]... [--policy-hash <hex>] [--allow-unsigned]
//! ```
//!
//! Exit status: 0 = all certificates valid, 1 = violations found,
//! 2 = usage or input error.

use std::fs;
use std::process::ExitCode;

use quod_graph::FootprintGraph;
use quod_hash::Digest;
use quod_model::Certificate;
use quod_verify::{VerifyConfig, verify};

const USAGE: &str = "usage: quod-verify --certs <certs.jsonl> --map <map.qmap> \
[--trusted-key ed25519:<hex>]... [--policy-hash <hex>] [--allow-unsigned]";

fn run() -> Result<bool, String> {
    let mut args = std::env::args().skip(1);
    let (mut certs_path, mut map_path) = (None, None);
    let mut cfg = VerifyConfig::default();
    while let Some(a) = args.next() {
        let mut value = || {
            args.next()
                .ok_or_else(|| format!("{a} needs a value\n{USAGE}"))
        };
        match a.as_str() {
            "--certs" => certs_path = Some(value()?),
            "--map" => map_path = Some(value()?),
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
    let certs_path = certs_path.ok_or(USAGE)?;
    let map_path = map_path.ok_or(USAGE)?;

    let map_bytes = fs::read(&map_path).map_err(|e| format!("reading {map_path}: {e}"))?;
    let (graph, root) =
        FootprintGraph::decode(&map_bytes).map_err(|e| format!("{map_path}: {e}"))?;
    let text = fs::read_to_string(&certs_path).map_err(|e| format!("reading {certs_path}: {e}"))?;
    let mut certs = Vec::new();
    for (i, line) in text
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
    {
        let c: Certificate =
            serde_json::from_str(line).map_err(|e| format!("{certs_path}:{}: {e}", i + 1))?;
        certs.push(c);
    }

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
