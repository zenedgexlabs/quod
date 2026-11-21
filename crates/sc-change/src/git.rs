//! Git access through the `git` CLI.
//!
//! The CLI, not libgit2, is the reference implementation of git's own
//! semantics (rename detection, ignore rules, attributes), and it is present
//! wherever CI or an agent sandbox runs.

use std::path::{Path, PathBuf};
use std::process::Command;

use sc_hash::{Digest, Domain, hash_domain, hash_file, merkle_root};

use crate::ChangeError;

/// How a file changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileStatus {
    /// New file (including untracked and copied files).
    Added,
    /// Content or type changed.
    Modified,
    /// File removed.
    Deleted,
    /// File moved, possibly with edits. `old_path` is set.
    Renamed,
}

/// One changed path, `/`-separated and relative to the repository root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileChange {
    /// Path at head or in the working tree.
    pub path: String,
    /// Path at base, for renames.
    pub old_path: Option<String>,
    /// Kind of change.
    pub status: FileStatus,
}

/// A git repository.
#[derive(Clone, Debug)]
pub struct Repo {
    root: PathBuf,
}

fn run(dir: &Path, args: &[&str]) -> Result<Vec<u8>, ChangeError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "core.quotepath=off"])
        .args(args)
        .output()
        .map_err(|e| ChangeError::Git(format!("failed to run git: {e}")))?;
    if !out.status.success() {
        return Err(ChangeError::Git(format!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(out.stdout)
}

fn utf8(bytes: &[u8]) -> Result<String, ChangeError> {
    String::from_utf8(bytes.to_vec()).map_err(|_| ChangeError::Git("non-UTF-8 output".into()))
}

impl Repo {
    /// Finds the repository containing `start`.
    pub fn discover(start: impl AsRef<Path>) -> Result<Self, ChangeError> {
        let out = run(start.as_ref(), &["rev-parse", "--show-toplevel"])?;
        let root = utf8(&out)?.trim().to_owned();
        Ok(Self {
            root: PathBuf::from(root),
        })
    }

    /// Repository root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolves a revision to a full commit SHA.
    pub fn rev_parse(&self, rev: &str) -> Result<String, ChangeError> {
        let spec = format!("{rev}^{{commit}}");
        Ok(utf8(&run(
            &self.root,
            &["rev-parse", "--verify", "--quiet", &spec],
        )?)?
        .trim()
        .to_owned())
    }

    /// Merge base of two revisions.
    pub fn merge_base(&self, a: &str, b: &str) -> Result<String, ChangeError> {
        Ok(utf8(&run(&self.root, &["merge-base", a, b])?)?
            .trim()
            .to_owned())
    }

    /// True if the working tree or index differs from `HEAD`, or untracked
    /// files exist.
    pub fn is_dirty(&self) -> Result<bool, ChangeError> {
        Ok(!run(
            &self.root,
            &["status", "--porcelain", "-z", "--untracked-files=all"],
        )?
        .is_empty())
    }

    /// Files changed between `base` and `head`, or between `base` and the
    /// working tree (plus untracked files) when `head` is `None`.
    pub fn changed_files(
        &self,
        base: &str,
        head: Option<&str>,
    ) -> Result<Vec<FileChange>, ChangeError> {
        let mut args = vec!["diff", "--name-status", "-z", "-M", "--no-ext-diff", base];
        if let Some(h) = head {
            args.push(h);
        }
        let mut changes = parse_name_status(&run(&self.root, &args)?)?;
        if head.is_none() {
            let untracked = run(
                &self.root,
                &["ls-files", "--others", "--exclude-standard", "-z"],
            )?;
            for p in untracked.split(|&b| b == 0).filter(|p| !p.is_empty()) {
                changes.push(FileChange {
                    path: utf8(p)?,
                    old_path: None,
                    status: FileStatus::Added,
                });
            }
        }
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        changes.dedup_by(|a, b| a.path == b.path);
        Ok(changes)
    }

    /// Contents of `path` at `rev`, or `None` if it doesn't exist there. This
    /// is the raw blob, with no line-ending conversion.
    pub fn read_at(&self, rev: &str, path: &str) -> Result<Option<Vec<u8>>, ChangeError> {
        let listed = run(
            &self.root,
            &["ls-tree", "-z", "--name-only", rev, "--", path],
        )?;
        if listed.is_empty() {
            return Ok(None);
        }
        let spec = format!("{rev}:{path}");
        Ok(Some(run(&self.root, &["cat-file", "blob", &spec])?))
    }

    /// Contents of `path` in the working tree, or `None` if absent.
    pub fn read_worktree(&self, path: &str) -> Result<Option<Vec<u8>>, ChangeError> {
        match std::fs::read(self.root.join(path)) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Digest pinning the working-tree state of every changed path relative
    /// to `base`: a Merkle root over (path, content hash or deletion marker),
    /// sorted by path.
    pub fn worktree_digest(
        &self,
        base: &str,
        changes: &[FileChange],
    ) -> Result<Digest, ChangeError> {
        let mut leaves = vec![hash_domain(Domain::Blob, base.as_bytes())];
        for c in changes {
            let full = self.root.join(&c.path);
            let content = if full.is_file() {
                hash_file(&full)?
            } else {
                hash_domain(Domain::Blob, b"<deleted>")
            };
            let mut buf = Vec::with_capacity(c.path.len() + 40);
            buf.extend_from_slice(&(c.path.len() as u64).to_le_bytes());
            buf.extend_from_slice(c.path.as_bytes());
            buf.extend_from_slice(content.as_bytes());
            leaves.push(hash_domain(Domain::Blob, &buf));
        }
        Ok(merkle_root(&leaves))
    }
}

fn parse_name_status(out: &[u8]) -> Result<Vec<FileChange>, ChangeError> {
    let mut fields = out.split(|&b| b == 0).filter(|f| !f.is_empty());
    let mut changes = Vec::new();
    while let Some(status) = fields.next() {
        let status = utf8(status)?;
        let mut next = || -> Result<String, ChangeError> {
            utf8(
                fields
                    .next()
                    .ok_or_else(|| ChangeError::Git("truncated diff output".into()))?,
            )
        };
        match status.chars().next() {
            Some('R') => {
                let old = next()?;
                let new = next()?;
                changes.push(FileChange {
                    path: new,
                    old_path: Some(old),
                    status: FileStatus::Renamed,
                });
            }
            Some('C') => {
                let _source = next()?;
                let new = next()?;
                changes.push(FileChange {
                    path: new,
                    old_path: None,
                    status: FileStatus::Added,
                });
            }
            Some('A') => changes.push(FileChange {
                path: next()?,
                old_path: None,
                status: FileStatus::Added,
            }),
            Some('D') => changes.push(FileChange {
                path: next()?,
                old_path: None,
                status: FileStatus::Deleted,
            }),
            // M, T (type change) and U (unmerged) are all treated as modifications.
            Some('M' | 'T' | 'U') => changes.push(FileChange {
                path: next()?,
                old_path: None,
                status: FileStatus::Modified,
            }),
            _ => return Err(ChangeError::Git(format!("unknown diff status {status:?}"))),
        }
    }
    Ok(changes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_name_status_z() {
        let raw = b"M\0src/a.py\0R087\0old.py\0new.py\0A\0b.py\0D\0gone.py\0";
        let c = parse_name_status(raw).unwrap();
        assert_eq!(c.len(), 4);
        assert_eq!(c[1].status, FileStatus::Renamed);
        assert_eq!(c[1].old_path.as_deref(), Some("old.py"));
        assert_eq!(c[3].status, FileStatus::Deleted);
    }

    #[test]
    fn rejects_unknown_status() {
        assert!(parse_name_status(b"X\0a\0").is_err());
    }
}
