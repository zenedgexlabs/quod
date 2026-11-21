//! Library behind the `skipcert` CLI: capture ingestion and selection.
//!
//! Kept separate from `main.rs` so the whole record → select → verify flow is
//! testable without a Python interpreter.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use sc_cert::{Ed25519Signer, Signer};
use sc_change::{ChangeOptions, ChangeSet, Repo};
use sc_graph::{FootprintGraph, GraphBuilder, MapMeta, NodeId};
use sc_hash::{Digest, hash_file};
use sc_ladder::{RunReason, SelectInput, Selection, select};
use sc_model::{Certificate, NodeKind, StableTestId, TestFlags, names};
use sc_policy::{EvalContext, Evaluation, Policy};

/// Engine version recorded in maps and certificates.
pub const ENGINE_VERSION: &str = concat!("skipcert/", env!("CARGO_PKG_VERSION"));

/// Capture protocol version this build understands.
pub const CAPTURE_VERSION: u32 = 1;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Record {
    Header {
        v: u32,
        probe: String,
    },
    Package {
        name: String,
        dist_hash: String,
    },
    Test {
        path: String,
        qual: String,
        params: String,
        name: String,
        outcome: String,
        duration_ms: u64,
        files: Vec<String>,
        functions: Vec<(String, String)>,
        packages: Vec<String>,
        flags: Vec<String>,
    },
    Root {
        files: Vec<String>,
        functions: Vec<(String, String)>,
        packages: Vec<String>,
    },
}

/// Maps probe flag names to [`TestFlags`]. Unknown names are treated as
/// `untraced` (fail closed).
fn flag(name: &str) -> TestFlags {
    match name {
        "network" => TestFlags::NETWORK,
        "subprocess" => TestFlags::SUBPROCESS,
        "dynamic_import" => TestFlags::DYNAMIC_IMPORT,
        "c_ext" => TestFlags::C_EXT,
        "untraced" => TestFlags::UNTRACED,
        _ => TestFlags::UNTRACED,
    }
}

struct Ingest<'a> {
    b: GraphBuilder,
    root: &'a Path,
    file_hashes: HashMap<String, Option<Digest>>,
    package_hashes: HashMap<String, Option<Digest>>,
}

impl Ingest<'_> {
    fn file(&mut self, rel: &str) -> NodeId {
        let name = names::file(rel);
        let root = self.root;
        let hash = *self
            .file_hashes
            .entry(name.clone())
            .or_insert_with(|| hash_file(root.join(&name)).ok());
        self.b.node(NodeKind::File, &name, hash, None)
    }

    fn function(&mut self, rel: &str, qual: &str) -> NodeId {
        let parent = self.file(rel);
        self.b.node(
            NodeKind::Function,
            &names::function(rel, qual),
            None,
            Some(parent),
        )
    }

    fn package(&mut self, name: &str) -> NodeId {
        let hash = self.package_hashes.get(name).copied().flatten();
        self.b.node(
            NodeKind::ExtPackage,
            &names::package(sc_model::Ecosystem::Pypi, name),
            hash,
            None,
        )
    }
}

/// Builds a footprint graph from one or more capture files (several when
/// pytest-xdist workers each wrote one).
///
/// Only tests that *passed* during recording are skippable later. A failed,
/// errored, skipped or unreported test may have stopped early, so its
/// footprint is incomplete and it is marked `untraced`.
pub fn ingest(captures: &[PathBuf], repo_root: &Path, mut meta: MapMeta) -> Result<FootprintGraph> {
    // Phase 1: parse and validate every capture file, collecting probe IDs.
    let mut records = Vec::new();
    for path in captures {
        let reader = BufReader::new(
            File::open(path).with_context(|| format!("opening {}", path.display()))?,
        );
        let mut saw_header = false;
        for (i, line) in reader.lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let rec: Record = serde_json::from_str(&line)
                .with_context(|| format!("{}:{}: invalid capture record", path.display(), i + 1))?;
            match &rec {
                Record::Header { v, probe } => {
                    if *v != CAPTURE_VERSION {
                        bail!(
                            "{}: capture protocol v{v} is not supported (expected v{CAPTURE_VERSION})",
                            path.display()
                        );
                    }
                    saw_header = true;
                    if !meta.probes.contains(probe) {
                        meta.probes.push(probe.clone());
                    }
                }
                _ if !saw_header => bail!(
                    "{}: capture file does not start with a header",
                    path.display()
                ),
                _ => records.push(rec),
            }
        }
        if !saw_header {
            bail!("{}: empty capture file", path.display());
        }
    }
    meta.probes.sort();

    // Phase 2: build the graph. Package hashes first, so nodes carry them.
    let mut st = Ingest {
        b: GraphBuilder::new(meta),
        root: repo_root,
        file_hashes: HashMap::new(),
        package_hashes: HashMap::new(),
    };
    for rec in &records {
        if let Record::Package { name, dist_hash } = rec {
            st.package_hashes
                .insert(name.clone(), Digest::from_hex(dist_hash).ok());
        }
    }
    for rec in records {
        match rec {
            Record::Header { .. } | Record::Package { .. } => {}
            Record::Test {
                path: tpath,
                qual,
                params,
                name,
                outcome,
                duration_ms,
                files,
                functions,
                packages,
                flags,
            } => {
                let mut f = TestFlags::default();
                for fl in &flags {
                    f.insert(flag(fl));
                }
                if outcome != "passed" {
                    f.insert(TestFlags::UNTRACED);
                }
                let id = StableTestId::derive(&tpath, &qual, &params);
                let cost = u32::try_from(duration_ms).unwrap_or(u32::MAX);
                let t = st.b.test(id, &name, f, cost);
                for file in &files {
                    let n = st.file(file);
                    st.b.observe(t, n);
                }
                for (p, q) in &functions {
                    let n = st.function(p, q);
                    st.b.observe(t, n);
                }
                for p in &packages {
                    let n = st.package(p);
                    st.b.observe(t, n);
                }
            }
            Record::Root {
                files,
                functions,
                packages,
            } => {
                for file in &files {
                    let n = st.file(file);
                    st.b.observe_root(n);
                }
                for (p, q) in &functions {
                    let n = st.function(p, q);
                    st.b.observe_root(n);
                }
                for p in &packages {
                    let n = st.package(p);
                    st.b.observe_root(n);
                }
            }
        }
    }
    Ok(st.b.build())
}

/// Options for [`run_select`].
#[derive(Clone, Debug, Default)]
pub struct SelectOptions {
    /// The `.scmap` map to select against.
    pub map: PathBuf,
    /// Head revision. `None` means the working tree.
    pub head: Option<String>,
    /// Policy file. Default: `<repo>/skipcert.toml` if present, else the built-in policy.
    pub policy: Option<PathBuf>,
    /// Signing key seed file. Required unless `advisory`.
    pub key: Option<PathBuf>,
    /// Produce advisory (unsigned) output for sandboxes and local use.
    pub advisory: bool,
    /// Repository identifier recorded in certificates.
    pub repo_id: String,
}

/// One test that must run, for reports.
#[derive(Clone, Debug, Serialize)]
pub struct RunEntry {
    /// Test name.
    pub name: String,
    /// Why it runs.
    pub reason: String,
}

/// Machine-readable selection report (agents, PR checks).
#[derive(Clone, Debug, Serialize)]
pub struct Report {
    /// Engine version.
    pub engine: String,
    /// Map base commit.
    pub base: String,
    /// Head commit.
    pub head: String,
    /// Whether the output is advisory.
    pub advisory: bool,
    /// Tests in the map.
    pub tests_in_map: usize,
    /// Tests that must run, with reasons.
    pub run: Vec<RunEntry>,
    /// Number of skipped tests.
    pub skipped: usize,
    /// Estimated skipped test time, in milliseconds (from recorded medians).
    pub skipped_ms: u64,
    /// Global triggers (rule: detail).
    pub global: Vec<String>,
    /// Changed node refs.
    pub changed: Vec<String>,
    /// Integrity findings (rule: path: detail).
    pub integrity: Vec<String>,
    /// Policy hash.
    pub policy: String,
    /// Map root.
    pub map: String,
}

/// Everything selection produced.
pub struct SelectResult {
    /// Signed (or advisory) certificates for skipped tests, in chain order.
    pub certificates: Vec<Certificate>,
    /// Names of skipped tests (the `--skipcert-skip` file content).
    pub skip_names: Vec<String>,
    /// The report.
    pub report: Report,
    /// The raw selection.
    pub selection: Selection,
    /// The policy evaluation.
    pub evaluation: Evaluation,
    /// The resolved change.
    pub change: ChangeSet,
}

fn reason_text(r: &RunReason) -> String {
    match r {
        RunReason::Global(rule) => format!("global {rule}"),
        RunReason::Rule(rule) => format!("rule {rule}"),
        RunReason::Affected(via) => format!("affected by {}", via.join(", ")),
    }
}

/// Loads the policy: an explicit file, else `skipcert.toml` at the repository
/// root, else the built-in default.
pub fn load_policy(repo: &Repo, explicit: Option<&Path>) -> Result<Policy> {
    let path = match explicit {
        Some(p) => Some(p.to_path_buf()),
        None => Some(repo.root().join("skipcert.toml")).filter(|p| p.is_file()),
    };
    match path {
        Some(p) => {
            let text =
                std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
            Policy::from_toml(&text).with_context(|| format!("{}", p.display()))
        }
        None => Ok(Policy::default_policy()),
    }
}

/// Change → policy → ladder → certificates, for the map's base commit.
pub fn run_select(repo: &Repo, opts: &SelectOptions) -> Result<SelectResult> {
    let bytes =
        std::fs::read(&opts.map).with_context(|| format!("reading {}", opts.map.display()))?;
    let (graph, map_root) = FootprintGraph::decode(&bytes).context("decoding map")?;
    let base = graph.meta().base_commit.clone();
    if base.is_empty() {
        bail!("map has no base commit");
    }

    // Diff against the commit the footprints were recorded at, not the merge
    // base: anything that differs from what was observed may matter.
    let change = sc_change::compute(
        repo,
        &ChangeOptions {
            base: base.clone(),
            head: opts.head.clone(),
        },
    )?;
    let behind = repo.commits_between(&base, &change.head).ok();
    let policy = load_policy(repo, opts.policy.as_deref())?;
    let evaluation = policy.evaluate(
        &change,
        &EvalContext {
            map_commits_behind: behind,
        },
    );

    let signer = match (&opts.key, opts.advisory) {
        (Some(k), _) => Some(Ed25519Signer::from_seed_hex(
            &std::fs::read_to_string(k).with_context(|| format!("reading key {}", k.display()))?,
        )?),
        (None, true) => None,
        (None, false) => bail!("a signing key (--key) is required unless --advisory is set"),
    };
    let advisory = opts.advisory || signer.is_none();

    let selection = select(&SelectInput {
        repo: &opts.repo_id,
        engine: ENGINE_VERSION,
        graph: &graph,
        map_digest: map_root,
        policy: &policy,
        evaluation: &evaluation,
        change: &change,
        advisory,
    })?;

    let mut certificates: Vec<Certificate> =
        selection.skipped.iter().map(|(_, c)| c.clone()).collect();
    if let Some(s) = &signer {
        s.sign_all(&mut certificates);
    }
    let skip_names: Vec<String> = selection
        .skipped
        .iter()
        .map(|(t, _)| graph.test_name(*t).to_owned())
        .collect();
    let skipped_ms = selection
        .skipped
        .iter()
        .map(|(t, _)| u64::from(graph.test(*t).cost_ms_p50))
        .sum();

    let report = Report {
        engine: ENGINE_VERSION.to_owned(),
        base,
        head: change.head.clone(),
        advisory,
        tests_in_map: graph.test_count(),
        run: selection
            .run
            .iter()
            .map(|(t, r)| RunEntry {
                name: graph.test_name(*t).to_owned(),
                reason: reason_text(r),
            })
            .collect(),
        skipped: selection.skipped.len(),
        skipped_ms,
        global: evaluation
            .global
            .iter()
            .map(|t| match &t.path {
                Some(p) => format!("{}: {} ({p})", t.rule, t.detail),
                None => format!("{}: {}", t.rule, t.detail),
            })
            .collect(),
        changed: selection.changed_refs.clone(),
        integrity: evaluation
            .integrity
            .iter()
            .map(|f| format!("{}: {}: {}", f.rule, f.path, f.detail))
            .collect(),
        policy: policy.hash().to_hex(),
        map: map_root.to_hex(),
    };

    Ok(SelectResult {
        certificates,
        skip_names,
        report,
        selection,
        evaluation,
        change,
    })
}
