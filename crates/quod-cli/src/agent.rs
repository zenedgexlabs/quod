//! What an agent needs besides the selection itself (MCP phase 2): the
//! test environment to compare against (environment mode, found without
//! flags), and a command that runs exactly the obligated tests.
//!
//! Everything here is a convenience for advisory answers. When detection
//! fails, the answer falls back to path mode, which is sound and only
//! less precise.

use std::path::{Path, PathBuf};

use serde::Serialize;

use quod_graph::MapMeta;
use quod_model::version::{CAP_JS_ENVIRONMENT, CAP_PY_ENVIRONMENT};

/// Which test runner recorded the map.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Runner {
    /// pytest, through the `quod_probe` plugin.
    Pytest,
    /// Vitest, through `quod-js`.
    Vitest,
    /// Jest, through `quod-js`.
    Jest,
}

/// The test environment the agent's runs will use.
#[derive(Clone, Debug, Default, Serialize)]
pub struct EnvSetup {
    /// Python interpreter (environment mode for `py.environment` maps).
    pub python: Option<PathBuf>,
    /// Fingerprint command (environment mode for `js.environment` maps).
    pub fingerprint_cmd: Option<Vec<String>>,
    /// The `quod-js` CLI, for JavaScript maps.
    pub quod_js: Option<PathBuf>,
    /// Why environment mode is off, if it is.
    pub note: Option<String>,
}

impl EnvSetup {
    /// True if selection compares environments (ADR 012).
    pub fn environment_mode(&self) -> bool {
        self.python.is_some() || self.fingerprint_cmd.is_some()
    }
}

/// A command that runs the tests a skip file leaves.
#[derive(Clone, Debug, Serialize)]
pub struct RunCommand {
    /// Directory to run it in, relative to the repository root.
    pub cwd: String,
    /// The command, one argument per element.
    pub argv: Vec<String>,
    /// The same, quoted for a POSIX shell.
    pub shell: String,
}

/// The runner that recorded the map, from its probes and invocation.
pub fn runner(meta: &MapMeta) -> Option<Runner> {
    if meta.probes.iter().any(|p| p.starts_with("python-probe")) {
        return Some(Runner::Pytest);
    }
    if meta.probes.iter().any(|p| p.starts_with("js-probe")) {
        return Some(
            match meta.invocation.get("invocation.runner").map(String::as_str) {
                Some("jest") => Runner::Jest,
                _ => Runner::Vitest,
            },
        );
    }
    None
}

/// Directory the recorded run started in, relative to the root (`.` when
/// the map predates `invocation.dir`).
pub fn invocation_dir(meta: &MapMeta) -> String {
    meta.invocation
        .get("invocation.dir")
        .filter(|d| {
            !d.is_empty()
                && !d.starts_with(['/', '\\'])
                && !d.contains(':')
                && !d.split(['/', '\\']).any(|c| c == "..")
        })
        .cloned()
        .unwrap_or_else(|| ".".into())
}

fn recorded_args(meta: &MapMeta) -> Vec<String> {
    meta.invocation
        .get("invocation.args")
        .and_then(|a| serde_json::from_str(a).ok())
        .unwrap_or_default()
}

fn venv_python(dir: &Path) -> Option<PathBuf> {
    ["Scripts/python.exe", "bin/python"]
        .iter()
        .map(|p| dir.join(".venv").join(p))
        .find(|p| p.is_file())
}

/// Finds the test environment for `meta` in the repository at `root`.
///
/// Python: `$QUOD_PYTHON`, else a `.venv` in the recorded run directory or
/// the root. JavaScript: `$QUOD_JS` or `node_modules/quod-js` there, run
/// with `node`.
pub fn detect(root: &Path, meta: &MapMeta) -> EnvSetup {
    let dir = root.join(invocation_dir(meta));
    let cap = |c: &str| meta.capabilities.iter().any(|x| x == c);
    let mut env = EnvSetup::default();
    match runner(meta) {
        Some(Runner::Pytest) => {
            let python = std::env::var_os("QUOD_PYTHON")
                .map(PathBuf::from)
                .or_else(|| venv_python(&dir))
                .or_else(|| venv_python(root));
            if !cap(CAP_PY_ENVIRONMENT) {
                env.note = Some("the map has no environment fingerprint (path mode)".into());
            } else if python.is_none() {
                env.note = Some(
                    "no test environment found (.venv); set QUOD_PYTHON for environment mode"
                        .into(),
                );
            } else {
                env.python = python;
            }
        }
        Some(Runner::Vitest | Runner::Jest) => {
            let cli = std::env::var_os("QUOD_JS").map(PathBuf::from).or_else(|| {
                [&dir, root]
                    .iter()
                    .map(|d| d.join("node_modules/quod-js/src/cli.mjs"))
                    .find(|p| p.is_file())
            });
            env.quod_js = cli.clone();
            match (cap(CAP_JS_ENVIRONMENT), cli) {
                (false, _) => {
                    env.note = Some("the map has no environment fingerprint (path mode)".into());
                }
                (true, None) => {
                    env.note = Some(
                        "quod-js not found (node_modules/quod-js); set QUOD_JS for environment mode"
                            .into(),
                    );
                }
                (true, Some(cli)) => {
                    env.fingerprint_cmd = Some(vec![
                        "node".into(),
                        cli.to_string_lossy().into_owned(),
                        "fingerprint".into(),
                        "--root".into(),
                        root.to_string_lossy().into_owned(),
                        "--dir".into(),
                        dir.to_string_lossy().into_owned(),
                    ]);
                }
            }
        }
        None => env.note = Some("unknown probe (path mode)".into()),
    }
    env
}

/// The recorded invocation again, with `skip_file`: every test runs except
/// those the skip file names. New and unrecorded tests always run, and the
/// probe re-checks the skip file's conditions (environment, invocation)
/// before skipping anything.
pub fn run_command(
    root: &Path,
    meta: &MapMeta,
    env: &EnvSetup,
    skip_file: &Path,
) -> Option<RunCommand> {
    let cwd = invocation_dir(meta);
    let skip = skip_file.to_string_lossy().into_owned();
    let args = recorded_args(meta);
    let argv: Vec<String> = match runner(meta)? {
        Runner::Pytest => {
            let python = env
                .python
                .as_ref()
                .map_or_else(|| "python".into(), |p| p.to_string_lossy().into_owned());
            [python, "-m".into(), "pytest".into()]
                .into_iter()
                .chain(args)
                .chain([format!("--quod-skip={skip}")])
                .collect()
        }
        r @ (Runner::Vitest | Runner::Jest) => {
            let cli = env.quod_js.as_ref().map_or_else(
                || "node_modules/quod-js/src/cli.mjs".into(),
                |p| p.to_string_lossy().into_owned(),
            );
            let runner = if r == Runner::Jest { "jest" } else { "vitest" };
            [
                "node".into(),
                cli,
                "run".into(),
                "--skip".into(),
                skip,
                "--runner".into(),
                runner.into(),
                "--root".into(),
                root.to_string_lossy().into_owned(),
                "--dir".into(),
                root.join(&cwd).to_string_lossy().into_owned(),
                "--".into(),
            ]
            .into_iter()
            .chain(args)
            .collect()
        }
    };
    let shell = argv
        .iter()
        .map(|a| sh_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    Some(RunCommand { cwd, argv, shell })
}

fn sh_quote(a: &str) -> String {
    let plain = !a.is_empty()
        && a.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./=:,@+%".contains(&b));
    if plain {
        a.to_owned()
    } else {
        format!("'{}'", a.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(probe: &str, inv: &[(&str, &str)]) -> MapMeta {
        MapMeta {
            probes: vec![probe.into()],
            invocation: inv
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect(),
            ..MapMeta::default()
        }
    }

    #[test]
    fn pytest_command_repeats_the_recorded_invocation() {
        let m = meta(
            "python-probe/0.3.0",
            &[
                ("invocation.args", r#"["-q", "tests"]"#),
                ("invocation.dir", "probes/python"),
            ],
        );
        let env = EnvSetup {
            python: Some(PathBuf::from("/r/.venv/bin/python")),
            ..EnvSetup::default()
        };
        let c = run_command(
            Path::new("/r"),
            &m,
            &env,
            Path::new("/r/.git/quod/skip.txt"),
        )
        .unwrap();
        assert_eq!(c.cwd, "probes/python");
        assert_eq!(
            c.argv,
            [
                "/r/.venv/bin/python",
                "-m",
                "pytest",
                "-q",
                "tests",
                "--quod-skip=/r/.git/quod/skip.txt"
            ]
        );
    }

    #[test]
    fn js_command_uses_the_recorded_runner() {
        let m = meta(
            "js-probe/0.1.0",
            &[
                ("invocation.args", r#"["--silent"]"#),
                ("invocation.runner", "jest"),
                ("invocation.dir", "."),
            ],
        );
        let c = run_command(Path::new("/r"), &m, &EnvSetup::default(), Path::new("/s")).unwrap();
        assert_eq!(c.argv[0], "node");
        let at = |flag: &str| c.argv[c.argv.iter().position(|a| a == flag).unwrap() + 1].clone();
        assert_eq!(at("--runner"), "jest");
        assert_eq!(at("--skip"), "/s");
        assert_eq!(c.argv.last().unwrap(), "--silent");
    }

    #[test]
    fn unsafe_invocation_dirs_fall_back_to_the_root() {
        for d in ["../elsewhere", "/abs", ""] {
            assert_eq!(
                invocation_dir(&meta("python-probe/0.3.0", &[("invocation.dir", d)])),
                "."
            );
        }
    }

    #[test]
    fn quoting() {
        assert_eq!(sh_quote("a=b/c"), "a=b/c");
        assert_eq!(sh_quote("it's x"), r"'it'\''s x'");
    }
}
