//! `quod map pull`: fetches the newest green footprint map that CI
//! published for an ancestor of HEAD (MCP phase 1).
//!
//! The GitHub Action uploads the map of every green default-branch run as
//! the artifact `quod-map-<sha>` (`map.qmap` and `sha.txt`). Pulling lists
//! the repository's artifacts with the GitHub CLI (`gh`, using the
//! developer's login), picks the one whose commit is the closest ancestor
//! of HEAD, downloads it, and installs it only if the map really was
//! recorded at that commit.
//!
//! A pulled map is only an input to advisory answers (local `select`,
//! `quod mcp`). It can't license CI skips: those need a trusted key.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;

use quod_change::Repo;
use quod_graph::FootprintGraph;

/// Artifact name prefix; the rest is the full commit SHA.
pub const MAP_ARTIFACT_PREFIX: &str = "quod-map-";
/// Artifact list pages read (100 artifacts each, newest first).
const MAX_PAGES: u32 = 5;

/// Default place for a pulled map: inside the git directory, so it never
/// shows up as a working-tree change (a file under `.quod/` would trigger
/// R004 and run everything).
pub fn default_map_path(repo: &Repo) -> Result<PathBuf> {
    Ok(repo.common_dir()?.join("quod").join("map.qmap"))
}

/// Options for [`pull_map`].
#[derive(Clone, Debug)]
pub struct PullOptions {
    /// Where to install the map.
    pub out: PathBuf,
    /// `OWNER/NAME` on GitHub (default: `gh repo view` in the repository).
    pub github_repo: Option<String>,
    /// Revision the map must be an ancestor of (default `HEAD`).
    pub at: String,
    /// How many ancestors of `at` to consider.
    pub depth: usize,
}

/// One published map artifact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MapArtifact {
    /// Artifact ID.
    pub id: u64,
    /// Commit the map was recorded at.
    pub commit: String,
    /// Workflow run that uploaded it.
    pub run_id: u64,
}

/// What was pulled.
#[derive(Clone, Debug, Serialize)]
pub struct Pulled {
    /// Commit the map was recorded at.
    pub commit: String,
    /// Commits between it and `at` (0 = the map is for `at` itself).
    pub behind: usize,
    /// Artifact ID.
    pub artifact_id: u64,
    /// Workflow run that published it.
    pub run_id: u64,
    /// GitHub repository.
    pub github_repo: String,
    /// Where it was installed.
    pub path: PathBuf,
}

/// Map artifacts in one page of `GET /repos/{repo}/actions/artifacts`;
/// expired artifacts and other names are left out.
pub fn parse_artifacts(page: &Value) -> Vec<MapArtifact> {
    page.get("artifacts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|a| !a.get("expired").and_then(Value::as_bool).unwrap_or(true))
        .filter_map(|a| {
            let commit = a.get("name")?.as_str()?.strip_prefix(MAP_ARTIFACT_PREFIX)?;
            if commit.len() != 40 || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            Some(MapArtifact {
                id: a.get("id")?.as_u64()?,
                commit: commit.to_ascii_lowercase(),
                run_id: a.get("workflow_run")?.get("id")?.as_u64()?,
            })
        })
        .collect()
}

/// The artifact for the closest ancestor (`ancestors` newest first), and
/// how many commits behind it is. Several artifacts for one commit (re-runs)
/// pick the newest, which the API lists first.
pub fn choose<'a>(
    ancestors: &[String],
    artifacts: &'a [MapArtifact],
) -> Option<(usize, &'a MapArtifact)> {
    ancestors
        .iter()
        .enumerate()
        .find_map(|(i, c)| artifacts.iter().find(|a| a.commit == *c).map(|a| (i, a)))
}

fn gh() -> Command {
    Command::new(std::env::var_os("QUOD_GH").unwrap_or_else(|| "gh".into()))
}

fn gh_output(dir: &Path, args: &[&str]) -> Result<String> {
    let out = gh()
        .current_dir(dir)
        .args(args)
        .output()
        .context("running the GitHub CLI (`gh`); install it and run `gh auth login`")?;
    if !out.status.success() {
        bail!(
            "`gh {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Pulls the newest green map for an ancestor of `opts.at`.
pub fn pull_map(repo: &Repo, opts: &PullOptions) -> Result<Pulled> {
    let dir = repo.root();
    let github_repo = match &opts.github_repo {
        Some(r) => r.clone(),
        None => gh_output(
            dir,
            &[
                "repo",
                "view",
                "--json",
                "nameWithOwner",
                "-q",
                ".nameWithOwner",
            ],
        )?
        .trim()
        .to_owned(),
    };
    let ancestors = repo.ancestors(&opts.at, opts.depth)?;

    let mut artifacts = Vec::new();
    let mut found = None;
    for page in 1..=MAX_PAGES {
        let endpoint = format!("repos/{github_repo}/actions/artifacts?per_page=100&page={page}");
        let json: Value = serde_json::from_str(&gh_output(dir, &["api", &endpoint])?)
            .context("parsing the artifact list")?;
        let n = json
            .get("artifacts")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        artifacts.extend(parse_artifacts(&json));
        // Artifacts come newest first, so the first page with any match
        // almost always has the closest one; one more page settles ties
        // with older re-runs.
        if let Some((i, a)) = choose(&ancestors, &artifacts) {
            if found
                .as_ref()
                .is_some_and(|(j, _): &(usize, MapArtifact)| *j <= i)
            {
                break;
            }
            found = Some((i, a.clone()));
        }
        if n < 100 {
            break;
        }
    }
    let Some((behind, art)) = found else {
        bail!(
            "no green map published for the last {} commits of {} in {github_repo} \
             (the Quod action uploads one per green run on the default branch)",
            opts.depth,
            opts.at
        );
    };

    let tmp = repo
        .common_dir()?
        .join("quod")
        .join(format!("pull-{}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;
    let name = format!("{MAP_ARTIFACT_PREFIX}{}", art.commit);
    let run = art.run_id.to_string();
    let tmp_s = tmp.to_string_lossy().into_owned();
    let downloaded = gh_output(
        dir,
        &[
            "run",
            "download",
            &run,
            "-R",
            &github_repo,
            "-n",
            &name,
            "-D",
            &tmp_s,
        ],
    );
    let installed = downloaded.and_then(|_| install(&tmp.join("map.qmap"), &art.commit, &opts.out));
    let _ = std::fs::remove_dir_all(&tmp);
    installed?;
    Ok(Pulled {
        commit: art.commit.clone(),
        behind,
        artifact_id: art.id,
        run_id: art.run_id,
        github_repo,
        path: opts.out.clone(),
    })
}

/// Checks that `map` was recorded at `commit` and moves it to `out`.
fn install(map: &Path, commit: &str, out: &Path) -> Result<()> {
    let bytes = std::fs::read(map).context("the artifact has no map.qmap")?;
    let (graph, _) = FootprintGraph::decode(&bytes).context("decoding the downloaded map")?;
    let base = &graph.meta().base_commit;
    if !base.eq_ignore_ascii_case(commit) {
        bail!("the artifact for {commit} holds a map recorded at {base}");
    }
    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let staging = out.with_extension("qmap.part");
    std::fs::write(&staging, &bytes)?;
    std::fs::rename(&staging, out).with_context(|| format!("installing {}", out.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sha(c: char) -> String {
        std::iter::repeat_n(c, 40).collect()
    }

    #[test]
    fn parses_only_live_map_artifacts() {
        let page = json!({"artifacts": [
            {"id": 1, "name": format!("quod-map-{}", sha('a')), "expired": false, "workflow_run": {"id": 10}},
            {"id": 2, "name": format!("quod-map-{}", sha('b')), "expired": true, "workflow_run": {"id": 11}},
            {"id": 3, "name": "coverage", "expired": false, "workflow_run": {"id": 12}},
            {"id": 4, "name": "quod-map-notasha", "expired": false, "workflow_run": {"id": 13}},
        ]});
        assert_eq!(
            parse_artifacts(&page),
            vec![MapArtifact {
                id: 1,
                commit: sha('a'),
                run_id: 10
            }]
        );
    }

    #[test]
    fn chooses_the_closest_ancestor() {
        let arts = vec![
            MapArtifact {
                id: 1,
                commit: sha('c'),
                run_id: 1,
            },
            MapArtifact {
                id: 2,
                commit: sha('b'),
                run_id: 2,
            },
            MapArtifact {
                id: 3,
                commit: sha('f'),
                run_id: 3,
            },
        ];
        // HEAD = a, then b, then c: b is the closest with a map.
        let ancestors = vec![sha('a'), sha('b'), sha('c')];
        let (behind, a) = choose(&ancestors, &arts).unwrap();
        assert_eq!((behind, a.id), (1, 2));
        // A map for a commit that isn't an ancestor (another branch) is
        // never chosen.
        assert!(choose(&[sha('a')], &arts).is_none());
    }
}
