//! Change resolution for Quod.
//!
//! Turns "what changed between the base commit and now" into facts the
//! policy and the skip ladder can act on:
//!
//! - [`git`]: changed files between a base and a head commit or the working
//!   tree, including untracked files, plus a digest that pins a dirty tree;
//! - [`python`]: function-level Python deltas. Formatting- and comment-only
//!   edits are not changes, and a block moved by re-indentation is one;
//! - [`lock`]: package-level lockfile deltas (uv, poetry, PEP 751, pinned
//!   requirements).
//!
//! This crate reports facts only. Deciding what a change *means* (fail closed,
//! widen, skip) is the job of `quod-policy` and `quod-ladder`. Anything it can't
//! analyse is reported as such and never silently dropped.

pub mod config;
pub mod git;
pub mod lang;
pub mod lock;
pub mod python;

use quod_hash::Digest;

pub use config::{ConfigDelta, ConfigKind};
pub use git::{FileChange, FileStatus, Repo};
pub use lang::{Language, LineMap};
pub use lock::{LockDelta, LockKind, PackageChange, PkgPin};
pub use python::{DiffOptions, SourceDelta};

/// Errors from change resolution.
#[derive(Debug, thiserror::Error)]
pub enum ChangeError {
    /// A git command failed or git is unavailable.
    #[error("git: {0}")]
    Git(String),
    /// Reading the working tree failed.
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    /// The parser could not be initialised.
    #[error("parser: {0}")]
    Parser(String),
}

/// What to compare.
#[derive(Clone, Debug)]
pub struct ChangeOptions {
    /// Base revision (usually the merge base with the target branch).
    pub base: String,
    /// Head revision. `None` compares against the working tree, including
    /// staged, unstaged and untracked files.
    pub head: Option<String>,
    /// How Python sources are compared.
    pub python: python::DiffOptions,
}

/// Analysis attached to one changed file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDetail {
    /// The raw change.
    pub change: FileChange,
    /// Function-level delta, for Python sources.
    pub source: Option<SourceDelta>,
    /// Package delta, for recognised lockfiles.
    pub lock: Option<LockDelta>,
    /// Section delta, for recognised configuration files.
    pub config: Option<ConfigDelta>,
}

/// The resolved change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeSet {
    /// Resolved base commit SHA.
    pub base: String,
    /// Resolved head commit SHA (`HEAD` when comparing the working tree).
    pub head: String,
    /// Digest pinning the working-tree contents, when the tree differs from
    /// `head`. It goes into the certificate's pinned inputs.
    pub tree: Option<Digest>,
    /// Per-file details, sorted by path.
    pub files: Vec<FileDetail>,
}

/// Resolves the change described by `opts` in `repo`.
pub fn compute(repo: &Repo, opts: &ChangeOptions) -> Result<ChangeSet, ChangeError> {
    let base = repo.rev_parse(&opts.base)?;
    let head = repo.rev_parse(opts.head.as_deref().unwrap_or("HEAD"))?;
    let changes = repo.changed_files(&base, opts.head.as_deref())?;
    let tree = match opts.head {
        None if repo.is_dirty()? => Some(repo.worktree_digest(&base, &changes)?),
        _ => None,
    };

    let mut files = Vec::with_capacity(changes.len());
    for change in changes {
        let old_path = change.old_path.as_deref().unwrap_or(&change.path);
        let old = match change.status {
            FileStatus::Added => None,
            _ => repo.read_at(&base, old_path)?,
        };
        let new = match change.status {
            FileStatus::Deleted => None,
            _ => match opts.head.as_deref() {
                Some(h) => repo.read_at(h, &change.path)?,
                None => repo.read_worktree(&change.path)?,
            },
        };

        let language = Language::from_path(&change.path).or_else(|| Language::from_path(old_path));
        let source = if let Some(lang) = language {
            let mut delta = match lang {
                Language::Python => python::diff_with(old.as_deref(), new.as_deref(), opts.python)?,
                _ => lang::diff(lang, old.as_deref(), new.as_deref())?,
            };
            // A rename changes the module's import path even when the content
            // is identical, so importers of the old path are affected.
            if change.status == FileStatus::Renamed {
                delta.module_changed = true;
            }
            Some(delta)
        } else {
            None
        };
        let lock =
            lock::lock_kind(&change.path).map(|k| lock::delta(k, old.as_deref(), new.as_deref()));
        let config = config::config_kind(&change.path)
            .map(|k| config::delta(k, old.as_deref(), new.as_deref()));
        files.push(FileDetail {
            change,
            source,
            lock,
            config,
        });
    }
    files.sort_by(|a, b| a.change.path.cmp(&b.change.path));
    observe_added_exports(repo, opts.head.as_deref(), &mut files)?;
    Ok(ChangeSet {
        base,
        head,
        tree,
        files,
    })
}

/// Lines that can observe a JS/TS module's set of names: star imports and
/// re-exports, dynamic `import()`, `require()`, and test-runner module mocks
/// (automocks and `requireActual`/`importActual` spreads enumerate exports).
const JS_NAMESPACE_PATTERN: &str = "^[[:space:]]*(import|export)([[:space:]]+type)?[[:space:]]*\\*\
     |import[[:space:]]*\\(|require[[:space:]]*\\(|\\.mock[[:space:]]*\\(|Actual";
const JS_PATHSPECS: &[&str] = &[
    "*.js", "*.mjs", "*.cjs", "*.jsx", "*.ts", "*.mts", "*.cts", "*.tsx",
];
// Longest first: `x.d.ts` is `x`, not `x.d`.
const JS_EXTENSIONS: &[&str] = &[
    ".d.ts", ".js", ".mjs", ".cjs", ".jsx", ".ts", ".mts", ".cts", ".tsx",
];

/// ADR 020: a JS/TS module whose only change is new top-level functions
/// stays a function-level change only if nothing in the repository (at
/// head) can observe its set of names. Otherwise it is a module change.
fn observe_added_exports(
    repo: &Repo,
    head: Option<&str>,
    files: &mut [FileDetail],
) -> Result<(), ChangeError> {
    let additive: Vec<usize> = files
        .iter()
        .enumerate()
        .filter(|(_, f)| {
            f.source
                .as_ref()
                .is_some_and(|s| !s.module_changed && !s.added.is_empty())
        })
        .map(|(i, _)| i)
        .collect();
    if additive.is_empty() {
        return Ok(());
    }
    let hits = repo.grep(head, JS_NAMESPACE_PATTERN, JS_PATHSPECS)?;
    for i in additive {
        let stems = module_stems(&files[i].change.path);
        if hits.iter().any(|(_, line)| line_may_observe(line, &stems)) {
            if let Some(s) = files[i].source.as_mut() {
                s.module_changed = true;
            }
        }
    }
    Ok(())
}

/// The specifier tails that can name `path`: its file stem, and for an
/// `index` file its directory's name (and `.`/`..`).
fn module_stems(path: &str) -> Vec<String> {
    let file = path.rsplit('/').next().unwrap_or(path);
    let stem = strip_js_ext(file).to_owned();
    let mut out = vec![stem.clone()];
    if stem == "index" {
        let mut parts = path.rsplit('/');
        parts.next();
        if let Some(dir) = parts.next() {
            out.push(dir.to_owned());
        }
        out.extend([".".to_owned(), "..".to_owned(), String::new()]);
    }
    out
}

fn strip_js_ext(s: &str) -> &str {
    JS_EXTENSIONS
        .iter()
        .find_map(|e| s.strip_suffix(e))
        .unwrap_or(s)
}

/// True if a matched line may refer to a module named by `stems`: one of
/// its string literals names it, or it has none we can read (a dynamic
/// specifier, or a statement continued on another line).
fn line_may_observe(line: &str, stems: &[String]) -> bool {
    match string_literals(line) {
        None => true,
        Some(lits) if lits.is_empty() => true,
        Some(lits) => lits.iter().any(|l| {
            let tail = l.trim_end_matches('/').rsplit('/').next().unwrap_or(l);
            stems.iter().any(|s| strip_js_ext(tail) == s)
        }),
    }
}

/// The string literals on a line, or `None` if one is unterminated or is
/// a template with substitutions (its value isn't known statically).
fn string_literals(line: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '"' && c != '\'' && c != '`' {
            continue;
        }
        let mut lit = String::new();
        let mut closed = false;
        while let Some(d) = chars.next() {
            if d == '\\' {
                chars.next();
                continue;
            }
            if d == c {
                closed = true;
                break;
            }
            lit.push(d);
        }
        if !closed || (c == '`' && lit.contains("${")) {
            return None;
        }
        out.push(lit);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn git(dir: &std::path::Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "-c",
                "core.autocrlf=false",
            ])
            .args(args)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    /// ADR 020: a new exported function stays function-level unless some
    /// file can observe the module's set of names.
    #[test]
    fn added_js_function_is_function_level_unless_the_namespace_is_observable() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q", "-b", "main"]);
        fs::create_dir_all(p.join("src/impl")).unwrap();
        fs::create_dir_all(p.join("test")).unwrap();
        let util = "export function round(v) {\n  return Math.round(v);\n}\n";
        fs::write(p.join("src/impl/util.js"), util).unwrap();
        fs::write(
            p.join("test/a.test.js"),
            "import { round } from '../src/impl/util.js';\nconst fs = require('fs');\nexport * from './other.js';\n",
        )
        .unwrap();
        git(p, &["add", "-A"]);
        git(p, &["commit", "-q", "-m", "base"]);
        fs::write(
            p.join("src/impl/util.js"),
            format!("{util}export function snap(v) {{\n  return v;\n}}\n"),
        )
        .unwrap();

        let repo = Repo::discover(p).unwrap();
        let util_delta = |repo: &Repo| {
            let cs = compute(
                repo,
                &ChangeOptions {
                    base: "HEAD".into(),
                    head: None,
                    python: DiffOptions::default(),
                },
            )
            .unwrap();
            cs.files
                .into_iter()
                .find(|f| f.change.path == "src/impl/util.js")
                .unwrap()
                .source
                .unwrap()
        };
        let d = util_delta(&repo);
        assert!(!d.module_changed, "{d:?}");
        assert_eq!(d.added, vec!["snap".to_string()]);

        // Each of these can see the new name: the change is a module change.
        for (file, text) in [
            (
                "test/ns.test.js",
                "import * as util from '../src/impl/util.js';\n",
            ),
            ("src/index.js", "export * from './impl/util';\n"),
            ("test/mock.test.js", "vi.mock('../src/impl/util.js');\n"),
            ("test/dyn.test.js", "const m = await import(name);\n"),
            (
                "test/req.test.js",
                "const u = require('../src/impl/util');\n",
            ),
        ] {
            fs::write(p.join(file), text).unwrap();
            let d = util_delta(&repo);
            assert!(d.module_changed, "{file}: {d:?}");
            fs::remove_file(p.join(file)).unwrap();
        }
        // Back to no observers.
        assert!(!util_delta(&repo).module_changed);
    }

    #[test]
    fn end_to_end_working_tree() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q", "-b", "main"]);
        fs::write(
            p.join("app.py"),
            "def a():\n    return 1\n\ndef b():\n    return 2\n",
        )
        .unwrap();
        fs::write(p.join("requirements.txt"), "requests==2.31.0\n").unwrap();
        fs::write(p.join("README.md"), "hi\n").unwrap();
        git(p, &["add", "-A"]);
        git(p, &["commit", "-q", "-m", "base"]);

        // Change only b(), reformat a(), bump a dependency, add an untracked file.
        fs::write(
            p.join("app.py"),
            "def a():\n    return  1  # same\n\ndef b():\n    return 3\n",
        )
        .unwrap();
        fs::write(p.join("requirements.txt"), "requests==2.32.0\n").unwrap();
        fs::write(p.join("new_mod.py"), "X = 1\n").unwrap();

        let repo = Repo::discover(p).unwrap();
        let cs = compute(
            &repo,
            &ChangeOptions {
                base: "HEAD".into(),
                head: None,
                python: DiffOptions::default(),
            },
        )
        .unwrap();
        assert!(cs.tree.is_some(), "dirty tree must be pinned");

        let paths: Vec<&str> = cs.files.iter().map(|f| f.change.path.as_str()).collect();
        assert_eq!(paths, vec!["app.py", "new_mod.py", "requirements.txt"]);

        let app = &cs.files[0];
        let src = app.source.as_ref().unwrap();
        assert!(!src.module_changed);
        assert_eq!(src.entities, vec!["b".to_string()]);

        let new_mod = &cs.files[1];
        assert_eq!(new_mod.change.status, FileStatus::Added);
        assert!(new_mod.source.as_ref().unwrap().module_changed);

        match cs.files[2].lock.as_ref().unwrap() {
            LockDelta::Packages(pk) => {
                assert_eq!(pk.len(), 1);
                assert_eq!(pk[0].name, "requests");
                assert_eq!(pk[0].new.as_ref().unwrap().version, "2.32.0");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn commit_to_commit_and_rename() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q", "-b", "main"]);
        fs::write(p.join("old_name.py"), "def f():\n    return 1\n").unwrap();
        git(p, &["add", "-A"]);
        git(p, &["commit", "-q", "-m", "base"]);
        git(p, &["mv", "old_name.py", "new_name.py"]);
        git(p, &["commit", "-q", "-m", "rename"]);

        let repo = Repo::discover(p).unwrap();
        let cs = compute(
            &repo,
            &ChangeOptions {
                base: "HEAD~1".into(),
                head: Some("HEAD".into()),
                python: DiffOptions::default(),
            },
        )
        .unwrap();
        assert!(cs.tree.is_none());
        assert_eq!(cs.files.len(), 1);
        let f = &cs.files[0];
        assert_eq!(f.change.status, FileStatus::Renamed);
        assert_eq!(f.change.old_path.as_deref(), Some("old_name.py"));
        assert!(
            f.source.as_ref().unwrap().module_changed,
            "a rename changes module identity"
        );
    }
}
