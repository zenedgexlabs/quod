//! Skipcert policy.
//!
//! Three jobs, all deterministic and versioned by a policy hash that goes into
//! every certificate:
//!
//! 1. **Global triggers.** Changes the footprint graph can't reason about
//!    (CI config, test config, runtime versions, build inputs, the policy
//!    itself, unparseable lockfiles, a stale map) make the run select
//!    everything.
//! 2. **Change → nodes.** Each changed file maps to the graph nodes whose
//!    observers must run: file nodes for module-level changes, function nodes
//!    for body-only changes, package nodes for lockfile changes.
//! 3. **Per-test rules.** Behaviour flags recorded by the probe (untraced,
//!    dynamic import, network, subprocess, mandatory) force a test to run;
//!    nondeterminism blocks L1 reuse.
//!
//! Test-integrity findings (deleted tests, new skip/xfail markers) are
//! reported for review. They don't change selection: a changed test file
//! already selects its own tests.
//!
//! # Rule catalog v1
//!
//! | ID | Scope | Meaning |
//! |---|---|---|
//! | R001 | global | CI configuration changed |
//! | R002 | global | Test-runner configuration changed (conftest, pytest.ini, pyproject, tox, jest/vitest config) |
//! | R003 | global | Runtime version pin changed (.python-version, .nvmrc, ...) |
//! | R004 | global | Skipcert policy changed |
//! | R005 | global | Build-affecting input changed (setup.py, C/Cython sources, native binaries, .pth, sitecustomize) |
//! | R006 | global | Lockfile unparseable, or requirement options (index, includes, editables) changed |
//! | R007 | global | Footprint map too stale |
//! | R008 | test | Dynamic import / eval with non-constant input observed |
//! | R009 | test | Network contacted without a declared stub |
//! | R010 | test (L1) | Nondeterminism, clock or randomness: no outcome reuse |
//! | R011 | report | Test integrity: test removed, test file deleted, skip/xfail added |
//! | R012 | test | No complete footprint (new or untraced test) |
//! | R013 | test | Subprocess spawned (child process not traced) |
//! | R014 | test | Never-skip (policy glob or marker) |
//! | R015 | global | Path matches the policy's `run_all_paths` |
//! | R016 | test | Native code executed and policy is `native_code = "run"` |

use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

use sc_change::{ChangeSet, FileStatus, LockDelta};
use sc_hash::{Digest, Domain, hash_value};
use sc_model::{Ecosystem, NodeKind, TestFlags, names};

/// Version of the rule catalog. Part of the policy hash.
pub const CATALOG_VERSION: u32 = 1;

/// Errors loading a policy.
#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    /// The policy file is not valid TOML or has unknown fields.
    #[error("invalid policy file: {0}")]
    Parse(String),
    /// A glob pattern is invalid.
    #[error("invalid glob {pattern:?}: {message}")]
    Glob {
        /// The pattern.
        pattern: String,
        /// The error.
        message: String,
    },
    /// Hashing the policy failed.
    #[error(transparent)]
    Hash(#[from] sc_hash::HashError),
}

/// How to treat tests that executed native extension code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeCode {
    /// Trust the package fingerprint: native code is covered by its package
    /// node, but files it reads through C I/O aren't traced. This is the default.
    TrustPackageHash,
    /// Always run tests that executed native code (rule R016).
    Run,
}

/// User-facing policy configuration (`[policy]` in `skipcert.toml`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PolicyConfig {
    /// Configuration schema version.
    pub version: u32,
    /// Maximum number of commits the footprint map may lag behind the base
    /// before everything runs (R007).
    pub max_staleness: u32,
    /// Globs that always trigger a full run when changed (R015).
    pub run_all_paths: Vec<String>,
    /// Globs over test file paths or full test names that are never skipped (R014).
    pub never_skip: Vec<String>,
    /// Globs identifying test files, for integrity findings.
    pub test_paths: Vec<String>,
    /// Native-code handling.
    pub native_code: NativeCode,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            version: 1,
            max_staleness: 100,
            run_all_paths: Vec::new(),
            never_skip: Vec::new(),
            test_paths: vec![
                "**/test_*.py".into(),
                "**/*_test.py".into(),
                "**/tests/**/*.py".into(),
                "**/conftest.py".into(),
            ],
            native_code: NativeCode::TrustPackageHash,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyFile {
    #[serde(default)]
    policy: PolicyConfig,
}

const CI_PATHS: &[&str] = &[
    ".github/workflows/**",
    ".github/actions/**",
    ".gitlab-ci.yml",
    ".gitlab/**",
    ".buildkite/**",
    ".circleci/**",
    "azure-pipelines.yml",
    "Jenkinsfile",
    "bitbucket-pipelines.yml",
];
const TEST_CONFIG_PATHS: &[&str] = &[
    "**/conftest.py",
    "**/pytest.ini",
    "**/pyproject.toml",
    "**/tox.ini",
    "**/setup.cfg",
    "**/noxfile.py",
    "**/jest.config.*",
    "**/vitest.config.*",
];
const RUNTIME_PATHS: &[&str] = &[
    "**/.python-version",
    "**/runtime.txt",
    "**/.tool-versions",
    "**/.nvmrc",
    "**/.node-version",
];
const POLICY_PATHS: &[&str] = &["skipcert.toml", ".skipcert/**"];
const BUILD_PATHS: &[&str] = &[
    "**/setup.py",
    "**/MANIFEST.in",
    "**/*.pyx",
    "**/*.pxd",
    "**/*.c",
    "**/*.cc",
    "**/*.cpp",
    "**/*.h",
    "**/*.hpp",
    "**/Makefile",
    "**/CMakeLists.txt",
    "**/*.pth",
    "**/*.so",
    "**/*.pyd",
    "**/*.dll",
    "**/*.dylib",
    "**/sitecustomize.py",
    "**/usercustomize.py",
];

fn globset(patterns: &[impl AsRef<str>]) -> Result<GlobSet, PolicyError> {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        let p = p.as_ref();
        let g = Glob::new(p).map_err(|e| PolicyError::Glob {
            pattern: p.to_owned(),
            message: e.to_string(),
        })?;
        b.add(g);
    }
    b.build().map_err(|e| PolicyError::Glob {
        pattern: "<set>".into(),
        message: e.to_string(),
    })
}

/// A global trigger: the run must select every test.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trigger {
    /// Rule ID.
    pub rule: &'static str,
    /// Path that triggered it, if any.
    pub path: Option<String>,
    /// Human-readable explanation.
    pub detail: String,
}

/// A test-integrity finding, reported for review.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    /// Rule ID (R011).
    pub rule: &'static str,
    /// Test file.
    pub path: String,
    /// What was found.
    pub detail: String,
}

/// A graph node a change touches.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ChangedNode {
    /// Node kind.
    pub kind: NodeKind,
    /// Canonical node name (see [`sc_model::names`]).
    pub name: String,
    /// For function nodes: the file node to use instead when the graph has
    /// no function-level observations for that file, which means the probe
    /// recorded it at file granularity.
    pub fallback_file: Option<String>,
}

/// Result of evaluating a change.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Evaluation {
    /// Global triggers. If any exist, every test runs.
    pub global: Vec<Trigger>,
    /// Changed nodes, sorted and deduplicated.
    pub changed: Vec<ChangedNode>,
    /// Integrity findings.
    pub integrity: Vec<Finding>,
    /// Global rules evaluated for this change. When `global` is empty, these
    /// all passed and go into certificate evidence.
    pub rules_checked: Vec<&'static str>,
}

impl Evaluation {
    /// True if a global trigger fired.
    pub fn runs_everything(&self) -> bool {
        !self.global.is_empty()
    }
}

/// Context the change set alone doesn't carry.
#[derive(Clone, Copy, Debug, Default)]
pub struct EvalContext {
    /// Commits between the footprint map's base and the change base, if known.
    /// `None` means unknown, which fails closed.
    pub map_commits_behind: Option<u32>,
}

/// Per-test rule outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TestRules {
    /// A rule that forces the test to run, if any.
    pub force_run: Option<&'static str>,
    /// Whether L1 outcome reuse is allowed (false under R010).
    pub l1_allowed: bool,
}

/// A compiled policy.
#[derive(Clone, Debug)]
pub struct Policy {
    config: PolicyConfig,
    hash: Digest,
    ci: GlobSet,
    test_config: GlobSet,
    runtime: GlobSet,
    policy_files: GlobSet,
    build: GlobSet,
    run_all: GlobSet,
    never_skip: GlobSet,
    tests: GlobSet,
}

impl Policy {
    /// Compiles a configuration. Glob lists are sorted and deduplicated
    /// first, so their order doesn't affect the policy hash.
    pub fn from_config(mut config: PolicyConfig) -> Result<Self, PolicyError> {
        for list in [
            &mut config.run_all_paths,
            &mut config.never_skip,
            &mut config.test_paths,
        ] {
            list.sort();
            list.dedup();
        }
        let hash = hash_value(Domain::Policy, &(CATALOG_VERSION, &config))?;
        Ok(Self {
            hash,
            ci: globset(CI_PATHS)?,
            test_config: globset(TEST_CONFIG_PATHS)?,
            runtime: globset(RUNTIME_PATHS)?,
            policy_files: globset(POLICY_PATHS)?,
            build: globset(BUILD_PATHS)?,
            run_all: globset(&config.run_all_paths)?,
            never_skip: globset(&config.never_skip)?,
            tests: globset(&config.test_paths)?,
            config,
        })
    }

    /// Parses `skipcert.toml` text.
    pub fn from_toml(text: &str) -> Result<Self, PolicyError> {
        let file: PolicyFile =
            toml::from_str(text).map_err(|e| PolicyError::Parse(e.to_string()))?;
        Self::from_config(file.policy)
    }

    /// The default policy.
    pub fn default_policy() -> Self {
        Self::from_config(PolicyConfig::default()).expect("default policy is valid")
    }

    /// Policy hash, recorded in certificates.
    pub fn hash(&self) -> Digest {
        self.hash
    }

    /// The configuration.
    pub fn config(&self) -> &PolicyConfig {
        &self.config
    }

    fn global_rule(&self, path: &str) -> Option<(&'static str, &'static str)> {
        if self.policy_files.is_match(path) {
            Some(("R004", "Skipcert policy changed"))
        } else if self.ci.is_match(path) {
            Some(("R001", "CI configuration changed"))
        } else if self.test_config.is_match(path) {
            Some(("R002", "test-runner configuration changed"))
        } else if self.runtime.is_match(path) {
            Some(("R003", "runtime version pin changed"))
        } else if self.build.is_match(path) {
            Some(("R005", "build-affecting input changed"))
        } else if self.run_all.is_match(path) {
            Some(("R015", "path is in run_all_paths"))
        } else {
            None
        }
    }

    /// Evaluates a change set.
    pub fn evaluate(&self, cs: &ChangeSet, ctx: &EvalContext) -> Evaluation {
        let mut ev = Evaluation {
            rules_checked: vec![
                "R001", "R002", "R003", "R004", "R005", "R006", "R007", "R015",
            ],
            ..Evaluation::default()
        };

        match ctx.map_commits_behind {
            Some(n) if n <= self.config.max_staleness => {}
            Some(n) => ev.global.push(Trigger {
                rule: "R007",
                path: None,
                detail: format!(
                    "map is {n} commits behind (max {})",
                    self.config.max_staleness
                ),
            }),
            None => ev.global.push(Trigger {
                rule: "R007",
                path: None,
                detail: "map staleness unknown".into(),
            }),
        }

        for fd in &cs.files {
            let path = fd.change.path.as_str();
            let mut paths = vec![path];
            if let Some(old) = fd.change.old_path.as_deref() {
                paths.push(old);
            }
            for p in &paths {
                if let Some((rule, detail)) = self.global_rule(p) {
                    ev.global.push(Trigger {
                        rule,
                        path: Some((*p).to_owned()),
                        detail: detail.into(),
                    });
                }
            }

            if let Some(lock) = &fd.lock {
                match lock {
                    LockDelta::Unparseable(e) => ev.global.push(Trigger {
                        rule: "R006",
                        path: Some(path.to_owned()),
                        detail: format!("lockfile could not be parsed: {e}"),
                    }),
                    LockDelta::Packages(pk) => {
                        for c in pk {
                            if c.name == "__options__" {
                                ev.global.push(Trigger {
                                    rule: "R006",
                                    path: Some(path.to_owned()),
                                    detail: "requirement options changed".into(),
                                });
                            } else {
                                ev.changed.push(ChangedNode {
                                    kind: NodeKind::ExtPackage,
                                    name: names::package(Ecosystem::Pypi, &c.name),
                                    fallback_file: None,
                                });
                            }
                        }
                    }
                }
                continue;
            }

            match &fd.source {
                Some(src) => {
                    if src.module_changed {
                        for p in &paths {
                            ev.changed.push(file_node(p));
                        }
                    }
                    for e in &src.entities {
                        for p in &paths {
                            ev.changed.push(ChangedNode {
                                kind: NodeKind::Function,
                                name: names::function(p, e),
                                fallback_file: Some(names::file(p)),
                            });
                        }
                    }
                    if self.tests.is_match(path) {
                        self.integrity(path, fd.change.status, Some(src), &mut ev.integrity);
                    }
                }
                None => {
                    for p in &paths {
                        ev.changed.push(file_node(p));
                    }
                }
            }
        }

        ev.changed.sort();
        ev.changed.dedup();
        ev.global
            .sort_by(|a, b| (a.rule, &a.path).cmp(&(b.rule, &b.path)));
        ev.global.dedup();
        ev
    }

    fn integrity(
        &self,
        path: &str,
        status: FileStatus,
        src: Option<&sc_change::SourceDelta>,
        out: &mut Vec<Finding>,
    ) {
        if status == FileStatus::Deleted {
            out.push(Finding {
                rule: "R011",
                path: path.to_owned(),
                detail: "test file deleted".into(),
            });
            return;
        }
        let Some(src) = src else { return };
        for q in &src.removed {
            let leaf = q.rsplit('.').next().unwrap_or(q);
            if leaf.starts_with("test") {
                out.push(Finding {
                    rule: "R011",
                    path: path.to_owned(),
                    detail: format!("test removed: {q}"),
                });
            }
        }
        let (before, after) = src.skip_markers;
        if after > before {
            out.push(Finding {
                rule: "R011",
                path: path.to_owned(),
                detail: format!("skip/xfail markers increased from {before} to {after}"),
            });
        }
    }

    /// Per-test rules from the test's recorded flags and name
    /// (`path::qualified_name[params]`).
    pub fn test_rules(&self, test_name: &str, flags: TestFlags) -> TestRules {
        let file = test_name.split("::").next().unwrap_or(test_name);
        let force_run = if flags.contains(TestFlags::MANDATORY)
            || self.never_skip.is_match(test_name)
            || self.never_skip.is_match(file)
        {
            Some("R014")
        } else if flags.contains(TestFlags::UNTRACED) {
            Some("R012")
        } else if flags.contains(TestFlags::DYNAMIC_IMPORT) {
            Some("R008")
        } else if flags.contains(TestFlags::NETWORK) {
            Some("R009")
        } else if flags.contains(TestFlags::SUBPROCESS) {
            Some("R013")
        } else if flags.contains(TestFlags::C_EXT) && self.config.native_code == NativeCode::Run {
            Some("R016")
        } else {
            None
        };
        let l1_allowed = !(flags.contains(TestFlags::NONDETERMINISTIC)
            || flags.contains(TestFlags::CLOCK)
            || flags.contains(TestFlags::RANDOM));
        TestRules {
            force_run,
            l1_allowed,
        }
    }
}

fn file_node(path: &str) -> ChangedNode {
    ChangedNode {
        kind: NodeKind::File,
        name: names::file(path),
        fallback_file: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_change::{FileChange, FileDetail, PackageChange, PkgPin, SourceDelta};
    use sc_hash::hash_domain;

    fn src(module_changed: bool, entities: &[&str]) -> SourceDelta {
        SourceDelta {
            module_changed,
            entities: entities.iter().map(|s| (*s).to_owned()).collect(),
            parse_error: false,
            removed: vec![],
            skip_markers: (0, 0),
        }
    }

    fn fd(
        path: &str,
        status: FileStatus,
        source: Option<SourceDelta>,
        lock: Option<LockDelta>,
    ) -> FileDetail {
        FileDetail {
            change: FileChange {
                path: path.into(),
                old_path: None,
                status,
            },
            source,
            lock,
        }
    }

    fn cs(files: Vec<FileDetail>) -> ChangeSet {
        ChangeSet {
            base: "b".into(),
            head: "h".into(),
            tree: None,
            files,
        }
    }

    const FRESH: EvalContext = EvalContext {
        map_commits_behind: Some(0),
    };

    #[test]
    fn body_only_change_maps_to_function_nodes_with_fallback() {
        let p = Policy::default_policy();
        let ev = p.evaluate(
            &cs(vec![fd(
                "src/app.py",
                FileStatus::Modified,
                Some(src(false, &["C.m"])),
                None,
            )]),
            &FRESH,
        );
        assert!(!ev.runs_everything());
        assert_eq!(
            ev.changed,
            vec![ChangedNode {
                kind: NodeKind::Function,
                name: "src/app.py::C.m".into(),
                fallback_file: Some("src/app.py".into()),
            }]
        );
    }

    #[test]
    fn formatting_only_change_touches_nothing() {
        let p = Policy::default_policy();
        let ev = p.evaluate(
            &cs(vec![fd(
                "src/app.py",
                FileStatus::Modified,
                Some(src(false, &[])),
                None,
            )]),
            &FRESH,
        );
        assert!(ev.changed.is_empty());
        assert!(!ev.runs_everything());
    }

    #[test]
    fn module_change_and_data_files_map_to_file_nodes() {
        let p = Policy::default_policy();
        let ev = p.evaluate(
            &cs(vec![
                fd(
                    "src/app.py",
                    FileStatus::Modified,
                    Some(src(true, &[])),
                    None,
                ),
                fd("fixtures/data.json", FileStatus::Modified, None, None),
            ]),
            &FRESH,
        );
        let names: Vec<&str> = ev.changed.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["fixtures/data.json", "src/app.py"]);
    }

    #[test]
    fn global_rules_fire() {
        let p = Policy::default_policy();
        for (path, rule) in [
            (".github/workflows/ci.yml", "R001"),
            ("tests/conftest.py", "R002"),
            ("pyproject.toml", "R002"),
            (".python-version", "R003"),
            ("skipcert.toml", "R004"),
            ("ext/fast.c", "R005"),
        ] {
            let ev = p.evaluate(
                &cs(vec![fd(path, FileStatus::Modified, None, None)]),
                &FRESH,
            );
            assert_eq!(ev.global.first().map(|t| t.rule), Some(rule), "{path}");
        }
    }

    #[test]
    fn staleness_unknown_or_too_old_fails_closed() {
        let p = Policy::default_policy();
        let empty = cs(vec![]);
        assert!(
            p.evaluate(
                &empty,
                &EvalContext {
                    map_commits_behind: None
                }
            )
            .runs_everything()
        );
        assert!(
            p.evaluate(
                &empty,
                &EvalContext {
                    map_commits_behind: Some(101)
                }
            )
            .runs_everything()
        );
        assert!(
            !p.evaluate(
                &empty,
                &EvalContext {
                    map_commits_behind: Some(100)
                }
            )
            .runs_everything()
        );
    }

    #[test]
    fn lock_deltas_map_to_packages_or_fail_closed() {
        let p = Policy::default_policy();
        let pin = |v: &str| {
            Some(PkgPin {
                version: v.into(),
                fingerprint: hash_domain(Domain::Blob, v.as_bytes()),
            })
        };
        let bump = LockDelta::Packages(vec![PackageChange {
            name: "requests".into(),
            old: pin("1"),
            new: pin("2"),
        }]);
        let ev = p.evaluate(
            &cs(vec![fd("uv.lock", FileStatus::Modified, None, Some(bump))]),
            &FRESH,
        );
        assert!(!ev.runs_everything());
        assert_eq!(ev.changed[0].name, "pypi:requests");
        assert_eq!(ev.changed[0].kind, NodeKind::ExtPackage);

        let bad = LockDelta::Unparseable("x".into());
        let ev = p.evaluate(
            &cs(vec![fd("uv.lock", FileStatus::Modified, None, Some(bad))]),
            &FRESH,
        );
        assert_eq!(ev.global[0].rule, "R006");

        let opts = LockDelta::Packages(vec![PackageChange {
            name: "__options__".into(),
            old: None,
            new: pin("x"),
        }]);
        let ev = p.evaluate(
            &cs(vec![fd(
                "requirements.txt",
                FileStatus::Modified,
                None,
                Some(opts),
            )]),
            &FRESH,
        );
        assert_eq!(ev.global[0].rule, "R006");
    }

    #[test]
    fn rename_covers_both_paths() {
        let p = Policy::default_policy();
        let mut d = fd(
            "src/new.py",
            FileStatus::Renamed,
            Some(src(true, &[])),
            None,
        );
        d.change.old_path = Some("src/old.py".into());
        let ev = p.evaluate(&cs(vec![d]), &FRESH);
        let names: Vec<&str> = ev.changed.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["src/new.py", "src/old.py"]);
    }

    #[test]
    fn integrity_findings() {
        let p = Policy::default_policy();
        let mut s = src(true, &["test_a", "helper"]);
        s.removed = vec!["test_a".into(), "helper".into()];
        s.skip_markers = (0, 2);
        let ev = p.evaluate(
            &cs(vec![fd(
                "tests/test_x.py",
                FileStatus::Modified,
                Some(s),
                None,
            )]),
            &FRESH,
        );
        let details: Vec<&str> = ev.integrity.iter().map(|f| f.detail.as_str()).collect();
        assert_eq!(
            details,
            vec![
                "test removed: test_a",
                "skip/xfail markers increased from 0 to 2"
            ]
        );

        let ev = p.evaluate(
            &cs(vec![fd(
                "tests/test_y.py",
                FileStatus::Deleted,
                Some(src(true, &[])),
                None,
            )]),
            &FRESH,
        );
        assert_eq!(ev.integrity[0].detail, "test file deleted");
    }

    #[test]
    fn per_test_rules() {
        let p = Policy::from_toml("[policy]\nnever_skip = [\"tests/smoke/**\"]\n").unwrap();
        assert_eq!(
            p.test_rules("tests/smoke/test_up.py::test_up", TestFlags::default())
                .force_run,
            Some("R014")
        );
        assert_eq!(
            p.test_rules("tests/t.py::test_a", TestFlags::NETWORK)
                .force_run,
            Some("R009")
        );
        assert_eq!(
            p.test_rules("tests/t.py::test_a", TestFlags::UNTRACED)
                .force_run,
            Some("R012")
        );
        assert_eq!(
            p.test_rules("tests/t.py::test_a", TestFlags::SUBPROCESS)
                .force_run,
            Some("R013")
        );
        let clock = p.test_rules("tests/t.py::test_a", TestFlags::CLOCK);
        assert_eq!(clock.force_run, None);
        assert!(!clock.l1_allowed);
        assert_eq!(
            p.test_rules("tests/t.py::test_a", TestFlags::C_EXT)
                .force_run,
            None
        );

        let strict = Policy::from_toml("[policy]\nnative_code = \"run\"\n").unwrap();
        assert_eq!(
            strict
                .test_rules("tests/t.py::test_a", TestFlags::C_EXT)
                .force_run,
            Some("R016")
        );
    }

    #[test]
    fn policy_hash_is_order_insensitive_and_content_sensitive() {
        let a = Policy::from_toml("[policy]\nrun_all_paths = [\"a/**\", \"b/**\"]\n").unwrap();
        let b = Policy::from_toml("[policy]\nrun_all_paths = [\"b/**\", \"a/**\"]\n").unwrap();
        let c = Policy::from_toml("[policy]\nrun_all_paths = [\"a/**\"]\n").unwrap();
        assert_eq!(a.hash(), b.hash());
        assert_ne!(a.hash(), c.hash());
        assert!(
            Policy::from_toml("[policy]\nbogus = 1\n").is_err(),
            "unknown fields are rejected"
        );
    }
}
