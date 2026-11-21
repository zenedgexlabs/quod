//! End to end without Python: git repo + synthetic capture → ingest → change →
//! select → sign → independent verification.

use std::fs;
use std::path::Path;
use std::process::Command;

use sc_cert::{Ed25519Signer, Signer};
use sc_change::Repo;
use sc_cli::{SelectOptions, ingest, run_select};
use sc_graph::{FootprintGraph, MapMeta};
use skipcert_verify::{VerifyConfig, verify};

fn git(dir: &Path, args: &[&str]) {
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

const BILLING: &str = "def charge(x):\n    return x * 2\n\ndef refund(x):\n    return -x\n";

fn capture() -> String {
    [
        r#"{"type":"header","v":1,"probe":"python-probe/0.1.0","python":"3.13.0","implementation":"cpython","repo_root":"/r"}"#,
        r#"{"type":"package","name":"requests","version":"2.31.0","native":false,"dist_hash":"0000000000000000000000000000000000000000000000000000000000000001"}"#,
        r#"{"type":"test","name":"tests/test_pay.py::test_charge","path":"tests/test_pay.py","qual":"test_charge","params":"","outcome":"passed","duration_ms":1200,"files":["app/billing.py","tests/test_pay.py"],"functions":[["app/billing.py","charge"],["tests/test_pay.py","test_charge"]],"packages":[],"flags":[]}"#,
        r#"{"type":"test","name":"tests/test_http.py::test_get","path":"tests/test_http.py","qual":"test_get","params":"","outcome":"passed","duration_ms":800,"files":["tests/test_http.py"],"functions":[["tests/test_http.py","test_get"]],"packages":["requests"],"flags":[]}"#,
        r#"{"type":"test","name":"tests/test_misc.py::test_ok","path":"tests/test_misc.py","qual":"test_ok","params":"","outcome":"passed","duration_ms":500,"files":["tests/test_misc.py"],"functions":[["tests/test_misc.py","test_ok"]],"packages":[],"flags":[]}"#,
        r#"{"type":"test","name":"tests/test_misc.py::test_broken","path":"tests/test_misc.py","qual":"test_broken","params":"","outcome":"failed","duration_ms":10,"files":["tests/test_misc.py"],"functions":[],"packages":[],"flags":[]}"#,
        r#"{"type":"root","files":["conftest.py"],"functions":[],"packages":[]}"#,
    ]
    .join("\n")
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
    map: std::path::PathBuf,
    key: std::path::PathBuf,
    signer: Ed25519Signer,
}

fn setup() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    fs::create_dir_all(root.join("app")).unwrap();
    fs::create_dir_all(root.join("tests")).unwrap();
    fs::write(root.join("app/billing.py"), BILLING).unwrap();
    fs::write(
        root.join("tests/test_pay.py"),
        "def test_charge():\n    pass\n",
    )
    .unwrap();
    fs::write(
        root.join("tests/test_http.py"),
        "def test_get():\n    pass\n",
    )
    .unwrap();
    fs::write(
        root.join("tests/test_misc.py"),
        "def test_ok():\n    pass\n\ndef test_broken():\n    assert 0\n",
    )
    .unwrap();
    fs::write(root.join("conftest.py"), "\n").unwrap();
    fs::write(root.join("requirements.txt"), "requests==2.31.0\n").unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "base"]);

    let cap = dir.path().join("capture.jsonl");
    fs::write(&cap, capture()).unwrap();
    let repo = Repo::discover(&root).unwrap();
    let meta = MapMeta {
        repo: "acme/app".into(),
        base_commit: repo.rev_parse("HEAD").unwrap(),
        engine: "test".into(),
        probes: vec![],
    };
    let graph = ingest(&[cap], repo.root(), meta).unwrap();
    assert_eq!(graph.meta().probes, vec!["python-probe/0.1.0".to_string()]);
    let (bytes, _) = graph.encode().unwrap();
    let map = dir.path().join("map.scmap");
    fs::write(&map, bytes).unwrap();

    let signer = Ed25519Signer::generate().unwrap();
    let key = dir.path().join("key.hex");
    fs::write(&key, signer.seed_hex()).unwrap();
    Fixture {
        root,
        map,
        key,
        signer,
        _dir: dir,
    }
}

fn select(f: &Fixture) -> sc_cli::SelectResult {
    let repo = Repo::discover(&f.root).unwrap();
    run_select(
        &repo,
        &SelectOptions {
            map: f.map.clone(),
            head: None,
            policy: None,
            key: Some(f.key.clone()),
            advisory: false,
            repo_id: "acme/app".into(),
        },
    )
    .unwrap()
}

fn verify_ok(f: &Fixture, res: &sc_cli::SelectResult) {
    let (graph, root) = FootprintGraph::decode(&fs::read(&f.map).unwrap()).unwrap();
    let cfg = VerifyConfig {
        trusted_keys: vec![f.signer.key_id()],
        ..VerifyConfig::default()
    };
    let report = verify(&res.certificates, &graph, root, &cfg);
    assert!(report.ok(), "{:?}", report.violations);
}

fn run_names(res: &sc_cli::SelectResult) -> Vec<String> {
    let mut v: Vec<String> = res.report.run.iter().map(|r| r.name.clone()).collect();
    v.sort();
    v
}

#[test]
fn function_edit_runs_only_its_test_and_skips_are_verified() {
    let f = setup();
    fs::write(
        f.root.join("app/billing.py"),
        BILLING.replace("x * 2", "x * 3"),
    )
    .unwrap();
    let res = select(&f);
    assert_eq!(
        run_names(&res),
        vec![
            "tests/test_misc.py::test_broken",
            "tests/test_pay.py::test_charge"
        ]
    );
    assert_eq!(res.report.skipped, 2);
    assert_eq!(res.report.skipped_ms, 1300);
    assert!(res.change.tree.is_some(), "dirty tree is pinned");
    assert!(
        res.skip_names
            .contains(&"tests/test_http.py::test_get".to_string())
    );
    verify_ok(&f, &res);
}

#[test]
fn unexecuted_function_edit_skips_everything_passing() {
    let f = setup();
    fs::write(
        f.root.join("app/billing.py"),
        BILLING.replace("-x", "0 - x"),
    )
    .unwrap();
    let res = select(&f);
    // Only the test that failed during recording runs.
    assert_eq!(run_names(&res), vec!["tests/test_misc.py::test_broken"]);
    verify_ok(&f, &res);
}

#[test]
fn dependency_bump_runs_only_package_users() {
    let f = setup();
    fs::write(f.root.join("requirements.txt"), "requests==2.32.0\n").unwrap();
    let res = select(&f);
    assert_eq!(
        run_names(&res),
        vec![
            "tests/test_http.py::test_get",
            "tests/test_misc.py::test_broken"
        ]
    );
    verify_ok(&f, &res);
}

#[test]
fn conftest_change_runs_everything() {
    let f = setup();
    fs::write(f.root.join("conftest.py"), "import os\n").unwrap();
    let res = select(&f);
    assert_eq!(res.report.skipped, 0);
    assert!(res.report.global.iter().any(|g| g.starts_with("R002")));
}

#[test]
fn committed_change_and_stale_map() {
    let f = setup();
    fs::write(
        f.root.join("app/billing.py"),
        BILLING.replace("x * 2", "x * 4"),
    )
    .unwrap();
    git(&f.root, &["commit", "-q", "-am", "edit"]);
    let res = select(&f);
    assert!(res.change.tree.is_none());
    assert_eq!(res.report.skipped, 2);

    let strict = f.root.join("skipcert.toml");
    fs::write(&strict, "[policy]\nmax_staleness = 0\n").unwrap();
    git(&f.root, &["add", "-A"]);
    git(&f.root, &["commit", "-q", "-m", "policy"]);
    let res = select(&f);
    assert_eq!(
        res.report.skipped, 0,
        "map is 2 commits behind with max_staleness 0"
    );
}

#[test]
fn advisory_output_is_not_accepted_by_the_verifier() {
    let f = setup();
    fs::write(
        f.root.join("app/billing.py"),
        BILLING.replace("x * 2", "x * 3"),
    )
    .unwrap();
    let repo = Repo::discover(&f.root).unwrap();
    let res = run_select(
        &repo,
        &SelectOptions {
            map: f.map.clone(),
            advisory: true,
            repo_id: "acme/app".into(),
            ..SelectOptions::default()
        },
    )
    .unwrap();
    assert!(res.report.advisory);
    let (graph, root) = FootprintGraph::decode(&fs::read(&f.map).unwrap()).unwrap();
    let cfg = VerifyConfig {
        trusted_keys: vec![f.signer.key_id()],
        ..VerifyConfig::default()
    };
    assert!(!verify(&res.certificates, &graph, root, &cfg).ok());
}

#[test]
fn missing_key_without_advisory_is_an_error() {
    let f = setup();
    let repo = Repo::discover(&f.root).unwrap();
    let err = run_select(
        &repo,
        &SelectOptions {
            map: f.map.clone(),
            repo_id: "r".into(),
            ..SelectOptions::default()
        },
    );
    assert!(err.is_err());
}
