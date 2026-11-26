//! `quod mcp install`: registers the Quod MCP server with a coding agent
//! (MCP phase 3) by writing the client's project-level configuration.
//!
//! | client      | file                 | shape                                   |
//! |-------------|----------------------|-----------------------------------------|
//! | Claude Code | `.mcp.json`          | `mcpServers.quod` (server runs in the project root) |
//! | Cursor      | `.cursor/mcp.json`   | `mcpServers.quod`, `${workspaceFolder}`  |
//! | VS Code     | `.vscode/mcp.json`   | `servers.quod` with `type: stdio`, `${workspaceFolder}` |
//!
//! Other servers in the file are kept; an existing `quod` entry is
//! replaced. A file that isn't plain JSON (VS Code allows comments) is
//! never rewritten: the entry is printed for the user to paste instead.
//!
//! It also writes a short section into the project's agent instructions
//! (`CLAUDE.md`, or `AGENTS.md` for Cursor and VS Code), between markers so
//! reinstalling replaces it. An attached server alone isn't enough: in E8b
//! an agent with the server connected never called it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Map, Value, json};

/// A supported MCP client.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Client {
    /// Claude Code (`.mcp.json`).
    ClaudeCode,
    /// Cursor (`.cursor/mcp.json`).
    Cursor,
    /// VS Code with Copilot agent mode (`.vscode/mcp.json`).
    Vscode,
}

impl Client {
    /// Configuration file, relative to the repository root.
    pub fn config_path(self) -> &'static str {
        match self {
            Client::ClaudeCode => ".mcp.json",
            Client::Cursor => ".cursor/mcp.json",
            Client::Vscode => ".vscode/mcp.json",
        }
    }

    fn servers_key(self) -> &'static str {
        match self {
            Client::Vscode => "servers",
            _ => "mcpServers",
        }
    }
}

/// Start and end markers of Quod's section in an instructions file.
pub const BEGIN: &str = "<!-- quod:begin -->";
/// See [`BEGIN`].
pub const END: &str = "<!-- quod:end -->";

/// The section itself.
pub const INSTRUCTIONS: &str = "## Running tests (Quod)

This repository has the Quod MCP server (`quod`). It knows which tests each change can affect.

- After changing code, call `tests_for` instead of choosing tests yourself. Run the `run` command it \
returns (`argv` in `cwd`): it runs exactly the tests your change can affect, plus any new ones. Don't \
run the whole suite unless `tests_for` says everything must run.
- Before finishing, call `check`: it reads that run's results and says whether the change is covered.
- If a test fails that your change shouldn't affect, call `culprit`.";

impl Client {
    /// Instructions file the client reads, relative to the repository root.
    pub fn instructions_path(self) -> &'static str {
        match self {
            Client::ClaudeCode => "CLAUDE.md",
            _ => "AGENTS.md",
        }
    }
}

/// Adds or replaces Quod's section in `client`'s instructions file.
pub fn install_instructions(root: &Path, client: Client) -> Result<Installed> {
    let path = root.join(client.instructions_path());
    let section = format!("{BEGIN}\n{INSTRUCTIONS}\n{END}\n");
    let (text, action) = match std::fs::read_to_string(&path) {
        Ok(old) => match (old.find(BEGIN), old.find(END)) {
            (Some(b), Some(e)) if b < e => {
                let mut end = e + END.len();
                if old[end..].starts_with('\n') {
                    end += 1;
                }
                (format!("{}{section}{}", &old[..b], &old[end..]), "updated")
            }
            _ => {
                let sep = if old.is_empty() || old.ends_with("\n\n") {
                    ""
                } else if old.ends_with('\n') {
                    "\n"
                } else {
                    "\n\n"
                };
                (format!("{old}{sep}{section}"), "added")
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (section, "created"),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(Installed {
        path,
        action,
        entry: Value::Null,
    })
}
/// What `install` did.
#[derive(Clone, Debug, Serialize)]
pub struct Installed {
    /// The file written.
    pub path: PathBuf,
    /// `created`, `added` (file existed) or `updated` (replaced a `quod` entry).
    pub action: &'static str,
    /// The server entry.
    pub entry: Value,
}

/// The `quod` server entry for `client`. `command` is the binary to run
/// (committed configs should use a name on PATH, not a local path).
pub fn entry(client: Client, command: &str, pull: bool) -> Value {
    let mut args: Vec<String> = Vec::new();
    // Claude Code starts project servers in the project root; the others
    // get the workspace folder explicitly.
    if client != Client::ClaudeCode {
        args.extend(["--repo-dir".into(), "${workspaceFolder}".into()]);
    }
    args.push("mcp".into());
    if pull {
        args.push("--pull".into());
    }
    let mut e = json!({"command": command, "args": args});
    if client == Client::Vscode {
        e["type"] = json!("stdio");
    }
    e
}

/// Adds or replaces the `quod` entry in `client`'s configuration under
/// `root`.
pub fn install(root: &Path, client: Client, command: &str, pull: bool) -> Result<Installed> {
    let path = root.join(client.config_path());
    let entry = entry(client, command, pull);
    let (mut doc, existed) = match std::fs::read_to_string(&path) {
        Ok(text) => {
            let doc: Value = serde_json::from_str(&text).map_err(|e| {
                anyhow::anyhow!(
                    "{} isn't plain JSON ({e}); add this under \"{}\" yourself:\n\"quod\": {}",
                    path.display(),
                    client.servers_key(),
                    serde_json::to_string_pretty(&entry).unwrap_or_default()
                )
            })?;
            if !doc.is_object() {
                bail!("{} is not a JSON object", path.display());
            }
            (doc, true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (json!({}), false),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let servers = doc
        .as_object_mut()
        .expect("checked above")
        .entry(client.servers_key())
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(servers) = servers.as_object_mut() else {
        bail!(
            "\"{}\" in {} is not an object",
            client.servers_key(),
            path.display()
        );
    };
    let replaced = servers.insert("quod".into(), entry.clone()).is_some();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut text = serde_json::to_string_pretty(&doc)?;
    text.push('\n');
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(Installed {
        path,
        action: match (existed, replaced) {
            (false, _) => "created",
            (true, false) => "added",
            (true, true) => "updated",
        },
        entry,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(p: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
    }

    #[test]
    fn entries_per_client() {
        assert_eq!(
            entry(Client::ClaudeCode, "quod", true),
            json!({"command": "quod", "args": ["mcp", "--pull"]})
        );
        assert_eq!(
            entry(Client::Vscode, "quod", false),
            json!({"type": "stdio", "command": "quod", "args": ["--repo-dir", "${workspaceFolder}", "mcp"]})
        );
        assert_eq!(
            entry(Client::Cursor, "quod", true)["args"][1],
            "${workspaceFolder}"
        );
    }

    #[test]
    fn keeps_other_servers_and_replaces_quod() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(".mcp.json");
        std::fs::write(
            &p,
            r#"{"mcpServers": {"other": {"command": "x"}, "quod": {"command": "old"}}, "keep": 1}"#,
        )
        .unwrap();
        let r = install(dir.path(), Client::ClaudeCode, "quod", true).unwrap();
        assert_eq!(r.action, "updated");
        let doc = read(&p);
        assert_eq!(doc["mcpServers"]["other"]["command"], "x");
        assert_eq!(doc["mcpServers"]["quod"]["command"], "quod");
        assert_eq!(doc["keep"], 1);

        let r = install(dir.path(), Client::Vscode, "quod", true).unwrap();
        assert_eq!(r.action, "created");
        assert_eq!(
            read(&dir.path().join(".vscode/mcp.json"))["servers"]["quod"]["type"],
            "stdio"
        );
    }

    #[test]
    fn instructions_are_one_replaceable_section() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("CLAUDE.md");
        std::fs::write(&p, "# Project\n\nUse tabs.\n").unwrap();
        assert_eq!(
            install_instructions(dir.path(), Client::ClaudeCode)
                .unwrap()
                .action,
            "added"
        );
        assert_eq!(
            install_instructions(dir.path(), Client::ClaudeCode)
                .unwrap()
                .action,
            "updated"
        );
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.starts_with("# Project\n\nUse tabs.\n\n<!-- quod:begin -->"));
        assert_eq!(text.matches(BEGIN).count(), 1, "{text}");
        assert!(text.contains("tests_for"));
        assert_eq!(
            install_instructions(dir.path(), Client::Cursor)
                .unwrap()
                .path,
            dir.path().join("AGENTS.md")
        );
    }

    #[test]
    fn never_rewrites_a_file_it_cannot_parse() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(".vscode/mcp.json");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let original = "{\n  // a comment\n  \"servers\": {}\n}\n";
        std::fs::write(&p, original).unwrap();
        let e = install(dir.path(), Client::Vscode, "quod", true).unwrap_err();
        assert!(format!("{e}").contains("\"quod\":"), "{e}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), original);
    }
}
