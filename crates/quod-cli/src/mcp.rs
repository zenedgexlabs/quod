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

use quod_graph::{FootprintGraph, MapMeta};

use crate::agent::{self, EnvSetup};
use crate::{
    CulpritOptions, SelectOptions, SelectResult, VerdictOptions, run_culprit, run_select,
    run_verdict,
};

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
    fn options(&self, env: &EnvSetup) -> SelectOptions {
        SelectOptions {
            map: self.map.clone(),
            head: None,
            policy: self.policy.clone(),
            key: None,
            advisory: true,
            repo_id: self.repo_id.clone(),
            python: env.python.clone(),
            fingerprint_cmd: env.fingerprint_cmd.clone(),
        }
    }

    /// The map's metadata and the detected test environment.
    fn setup(&self) -> Result<(FootprintGraph, EnvSetup)> {
        let bytes = std::fs::read(&self.map)?;
        let (graph, _) = FootprintGraph::decode(&bytes)?;
        let env = agent::detect(self.repo.root(), graph.meta());
        Ok((graph, env))
    }

    /// Selection in environment mode when the environment was found, else
    /// (or if fingerprinting fails) in path mode, which is sound too.
    fn select(&self, env: &mut EnvSetup) -> Result<SelectResult> {
        if env.environment_mode() {
            match run_select(&self.repo, &self.options(env)) {
                Ok(r) => return Ok(r),
                Err(e) => {
                    env.python = None;
                    env.fingerprint_cmd = None;
                    env.note = Some(format!("environment mode failed, path mode: {e:#}"));
                }
            }
        }
        run_select(&self.repo, &self.options(env))
    }

    /// Where `tests_for` writes the skip file (git directory: never a
    /// working-tree change).
    fn skip_file(&self) -> Result<PathBuf> {
        let dir = self.repo.common_dir()?.join("quod");
        std::fs::create_dir_all(&dir)?;
        Ok(dir.join("skip.txt"))
    }
}

fn tools() -> Value {
    json!([
        {
            "name": "tests_for",
            "description": "Which tests the current working-tree change obligates, why, \
                and what the change risks: changed functions no test executes, and test \
                edits that may weaken the tests guarding the change. Returns `run`: a command \
                (cwd, argv) that runs exactly the obligated tests plus any new ones. \
                Advisory: CI recomputes with proof.",
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
            "name": "status",
            "description": "The footprint map in use (commit, how far behind HEAD), the \
                test runner, and whether environment mode is on. Call it when answers look \
                too conservative.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
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
    let (graph, mut env) = ctx.setup()?;
    let res = ctx.select(&mut env)?;
    let r = &res.report;
    let skip_file = ctx.skip_file()?;
    let mut text = res
        .skip_header
        .iter()
        .chain(res.skip_names.iter())
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    text.push('\n');
    std::fs::write(&skip_file, text)?;
    let run = agent::run_command(ctx.repo.root(), graph.meta(), &env, &skip_file);
    Ok(json!({
        "advisory": true,
        "run": run,
        "environment_mode": env.environment_mode(),
        "note": env.note,
        "obligated_count": r.run.len(),
        "obligated": r.run.iter().take(MAX_LISTED).map(|e| json!({"test": e.name, "reason": e.reason})).collect::<Vec<_>>(),
        "truncated": r.run.len() > MAX_LISTED,
        "skippable_count": r.skipped,
        "fallback": r.global,
        "findings": r.findings,
    }))
}

fn status(ctx: &McpContext) -> Result<Value> {
    let (graph, env) = ctx.setup()?;
    let meta: &MapMeta = graph.meta();
    let head = ctx.repo.rev_parse("HEAD")?;
    let ancestor = ctx
        .repo
        .merge_base(&meta.base_commit, &head)
        .is_ok_and(|b| b == meta.base_commit);
    let behind = ancestor
        .then(|| ctx.repo.commits_between(&meta.base_commit, &head).ok())
        .flatten();
    Ok(json!({
        "advisory": true,
        "map": ctx.map,
        "map_commit": meta.base_commit,
        "commits_behind_head": behind,
        "map_is_ancestor_of_head": ancestor,
        "tests_recorded": graph.test_count(),
        "runner": agent::runner(meta),
        "invocation_dir": agent::invocation_dir(meta),
        "environment_mode": env.environment_mode(),
        "python": env.python,
        "fingerprint_cmd": env.fingerprint_cmd,
        "note": env.note,
        "refresh": "`quod map pull` (or restart the server with --pull) fetches the newest green map",
    }))
}

fn why(ctx: &McpContext, test: &str) -> Result<Value> {
    let (_, mut env) = ctx.setup()?;
    let res = ctx.select(&mut env)?;
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
    let (_, mut env) = ctx.setup()?;
    // Fall back to path mode the same way selection does.
    ctx.select(&mut env)?;
    let out = run_verdict(
        &ctx.repo,
        &ctx.options(&env),
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
    // No map yet: nothing can be skipped, which is always safe to say.
    if !ctx.map.is_file() {
        let hint = "no footprint map: run `quod map pull` (or start the server with --pull)                     to fetch the newest green map CI published";
        let v = match name {
            "tests_for" => json!({"advisory": true, "run_everything": true, "reason": hint}),
            "why" => json!({"test": arg("test"), "decision": "run", "reason": hint}),
            "status" => {
                json!({"advisory": true, "map": ctx.map, "map_present": false, "note": hint})
            }
            _ => return text_result(json!({"error": hint}), true),
        };
        return text_result(v, false);
    }
    let env = ctx.setup().map(|(_, e)| e).unwrap_or_default();
    let r = match name {
        "tests_for" => tests_for(ctx),
        "status" => status(ctx),
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
                        python: env.python.clone(),
                        fingerprint_cmd: env.fingerprint_cmd.clone(),
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
