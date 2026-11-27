//! The Quod skip ladder.
//!
//! [`select`] decides, for every test in the footprint map, whether it must
//! run or may be skipped, and mints a sealed certificate for each skip. The
//! output is a *skip list*: callers run every test that isn't on it. So a
//! test that is new, renamed, or missing from the map always runs (R012),
//! without the caller having to know that.
//!
//! The M1 ladder implements L0 (footprint disjointness) and L3 (run). L1
//! (outcome reuse) and L2 (witnesses) plug in later behind the same output
//! shape.
//!
//! Order of checks per test, first match wins:
//! 1. a global policy trigger, or a changed node in the root scope (R017),
//!    means run;
//! 2. a per-test rule (R008, R009, R012, R013, R014, R016) means run;
//! 3. the change intersects the test's footprint means run;
//! 4. otherwise, skip with a `disjoint` certificate.

use std::collections::{BTreeSet, HashMap, HashSet};

use quod_change::{ChangeSet, LockDelta};
use quod_graph::{FootprintGraph, NodeId, TestIdx, kind_tag};
use quod_hash::{Digest, Domain, hash_value};
use quod_model::{
    CertKind, Certificate, CertificateBody, Evidence, ModelError, NodeKind, PinnedInputs, SkipType,
    StableTestId, Traceability, version::CERT_VERSION,
};
use quod_policy::{ChangedNode, Evaluation, Policy};

/// Rule ID used when a changed node was observed in the root scope.
pub const ROOT_SCOPE_RULE: &str = "R017";

/// Per-test rules the ladder checks, recorded in certificate evidence.
const PER_TEST_RULES: [&str; 6] = ["R008", "R009", "R012", "R013", "R014", "R016"];

/// Errors from selection.
#[derive(Debug, thiserror::Error)]
pub enum LadderError {
    /// Certificate construction failed.
    #[error(transparent)]
    Model(#[from] ModelError),
    /// Hashing failed.
    #[error(transparent)]
    Hash(#[from] quod_hash::HashError),
}

/// Why a test must run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunReason {
    /// A global policy trigger fired.
    Global(&'static str),
    /// A per-test rule fired.
    Rule(&'static str),
    /// The change touched nodes in the test's footprint (canonical refs).
    Affected(Vec<String>),
}

/// Everything selection needs besides the policy outputs.
#[derive(Clone, Debug)]
pub struct SelectInput<'a> {
    /// Repository identifier.
    pub repo: &'a str,
    /// Engine version recorded in certificates.
    pub engine: &'a str,
    /// The footprint map.
    pub graph: &'a FootprintGraph,
    /// Merkle root of the map.
    pub map_digest: Digest,
    /// The compiled policy.
    pub policy: &'a Policy,
    /// The policy's evaluation of the change.
    pub evaluation: &'a Evaluation,
    /// The resolved change.
    pub change: &'a ChangeSet,
    /// True outside a trusted CI identity (sandbox, local). Advisory
    /// certificates never license a skip in CI (ADR-004).
    pub advisory: bool,
    /// Environment mode: the map's environment binding (ADR 012).
    pub environment: Option<Digest>,
}

/// The selection result.
#[derive(Clone, Debug)]
pub struct Selection {
    /// Tests in the map that must run, with reasons, in canonical order.
    pub run: Vec<(TestIdx, RunReason)>,
    /// Tests that may be skipped, each with its sealed certificate (chained
    /// in this order).
    pub skipped: Vec<(TestIdx, Certificate)>,
    /// Canonical refs of the changed nodes after resolution against the map.
    pub changed_refs: Vec<String>,
}

impl Selection {
    /// Stable IDs of skipped tests. Callers run every collected test whose ID
    /// is not in this set.
    pub fn skip_set(&self) -> HashSet<StableTestId> {
        self.skipped.iter().map(|(_, c)| c.body.test).collect()
    }
}

/// Resolves policy-level changed nodes to graph nodes and evidence refs.
///
/// Returns the graph nodes to intersect with footprints, plus the refs
/// recorded as evidence. A changed entity missing from the graph was observed
/// by no test, so it contributes a ref but no node.
pub fn resolve_changes(
    graph: &FootprintGraph,
    changed: &[ChangedNode],
) -> (Vec<NodeId>, Vec<String>) {
    // Files that have function-level observations. For those, an absent
    // function means no test executed it.
    let mut fn_files: HashSet<NodeId> = HashSet::new();
    // Function nodes per file. A function's globals are its module's
    // namespace, so a test that ran any function of a file depends on the
    // file's module-level code, even if the module body ran earlier, in
    // another test's scope (imports are cached).
    let mut fns_of: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    for i in 0..graph.node_count() as u32 {
        let node = graph.node(NodeId(i));
        if node.kind == NodeKind::Function {
            if let Some(p) = node.parent {
                fn_files.insert(NodeId(p));
                fns_of.entry(NodeId(p)).or_default().push(NodeId(i));
            }
        }
    }

    let mut nodes = BTreeSet::new();
    let mut refs = BTreeSet::new();
    for c in changed {
        match graph.find_node(c.kind, &c.name) {
            Some(n) => {
                nodes.insert(n);
                refs.insert(graph.node_ref(n));
                if c.kind == NodeKind::File {
                    nodes.extend(fns_of.get(&n).into_iter().flatten().copied());
                }
            }
            None => match (&c.kind, &c.fallback_file) {
                (NodeKind::Function, Some(file)) => match graph.find_node(NodeKind::File, file) {
                    // The file is known only at file granularity: use the file node.
                    Some(f) if !fn_files.contains(&f) => {
                        nodes.insert(f);
                        refs.insert(graph.node_ref(f));
                    }
                    // Function-level coverage exists and nobody executed it, or the
                    // file was never observed.
                    _ => {
                        refs.insert(format!("{}:{}", kind_tag(c.kind), c.name));
                    }
                },
                _ => {
                    refs.insert(format!("{}:{}", kind_tag(c.kind), c.name));
                }
            },
        }
    }
    (nodes.into_iter().collect(), refs.into_iter().collect())
}

fn lock_digest(change: &ChangeSet) -> Result<Option<Digest>, LadderError> {
    let mut entries: Vec<(String, String, Option<Digest>, Option<Digest>)> = Vec::new();
    let mut any = false;
    for f in &change.files {
        match &f.lock {
            Some(LockDelta::Packages(pk)) => {
                any = true;
                for c in pk {
                    entries.push((
                        f.change.path.clone(),
                        c.name.clone(),
                        c.old.as_ref().map(|p| p.fingerprint),
                        c.new.as_ref().map(|p| p.fingerprint),
                    ));
                }
            }
            Some(LockDelta::Unparseable(_)) => {
                any = true;
                entries.push((f.change.path.clone(), "<unparseable>".into(), None, None));
            }
            None => {}
        }
    }
    if !any {
        return Ok(None);
    }
    entries.sort();
    Ok(Some(hash_value(Domain::Blob, &entries)?))
}

/// Runs the ladder.
pub fn select(input: &SelectInput<'_>) -> Result<Selection, LadderError> {
    let g = input.graph;
    let (changed_nodes, changed_refs) = resolve_changes(g, &input.evaluation.changed);
    let affected = g.affected(&changed_nodes);

    let global: Option<&'static str> = input
        .evaluation
        .global
        .first()
        .map(|t| t.rule)
        .or(affected.root_hit.then_some(ROOT_SCOPE_RULE));

    let inputs = PinnedInputs {
        repo: input.repo.to_owned(),
        base: input.change.base.clone(),
        head: input.change.head.clone(),
        tree: input.change.tree,
        map: input.map_digest,
        policy: input.policy.hash(),
        engine: input.engine.to_owned(),
        lock_delta: lock_digest(input.change)?,
        environment: input.environment,
    };
    let mut rules_passed: Vec<String> = input
        .evaluation
        .rules_checked
        .iter()
        .chain(PER_TEST_RULES.iter())
        .chain([ROOT_SCOPE_RULE].iter())
        .map(|s| (*s).to_owned())
        .collect();
    rules_passed.sort();
    rules_passed.dedup();

    let mut run = Vec::new();
    let mut skipped: Vec<(TestIdx, Certificate)> = Vec::new();
    let mut prev: Option<Digest> = None;

    for i in 0..g.test_count() as u32 {
        let t = TestIdx(i);
        if let Some(rule) = global {
            run.push((t, RunReason::Global(rule)));
            continue;
        }
        let entry = g.test(t);
        let name = g.test_name(t);
        if let Some(rule) = input.policy.test_rules(name, entry.flags).force_run {
            run.push((t, RunReason::Rule(rule)));
            continue;
        }
        if affected.tests.contains(t) {
            let changed: HashSet<NodeId> = changed_nodes.iter().copied().collect();
            let via = g
                .footprint(t)
                .filter(|n| changed.contains(n))
                .map(|n| g.node_ref(n))
                .collect();
            run.push((t, RunReason::Affected(via)));
            continue;
        }

        let body = CertificateBody {
            v: CERT_VERSION,
            kind: CertKind::Skip,
            skip_type: Some(SkipType::Disjoint),
            test: entry.id,
            test_name: name.to_owned(),
            inputs: inputs.clone(),
            evidence: Evidence {
                changed_nodes: changed_refs.clone(),
                intersection: Vec::new(),
                outcome_ref: None,
                rules_passed: rules_passed.clone(),
            },
            trace: Traceability::default(),
            advisory: input.advisory,
            prev,
        };
        let cert = Certificate::seal(body)?;
        prev = Some(cert.self_hash);
        skipped.push((t, cert));
    }

    Ok(Selection {
        run,
        skipped,
        changed_refs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use quod_change::{FileChange, FileDetail, FileStatus, SourceDelta};
    use quod_graph::{GraphBuilder, MapMeta};
    use quod_hash::hash_domain;
    use quod_model::{TestFlags, names, verify_chain};
    use quod_policy::{EvalContext, Trigger};

    /// Three tests:
    /// - `test_pay` calls `src/billing.py::charge`
    /// - `test_util` imports `src/util.py` (file-level only)
    /// - `test_net` touches nothing relevant but uses the network
    ///
    /// `tests/conftest.py` is root scope.
    fn graph() -> (FootprintGraph, Digest) {
        let mut b = GraphBuilder::new(MapMeta {
            repo: "acme/app".into(),
            ..MapMeta::default()
        });
        let h = |s: &str| Some(hash_domain(Domain::Blob, s.as_bytes()));
        let billing = b.node(NodeKind::File, "src/billing.py", h("b"), None);
        let charge = b.node(
            NodeKind::Function,
            &names::function("src/billing.py", "charge"),
            h("c"),
            Some(billing),
        );
        let util = b.node(NodeKind::File, "src/util.py", h("u"), None);
        let conftest = b.node(NodeKind::File, "tests/conftest.py", h("cf"), None);
        let pkg = b.node(NodeKind::ExtPackage, "pypi:requests", h("r"), None);
        b.observe_root(conftest);

        let t1 = b.test(
            StableTestId::derive("tests/test_pay.py", "test_pay", ""),
            "tests/test_pay.py::test_pay",
            TestFlags::default(),
            5,
        );
        b.observe(t1, billing);
        b.observe(t1, charge);
        b.observe(t1, pkg);
        let t2 = b.test(
            StableTestId::derive("tests/test_util.py", "test_util", ""),
            "tests/test_util.py::test_util",
            TestFlags::default(),
            5,
        );
        b.observe(t2, util);
        let t3 = b.test(
            StableTestId::derive("tests/test_net.py", "test_net", ""),
            "tests/test_net.py::test_net",
            TestFlags::NETWORK,
            5,
        );
        b.observe(t3, util);
        let g = b.build();
        let (_, root) = g.encode().unwrap();
        (g, root)
    }

    fn change(files: Vec<FileDetail>) -> ChangeSet {
        ChangeSet {
            base: "base".into(),
            head: "head".into(),
            tree: None,
            files,
        }
    }

    fn py(path: &str, module_changed: bool, entities: &[&str]) -> FileDetail {
        FileDetail {
            change: FileChange {
                path: path.into(),
                old_path: None,
                status: FileStatus::Modified,
            },
            source: Some(SourceDelta {
                module_changed,
                entities: entities.iter().map(|s| (*s).to_owned()).collect(),
                parse_error: false,
                removed: vec![],
                skip_markers: (0, 0),
                added: vec![],
            }),
            lock: None,
            config: None,
        }
    }

    fn run_select(files: Vec<FileDetail>, advisory: bool) -> (FootprintGraph, Selection) {
        let (g, root) = graph();
        let policy = Policy::default_policy();
        let cs = change(files);
        let ev = policy.evaluate(
            &cs,
            &EvalContext {
                map_commits_behind: Some(0),
                environment: None,
                transpile_only: false,
            },
        );
        let sel = select(&SelectInput {
            repo: "acme/app",
            engine: "0.1.0",
            graph: &g,
            map_digest: root,
            policy: &policy,
            evaluation: &ev,
            change: &cs,
            advisory,
            environment: None,
        })
        .unwrap();
        (g, sel)
    }

    fn names_of(g: &FootprintGraph, ts: impl Iterator<Item = TestIdx>) -> Vec<String> {
        let mut v: Vec<String> = ts.map(|t| g.test_name(t).to_owned()).collect();
        v.sort();
        v
    }

    /// `test_b` ran `util.f` but not util's module body: that ran once, in
    /// `test_a`'s scope (imports are cached). `f` reads module globals, so a
    /// module-level change must still run `test_b`.
    #[test]
    fn module_change_runs_callers_of_its_functions() {
        let mut b = GraphBuilder::new(MapMeta {
            repo: "acme/app".into(),
            ..MapMeta::default()
        });
        let h = |s: &str| Some(hash_domain(Domain::Blob, s.as_bytes()));
        let util = b.node(NodeKind::File, "src/util.py", h("u"), None);
        let f = b.node(
            NodeKind::Function,
            &names::function("src/util.py", "f"),
            h("f"),
            Some(util),
        );
        let ta = b.test(
            StableTestId::derive("tests/test_a.py", "test_a", ""),
            "tests/test_a.py::test_a",
            TestFlags::default(),
            5,
        );
        b.observe(ta, util);
        b.observe(ta, f);
        let tb = b.test(
            StableTestId::derive("tests/test_b.py", "test_b", ""),
            "tests/test_b.py::test_b",
            TestFlags::default(),
            5,
        );
        b.observe(tb, f);
        let g = b.build();
        let (_, root) = g.encode().unwrap();
        let policy = Policy::default_policy();
        let cs = change(vec![py("src/util.py", true, &[])]);
        let ev = policy.evaluate(
            &cs,
            &EvalContext {
                map_commits_behind: Some(0),
                environment: None,
                transpile_only: false,
            },
        );
        let sel = select(&SelectInput {
            repo: "acme/app",
            engine: "0.1.0",
            graph: &g,
            map_digest: root,
            policy: &policy,
            evaluation: &ev,
            change: &cs,
            advisory: false,
            environment: None,
        })
        .unwrap();
        assert_eq!(
            names_of(&g, sel.run.iter().map(|r| r.0)),
            vec!["tests/test_a.py::test_a", "tests/test_b.py::test_b"]
        );
    }

    #[test]
    fn function_change_runs_only_its_callers() {
        let (g, sel) = run_select(vec![py("src/billing.py", false, &["charge"])], false);
        assert_eq!(
            names_of(&g, sel.run.iter().map(|r| r.0)),
            vec!["tests/test_net.py::test_net", "tests/test_pay.py::test_pay"]
        );
        assert_eq!(
            names_of(&g, sel.skipped.iter().map(|s| s.0)),
            vec!["tests/test_util.py::test_util"]
        );
        let pay = sel
            .run
            .iter()
            .find(|(t, _)| g.test_name(*t).contains("test_pay"))
            .unwrap();
        assert_eq!(
            pay.1,
            RunReason::Affected(vec!["fn:src/billing.py::charge".into()])
        );
        let net = sel
            .run
            .iter()
            .find(|(t, _)| g.test_name(*t).contains("test_net"))
            .unwrap();
        assert_eq!(net.1, RunReason::Rule("R009"));
    }

    #[test]
    fn unexecuted_function_in_function_level_file_affects_nobody() {
        let (_, sel) = run_select(vec![py("src/billing.py", false, &["refund"])], false);
        assert_eq!(sel.skipped.len(), 2, "only the network test runs");
        assert!(
            sel.changed_refs
                .contains(&"fn:src/billing.py::refund".to_string())
        );
    }

    #[test]
    fn function_change_in_file_level_file_falls_back_to_file() {
        let (g, sel) = run_select(vec![py("src/util.py", false, &["helper"])], false);
        assert_eq!(
            names_of(&g, sel.run.iter().map(|r| r.0)),
            vec![
                "tests/test_net.py::test_net",
                "tests/test_util.py::test_util"
            ]
        );
    }

    #[test]
    fn root_scope_change_runs_everything() {
        let mut f = py("tests/conftest.py", true, &[]);
        f.source = None; // treat as a plain file so only R017 (not R002) can fire
        let (g, root) = graph();
        let policy = Policy::from_toml("[policy]\ntest_paths = []\n").unwrap();
        // Evaluate with the conftest change, but drop policy globals to isolate R017.
        let cs = change(vec![f]);
        let mut ev = policy.evaluate(
            &cs,
            &EvalContext {
                map_commits_behind: Some(0),
                environment: None,
                transpile_only: false,
            },
        );
        ev.global.clear();
        let sel = select(&SelectInput {
            repo: "r",
            engine: "e",
            graph: &g,
            map_digest: root,
            policy: &policy,
            evaluation: &ev,
            change: &cs,
            advisory: false,
            environment: None,
        })
        .unwrap();
        assert!(sel.skipped.is_empty());
        assert!(
            sel.run
                .iter()
                .all(|(_, r)| *r == RunReason::Global(ROOT_SCOPE_RULE))
        );
    }

    #[test]
    fn global_trigger_runs_everything() {
        let (g, root) = graph();
        let policy = Policy::default_policy();
        let cs = change(vec![]);
        let mut ev = policy.evaluate(
            &cs,
            &EvalContext {
                map_commits_behind: Some(0),
                environment: None,
                transpile_only: false,
            },
        );
        ev.global.push(Trigger {
            rule: "R001",
            path: None,
            detail: String::new(),
        });
        let sel = select(&SelectInput {
            repo: "r",
            engine: "e",
            graph: &g,
            map_digest: root,
            policy: &policy,
            evaluation: &ev,
            change: &cs,
            advisory: false,
            environment: None,
        })
        .unwrap();
        assert!(sel.skipped.is_empty());
        assert_eq!(sel.run.len(), 3);
    }

    #[test]
    fn dependency_bump_runs_only_package_users() {
        let pin = |v: &str| {
            Some(quod_change::PkgPin {
                version: v.into(),
                fingerprint: hash_domain(Domain::Blob, v.as_bytes()),
            })
        };
        let lock = FileDetail {
            change: FileChange {
                path: "uv.lock".into(),
                old_path: None,
                status: FileStatus::Modified,
            },
            source: None,
            lock: Some(LockDelta::Packages(vec![quod_change::PackageChange {
                name: "requests".into(),
                old: pin("1"),
                new: pin("2"),
            }])),
            config: None,
        };
        let (g, sel) = run_select(vec![lock], false);
        assert_eq!(
            names_of(&g, sel.run.iter().map(|r| r.0)),
            vec!["tests/test_net.py::test_net", "tests/test_pay.py::test_pay"]
        );
        assert!(sel.skipped[0].1.body.inputs.lock_delta.is_some());
    }

    #[test]
    fn certificates_are_valid_chained_and_disjoint() {
        let (g, sel) = run_select(vec![py("src/billing.py", false, &["charge"])], false);
        let certs: Vec<Certificate> = sel.skipped.iter().map(|s| s.1.clone()).collect();
        verify_chain(&certs).unwrap();
        for (t, c) in &sel.skipped {
            assert!(c.licenses_skip());
            assert_eq!(c.body.skip_type, Some(SkipType::Disjoint));
            let fp: HashSet<String> = g.footprint(*t).map(|n| g.node_ref(n)).collect();
            assert!(
                c.body
                    .evidence
                    .changed_nodes
                    .iter()
                    .all(|r| !fp.contains(r)),
                "evidence must be disjoint from footprint"
            );
        }
    }

    #[test]
    fn advisory_certificates_do_not_license_skips() {
        let (_, sel) = run_select(vec![py("src/billing.py", false, &["charge"])], true);
        assert!(!sel.skipped.is_empty());
        assert!(sel.skipped.iter().all(|(_, c)| !c.licenses_skip()));
    }

    #[test]
    fn more_change_never_runs_fewer_tests() {
        // I3, monotonic widening: for every pair of change sets A ⊆ B, run(A) ⊆ run(B).
        let options = [
            py("src/billing.py", false, &["charge"]),
            py("src/billing.py", false, &["refund"]),
            py("src/util.py", true, &[]),
            py("README.md", true, &[]),
        ];
        let n = options.len();
        let run_set = |mask: usize| -> HashSet<String> {
            let files = (0..n)
                .filter(|i| mask & (1 << i) != 0)
                .map(|i| options[i].clone())
                .collect();
            let (g, sel) = run_select(files, false);
            sel.run
                .iter()
                .map(|(t, _)| g.test_name(*t).to_owned())
                .collect()
        };
        for a in 0..(1 << n) {
            for b in 0..(1 << n) {
                if a & b == a {
                    assert!(run_set(a).is_subset(&run_set(b)), "masks {a:#b} ⊆ {b:#b}");
                }
            }
        }
    }
}
