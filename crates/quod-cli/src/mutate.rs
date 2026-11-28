//! `quod mutate`: mutation testing scoped to a change (ADR 025).
//!
//! For each function a change touched, small valid edits (mutants) are
//! applied one at a time, and only the tests that execute the function run.
//! A mutant that no test notices *survives*: the changed logic isn't checked
//! by any test. The footprint map is what makes this cheap: a function's
//! observers are a handful of tests, not the suite.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use quod_change::{FileStatus, Repo};
use quod_model::{NodeKind, TestFlags, names};

use crate::{SelectOptions, run_select};

/// Inputs of `quod mutate`.
pub struct MutateOptions {
    /// The plan's inputs (map, policy, environment).
    pub select: SelectOptions,
    /// Interpreter that lists mutants (`python -m quod_probe mutants`).
    pub python: PathBuf,
    /// The test command (pytest); Quod appends `--quod-only` or `--quod-skip`.
    pub command: Vec<String>,
    /// At most this many mutants in all.
    pub max_mutants: usize,
    /// Per test run.
    pub timeout: Duration,
}

/// One mutant and what happened to it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Mutant {
    /// 1-based line.
    pub line: u32,
    /// 0-based character column where the token starts.
    pub col: u32,
    /// 0-based character column just past the token.
    pub end: u32,
    /// The token.
    pub old: String,
    /// Its replacement.
    pub new: String,
    /// `killed`, `survived`, `timeout` (counts as killed) or `no-tests`.
    #[serde(default)]
    pub outcome: String,
    /// The mutated line, trimmed.
    #[serde(default)]
    pub source: String,
}

/// Per changed function.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FunctionReport {
    /// Repository-relative file.
    pub path: String,
    /// Qualified name, as the probe records it.
    pub qual: String,
    /// How the tests were chosen: `observers` (the recorded tests that run
    /// it) or `plan` (the plan's tests, when the change also edits tests).
    pub tests: String,
    /// How many tests each mutant ran against.
    pub test_count: usize,
    /// Set when the tests fail without any mutant: nothing was measured.
    pub baseline_failed: bool,
    /// The mutants tried, with their outcomes.
    pub mutants: Vec<Mutant>,
}

/// The whole run.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct MutateReport {
    /// One entry per changed function with mutants.
    pub functions: Vec<FunctionReport>,
    /// Mutants not tried because of `max_mutants`.
    pub not_tried: usize,
}

impl MutateReport {
    /// Mutants with this outcome.
    pub fn count(&self, outcome: &str) -> usize {
        self.functions
            .iter()
            .flat_map(|f| &f.mutants)
            .filter(|m| m.outcome == outcome)
            .count()
    }

    /// The card section (Markdown).
    pub fn markdown(&self) -> String {
        let killed = self.count("killed") + self.count("timeout");
        let survived = self.count("survived");
        let mut out = format!(
            "### Quod mutation check · {killed} killed · {survived} survived\n\n\
             Small edits to the functions this change touched, each run against the tests that execute the function. \
             A surviving mutant is logic no test checks.\n"
        );
        if self.functions.is_empty() {
            out.push_str("\nNo changed functions to mutate.\n");
        }
        for f in &self.functions {
            let k = f
                .mutants
                .iter()
                .filter(|m| m.outcome == "killed" || m.outcome == "timeout")
                .count();
            out.push_str(&format!(
                "\n**`{}::{}`** · {} test{} ({}) · {}/{} killed\n",
                f.path,
                f.qual,
                f.test_count,
                if f.test_count == 1 { "" } else { "s" },
                f.tests,
                k,
                f.mutants.len()
            ));
            if f.baseline_failed {
                out.push_str("- the tests fail without any mutant: not measured\n");
                continue;
            }
            for m in f
                .mutants
                .iter()
                .filter(|m| m.outcome == "survived" || m.outcome == "no-tests")
            {
                let why = if m.outcome == "no-tests" {
                    " (no test ran)"
                } else {
                    ""
                };
                out.push_str(&format!(
                    "- line {}: `{}` → `{}` survived{why}: `{}`\n",
                    m.line, m.old, m.new, m.source
                ));
            }
        }
        if self.not_tried > 0 {
            out.push_str(&format!(
                "\n{} more mutants not tried (limit).\n",
                self.not_tried
            ));
        }
        out
    }
}

/// Puts the original file back when dropped, whatever happened.
struct Restore {
    path: PathBuf,
    original: Vec<u8>,
}

impl Drop for Restore {
    fn drop(&mut self) {
        let _ = fs::write(&self.path, &self.original);
    }
}

/// Runs the mutation check for the change since the map's commit.
pub fn run_mutate(repo: &Repo, opts: &MutateOptions) -> Result<MutateReport> {
    if opts.command.is_empty() {
        bail!("no test command");
    }
    let res = run_select(repo, &opts.select)?;
    let graph = &res.graph;
    let tests_changed = res
        .change
        .files
        .iter()
        .any(|f| res.policy.is_test_path(&f.change.path));
    let dir = std::env::temp_dir().join(format!("quod-mutate-{}", std::process::id()));
    fs::create_dir_all(&dir)?;
    let plan_skip = dir.join("plan-skip.txt");
    let mut text: String = res
        .skip_header
        .iter()
        .chain(res.skip_names.iter())
        .map(|l| format!("{l}\n"))
        .collect();
    if text.is_empty() {
        text.push('\n');
    }
    fs::write(&plan_skip, text)?;

    // Changed functions in source (not test) files.
    let mut todo: Vec<(String, Vec<String>)> = Vec::new();
    for f in &res.change.files {
        let path = &f.change.path;
        if f.change.status == FileStatus::Deleted
            || !path.ends_with(".py")
            || res.policy.is_test_path(path)
        {
            continue;
        }
        let Some(src) = &f.source else { continue };
        if src.parse_error {
            continue;
        }
        let quals: Vec<String> = src
            .entities
            .iter()
            .filter(|q| !src.removed.contains(q))
            .cloned()
            .collect();
        if !quals.is_empty() {
            todo.push((path.clone(), quals));
        }
    }

    let mut report = MutateReport::default();
    let mut budget = opts.max_mutants;
    for (path, quals) in todo {
        let file = repo.root().join(&path);
        let listed = list_mutants(&opts.python, &file, &quals)?;
        for qual in quals {
            let mut mutants = listed.get(&qual).cloned().unwrap_or_default();
            if mutants.is_empty() {
                continue;
            }
            // The tests to run: the recorded tests that execute the function,
            // unless the change edits tests (new tests aren't in the map).
            let node = graph.find_node(NodeKind::Function, &names::function(&path, &qual));
            let observers: Vec<String> = node
                .map(|n| {
                    graph
                        .observers(n)
                        .filter(|&t| !graph.test(t).flags.contains(TestFlags::UNTRACED))
                        .map(|t| graph.test_name(t).to_owned())
                        .collect()
                })
                .unwrap_or_default();
            let (how, extra, count) = if !tests_changed && !observers.is_empty() {
                let only = dir.join("only.txt");
                fs::write(&only, observers.join("\n") + "\n")?;
                (
                    "observers",
                    format!("--quod-only={}", only.display()),
                    observers.len(),
                )
            } else {
                (
                    "plan",
                    format!("--quod-skip={}", plan_skip.display()),
                    res.selection.run.len(),
                )
            };
            let mut fr = FunctionReport {
                path: path.clone(),
                qual: qual.clone(),
                tests: how.into(),
                test_count: count,
                baseline_failed: false,
                mutants: Vec::new(),
            };
            if count == 0 {
                for m in &mut mutants {
                    m.outcome = "no-tests".into();
                }
                fr.mutants = mutants;
                report.functions.push(fr);
                continue;
            }
            let run = |extra: &str| run_tests(repo.root(), &opts.command, extra, opts.timeout);
            if run(&extra)? != Outcome::Passed {
                fr.baseline_failed = true;
                report.functions.push(fr);
                continue;
            }
            let original =
                fs::read(&file).with_context(|| format!("reading {}", file.display()))?;
            let text = String::from_utf8(original.clone()).context("source isn't UTF-8")?;
            for mut m in mutants {
                if budget == 0 {
                    report.not_tried += 1;
                    continue;
                }
                budget -= 1;
                let (mutated, line) = apply(&text, &m)?;
                m.source = line.trim().to_owned();
                let _restore = Restore {
                    path: file.clone(),
                    original: original.clone(),
                };
                fs::write(&file, mutated)?;
                m.outcome = match run(&extra)? {
                    Outcome::Passed => "survived",
                    Outcome::Failed => "killed",
                    Outcome::Timeout => "timeout",
                    Outcome::NoTests => "no-tests",
                }
                .into();
                fr.mutants.push(m);
            }
            report.functions.push(fr);
        }
    }
    let _ = fs::remove_dir_all(&dir);
    Ok(report)
}

fn list_mutants(
    python: &Path,
    file: &Path,
    quals: &[String],
) -> Result<BTreeMap<String, Vec<Mutant>>> {
    let out = Command::new(python)
        .args(["-m", "quod_probe", "mutants"])
        .arg(file)
        .args(quals)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running {} -m quod_probe mutants", python.display()))?;
    if !out.status.success() {
        bail!(
            "listing mutants failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    serde_json::from_slice(&out.stdout).context("parsing the mutants")
}

/// The file with mutant `m` applied, and the mutated line.
fn apply(text: &str, m: &Mutant) -> Result<(String, String)> {
    let mut lines: Vec<&str> = text.split_inclusive('\n').collect();
    let i = (m.line as usize).checked_sub(1).context("line 0")?;
    let line = *lines.get(i).context("mutant past the end of the file")?;
    let chars: Vec<char> = line.chars().collect();
    let (c, e) = (m.col as usize, m.end as usize);
    if e > chars.len() || chars[c..e].iter().collect::<String>() != m.old {
        bail!("mutant doesn't match the source at line {}", m.line);
    }
    let new_line: String = chars[..c]
        .iter()
        .chain(m.new.chars().collect::<Vec<_>>().iter())
        .chain(chars[e..].iter())
        .collect();
    let owned = new_line.clone();
    lines[i] = &owned;
    Ok((lines.concat(), new_line))
}

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Passed,
    Failed,
    Timeout,
    NoTests,
}

fn run_tests(root: &Path, command: &[String], extra: &str, timeout: Duration) -> Result<Outcome> {
    let mut child = Command::new(&command[0])
        .args(&command[1..])
        .arg(extra)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("running {:?}", command[0]))?;
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(match status.code() {
                Some(0) => Outcome::Passed,
                Some(5) => Outcome::NoTests, // pytest: no tests collected
                _ => Outcome::Failed,
            });
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(Outcome::Timeout);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(line: u32, col: u32, end: u32, old: &str, new: &str) -> Mutant {
        Mutant {
            line,
            col,
            end,
            old: old.into(),
            new: new.into(),
            outcome: String::new(),
            source: String::new(),
        }
    }

    #[test]
    fn apply_replaces_one_token_and_keeps_line_endings() {
        let text = "def f(x):\r\n    return x <= 1  # é\r\n";
        let (out, line) = apply(text, &m(2, 13, 15, "<=", ">")).unwrap();
        assert_eq!(out, "def f(x):\r\n    return x > 1  # é\r\n");
        assert_eq!(line.trim(), "return x > 1  # é");
    }

    #[test]
    fn apply_refuses_a_mismatched_mutant() {
        assert!(apply("x = 1\n", &m(1, 0, 1, "y", "z")).is_err());
    }
}
