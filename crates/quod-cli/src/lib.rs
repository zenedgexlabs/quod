//! Library behind the `quod` CLI: capture ingestion and selection.
//!
//! Kept separate from `main.rs` so the whole record → select → verify flow is
//! testable without a Python interpreter.

pub mod mcp;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use quod_cert::{Ed25519Signer, Signer};
use quod_change::{ChangeOptions, ChangeSet, DiffOptions, Language, LineMap, Repo};
use quod_graph::{FootprintGraph, GraphBuilder, MapMeta, NodeId};
use quod_hash::{Digest, hash_file};
use quod_ladder::{RunReason, SelectInput, Selection, select};
use quod_model::version::{CAP_JS_ENVIRONMENT, CAP_PY_ENVIRONMENT, CAP_PY_INTROSPECTION};
use quod_model::{Certificate, NodeKind, StableTestId, TestFlags, names};
use quod_policy::{EnvironmentDelta, EvalContext, Evaluation, Policy};
use quod_verdict::{Finding, LensInput, lens};

/// Engine version recorded in maps and certificates.
pub const ENGINE_VERSION: &str = concat!("quod/", env!("CARGO_PKG_VERSION"));

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
        /// Package ecosystem of the capture's `package` records and package
        /// references (`pypi` when absent).
        #[serde(default)]
        ecosystem: Option<String>,
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
        #[serde(default)]
        positions: Vec<(String, Vec<(u32, u32)>)>,
    },
    Root {
        files: Vec<String>,
        functions: Vec<(String, String)>,
        packages: Vec<String>,
        #[serde(default)]
        env: Vec<(String, String)>,
        #[serde(default)]
        lines: Vec<(String, Vec<(u32, u32)>)>,
        #[serde(default)]
        positions: Vec<(String, Vec<(u32, u32)>)>,
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
        self.load_map(&name);
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

    /// Nodes observed by executing the functions that start at `positions`
    /// (1-based line, 0-based column) in `path`: the innermost entity of
    /// each, or the file node when a position is outside every entity.
    fn positions(&mut self, path: &str, positions: &[(u32, u32)]) -> Vec<NodeId> {
        let name = names::file(path);
        self.load_map(&name);
        let quals: Option<Vec<Option<String>>> = self.line_maps[&name].as_ref().map(|m| {
            positions
                .iter()
                .map(|&(l, c)| m.entity_at(l, c).map(str::to_owned))
                .collect()
        });
        let mut out = Vec::new();
        match quals {
            Some(qs) => {
                for q in qs {
                    match q {
                        Some(q) => out.push(self.function(&name, &q)),
                        None => out.push(self.file(&name)),
                    }
                }
            }
            None => out.push(self.file(&name)),
        }
        out
    }

    /// Module-state widening (ADR 014): for every touched source file that
    /// keeps mutable state at module or class level, every entity of the
    /// file and the file itself. A memoised result can be read without
    /// re-running the function that computed it, possibly in another test.
    fn stateful_widening(
        &mut self,
        files: &[String],
        functions: &[(String, String)],
        positions: &[(String, Vec<(u32, u32)>)],
        lines: &[(String, Vec<(u32, u32)>)],
    ) -> Vec<NodeId> {
        let touched: BTreeSet<String> = files
            .iter()
            .map(|f| names::file(f))
            .chain(functions.iter().map(|(p, _)| names::file(p)))
            .chain(positions.iter().map(|(p, _)| names::file(p)))
            .chain(lines.iter().map(|(p, _)| names::file(p)))
            .collect();
        let mut out = Vec::new();
        for f in touched {
            self.load_map(&f);
            let entities: Vec<String> = match &self.line_maps[&f] {
                Some(m) if m.module_state() => m.entities().to_vec(),
                _ => continue,
            };
            out.push(self.file(&f));
            for q in &entities {
                out.push(self.function(&f, q));
            }
        }
        out
    }

    fn load_map(&mut self, name: &str) {
        if !self.line_maps.contains_key(name) {
            let map = Language::from_path(name).and_then(|lang| {
                let src = std::fs::read(self.root.join(name)).ok()?;
                LineMap::new(lang, &src).ok()
            });
            self.line_maps.insert(name.to_owned(), map);
        }
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

    /// `name` is ecosystem-qualified (see [`qualify_packages`]).
    fn package(&mut self, name: &str) -> NodeId {
        let hash = self.package_hashes.get(name).copied().flatten();
        self.b.node(NodeKind::ExtPackage, name, hash, None)
    }
}

/// Rewrites a record's package names to graph node names (`pypi:x`,
/// `npm:x`) for the capture's ecosystem.
fn qualify_packages(rec: &mut Record, eco: quod_model::Ecosystem) {
    let q = |n: &mut String| *n = names::package(eco, n);
    match rec {
        Record::Package { name, .. } => q(name),
        Record::Test { packages, .. } | Record::Root { packages, .. } => {
            packages.iter_mut().for_each(q)
        }
        Record::Header { .. } => {}
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
        let mut eco = quod_model::Ecosystem::Pypi;
        for (i, line) in reader.lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let mut rec: Record = serde_json::from_str(&line)
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
                    ecosystem,
                } => {
                    eco = match ecosystem.as_deref() {
                        None | Some("pypi") => quod_model::Ecosystem::Pypi,
                        Some("npm") => quod_model::Ecosystem::Npm,
                        Some(other) => bail!(
                            "{}: unsupported package ecosystem {other:?}",
                            path.display()
                        ),
                    };
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
                _ => {
                    qualify_packages(&mut rec, eco);
                    records.push(rec);
                }
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
                positions,
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
                for (path, pos) in &positions {
                    for n in st.positions(path, pos) {
                        st.b.observe(t, n);
                    }
                }
                for n in st.stateful_widening(&files, &functions, &positions, &lines) {
                    st.b.observe(t, n);
                }
            }
            Record::Root {
                files,
                functions,
                packages,
                env,
                lines,
                positions,
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
                for (path, pos) in &positions {
                    for n in st.positions(path, pos) {
                        st.b.observe_root(n);
                    }
                }
                for n in st.stateful_widening(&files, &functions, &positions, &lines) {
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
    /// The `.qmap` map to select against.
    pub map: PathBuf,
    /// Head revision. `None` means the working tree.
    pub head: Option<String>,
    /// Policy file. Default: `<repo>/quod.toml` if present, else the built-in policy.
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
    /// The probe's fingerprint command for the target environment, for
    /// probes other than Python's (for example `node <quod-js>/src/cli.mjs
    /// fingerprint --root <repo>`). `--env NAME` arguments are appended.
    pub fingerprint_cmd: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct Fingerprint {
    environment: BTreeMap<String, String>,
    env: BTreeMap<String, String>,
}

/// Runs the probe's fingerprint command in the target environment.
fn fingerprint(argv: &[String], env_names: &[String]) -> Result<Fingerprint> {
    let (program, args) = argv.split_first().context("empty fingerprint command")?;
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    for n in env_names {
        cmd.arg("--env").arg(n);
    }
    let out = cmd
        .output()
        .with_context(|| format!("running the fingerprint command {}", argv.join(" ")))?;
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
            evaluation.changed.push(quod_policy::ChangedNode {
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
    /// Plan-stage lens findings (Untested-change, Integrity), sorted.
    pub findings: Vec<Finding>,
    /// Policy hash.
    pub policy: String,
    /// Map root.
    pub map: String,
}

/// Everything selection produced.
pub struct SelectResult {
    /// Signed (or advisory) certificates for skipped tests, in chain order.
    pub certificates: Vec<Certificate>,
    /// Names of skipped tests (the `--quod-skip` file content).
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
    /// The footprint map selection ran against.
    pub graph: FootprintGraph,
    /// The policy in force.
    pub policy: Policy,
    /// True if the environment was fingerprinted and compared.
    pub environment_checked: bool,
}

impl SelectResult {
    /// The lens input for this selection.
    pub fn lens_input(&self) -> LensInput<'_> {
        LensInput {
            graph: &self.graph,
            policy: &self.policy,
            change: &self.change,
            evaluation: &self.evaluation,
            environment_checked: self.environment_checked,
        }
    }
}

fn reason_text(r: &RunReason) -> String {
    match r {
        RunReason::Global(rule) => format!("global {rule}"),
        RunReason::Rule(rule) => format!("rule {rule}"),
        RunReason::Affected(via) => format!("affected by {}", via.join(", ")),
    }
}

/// Loads the policy: an explicit file, else `quod.toml` at the repository
/// root, else the built-in default.
pub fn load_policy(repo: &Repo, explicit: Option<&Path>) -> Result<Policy> {
    let path = match explicit {
        Some(p) => Some(p.to_path_buf()),
        None => Some(repo.root().join("quod.toml")).filter(|p| p.is_file()),
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
    let change = quod_change::compute(
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
    let fingerprint_cmd: Option<Vec<String>> = match (&opts.fingerprint_cmd, &opts.python) {
        (Some(cmd), _) => Some(cmd.clone()),
        (None, Some(py)) => Some(vec![
            py.to_string_lossy().into_owned(),
            "-m".into(),
            "quod_probe".into(),
            "fingerprint".into(),
        ]),
        (None, None) => None,
    };
    let env_mode = fingerprint_cmd.is_some()
        && graph
            .meta()
            .capabilities
            .iter()
            .any(|c| c == CAP_PY_ENVIRONMENT || c == CAP_JS_ENVIRONMENT);
    let environment = match (&fingerprint_cmd, env_mode) {
        (Some(cmd), true) => Some(environment_delta(
            &graph,
            &fingerprint(cmd, &env_var_names(&graph))?,
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
        skip_header.push(format!("# quod-environment {}", meta.environment_digest));
        skip_header.push(format!("# quod-invocation {}", meta.invocation_digest));
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
            skip_header.push(format!("# quod-env {name} {}", h.to_hex()));
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
        findings: Vec::new(),
        policy: policy.hash().to_hex(),
        map: map_root.to_hex(),
    };

    let mut res = SelectResult {
        certificates,
        skip_names,
        skip_header,
        report,
        selection,
        evaluation,
        change,
        graph,
        policy,
        environment_checked: env_mode,
    };
    let inp = res.lens_input();
    let mut findings = lens::untested_changes(&inp);
    findings.extend(lens::integrity(&inp));
    findings.sort();
    res.report.findings = findings;
    Ok(res)
}

/// Options for [`run_verdict`] beyond selection's.
#[derive(Clone, Debug)]
pub struct VerdictOptions {
    /// Results files written by the probes (JSON Lines).
    pub results: Vec<PathBuf>,
    /// Who authored the change.
    pub author: quod_verdict::AuthorKind,
    /// Agent name, if an agent authored it.
    pub agent: Option<String>,
}

/// A sealed Verdict and its card.
pub struct VerdictOutput {
    /// The signed (or, when advisory, unsigned) DSSE envelope.
    pub envelope: quod_verdict::Envelope,
    /// The Verdict.
    pub verdict: quod_verdict::Verdict,
    /// Markdown card.
    pub card: String,
}

#[derive(Deserialize)]
struct ResultLine {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    name: Option<String>,
    outcome: Option<String>,
    #[serde(default)]
    duration_ms: f64,
}

/// Reads probe results files: JSON Lines of `{"type": "result", "name",
/// "outcome", "duration_ms"}`. Other record types are ignored. A test that
/// appears more than once (reruns, workers) keeps its worst outcome.
pub fn read_results(paths: &[PathBuf]) -> Result<Vec<quod_verdict::TestResult>> {
    use quod_verdict::Outcome;
    let rank = |o: Outcome| match o {
        Outcome::Passed => 0,
        Outcome::Skipped => 1,
        Outcome::Failed => 2,
    };
    let mut by_name: BTreeMap<String, quod_verdict::TestResult> = BTreeMap::new();
    for p in paths {
        let f = File::open(p).with_context(|| format!("reading {}", p.display()))?;
        for (i, line) in BufReader::new(f).lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let r: ResultLine = serde_json::from_str(&line)
                .with_context(|| format!("{}:{}", p.display(), i + 1))?;
            if r.kind.as_deref().is_some_and(|k| k != "result") {
                continue;
            }
            let (Some(name), Some(outcome)) = (r.name, r.outcome) else {
                continue;
            };
            let outcome = match outcome.as_str() {
                "passed" => Outcome::Passed,
                "skipped" => Outcome::Skipped,
                // "failed", "error" and anything unknown count as failures.
                _ => Outcome::Failed,
            };
            let entry = by_name
                .entry(name.clone())
                .or_insert(quod_verdict::TestResult {
                    name,
                    outcome,
                    duration_ms: 0,
                });
            if rank(outcome) > rank(entry.outcome) {
                entry.outcome = outcome;
            }
            entry.duration_ms += r.duration_ms.max(0.0).round() as u64;
        }
    }
    Ok(by_name.into_values().collect())
}

/// Plan → results → lenses → sealed Verdict (architecture v5, F1).
///
/// Selection is recomputed from the same pinned inputs, so the Verdict
/// describes exactly the obligations the plan stage handed out.
pub fn run_verdict(
    repo: &Repo,
    opts: &SelectOptions,
    vopts: &VerdictOptions,
) -> Result<VerdictOutput> {
    use quod_verdict::{CertRef, ChangeRef, CompileInput, Inputs};

    let res = run_select(repo, opts)?;
    let results = read_results(&vopts.results)?;
    let failed: Vec<String> = results
        .iter()
        .filter(|r| r.outcome == quod_verdict::Outcome::Failed)
        .map(|r| r.name.clone())
        .collect();

    let inp = res.lens_input();
    let mut findings = res.report.findings.clone();
    findings.extend(lens::flake_proof(&inp, &failed));

    let changed_test_files = res
        .change
        .files
        .iter()
        .filter(|f| res.policy.is_test_path(&f.change.path))
        .map(|f| f.change.path.clone())
        .collect();
    let input = CompileInput {
        change: ChangeRef {
            repo: opts.repo_id.clone(),
            base: res.report.base.clone(),
            head: res.report.head.clone(),
            tree: res.change.tree.map(|t| t.to_hex()),
            author_kind: vopts.author,
            agent: vopts.agent.clone(),
        },
        inputs: Inputs {
            map: res.report.map.clone(),
            policy: res.report.policy.clone(),
            engine: res.report.engine.clone(),
            environment: res
                .environment_checked
                .then(|| res.graph.meta().environment_digest.clone()),
        },
        changed: res.report.changed.clone(),
        advisory: res.report.advisory,
        obligated: res.report.run.iter().map(|r| r.name.clone()).collect(),
        changed_test_files,
        results,
        certificates: CertRef {
            chain_head: res
                .certificates
                .last()
                .map(|c| c.self_hash.to_hex())
                .unwrap_or_default(),
            count: res.certificates.len(),
        },
        fallback_rules: res
            .evaluation
            .global
            .iter()
            .map(|t| t.rule.to_owned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
    };
    let verdict = quod_verdict::compile(input, findings);
    let signer = match (&opts.key, verdict.advisory) {
        (Some(k), false) => Some(Ed25519Signer::from_seed_hex(
            &std::fs::read_to_string(k).with_context(|| format!("reading key {}", k.display()))?,
        )?),
        _ => None,
    };
    let st = quod_verdict::statement(verdict.clone());
    let envelope = quod_verdict::seal(&st, signer.as_ref().map(|s| s as &dyn Signer))?;
    let card = quod_verdict::card::render(&verdict, quod_verdict::verdict::signer_of(&envelope));
    Ok(VerdictOutput {
        envelope,
        verdict,
        card,
    })
}

/// Options for [`run_culprit`].
#[derive(Clone, Debug)]
pub struct CulpritOptions {
    /// Footprint map, recorded at or before the last green commit.
    pub map: PathBuf,
    /// Last commit where the tests passed.
    pub good: String,
    /// First commit where they fail.
    pub bad: String,
    /// Policy file (default: `quod.toml` at the repository root).
    pub policy: Option<PathBuf>,
    /// Failing tests.
    pub failed: Vec<String>,
}

/// The Culprit report.
#[derive(Clone, Debug, Serialize)]
pub struct CulpritReport {
    /// Last green commit.
    pub good: String,
    /// Failing commit.
    pub bad: String,
    /// Commits searched, oldest first, with subjects.
    pub commits: Vec<(String, String)>,
    /// Map root.
    pub map: String,
    /// Per failing test.
    pub findings: Vec<quod_verdict::CulpritFinding>,
}

/// Which commit between `good` and `bad` broke each failing test
/// (architecture v5, F5): every first-parent commit's change is evaluated
/// against its parent and intersected with the test's footprint.
pub fn run_culprit(repo: &Repo, opts: &CulpritOptions) -> Result<CulpritReport> {
    use quod_verdict::{CommitEval, lens};

    let bytes =
        std::fs::read(&opts.map).with_context(|| format!("reading {}", opts.map.display()))?;
    let (graph, map_root) = FootprintGraph::decode(&bytes).context("decoding map")?;
    let policy = load_policy(repo, opts.policy.as_deref())?;
    let good = repo.rev_parse(&opts.good)?;
    let bad = repo.rev_parse(&opts.bad)?;
    let diff = DiffOptions {
        split_headers: graph
            .meta()
            .capabilities
            .iter()
            .any(|c| c == CAP_PY_INTROSPECTION),
    };
    // Staleness is handled here (the map-to-green check), not by R007.
    let ctx = EvalContext {
        map_commits_behind: Some(0),
        environment: None,
    };
    let evaluate = |base: &str, head: &str| -> Result<Evaluation> {
        let change = quod_change::compute(
            repo,
            &ChangeOptions {
                base: base.to_owned(),
                head: Some(head.to_owned()),
                python: diff,
            },
        )?;
        Ok(policy.evaluate(&change, &ctx))
    };

    let map_base = graph.meta().base_commit.clone();
    if map_base.is_empty() {
        bail!("map has no base commit");
    }
    if repo.merge_base(&map_base, &good)? != map_base {
        bail!("the map's commit {map_base} is not an ancestor of the last green commit {good}");
    }
    let map_to_green = if map_base == good {
        None
    } else {
        Some(evaluate(&map_base, &good)?)
    };

    let shas = repo.first_parent_commits(&good, &bad)?;
    if shas.is_empty() {
        bail!("no commits between {good} and {bad}");
    }
    let mut evals = Vec::with_capacity(shas.len());
    let mut parent = good.clone();
    for c in &shas {
        evals.push(evaluate(&parent, c)?);
        parent = c.clone();
    }
    let commit_evals: Vec<CommitEval<'_>> = shas
        .iter()
        .zip(&evals)
        .map(|(c, e)| CommitEval {
            commit: c,
            evaluation: e,
        })
        .collect();
    let findings = lens::culprit(&graph, map_to_green.as_ref(), &commit_evals, &opts.failed);
    let commits = shas
        .iter()
        .map(|c| Ok((c.clone(), repo.subject(c)?)))
        .collect::<Result<Vec<_>>>()?;
    Ok(CulpritReport {
        good,
        bad,
        commits,
        map: map_root.to_hex(),
        findings,
    })
}
