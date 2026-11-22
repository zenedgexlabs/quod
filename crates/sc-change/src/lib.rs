//! Change resolution for Skipcert.
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
//! widen, skip) is the job of `sc-policy` and `sc-ladder`. Anything it can't
//! analyse is reported as such and never silently dropped.

pub mod config;
pub mod git;
pub mod lock;
pub mod python;

use sc_hash::Digest;

pub use config::{ConfigDelta, ConfigKind};
pub use git::{FileChange, FileStatus, Repo};
pub use lock::{LockDelta, LockKind, PackageChange, PkgPin};
pub use python::SourceDelta;

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

        let source = if change.path.ends_with(".py") || old_path.ends_with(".py") {
            let mut delta = python::diff(old.as_deref(), new.as_deref())?;
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
    Ok(ChangeSet {
        base,
        head,
        tree,
        files,
    })
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
