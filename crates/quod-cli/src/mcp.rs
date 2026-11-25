//! `quod mcp`: a Model Context Protocol server over stdio (architecture v5,
//! §6.2) for coding agents.
//!
//! Everything it returns is **advisory**: it runs inside the agent's
//! sandbox, where the agent controls the files, so nothing it says is
//! proof. Pull-request CI recomputes everything with a trusted key.
//!
//! Transport: newline-delimited JSON-RPC 2.0 on stdin/stdout.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use anyhow::Result;
use serde_json::{Value, json};

use quod_change::Repo;

use crate::{CulpritOptions, SelectOptions, VerdictOptions, run_culprit, run_select, run_verdict};

/// Protocol version this server implements by default.
pub const PROTOCOL_VERSION: &str = "2025-06-18";
/// Maximum test names listed in one response.
const MAX_LISTED: usize = 200;

/// What the server selects against.
pub struct McpContext {
    /// The repository (working tree = the agent's edits).
    pub repo: Repo,
    /// Footprint map baked into the sandbox.
    pub map: PathBuf,
    /// Policy file, if not the default.
    pub policy: Option<PathBuf>,
    /// Repository ID.
    pub repo_id: String,
}

impl McpContext {
    fn options(&self) -> SelectOptions {
        SelectOptions {
            map: self.map.clone(),
            head: None,
            policy: self.policy.clone(),
            key: None,
            advisory: true,
            repo_id: self.repo_id.clone(),
            python: None,
            fingerprint_cmd: None,
        }
    }
}

fn tools() -> Value {
    json!([
        {
            "name": "tests_for",
            "description": "Which tests the current working-tree change obligates, why, \
                and what the change risks: changed functions no test executes, and test \
                edits that may weaken the tests guarding the change. Advisory: CI recomputes \
                with proof.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
        },
        {
            "name": "why",
            "description": "Why a test must run, or why it can be skipped, for the current change.",
            "inputSchema": {
                "type": "object",
                "properties": {"test": {"type": "string", "description": "Test name, e.g. tests/test_a.py::test_x"}},
                "required": ["test"],
                "additionalProperties": false
            }
        },
        {
            "name": "culprit",
            "description": "Which commit between the last green commit and HEAD broke the given \
                failing tests: the commit(s) that changed code each test executes, or 'not_code' \
                when no commit did (flaky or environment). Needs a map recorded at or before \
                the last green commit.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "good": {"type": "string", "description": "Last commit where the tests passed"},
                    "bad": {"type": "string", "description": "Failing commit (default HEAD)"},
                    "failed": {"type": "array", "items": {"type": "string"}, "description": "Failing test names"}
                },
                "required": ["good", "failed"],
                "additionalProperties": false
            }
        },
        {
            "name": "check",
            "description": "An advisory Verdict for the current change from a results file the \
                probe wrote (pytest --quod-results / quod-vitest --results): obligations met, \
                failures this change did not cause, untested changes, test integrity.",
            "inputSchema": {
                "type": "object",
                "properties": {"results": {"type": "string", "description": "Path to the results JSON Lines file"}},
                "required": ["results"],
                "additionalProperties": false
            }
        }
    ])
}

fn text_result(v: Value, is_error: bool) -> Value {
    json!({
        "content": [{"type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default()}],
        "isError": is_error
    })
}

fn tests_for(ctx: &McpContext) -> Result<Value> {
    let res = run_select(&ctx.repo, &ctx.options())?;
    let r = &res.report;
    Ok(json!({
        "advisory": true,
        "obligated_count": r.run.len(),
        "obligated": r.run.iter().take(MAX_LISTED).map(|e| json!({"test": e.name, "reason": e.reason})).collect::<Vec<_>>(),
        "truncated": r.run.len() > MAX_LISTED,
        "skippable_count": r.skipped,
        "fallback": r.global,
        "findings": r.findings,
    }))
}

fn why(ctx: &McpContext, test: &str) -> Result<Value> {
    let res = run_select(&ctx.repo, &ctx.options())?;
    if let Some(e) = res.report.run.iter().find(|e| e.name == test) {
        return Ok(json!({"test": test, "decision": "run", "reason": e.reason}));
    }
    if res.skip_names.iter().any(|n| n == test) {
        return Ok(json!({
            "test": test,
            "decision": "skip",
            "reason": "nothing this test executed or read changed",
            "changed": res.report.changed,
        }));
    }
    Ok(json!({
        "test": test,
        "decision": "run",
        "reason": "not in the footprint map (new or never recorded): it always runs",
    }))
}

fn check(ctx: &McpContext, results: &str) -> Result<Value> {
    let out = run_verdict(
        &ctx.repo,
        &ctx.options(),
        &VerdictOptions {
            results: vec![PathBuf::from(results)],
            author: quod_verdict::AuthorKind::Agent,
            agent: None,
        },
    )?;
    Ok(json!({
        "advisory": true,
        "status": out.verdict.status,
        "obligations": out.verdict.obligations,
        "findings": out.verdict.findings,
        "card": out.card,
    }))
}

fn call(ctx: &McpContext, params: &Value) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    let arg = |k: &str| args.get(k).and_then(Value::as_str).map(str::to_owned);
    let r = match name {
        "tests_for" => tests_for(ctx),
        "why" => match arg("test") {
            Some(t) => why(ctx, &t),
            None => Err(anyhow::anyhow!("missing argument: test")),
        },
        "culprit" => {
            let failed: Vec<String> = args
                .get("failed")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            match (arg("good"), failed.is_empty()) {
                (Some(good), false) => run_culprit(
                    &ctx.repo,
                    &CulpritOptions {
                        map: ctx.map.clone(),
                        good,
                        bad: arg("bad").unwrap_or_else(|| "HEAD".into()),
                        policy: ctx.policy.clone(),
                        failed,
                        python: None,
                        fingerprint_cmd: None,
                    },
                )
                .and_then(|r| Ok(serde_json::to_value(r)?)),
                _ => Err(anyhow::anyhow!("missing argument: good and failed")),
            }
        }
        "check" => match arg("results") {
            Some(p) => check(ctx, &p),
            None => Err(anyhow::anyhow!("missing argument: results")),
        },
        other => Err(anyhow::anyhow!("unknown tool {other}")),
    };
    match r {
        Ok(v) => text_result(v, false),
        Err(e) => text_result(json!({"error": format!("{e:#}")}), true),
    }
}

/// Handles one JSON-RPC message. Returns the response, or `None` for
/// notifications.
pub fn handle(ctx: &McpContext, msg: &Value) -> Option<Value> {
    let id = msg.get("id").cloned()?;
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(json!({}));
    let result = match method {
        "initialize" => json!({
            "protocolVersion": params.get("protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL_VERSION),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "quod", "version": env!("CARGO_PKG_VERSION")},
            "instructions": "Call tests_for after each edit to learn which tests your change obligates, \
                then run exactly those. Results here are advisory; CI issues the signed Verdict."
        }),
        "ping" => json!({}),
        "tools/list" => json!({"tools": tools()}),
        "tools/call" => call(ctx, &params),
        _ => {
            return Some(json!({
                "jsonrpc": "2.0", "id": id,
                "error": {"code": -32601, "message": format!("method not found: {method}")}
            }));
        }
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

/// Serves until stdin closes.
pub fn serve<R: BufRead, W: Write>(ctx: &McpContext, input: R, mut out: W) -> Result<()> {
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => handle(ctx, &msg),
            Err(e) => Some(json!({
                "jsonrpc": "2.0", "id": null,
                "error": {"code": -32700, "message": format!("parse error: {e}")}
            })),
        };
        if let Some(r) = reply {
            writeln!(out, "{r}")?;
            out.flush()?;
        }
    }
    Ok(())
}
