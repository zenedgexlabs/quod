//! `quod`: sound, certificate-based test selection.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use quod_cert::{Ed25519Signer, Signer, write_jsonl};
use quod_change::Repo;
use quod_cli::{ENGINE_VERSION, SelectOptions, VerdictOptions, ingest, run_select, run_verdict};
use quod_graph::MapMeta;
use quod_verdict::{AuthorKind, Status};

#[derive(Parser)]
#[command(
    name = "quod",
    version,
    about = "Run only the tests a change can affect, with a certificate for every skip"
)]
struct Cli {
    /// Repository directory (default: current directory).
    #[arg(long, global = true, default_value = ".")]
    repo_dir: PathBuf,
    /// Repository identifier recorded in maps and certificates (default: repository directory name).
    #[arg(long, global = true)]
    repo_id: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Generate an Ed25519 signing key (store the seed file as a CI secret).
    Keygen {
        /// Where to write the secret seed (must not exist).
        #[arg(long)]
        out: PathBuf,
    },
    /// Build a footprint map from probe capture files.
    Ingest {
        /// Capture files (one per pytest-xdist worker is fine).
        #[arg(long = "capture", required = true)]
        captures: Vec<PathBuf>,
        /// Output map.
        #[arg(long)]
        out: PathBuf,
        /// Commit the captures were recorded at (default: HEAD).
        #[arg(long)]
        commit: Option<String>,
        /// Allow a dirty working tree (the map would not match any commit).
        #[arg(long)]
        allow_dirty: bool,
    },
    /// Run a test command with the probe recording, then build the map.
    ///
    /// Example: `quod record --out .quod/map.qmap -- python -m pytest`
    Record {
        /// Output map.
        #[arg(long)]
        out: PathBuf,
        /// Allow a dirty working tree.
        #[arg(long)]
        allow_dirty: bool,
        /// The test command (the probe flag is appended).
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Decide which tests to run and certify the skips (the Verdict's plan stage).
    Select {
        #[command(flatten)]
        sel: SelectArgs,
        /// Write skipped test names here (pass to pytest as --quod-skip).
        #[arg(long)]
        skip_out: Option<PathBuf>,
        /// Write certificates here (JSON Lines).
        #[arg(long)]
        certs_out: Option<PathBuf>,
        /// Write the JSON report here.
        #[arg(long)]
        report_out: Option<PathBuf>,
        /// Print the JSON report to stdout instead of the summary.
        #[arg(long)]
        json: bool,
    },
    /// Seal test results into a signed Verdict (architecture v5).
    ///
    /// Takes the same selection inputs as `select`, plus the results files
    /// the probes wrote. Exits 0 on PASS, 1 on FAIL or BLOCKED.
    Verdict {
        #[command(flatten)]
        sel: SelectArgs,
        /// Results files written by the probes (`--quod-results`), JSON Lines.
        #[arg(long = "results", required = true)]
        results: Vec<PathBuf>,
        /// Write the DSSE envelope (in-toto Statement) here.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Write the Markdown card here (for a PR check or comment).
        #[arg(long)]
        card: Option<PathBuf>,
        /// Who authored the change.
        #[arg(long, value_enum, default_value = "unknown")]
        author: Author,
        /// Agent name, when an agent authored the change.
        #[arg(long)]
        agent: Option<String>,
    },
    /// Serve the agent tools (tests_for, why, check) over MCP on stdio.
    ///
    /// Advisory only: run it inside the agent's sandbox with the map baked
    /// in (`quod mcp --map .quod/map.qmap`).
    Mcp {
        /// Footprint map.
        #[arg(long)]
        map: PathBuf,
        /// Policy file (default: quod.toml at the repository root, if present).
        #[arg(long)]
        policy: Option<PathBuf>,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum Author {
    Human,
    Agent,
    Unknown,
}

/// Selection inputs shared by `select` and `verdict`.
#[derive(clap::Args)]
struct SelectArgs {
    /// Footprint map.
    #[arg(long)]
    map: PathBuf,
    /// Head revision (default: the working tree).
    #[arg(long)]
    head: Option<String>,
    /// Policy file (default: quod.toml at the repository root, if present).
    #[arg(long)]
    policy: Option<PathBuf>,
    /// Signing key seed file. Required unless --advisory.
    #[arg(long)]
    key: Option<PathBuf>,
    /// Advisory output (sandbox or local): certificates can't license CI skips.
    #[arg(long)]
    advisory: bool,
    /// Python interpreter of the environment the tests will run in.
    /// Turns on environment mode for maps recorded by a probe with the
    /// `py.environment` capability (ADR 012).
    #[arg(long)]
    python: Option<PathBuf>,
    /// One argument of the probe's fingerprint command for the target
    /// environment (repeat for each), e.g. `node`,
    /// `<quod-js>/src/cli.mjs`, `fingerprint`, `--root`, `.`.
    /// Turns on environment mode for maps recorded with an environment
    /// capability. `--env NAME` arguments are appended.
    #[arg(long = "fingerprint-cmd", allow_hyphen_values = true)]
    fingerprint_cmd: Vec<String>,
}

fn repo_id(cli: &Cli, repo: &Repo) -> String {
    cli.repo_id.clone().unwrap_or_else(|| {
        repo.root()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "repo".into())
    })
}

fn require_clean(repo: &Repo, allow_dirty: bool) -> Result<()> {
    if !allow_dirty && repo.is_dirty()? {
        bail!(
            "the working tree has uncommitted or untracked changes, so the map would not describe any commit \
             (commit or stash them, or pass --allow-dirty)"
        );
    }
    Ok(())
}

fn write_map(repo: &Repo, id: &str, commit: &str, captures: &[PathBuf], out: &Path) -> Result<()> {
    let meta = MapMeta {
        repo: id.to_owned(),
        base_commit: repo.rev_parse(commit)?,
        engine: ENGINE_VERSION.to_owned(),
        probes: Vec::new(),
        capabilities: Vec::new(),
        ..MapMeta::default()
    };
    let graph = ingest(captures, repo.root(), meta)?;
    let (bytes, root) = graph.encode()?;
    if let Some(dir) = out.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir)?;
    }
    fs::write(out, &bytes).with_context(|| format!("writing {}", out.display()))?;
    println!(
        "map {} : {} tests, {} nodes, {} bytes, root {}",
        out.display(),
        graph.test_count(),
        graph.node_count(),
        bytes.len(),
        root
    );
    Ok(())
}

fn run(cli: Cli) -> Result<ExitCode> {
    match &cli.cmd {
        Cmd::Keygen { out } => {
            let key = Ed25519Signer::generate()?;
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(out)
                .with_context(|| format!("creating {} (refusing to overwrite)", out.display()))?;
            writeln!(f, "{}", key.seed_hex())?;
            println!("wrote secret seed to {}", out.display());
            println!("trusted key id: {}", key.key_id());
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Ingest {
            captures,
            out,
            commit,
            allow_dirty,
        } => {
            let repo = Repo::discover(&cli.repo_dir)?;
            require_clean(&repo, *allow_dirty)?;
            let id = repo_id(&cli, &repo);
            write_map(
                &repo,
                &id,
                commit.as_deref().unwrap_or("HEAD"),
                captures,
                out,
            )?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Record {
            out,
            allow_dirty,
            command,
        } => {
            let repo = Repo::discover(&cli.repo_dir)?;
            require_clean(&repo, *allow_dirty)?;
            let id = repo_id(&cli, &repo);
            let dir = std::env::temp_dir().join(format!("quod-capture-{}", std::process::id()));
            fs::create_dir_all(&dir)?;
            let capture = dir.join("capture.jsonl");
            let status = Command::new(&command[0])
                .args(&command[1..])
                .arg(format!("--quod-record={}", capture.display()))
                .current_dir(repo.root())
                .status()
                .with_context(|| format!("running {:?}", command[0]))?;
            let mut captures: Vec<PathBuf> = fs::read_dir(&dir)?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with("capture.jsonl"))
                })
                .collect();
            captures.sort();
            if captures.is_empty() {
                bail!("the test command produced no capture (is the quod-probe plugin installed?)");
            }
            write_map(&repo, &id, "HEAD", &captures, out)?;
            let _ = fs::remove_dir_all(&dir);
            if !status.success() {
                eprintln!(
                    "note: the test command failed; failing tests are recorded as untraced and always run"
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Select {
            sel,
            skip_out,
            certs_out,
            report_out,
            json,
        } => {
            let repo = Repo::discover(&cli.repo_dir)?;
            let opts = select_options(&cli, &repo, sel);
            let key = &sel.key;
            let res = run_select(&repo, &opts)?;
            if let Some(p) = skip_out {
                let lines: Vec<&str> = res
                    .skip_header
                    .iter()
                    .chain(res.skip_names.iter())
                    .map(String::as_str)
                    .collect();
                let mut text = lines.join("\n");
                if !text.is_empty() {
                    text.push('\n');
                }
                fs::write(p, text).with_context(|| format!("writing {}", p.display()))?;
            }
            if let Some(p) = certs_out {
                write_jsonl(&res.certificates, fs::File::create(p)?)?;
            }
            let report_json = serde_json::to_string_pretty(&res.report)?;
            if let Some(p) = report_out {
                fs::write(p, &report_json)?;
            }
            if *json {
                println!("{report_json}");
            } else {
                let r = &res.report;
                println!(
                    "{} tests in map: {} run, {} skipped (~{:.1}s saved){}",
                    r.tests_in_map,
                    r.run.len(),
                    r.skipped,
                    r.skipped_ms as f64 / 1000.0,
                    if r.advisory { " [advisory]" } else { "" }
                );
                for g in &r.global {
                    println!("  full run: {g}");
                }
                for f in &r.integrity {
                    println!("  integrity: {f}");
                }
                if let Some(k) = key.as_ref().filter(|_| !r.advisory) {
                    let signer = Ed25519Signer::from_seed_hex(&fs::read_to_string(k)?)?;
                    println!("  certificates signed by {}", signer.key_id());
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Verdict {
            sel,
            results,
            out,
            card,
            author,
            agent,
        } => {
            let repo = Repo::discover(&cli.repo_dir)?;
            let opts = select_options(&cli, &repo, sel);
            let vopts = VerdictOptions {
                results: results.clone(),
                author: match author {
                    Author::Human => AuthorKind::Human,
                    Author::Agent => AuthorKind::Agent,
                    Author::Unknown => AuthorKind::Unknown,
                },
                agent: agent.clone(),
            };
            let v = run_verdict(&repo, &opts, &vopts)?;
            if let Some(p) = out {
                fs::write(p, serde_json::to_string_pretty(&v.envelope)?)
                    .with_context(|| format!("writing {}", p.display()))?;
            }
            if let Some(p) = card {
                fs::write(p, &v.card).with_context(|| format!("writing {}", p.display()))?;
            }
            print!("{}", v.card);
            Ok(if v.verdict.status == Status::Pass {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
        Cmd::Mcp { map, policy } => serve_mcp(&cli, map, policy.as_deref()),
    }
}

fn serve_mcp(cli: &Cli, map: &Path, policy: Option<&Path>) -> Result<ExitCode> {
    let repo = Repo::discover(&cli.repo_dir)?;
    let ctx = quod_cli::mcp::McpContext {
        repo_id: repo_id(cli, &repo),
        repo,
        map: map.to_path_buf(),
        policy: policy.map(Path::to_path_buf),
    };
    let stdin = std::io::stdin();
    quod_cli::mcp::serve(&ctx, stdin.lock(), std::io::stdout().lock())?;
    Ok(ExitCode::SUCCESS)
}

fn select_options(cli: &Cli, repo: &Repo, sel: &SelectArgs) -> SelectOptions {
    SelectOptions {
        map: sel.map.clone(),
        head: sel.head.clone(),
        policy: sel.policy.clone(),
        key: sel.key.clone(),
        advisory: sel.advisory,
        repo_id: repo_id(cli, repo),
        python: sel.python.clone(),
        fingerprint_cmd: (!sel.fingerprint_cmd.is_empty()).then(|| sel.fingerprint_cmd.clone()),
    }
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("quod: {e:#}");
            ExitCode::from(2)
        }
    }
}
