//! Lenses: pure analyses over the evidence graph, the change and the policy
//! evaluation (architecture v5, §4.3). Same inputs, same findings, so the
//! verifier can re-derive any finding a Verdict relies on.

use std::collections::{BTreeSet, HashMap, HashSet};

use quod_change::{ChangeSet, FileStatus};
use quod_graph::{FootprintGraph, NodeId, TestIdx};
use quod_ladder::resolve_changes;
use quod_model::{NodeKind, TestFlags, names};
use quod_policy::{ChangedNode, Evaluation, Policy};
use serde::{Deserialize, Serialize};

/// What a finding is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Changed code that no test executes.
    UntestedChange,
    /// Tests weakened, removed, or edited together with the code they guard.
    Integrity,
    /// A failure this change provably did not cause.
    FlakeProof,
    /// An obligated test did not run (or was skipped by the runner).
    Obligation,
}

/// How much a finding matters. Ordered: `Block` is the most severe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// For information.
    Info,
    /// Worth a look.
    Warn,
    /// Needs a human reviewer's attention.
    Review,
    /// The Verdict can't pass.
    Block,
}

/// One finding.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Finding {
    /// What it is about.
    pub kind: Kind,
    /// How much it matters.
    pub severity: Severity,
    /// Rule ID.
    pub rule: String,
    /// The test, function or path concerned.
    pub subject: String,
    /// Human-readable explanation.
    pub detail: String,
}

/// Rule: changed entity that no test executes.
pub const U001: &str = "U001";
/// Rule: test edited together with the code it guards.
pub const I001: &str = "I001";
/// Rule: failure not caused by this change (nothing in its footprint changed).
pub const F001: &str = "F001";
/// Rule: obligated test did not run.
pub const O001: &str = "O001";
/// Rule: obligated test was skipped by the test runner.
pub const O002: &str = "O002";

/// Everything a lens may read. All of it is pinned by the Verdict's inputs.
pub struct LensInput<'a> {
    /// Footprint map at the base.
    pub graph: &'a FootprintGraph,
    /// Policy in force.
    pub policy: &'a Policy,
    /// The resolved change.
    pub change: &'a ChangeSet,
    /// The policy evaluation of the change (including untracked changes).
    pub evaluation: &'a Evaluation,
    /// True if the environment was fingerprinted and compared (ADR 012).
    pub environment_checked: bool,
}

/// Files that have function-level observations.
fn function_level_files(g: &FootprintGraph) -> HashSet<NodeId> {
    (0..g.node_count() as u32)
        .map(NodeId)
        .filter_map(|n| {
            let node = g.node(n);
            (node.kind == NodeKind::Function)
                .then_some(node.parent)
                .flatten()
                .map(NodeId)
        })
        .collect()
}

fn executed(g: &FootprintGraph, n: NodeId) -> bool {
    g.is_root(n) || g.observers(n).next().is_some()
}

/// **Untested-change lens.** Changed (added or modified) entities in source
/// files that no recorded test executes.
///
/// Claims are about the base map: a test added or edited by the same change
/// may cover the entity, which the detail says when test files changed.
/// Entities in files recorded only at file granularity are not claimed.
pub fn untested_changes(inp: &LensInput<'_>) -> Vec<Finding> {
    let g = inp.graph;
    let fn_files = function_level_files(g);
    let tests_changed = inp
        .change
        .files
        .iter()
        .any(|f| inp.policy.is_test_path(&f.change.path));
    let mut out = Vec::new();
    for fd in &inp.change.files {
        let path = &fd.change.path;
        if fd.change.status == FileStatus::Deleted
            || inp.policy.is_test_path(path)
            || fd.lock.is_some()
            || fd.config.is_some()
        {
            continue;
        }
        let Some(src) = &fd.source else { continue };
        if src.parse_error {
            continue;
        }
        let removed: HashSet<&String> = src.removed.iter().collect();
        let file = g.find_node(NodeKind::File, &names::file(path));
        for q in src.entities.iter().filter(|q| !removed.contains(q)) {
            let name = names::function(path, q);
            let covered = match g.find_node(NodeKind::Function, &name) {
                Some(n) => executed(g, n),
                None => match file {
                    // Known only at file granularity: no claim.
                    Some(f) if !fn_files.contains(&f) => true,
                    // Function-level coverage exists and nobody executed it,
                    // or no test loaded the file at all.
                    _ => false,
                },
            };
            if !covered {
                out.push(Finding {
                    kind: Kind::UntestedChange,
                    severity: Severity::Warn,
                    rule: U001.into(),
                    subject: name,
                    detail: if tests_changed {
                        "no recorded test executes it (tests changed in this change may; they are not recorded yet)".into()
                    } else {
                        "no test executes it".into()
                    },
                });
            }
        }
    }
    out.sort();
    out
}

/// The changed nodes outside test files: what tests guard.
fn guarded_changes(inp: &LensInput<'_>) -> Vec<ChangedNode> {
    inp.evaluation
        .changed
        .iter()
        .filter(|c| {
            let path = match c.kind {
                NodeKind::Function => c.fallback_file.as_deref(),
                NodeKind::File => Some(c.name.as_str()),
                _ => None,
            };
            !path.is_some_and(|p| inp.policy.is_test_path(p))
        })
        .cloned()
        .collect()
}

/// A test name without its parameters: `path::qual`.
fn base_name(name: &str) -> &str {
    // Parameters may contain anything, including `::`; paths and qualified
    // names don't contain `[`.
    match (name.ends_with(']'), name.find('[')) {
        (true, Some(i)) => &name[..i],
        _ => name,
    }
}

/// **Integrity lens.** The policy's test-integrity findings (R011: tests
/// removed, test files deleted, skip markers added), plus tests whose own
/// code changed while code they execute changed too (I001): the edit may
/// weaken the very test that guards the change.
pub fn integrity(inp: &LensInput<'_>) -> Vec<Finding> {
    let g = inp.graph;
    let mut out: Vec<Finding> = inp
        .evaluation
        .integrity
        .iter()
        .map(|f| Finding {
            kind: Kind::Integrity,
            severity: Severity::Review,
            rule: f.rule.into(),
            subject: f.path.clone(),
            detail: f.detail.clone(),
        })
        .collect();

    // Tests whose own definition changed, by `path::qual`.
    let mut edited: HashSet<String> = HashSet::new();
    for fd in &inp.change.files {
        let path = &fd.change.path;
        if fd.change.status != FileStatus::Modified || !inp.policy.is_test_path(path) {
            continue;
        }
        let Some(src) = &fd.source else { continue };
        let removed: HashSet<&String> = src.removed.iter().collect();
        for q in src.entities.iter().filter(|q| !removed.contains(q)) {
            edited.insert(format!("{path}::{}", q.replace('.', "::")));
        }
    }
    if edited.is_empty() {
        out.sort();
        return out;
    }
    let (guard, _) = resolve_changes(g, &guarded_changes(inp));
    let guard: HashSet<NodeId> = guard.into_iter().collect();
    for i in 0..g.test_count() as u32 {
        let t = TestIdx(i);
        let name = g.test_name(t);
        if !edited.contains(base_name(name)) {
            continue;
        }
        let shared: BTreeSet<String> = g
            .footprint(t)
            .filter(|n| guard.contains(n))
            .map(|n| g.node_ref(n))
            .collect();
        if shared.is_empty() {
            continue;
        }
        let mut via: Vec<&str> = shared.iter().map(String::as_str).take(3).collect();
        if shared.len() > 3 {
            via.push("…");
        }
        out.push(Finding {
            kind: Kind::Integrity,
            severity: Severity::Review,
            rule: I001.into(),
            subject: name.to_owned(),
            detail: format!(
                "test edited together with code it executes ({})",
                via.join(", ")
            ),
        });
    }
    out.sort();
    out
}

/// Flags that make a footprint incomplete: a change outside it could still
/// reach the test.
const INCOMPLETE: TestFlags = TestFlags(
    TestFlags::UNTRACED.0
        | TestFlags::DYNAMIC_IMPORT.0
        | TestFlags::NETWORK.0
        | TestFlags::SUBPROCESS.0,
);

/// **Flake-proof lens.** For each failed test: if nothing in its recorded
/// footprint changed, the failure was not caused by this change (F001).
///
/// No claim is made when a global rule fired, when a changed node was
/// observed in the root scope, for tests not in the map, for tests whose
/// file changed, or for tests with incomplete footprints. A failure the lens
/// doesn't clear stays a real failure.
pub fn flake_proof(inp: &LensInput<'_>, failed: &[String]) -> Vec<Finding> {
    let g = inp.graph;
    if inp.evaluation.runs_everything() || failed.is_empty() {
        return Vec::new();
    }
    let (changed, _) = resolve_changes(g, &inp.evaluation.changed);
    if changed.iter().any(|n| g.is_root(*n)) {
        return Vec::new();
    }
    let changed: HashSet<NodeId> = changed.into_iter().collect();
    let changed_tests: HashSet<&str> = inp
        .change
        .files
        .iter()
        .filter(|f| inp.policy.is_test_path(&f.change.path))
        .map(|f| f.change.path.as_str())
        .collect();
    let index: HashMap<&str, TestIdx> = (0..g.test_count() as u32)
        .map(|i| (g.test_name(TestIdx(i)), TestIdx(i)))
        .collect();
    let detail = if inp.environment_checked {
        "nothing this test depends on changed (code, packages, environment): this change did not cause the failure"
    } else {
        "no code or package this test depends on changed (environment not fingerprinted): this change's code did not cause the failure"
    };
    let mut out = Vec::new();
    for name in failed {
        let Some(&t) = index.get(name.as_str()) else {
            continue;
        };
        let flags = g.test(t).flags;
        if flags.0 & INCOMPLETE.0 != 0 {
            continue;
        }
        let file = name.split("::").next().unwrap_or(name);
        if changed_tests.contains(file) {
            continue;
        }
        if g.footprint(t).any(|n| changed.contains(&n)) {
            continue;
        }
        out.push(Finding {
            kind: Kind::FlakeProof,
            severity: Severity::Info,
            rule: F001.into(),
            subject: name.clone(),
            detail: detail.into(),
        });
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::base_name;

    #[test]
    fn base_names() {
        assert_eq!(base_name("t/a.py::test_x"), "t/a.py::test_x");
        assert_eq!(base_name("t/a.py::test_x[1-2]"), "t/a.py::test_x");
        assert_eq!(base_name("t/a.py::C::test_x[a::b]"), "t/a.py::C::test_x");
    }
}
