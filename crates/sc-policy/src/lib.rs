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
//! | R001 | global | CI configuration that runs the tests changed (single-file CI configs, composite actions, GitHub workflows matching `test_workflows`) |
//! | R002 | global | Test-runner configuration changed: conftest.py anywhere, root noxfile.py, jest/vitest config, or a test-relevant section of the root pyproject.toml / setup.cfg / tox.ini / pytest.ini |
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
//! | R017 | global (ladder) | A changed node was observed in the root scope (conftest, plugins, startup) |
//! | R018 | global (environment mode) | The interpreter, system libraries or process-level environment differ from the recorded fingerprint (ADR 012) |

use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

use sc_change::{ChangeSet, ConfigDelta, FileStatus, LockDelta};
use sc_hash::{Digest, Domain, hash_value};
use sc_model::{Ecosystem, NodeKind, TestFlags, names};

/// Version of the rule catalog. Part of the policy hash.
pub const CATALOG_VERSION: u32 = 2;

/// Pseudo-package the probe uses for installed files no distribution claims
/// (must match `skipcert_probe.packages.SITE_PSEUDO_PACKAGE`).
pub const SITE_PSEUDO_PACKAGE: &str = "__site__";

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
    /// GitHub workflow globs that run the tests. Changes to other workflows
    /// (publishing, docs, security scanners) can't change test outcomes in the
    /// job where selection runs, so they don't trigger R001.
    pub test_workflows: Vec<String>,
    /// Extra configuration sections to treat as irrelevant to test outcomes,
    /// on top of the built-in list (linters, formatters, docs metadata).
    /// Entries ending in `*` match by prefix.
    pub irrelevant_config_sections: Vec<String>,
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
            test_workflows: vec![
                ".github/workflows/*test*".into(),
                ".github/workflows/ci.y*ml".into(),
                ".github/workflows/ci-*".into(),
                ".github/workflows/main.y*ml".into(),
                ".github/workflows/build.y*ml".into(),
            ],
            irrelevant_config_sections: Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyFile {
    #[serde(default)]
    policy: PolicyConfig,
}

/// CI configuration that always triggers R001: single-file CI systems and
/// composite actions, whose test steps can't be told apart by file name.
const CI_PATHS: &[&str] = &[
    ".github/actions/**",
    ".gitlab-ci.yml",
    ".gitlab/**",
    ".buildkite/**",
    ".circleci/**",
    "azure-pipelines.yml",
    "Jenkinsfile",
    "bitbucket-pipelines.yml",
];
/// Test-runner configuration that always triggers R002 when changed.
const TEST_CONFIG_PATHS: &[&str] = &[
    "**/conftest.py",
    "noxfile.py",
    "**/jest.config.*",
    "**/vitest.config.*",
];

/// Root configuration files that trigger R002 only when a test-relevant
/// section changes. The same names elsewhere (example projects, fixtures)
/// are ordinary data files.
const ROOT_CONFIG_FILES: &[&str] = &["pyproject.toml", "setup.cfg", "tox.ini", "pytest.ini"];

/// Configuration sections that can't change test outcomes: linters,
/// formatters, type checkers, coverage reporting, docs and package metadata.
/// Everything else (`tool.pytest`, `project.version`, dependencies,
/// `build-system`, `[testenv]`, ...) is treated as relevant.
const IRRELEVANT_CONFIG_SECTIONS: &[&str] = &[
    "tool.ruff",
    "tool.mypy",
    "tool.pyright",
    "tool.black",
    "tool.isort",
    "tool.codespell",
    "tool.coverage",
    "tool.towncrier",
    "tool.bumpversion",
    "tool.bumpver",
    "tool.flake8",
    "tool.pylint",
    "tool.docformatter",
    "tool.check-manifest",
    "tool.check-wheel-contents",
    "tool.cibuildwheel",
    "tool.pyproject-fmt",
    "tool.interrogate",
    "tool.vulture",
    "tool.commitizen",
    "tool.semantic_release",
    "tool.pydocstyle",
    "tool.typos",
    "tool.zizmor",
    "project.urls",
    "project.description",
    "project.readme",
    "project.keywords",
    "project.classifiers",
    "project.authors",
    "project.maintainers",
    "project.license",
    "project.license-files",
    "flake8",
    "mypy",
    "mypy-*",
    "isort",
    "pycodestyle",
    "pydocstyle",
    "coverage:*",
    "bdist_wheel",
    "sdist",
    "check-manifest",
    "codespell",
    "pylint",
    "yapf",
];

fn section_matches(section: &str, pattern: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => section.starts_with(prefix),
        None => section == pattern,
    }
}
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
#[derive(Clone, Debug, Default)]
pub struct EvalContext {
    /// Commits between the footprint map's base and the change base, if known.
    /// `None` means unknown, which fails closed.
    pub map_commits_behind: Option<u32>,
    /// Environment mode (ADR 012): how the target environment differs from
    /// the one the map was recorded in. `None` means path mode.
    pub environment: Option<EnvironmentDelta>,
}

/// How the target environment differs from the recorded one (ADR 012).
///
/// In environment mode the environment is observed instead of inferred from
/// file paths: CI configuration (R001), runtime pins (R003), non-pytest
/// sections of root configuration (R002) and lockfiles no longer trigger
/// anything by themselves, because whatever they change reaches the tests
/// only through what is compared here.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EnvironmentDelta {
    /// Fingerprint keys outside `dist.*` that differ (added, removed or
    /// changed), sorted. Any entry triggers R018.
    pub differing: Vec<String>,
    /// Distributions (normalised names) that differ, sorted.
    pub dists: Vec<String>,
    /// Environment variables (`*` is the whole environment) whose value
    /// differs from the recorded one, sorted.
    pub env_vars: Vec<String>,
}

/// pytest's own configuration sections. In environment mode these are the
/// only root-configuration sections that trigger R002 (the plugin also
/// re-checks the invocation fingerprint at session start).
fn pytest_section(section: &str) -> bool {
    section == "tool.pytest"
        || section.starts_with("tool.pytest.")
        || section == "pytest"
        || section == "tool:pytest"
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
    test_workflows: GlobSet,
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
            &mut config.test_workflows,
            &mut config.irrelevant_config_sections,
        ] {
            list.sort();
            list.dedup();
        }
        let hash = hash_value(Domain::Policy, &(CATALOG_VERSION, &config))?;
        Ok(Self {
            hash,
            ci: globset(CI_PATHS)?,
            test_workflows: globset(&config.test_workflows)?,
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

    fn irrelevant_section(&self, section: &str) -> bool {
        IRRELEVANT_CONFIG_SECTIONS
            .iter()
            .any(|p| section_matches(section, p))
            || self
                .config
                .irrelevant_config_sections
                .iter()
                .any(|p| section_matches(section, p))
    }

    /// For a root configuration file, the reason R002 fires, if it does.
    fn root_config_rule(&self, config: Option<&ConfigDelta>) -> Option<String> {
        match config {
            None => Some("configuration file changed".into()),
            Some(ConfigDelta::Unparseable(e)) => {
                Some(format!("configuration could not be parsed: {e}"))
            }
            Some(ConfigDelta::Sections(sections)) => {
                let relevant: Vec<&str> = sections
                    .iter()
                    .map(String::as_str)
                    .filter(|s| !self.irrelevant_section(s))
                    .collect();
                (!relevant.is_empty())
                    .then(|| format!("test-relevant sections changed: {}", relevant.join(", ")))
            }
        }
    }

    /// R002 for a root configuration file in environment mode.
    fn root_config_rule_env(config: Option<&ConfigDelta>) -> Option<String> {
        match config {
            None => Some("configuration file changed".into()),
            Some(ConfigDelta::Unparseable(e)) => {
                Some(format!("configuration could not be parsed: {e}"))
            }
            Some(ConfigDelta::Sections(sections)) => {
                let relevant: Vec<&str> = sections
                    .iter()
                    .map(String::as_str)
                    .filter(|s| pytest_section(s))
                    .collect();
                (!relevant.is_empty())
                    .then(|| format!("pytest configuration changed: {}", relevant.join(", ")))
            }
        }
    }

    fn global_rule(
        &self,
        path: &str,
        config: Option<&ConfigDelta>,
        env_mode: bool,
    ) -> Option<(&'static str, String)> {
        if self.policy_files.is_match(path) {
            Some(("R004", "Skipcert policy changed".into()))
        } else if env_mode {
            // noxfile.py defines sessions (installs, commands), whose effect
            // is observed through the environment and invocation.
            if self.test_config.is_match(path) && path != "noxfile.py" {
                Some(("R002", "test-runner configuration changed".into()))
            } else if ROOT_CONFIG_FILES.contains(&path) {
                Self::root_config_rule_env(config).map(|d| ("R002", d))
            } else if self.build.is_match(path) {
                Some(("R005", "build-affecting input changed".into()))
            } else if self.run_all.is_match(path) {
                Some(("R015", "path is in run_all_paths".into()))
            } else {
                None
            }
        } else if self.ci.is_match(path) || self.test_workflows.is_match(path) {
            Some((
                "R001",
                "CI configuration that runs the tests changed".into(),
            ))
        } else if self.test_config.is_match(path) {
            Some(("R002", "test-runner configuration changed".into()))
        } else if ROOT_CONFIG_FILES.contains(&path) {
            self.root_config_rule(config).map(|d| ("R002", d))
        } else if self.runtime.is_match(path) {
            Some(("R003", "runtime version pin changed".into()))
        } else if self.build.is_match(path) {
            Some(("R005", "build-affecting input changed".into()))
        } else if self.run_all.is_match(path) {
            Some(("R015", "path is in run_all_paths".into()))
        } else {
            None
        }
    }

    /// Evaluates a change set.
    pub fn evaluate(&self, cs: &ChangeSet, ctx: &EvalContext) -> Evaluation {
        let env = ctx.environment.as_ref();
        let mut ev = Evaluation {
            rules_checked: if env.is_some() {
                vec!["R002", "R004", "R005", "R007", "R015", "R018"]
            } else {
                vec![
                    "R001", "R002", "R003", "R004", "R005", "R006", "R007", "R015",
                ]
            },
            ..Evaluation::default()
        };

        if let Some(env) = env {
            if !env.differing.is_empty() {
                ev.global.push(Trigger {
                    rule: "R018",
                    path: None,
                    detail: format!(
                        "environment differs from the recorded one: {}",
                        env.differing.join(", ")
                    ),
                });
            }
            for d in &env.dists {
                ev.changed.push(ChangedNode {
                    kind: NodeKind::ExtPackage,
                    name: names::package(Ecosystem::Pypi, d),
                    fallback_file: None,
                });
            }
            for v in &env.env_vars {
                ev.changed.push(ChangedNode {
                    kind: NodeKind::EnvVar,
                    name: v.clone(),
                    fallback_file: None,
                });
            }
        }

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
                if let Some((rule, detail)) = self.global_rule(p, fd.config.as_ref(), env.is_some())
                {
                    ev.global.push(Trigger {
                        rule,
                        path: Some((*p).to_owned()),
                        detail,
                    });
                }
            }

            // In environment mode the installed distributions are compared
            // directly, so a lockfile edit by itself changes nothing.
            if fd.lock.is_some() && env.is_some() {
                continue;
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
                        if !pk.is_empty() {
                            // Installed files no distribution claims are attributed to
                            // this pseudo-package by the probe. Any dependency change may
                            // have changed them, so their users re-run.
                            ev.changed.push(ChangedNode {
                                kind: NodeKind::ExtPackage,
                                name: names::package(Ecosystem::Pypi, SITE_PSEUDO_PACKAGE),
                                fallback_file: None,
                            });
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
            config: None,
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
        environment: None,
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

    fn with_config(path: &str, config: ConfigDelta) -> FileDetail {
        let mut d = fd(path, FileStatus::Modified, None, None);
        d.config = Some(config);
        d
    }

    #[test]
    fn only_test_workflows_trigger_r001() {
        let p = Policy::default_policy();
        for path in [
            ".github/workflows/tests.yaml",
            ".github/workflows/ci.yml",
            ".gitlab-ci.yml",
            ".github/actions/setup/action.yml",
        ] {
            let ev = p.evaluate(
                &cs(vec![fd(path, FileStatus::Modified, None, None)]),
                &FRESH,
            );
            assert_eq!(ev.global.first().map(|t| t.rule), Some("R001"), "{path}");
        }
        for path in [
            ".github/workflows/publish.yaml",
            ".github/workflows/codeql-analysis.yml",
            ".github/workflows/zizmor.yml",
        ] {
            let ev = p.evaluate(
                &cs(vec![fd(path, FileStatus::Modified, None, None)]),
                &FRESH,
            );
            assert!(!ev.runs_everything(), "{path}");
        }
        let custom =
            Policy::from_toml("[policy]\ntest_workflows = [\".github/workflows/publish.yaml\"]\n")
                .unwrap();
        let ev = custom.evaluate(
            &cs(vec![fd(
                ".github/workflows/publish.yaml",
                FileStatus::Modified,
                None,
                None,
            )]),
            &FRESH,
        );
        assert!(ev.runs_everything());
    }

    #[test]
    fn root_config_sections_decide_r002() {
        let p = Policy::default_policy();
        let sections =
            |v: &[&str]| ConfigDelta::Sections(v.iter().map(|s| (*s).to_owned()).collect());

        let lint = p.evaluate(
            &cs(vec![with_config(
                "pyproject.toml",
                sections(&["tool.ruff", "project.urls"]),
            )]),
            &FRESH,
        );
        assert!(
            !lint.runs_everything(),
            "lint/metadata edits are irrelevant"
        );
        assert_eq!(
            lint.changed[0].name, "pyproject.toml",
            "still a data-file change"
        );

        for relevant in [
            "tool.pytest",
            "project.version",
            "project.dependencies",
            "build-system",
            "testenv",
        ] {
            let ev = p.evaluate(
                &cs(vec![with_config(
                    "pyproject.toml",
                    sections(&["tool.ruff", relevant]),
                )]),
                &FRESH,
            );
            assert_eq!(
                ev.global.first().map(|t| t.rule),
                Some("R002"),
                "{relevant}"
            );
        }
        let ini = p.evaluate(
            &cs(vec![with_config(
                "tox.ini",
                sections(&["flake8", "mypy-foo", "coverage:run"]),
            )]),
            &FRESH,
        );
        assert!(!ini.runs_everything());

        let broken = p.evaluate(
            &cs(vec![with_config(
                "setup.cfg",
                ConfigDelta::Unparseable("x".into()),
            )]),
            &FRESH,
        );
        assert!(broken.runs_everything());

        let example = p.evaluate(
            &cs(vec![with_config(
                "examples/demo/pyproject.toml",
                sections(&["tool.pytest"]),
            )]),
            &FRESH,
        );
        assert!(
            !example.runs_everything(),
            "only the root config configures the runner"
        );

        let extra = Policy::from_toml("[policy]\nirrelevant_config_sections = [\"tool.hatch*\"]\n")
            .unwrap();
        let ev = extra.evaluate(
            &cs(vec![with_config(
                "pyproject.toml",
                sections(&["tool.hatch.version"]),
            )]),
            &FRESH,
        );
        assert!(!ev.runs_everything());
    }

    #[test]
    fn staleness_unknown_or_too_old_fails_closed() {
        let p = Policy::default_policy();
        let empty = cs(vec![]);
        assert!(
            p.evaluate(
                &empty,
                &EvalContext {
                    map_commits_behind: None,
                    environment: None,
                }
            )
            .runs_everything()
        );
        assert!(
            p.evaluate(
                &empty,
                &EvalContext {
                    map_commits_behind: Some(101),
                    environment: None,
                }
            )
            .runs_everything()
        );
        assert!(
            !p.evaluate(
                &empty,
                &EvalContext {
                    map_commits_behind: Some(100),
                    environment: None,
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
        let names: Vec<&str> = ev.changed.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["pypi:__site__", "pypi:requests"]);
        assert!(ev.changed.iter().all(|c| c.kind == NodeKind::ExtPackage));

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

    fn env_mode(delta: EnvironmentDelta) -> EvalContext {
        EvalContext {
            map_commits_behind: Some(0),
            environment: Some(delta),
        }
    }

    /// ADR 012: with the environment observed, files that can only change
    /// the environment don't trigger anything by themselves.
    #[test]
    fn environment_mode_drops_path_based_environment_rules() {
        let p = Policy::default_policy();
        let same = env_mode(EnvironmentDelta::default());
        for path in [
            ".github/workflows/tests.yaml",
            ".github/actions/setup/action.yml",
            ".gitlab-ci.yml",
            ".python-version",
            "noxfile.py",
        ] {
            let ev = p.evaluate(&cs(vec![fd(path, FileStatus::Modified, None, None)]), &same);
            assert!(ev.global.is_empty(), "{path}: {:?}", ev.global);
        }
        for (path, sections) in [
            ("tox.ini", vec!["testenv", "tox"]),
            (
                "pyproject.toml",
                vec![
                    "tool.tox",
                    "tool.uv",
                    "project.version",
                    "dependency-groups",
                ],
            ),
            ("setup.cfg", vec!["metadata", "options"]),
        ] {
            let d = with_config(
                path,
                ConfigDelta::Sections(sections.iter().map(|s| (*s).to_owned()).collect()),
            );
            assert!(p.evaluate(&cs(vec![d]), &same).global.is_empty(), "{path}");
        }
        // A lockfile edit alone changes nothing: installed distributions are
        // compared directly.
        let lock = fd(
            "uv.lock",
            FileStatus::Modified,
            None,
            Some(LockDelta::Unparseable("x".into())),
        );
        let ev = p.evaluate(&cs(vec![lock]), &same);
        assert!(ev.global.is_empty() && ev.changed.is_empty());
        assert!(ev.rules_checked.contains(&"R018") && !ev.rules_checked.contains(&"R001"));
    }

    #[test]
    fn environment_mode_keeps_test_runner_configuration() {
        let p = Policy::default_policy();
        let same = env_mode(EnvironmentDelta::default());
        for (path, section) in [
            ("pyproject.toml", "tool.pytest"),
            ("pytest.ini", "pytest"),
            ("setup.cfg", "tool:pytest"),
            ("tox.ini", "pytest"),
        ] {
            let d = with_config(path, ConfigDelta::Sections(vec![section.into()]));
            let ev = p.evaluate(&cs(vec![d]), &same);
            assert_eq!(ev.global.first().map(|t| t.rule), Some("R002"), "{path}");
        }
        let bad = with_config("pyproject.toml", ConfigDelta::Unparseable("x".into()));
        assert_eq!(p.evaluate(&cs(vec![bad]), &same).global[0].rule, "R002");
        for path in ["tests/conftest.py", "skipcert.toml", "ext/fast.c"] {
            let ev = p.evaluate(&cs(vec![fd(path, FileStatus::Modified, None, None)]), &same);
            assert!(!ev.global.is_empty(), "{path}");
        }
    }

    #[test]
    fn environment_differences_become_r018_or_changed_nodes() {
        let p = Policy::default_policy();
        let ev = p.evaluate(
            &cs(vec![]),
            &env_mode(EnvironmentDelta {
                differing: vec!["interpreter.version".into()],
                ..EnvironmentDelta::default()
            }),
        );
        assert_eq!(ev.global[0].rule, "R018");
        assert!(ev.global[0].detail.contains("interpreter.version"));

        let ev = p.evaluate(
            &cs(vec![]),
            &env_mode(EnvironmentDelta {
                differing: vec![],
                dists: vec!["requests".into(), "__site__".into()],
                env_vars: vec!["API_URL".into()],
            }),
        );
        assert!(!ev.runs_everything());
        let got: Vec<(NodeKind, &str)> = ev
            .changed
            .iter()
            .map(|c| (c.kind, c.name.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                (NodeKind::ExtPackage, "pypi:__site__"),
                (NodeKind::ExtPackage, "pypi:requests"),
                (NodeKind::EnvVar, "API_URL"),
            ]
        );
    }
}
