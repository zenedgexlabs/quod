//! `skipcert`: sound, certificate-based test selection.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use sc_cert::{Ed25519Signer, Signer, write_jsonl};
use sc_change::Repo;
use sc_cli::{ENGINE_VERSION, SelectOptions, ingest, run_select};
use sc_graph::MapMeta;

#[derive(Parser)]
#[command(
    name = "skipcert",
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
    /// Example: `skipcert record --out .skipcert/map.scmap -- python -m pytest`
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
    /// Decide which tests to run and certify the skips.
    Select {
        /// Footprint map.
        #[arg(long)]
        map: PathBuf,
        /// Head revision (default: the working tree).
        #[arg(long)]
        head: Option<String>,
        /// Policy file (default: skipcert.toml at the repository root, if present).
        #[arg(long)]
        policy: Option<PathBuf>,
        /// Signing key seed file. Required unless --advisory.
        #[arg(long)]
        key: Option<PathBuf>,
        /// Advisory output (sandbox or local): certificates can't license CI skips.
        #[arg(long)]
        advisory: bool,
        /// Write skipped test names here (pass to pytest as --skipcert-skip).
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
            let dir = std::env::temp_dir().join(format!("skipcert-capture-{}", std::process::id()));
            fs::create_dir_all(&dir)?;
            let capture = dir.join("capture.jsonl");
            let status = Command::new(&command[0])
                .args(&command[1..])
                .arg(format!("--skipcert-record={}", capture.display()))
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
                bail!(
                    "the test command produced no capture (is the skipcert-probe plugin installed?)"
                );
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
            map,
            head,
            policy,
            key,
            advisory,
            skip_out,
            certs_out,
            report_out,
            json,
        } => {
            let repo = Repo::discover(&cli.repo_dir)?;
            let opts = SelectOptions {
                map: map.clone(),
                head: head.clone(),
                policy: policy.clone(),
                key: key.clone(),
                advisory: *advisory,
                repo_id: repo_id(&cli, &repo),
            };
            let res = run_select(&repo, &opts)?;
            if let Some(p) = skip_out {
                let mut text = res.skip_names.join("\n");
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
    }
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("skipcert: {e:#}");
            ExitCode::from(2)
        }
    }
}
