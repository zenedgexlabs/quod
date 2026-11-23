//! Library behind the `skipcert` CLI: capture ingestion and selection.
//!
//! Kept separate from `main.rs` so the whole record → select → verify flow is
//! testable without a Python interpreter.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use sc_cert::{Ed25519Signer, Signer};
use sc_change::{ChangeOptions, ChangeSet, DiffOptions, Language, LineMap, Repo};
use sc_graph::{FootprintGraph, GraphBuilder, MapMeta, NodeId};
use sc_hash::{Digest, hash_file};
use sc_ladder::{RunReason, SelectInput, Selection, select};
use sc_model::version::{CAP_PY_ENVIRONMENT, CAP_PY_INTROSPECTION};
use sc_model::{Certificate, NodeKind, StableTestId, TestFlags, names};
use sc_policy::{EnvironmentDelta, EvalContext, Evaluation, Policy};

/// Engine version recorded in maps and certificates.
pub const ENGINE_VERSION: &str = concat!("skipcert/", env!("CARGO_PKG_VERSION"));

/// Capture protocol version this build understands.
pub const CAPTURE_VERSION: u32 = 1;

/// A capture's (environment, invocation, environment digest, invocation
/// digest).
type CaptureFingerprint = (
    BTreeMap<String, String>,
    BTreeMap<String, String>,
    String,
    String,
);

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Record {
    Header {
        v: u32,
        probe: String,
        #[serde(default)]
        capabilities: Vec<String>,
        #[serde(default)]
        environment: BTreeMap<String, String>,
        #[serde(default)]
        invocation: BTreeMap<String, String>,
        #[serde(default)]
        environment_digest: String,
        #[serde(default)]
        invocation_digest: String,
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
        #[serde(default)]
        env: Vec<(String, String)>,
        #[serde(default)]
        lines: Vec<(String, Vec<(u32, u32)>)>,
    },
    Root {
        files: Vec<String>,
        functions: Vec<(String, String)>,
        packages: Vec<String>,
        #[serde(default)]
        env: Vec<(String, String)>,
        #[serde(default)]
        lines: Vec<(String, Vec<(u32, u32)>)>,
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
    /// Line maps of recorded source files (ADR 013). `None`: no entity spec
    /// for the language, or the file couldn't be read or parsed, so lines
    /// observe the file node.
    line_maps: HashMap<String, Option<LineMap>>,
}

impl Ingest<'_> {
    /// Nodes observed by executing `ranges` of `path`: the innermost entity
    /// of each line, and the file node for module-level lines.
    fn lines(&mut self, path: &str, ranges: &[(u32, u32)]) -> Vec<NodeId> {
        let name = names::file(path);
        if !self.line_maps.contains_key(&name) {
            let map = Language::from_path(&name).and_then(|lang| {
                let src = std::fs::read(self.root.join(&name)).ok()?;
                LineMap::new(lang, &src).ok()
            });
            self.line_maps.insert(name.clone(), map);
        }
        let resolved = self.line_maps[&name].as_ref().map(|m| {
            let (ents, module) = m.resolve(ranges);
            (
                ents.into_iter().map(str::to_owned).collect::<Vec<_>>(),
                module,
            )
        });
        let mut out = Vec::new();
        match resolved {
            Some((ents, module)) => {
                for q in &ents {
                    out.push(self.function(&name, q));
                }
                if module || ents.is_empty() {
                    out.push(self.file(&name));
                }
            }
            None => out.push(self.file(&name)),
        }
        out
    }

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

    fn env(&mut self, name: &str, value_hash: &str) -> NodeId {
        self.b.node(
            NodeKind::EnvVar,
            name,
            Digest::from_hex(value_hash).ok(),
            None,
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
    // Capabilities every capture declares. A map is only as capable as its
    // least capable capture.
    let mut capabilities: Option<BTreeSet<String>> = None;
    // Environment fingerprints of every capture. They must all agree;
    // otherwise the map can't vouch for one environment.
    let mut fingerprints: BTreeSet<CaptureFingerprint> = BTreeSet::new();
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
                Record::Header {
                    v,
                    probe,
                    capabilities: caps,
                    environment,
                    invocation,
                    environment_digest,
                    invocation_digest,
                } => {
                    fingerprints.insert((
                        environment.clone(),
                        invocation.clone(),
                        environment_digest.clone(),
                        invocation_digest.clone(),
                    ));
                    if *v != CAPTURE_VERSION {
                        bail!(
                            "{}: capture protocol v{v} is not supported (expected v{CAPTURE_VERSION})",
                            path.display()
                        );
                    }
                    saw_header = true;
                    let caps: BTreeSet<String> = caps.iter().cloned().collect();
                    capabilities = Some(match capabilities.take() {
                        None => caps,
                        Some(prev) => prev.intersection(&caps).cloned().collect(),
                    });
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
    let mut capabilities = capabilities.unwrap_or_default();
    match fingerprints.into_iter().collect::<Vec<_>>().as_slice() {
        [(env, inv, env_d, inv_d)] if !env.is_empty() && !env_d.is_empty() && !inv_d.is_empty() => {
            meta.environment = env.clone();
            meta.invocation = inv.clone();
            meta.environment_digest = env_d.clone();
            meta.invocation_digest = inv_d.clone();
        }
        _ => {
            capabilities.remove(CAP_PY_ENVIRONMENT);
        }
    }
    meta.capabilities = capabilities.into_iter().collect();

    // Phase 2: build the graph. Package hashes first, so nodes carry them.
    let mut st = Ingest {
        b: GraphBuilder::new(meta),
        root: repo_root,
        file_hashes: HashMap::new(),
        package_hashes: HashMap::new(),
        line_maps: HashMap::new(),
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
                env,
                lines,
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
                for (name, h) in &env {
                    let n = st.env(name, h);
                    st.b.observe(t, n);
                }
                for (path, ranges) in &lines {
                    for n in st.lines(path, ranges) {
                        st.b.observe(t, n);
                    }
                }
            }
            Record::Root {
                files,
                functions,
                packages,
                env,
                lines,
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
                for (name, h) in &env {
                    let n = st.env(name, h);
                    st.b.observe_root(n);
                }
                for (path, ranges) in &lines {
                    for n in st.lines(path, ranges) {
                        st.b.observe_root(n);
                    }
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
    /// Python interpreter of the target environment. With a map recorded by
    /// a `py.environment` probe, this turns on environment mode (ADR 012).
    pub python: Option<PathBuf>,
}

#[derive(Deserialize)]
struct Fingerprint {
    environment: BTreeMap<String, String>,
    env: BTreeMap<String, String>,
}

/// Runs the probe's fingerprint command in the target environment.
fn fingerprint(python: &Path, env_names: &[String]) -> Result<Fingerprint> {
    let mut cmd = std::process::Command::new(python);
    cmd.args(["-m", "skipcert_probe", "fingerprint"]);
    for n in env_names {
        cmd.arg("--env").arg(n);
    }
    let out = cmd
        .output()
        .with_context(|| format!("running {} -m skipcert_probe fingerprint", python.display()))?;
    if !out.status.success() {
        bail!(
            "fingerprint command failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    serde_json::from_slice(&out.stdout).context("parsing the fingerprint")
}

/// Compares the recorded environment with the target one.
fn environment_delta(graph: &FootprintGraph, current: &Fingerprint) -> EnvironmentDelta {
    let recorded = &graph.meta().environment;
    let mut delta = EnvironmentDelta::default();
    let keys: BTreeSet<&String> = recorded.keys().chain(current.environment.keys()).collect();
    for k in keys {
        if recorded.get(k) == current.environment.get(k) {
            continue;
        }
        match k.strip_prefix("dist.") {
            Some(d) => delta.dists.push(d.to_owned()),
            None => delta.differing.push(k.clone()),
        }
    }
    for i in 0..graph.node_count() as u32 {
        let n = NodeId(i);
        let node = graph.node(n);
        if node.kind != NodeKind::EnvVar {
            continue;
        }
        let name = graph.node_name(n);
        let now = current.env.get(name).and_then(|h| Digest::from_hex(h).ok());
        if now.is_none() || now != node.hash {
            delta.env_vars.push(name.to_owned());
        }
    }
    delta
}

/// Observed repository files git doesn't track at the base (generated
/// sources, extensions built in place, test data written by a setup step)
/// can change without appearing in any diff. They are re-hashed against the
/// content recorded in the map, and a difference is a changed file.
fn untracked_changes(
    repo: &Repo,
    graph: &FootprintGraph,
    base: &str,
    change: &ChangeSet,
    evaluation: &mut Evaluation,
) -> Result<()> {
    let tracked = repo.tracked_files(base)?;
    let in_change: BTreeSet<&str> = change
        .files
        .iter()
        .flat_map(|f| [Some(f.change.path.as_str()), f.change.old_path.as_deref()])
        .flatten()
        .collect();
    let mut added = false;
    for i in 0..graph.node_count() as u32 {
        let n = NodeId(i);
        let node = graph.node(n);
        if node.kind != NodeKind::File {
            continue;
        }
        let path = graph.node_name(n);
        if tracked.contains(path) || in_change.contains(path) {
            continue;
        }
        if hash_file(repo.root().join(path)).ok() != node.hash {
            evaluation.changed.push(sc_policy::ChangedNode {
                kind: NodeKind::File,
                name: path.to_owned(),
                fallback_file: None,
            });
            added = true;
        }
    }
    if added {
        evaluation.changed.sort();
        evaluation.changed.dedup();
    }
    Ok(())
}

fn env_var_names(graph: &FootprintGraph) -> Vec<String> {
    (0..graph.node_count() as u32)
        .map(NodeId)
        .filter(|n| graph.node(*n).kind == NodeKind::EnvVar)
        .map(|n| graph.node_name(n).to_owned())
        .collect()
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
    /// Header lines for the skip file, checked by the plugin (environment
    /// mode only).
    pub skip_header: Vec<String>,
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
            python: DiffOptions {
                // Splitting `def` headers is sound only if the probe recorded
                // signature introspection as a use of the function.
                split_headers: graph
                    .meta()
                    .capabilities
                    .iter()
                    .any(|c| c == CAP_PY_INTROSPECTION),
            },
        },
    )?;
    let behind = repo.commits_between(&base, &change.head).ok();
    let policy = load_policy(repo, opts.policy.as_deref())?;
    let env_mode = opts.python.is_some()
        && graph
            .meta()
            .capabilities
            .iter()
            .any(|c| c == CAP_PY_ENVIRONMENT);
    let environment = match (&opts.python, env_mode) {
        (Some(py), true) => Some(environment_delta(
            &graph,
            &fingerprint(py, &env_var_names(&graph))?,
        )),
        _ => None,
    };
    let mut evaluation = policy.evaluate(
        &change,
        &EvalContext {
            map_commits_behind: behind,
            environment,
        },
    );
    untracked_changes(repo, &graph, &base, &change, &mut evaluation)?;
    let binding = if env_mode {
        graph.meta().environment_binding()
    } else {
        None
    };

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
        environment: binding,
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
    // Conditions the pytest plugin re-checks before deselecting anything:
    // the environment and invocation the map was recorded in, and the
    // environment variables skipped tests (or the root scope) read.
    let mut skip_header = Vec::new();
    if binding.is_some() && !selection.skipped.is_empty() {
        let meta = graph.meta();
        skip_header.push(format!(
            "# skipcert-environment {}",
            meta.environment_digest
        ));
        skip_header.push(format!("# skipcert-invocation {}", meta.invocation_digest));
        let mut vars: BTreeMap<String, Digest> = BTreeMap::new();
        let nodes = selection
            .skipped
            .iter()
            .flat_map(|(t, _)| graph.footprint(*t).collect::<Vec<_>>())
            .chain(graph.root_nodes());
        for n in nodes {
            let node = graph.node(n);
            if node.kind == NodeKind::EnvVar {
                if let Some(h) = node.hash {
                    vars.insert(graph.node_name(n).to_owned(), h);
                }
            }
        }
        for (name, h) in vars {
            skip_header.push(format!("# skipcert-env {name} {}", h.to_hex()));
        }
    }
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
        skip_header,
        report,
        selection,
        evaluation,
        change,
    })
}
