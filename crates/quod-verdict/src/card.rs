//! The Verdict card: Markdown for a PR check or comment (architecture v5, §6.1).

use std::fmt::Write;

use crate::lens::{Finding, Kind, Severity};
use crate::verdict::{Status, Verdict};

const MAX_LISTED: usize = 10;

fn status_line(v: &Verdict) -> &'static str {
    match (v.advisory, v.status) {
        (true, Status::Pass) => "PASS (advisory)",
        (true, Status::Fail) => "FAIL (advisory)",
        (true, Status::Blocked) => "BLOCKED (advisory)",
        (false, Status::Pass) => "PASS",
        (false, Status::Fail) => "FAIL",
        (false, Status::Blocked) => "BLOCKED",
    }
}

fn icon(f: &Finding) -> &'static str {
    match f.severity {
        Severity::Block => "⛔",
        Severity::Review => "👀",
        Severity::Warn => "⚠️",
        Severity::Info => "ℹ️",
    }
}

fn section(out: &mut String, title: &str, items: &[&Finding]) {
    if items.is_empty() {
        return;
    }
    let _ = writeln!(out, "\n**{title}** ({})\n", items.len());
    for f in items.iter().take(MAX_LISTED) {
        let _ = writeln!(
            out,
            "- {} `{}`: {} ({})",
            icon(f),
            f.subject,
            f.detail,
            f.rule
        );
    }
    if items.len() > MAX_LISTED {
        let _ = writeln!(out, "- … and {} more", items.len() - MAX_LISTED);
    }
}

/// Renders the card. `signer` is the key ID that sealed it, if any.
pub fn render(v: &Verdict, signer: Option<&str>) -> String {
    let o = &v.obligations;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "### Quod Verdict: {} · {} ran · {} proven skipped",
        status_line(v),
        o.ran,
        o.certified_skips
    );
    let _ = writeln!(
        out,
        "\n{} passed · {} failed{} · {} obligated by this change",
        o.passed,
        o.failed,
        if o.failed_not_caused > 0 {
            format!(" ({} not caused by this change)", o.failed_not_caused)
        } else {
            String::new()
        },
        o.obligated
    );
    if !o.fallback_rules.is_empty() {
        let _ = writeln!(
            out,
            "\nEverything ran because of {}.",
            o.fallback_rules.join(", ")
        );
    }
    let of = |k: Kind| -> Vec<&Finding> { v.findings.iter().filter(|f| f.kind == k).collect() };
    section(&mut out, "Obligations", &of(Kind::Obligation));
    section(&mut out, "Test integrity", &of(Kind::Integrity));
    section(&mut out, "Untested changes", &of(Kind::UntestedChange));
    section(
        &mut out,
        "Failures not caused by this change",
        &of(Kind::FlakeProof),
    );
    let _ = writeln!(
        out,
        "\n<sub>{} · map `{}` · policy `{}` · engine {}</sub>",
        match signer {
            Some(k) if !v.advisory => format!("signed by `{}`", short(k)),
            _ => "unsigned (advisory: not proof)".to_owned(),
        },
        short(&v.inputs.map),
        short(&v.inputs.policy),
        v.inputs.engine
    );
    out
}

fn short(s: &str) -> &str {
    let s = s.strip_prefix("ed25519:").unwrap_or(s);
    &s[..s.len().min(12)]
}
