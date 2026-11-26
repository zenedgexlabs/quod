//! End to end without Python: git repo + synthetic capture → ingest → change →
//! select → sign → independent verification.

use std::fs;
use std::path::Path;
use std::process::Command;

use quod_cert::{Ed25519Signer, Signer};
use quod_change::Repo;
use quod_cli::{SelectOptions, ingest, run_select};
use quod_graph::{FootprintGraph, MapMeta};
use quod_verify::{VerifyConfig, verify};

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
    capture_with(&[])
}

fn capture_with(capabilities: &[&str]) -> String {
    let header = format!(
        r#"{{"type":"header","v":1,"probe":"python-probe/0.1.0","capabilities":{},"python":"3.13.0","implementation":"cpython","repo_root":"/r"}}"#,
        serde_json::to_string(capabilities).unwrap()
    );
    [
        header.as_str(),
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
    setup_with(&[])
}

fn setup_with(capabilities: &[&str]) -> Fixture {
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
    fs::write(&cap, capture_with(capabilities)).unwrap();
    let repo = Repo::discover(&root).unwrap();
    let meta = MapMeta {
        repo: "acme/app".into(),
        base_commit: repo.rev_parse("HEAD").unwrap(),
        engine: "test".into(),
        probes: vec![],
        capabilities: vec![],
        ..MapMeta::default()
    };
    let graph = ingest(&[cap], repo.root(), meta).unwrap();
    assert_eq!(graph.meta().probes, vec!["python-probe/0.1.0".to_string()]);
    let (bytes, _) = graph.encode().unwrap();
    let map = dir.path().join("map.qmap");
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

fn select(f: &Fixture) -> quod_cli::SelectResult {
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
            python: None,
            fingerprint_cmd: None,
        },
    )
    .unwrap()
}

fn verify_ok(f: &Fixture, res: &quod_cli::SelectResult) {
    let (graph, root) = FootprintGraph::decode(&fs::read(&f.map).unwrap()).unwrap();
    let cfg = VerifyConfig {
        trusted_keys: vec![f.signer.key_id()],
        ..VerifyConfig::default()
    };
    let report = verify(&res.certificates, &graph, root, &cfg);
    assert!(report.ok(), "{:?}", report.violations);
}

fn run_names(res: &quod_cli::SelectResult) -> Vec<String> {
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

    let strict = f.root.join("quod.toml");
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
            python: None,
            fingerprint_cmd: None,
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

/// A new keyword parameter with a constant default on a function no test
/// executed. With the introspection capability the header change belongs to
/// `refund` alone. Without it, the module changed and importers re-run.
#[test]
fn header_split_requires_the_introspection_capability() {
    let edit = BILLING.replace("def refund(x):", "def refund(x, fee=0):");

    let old = setup();
    fs::write(old.root.join("app/billing.py"), &edit).unwrap();
    assert!(run_names(&select(&old)).contains(&"tests/test_pay.py::test_charge".to_string()));

    let new = setup_with(&["py.introspection"]);
    fs::write(new.root.join("app/billing.py"), &edit).unwrap();
    let res = select(&new);
    assert_eq!(run_names(&res), vec!["tests/test_misc.py::test_broken"]);
    verify_ok(&new, &res);
}

#[test]
fn map_capabilities_are_the_intersection_of_its_captures() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.jsonl");
    let b = dir.path().join("b.jsonl");
    fs::write(&a, capture_with(&["py.introspection", "x.other"])).unwrap();
    fs::write(&b, capture_with(&["py.introspection"])).unwrap();
    let g = ingest(&[a.clone(), b], dir.path(), MapMeta::default()).unwrap();
    assert_eq!(g.meta().capabilities, vec!["py.introspection".to_string()]);

    let old = dir.path().join("old.jsonl");
    fs::write(&old, capture()).unwrap();
    let g = ingest(&[a, old], dir.path(), MapMeta::default()).unwrap();
    assert!(g.meta().capabilities.is_empty());
}

const H1: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const H2: &str = "2222222222222222222222222222222222222222222222222222222222222222";

fn recorded_environment() -> serde_json::Value {
    serde_json::json!({
        "interpreter.version": "3.13.0",
        "system.openssl": "OpenSSL 3.0",
        "dist.requests": "2.31.0 aa",
        "dist.__site__": "ss",
    })
}

/// A map recorded by a `py.environment` probe: test_get reads `API_URL`.
fn setup_env_mode() -> Fixture {
    let f = setup_with(&["py.environment", "py.introspection"]);
    let header = serde_json::json!({
        "type": "header", "v": 1, "probe": "python-probe/0.3.0",
        "capabilities": ["py.environment", "py.introspection"],
        "environment": recorded_environment(),
        "invocation": {"invocation.args": "[]"},
        "environment_digest": "envdigest",
        "invocation_digest": "invdigest",
    });
    let cap: Vec<String> = capture()
        .lines()
        .enumerate()
        .map(|(i, l)| {
            if i == 0 {
                header.to_string()
            } else if l.contains("test_get") {
                l.replace(
                    r#""flags":[]}"#,
                    &format!(r#""flags":[],"env":[["API_URL","{H1}"]]}}"#),
                )
            } else {
                l.to_owned()
            }
        })
        .collect();
    let dir = f.map.parent().unwrap().to_path_buf();
    let capf = dir.join("capture-env.jsonl");
    fs::write(&capf, cap.join("\n")).unwrap();
    let repo = Repo::discover(&f.root).unwrap();
    let meta = MapMeta {
        repo: "acme/app".into(),
        base_commit: repo.rev_parse("HEAD").unwrap(),
        engine: "test".into(),
        ..MapMeta::default()
    };
    let graph = ingest(&[capf], repo.root(), meta).unwrap();
    assert!(
        graph
            .meta()
            .capabilities
            .contains(&"py.environment".to_string())
    );
    fs::write(&f.map, graph.encode().unwrap().0).unwrap();
    f
}

/// A stand-in interpreter that prints a fixed fingerprint.
fn fake_python(f: &Fixture, environment: serde_json::Value, api_url: &str) -> std::path::PathBuf {
    let dir = f.map.parent().unwrap();
    let json = dir.join("fp.json");
    fs::write(
        &json,
        serde_json::json!({"probe": "x", "environment": environment, "env": {"API_URL": api_url}})
            .to_string(),
    )
    .unwrap();
    if cfg!(windows) {
        let p = dir.join("python.cmd");
        fs::write(&p, format!("@type \"{}\"\r\n", json.display())).unwrap();
        p
    } else {
        let p = dir.join("python.sh");
        fs::write(&p, format!("#!/bin/sh\ncat '{}'\n", json.display())).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }
}

fn select_with(f: &Fixture, python: Option<std::path::PathBuf>) -> quod_cli::SelectResult {
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
            python,
            fingerprint_cmd: None,
        },
    )
    .unwrap()
}

#[test]
fn environment_mode_replaces_ci_rules_and_binds_the_environment() {
    let f = setup_env_mode();
    fs::create_dir_all(f.root.join(".github/workflows")).unwrap();
    fs::write(f.root.join(".github/workflows/tests.yaml"), "on: push\n").unwrap();

    // Path mode: a test workflow changed, so everything runs.
    assert!(select_with(&f, None).report.global[0].starts_with("R001"));

    // Environment mode, same environment: the workflow edit is irrelevant.
    let py = fake_python(&f, recorded_environment(), H1);
    let res = select_with(&f, Some(py));
    assert!(res.report.global.is_empty(), "{:?}", res.report.global);
    assert_eq!(run_names(&res), vec!["tests/test_misc.py::test_broken"]);
    assert_eq!(
        res.skip_header,
        vec![
            "# quod-environment envdigest".to_string(),
            "# quod-invocation invdigest".to_string(),
            format!("# quod-env API_URL {H1}"),
        ]
    );
    let graph = FootprintGraph::decode(&fs::read(&f.map).unwrap())
        .unwrap()
        .0;
    let binding = graph.meta().environment_binding();
    assert!(binding.is_some());
    assert!(
        res.certificates
            .iter()
            .all(|c| c.body.inputs.environment == binding)
    );
    verify_ok(&f, &res);
}

#[test]
fn environment_mode_reruns_what_the_environment_change_reaches() {
    let f = setup_env_mode();
    // A variable test_get read changed.
    let res = select_with(&f, Some(fake_python(&f, recorded_environment(), H2)));
    assert_eq!(
        run_names(&res),
        vec![
            "tests/test_http.py::test_get",
            "tests/test_misc.py::test_broken"
        ]
    );
    verify_ok(&f, &res);

    // A distribution test_get uses changed.
    let mut env = recorded_environment();
    env["dist.requests"] = "2.32.0 bb".into();
    let res = select_with(&f, Some(fake_python(&f, env, H1)));
    assert_eq!(
        run_names(&res),
        vec![
            "tests/test_http.py::test_get",
            "tests/test_misc.py::test_broken"
        ]
    );

    // The interpreter changed: everything runs.
    let mut env = recorded_environment();
    env["interpreter.version"] = "3.13.1".into();
    let res = select_with(&f, Some(fake_python(&f, env, H1)));
    assert!(
        res.report.global[0].starts_with("R018"),
        "{:?}",
        res.report.global
    );
    assert_eq!(res.report.skipped, 0);
}

#[test]
fn verifier_rejects_a_forged_environment_binding() {
    let f = setup_env_mode();
    let mut res = select_with(&f, Some(fake_python(&f, recorded_environment(), H1)));
    let mut body = res.certificates[0].body.clone();
    body.inputs.environment = Some(quod_hash::hash_domain(
        quod_hash::Domain::Blob,
        b"elsewhere",
    ));
    body.prev = None;
    let mut forged = quod_model::Certificate::seal(body).unwrap();
    f.signer.sign(&mut forged);
    res.certificates = vec![forged];
    let (graph, root) = FootprintGraph::decode(&fs::read(&f.map).unwrap()).unwrap();
    let cfg = VerifyConfig {
        trusted_keys: vec![f.signer.key_id()],
        ..VerifyConfig::default()
    };
    let report = verify(&res.certificates, &graph, root, &cfg);
    assert!(
        report
            .violations
            .iter()
            .any(|v| v.reason.contains("binding: environment")),
        "{:?}",
        report.violations
    );
}

/// A git-ignored file a test read (generated data, an in-place build) can
/// change without showing up in any diff.
#[test]
fn ignored_observed_files_are_rehashed() {
    let f = setup();
    fs::write(f.root.join(".gitignore"), "gen/\n").unwrap();
    git(&f.root, &["add", "-A"]);
    git(&f.root, &["commit", "-q", "-m", "ignore gen"]);
    fs::create_dir_all(f.root.join("gen")).unwrap();
    fs::write(f.root.join("gen/data.txt"), "v1").unwrap();

    let cap = f.map.parent().unwrap().join("capture-gen.jsonl");
    fs::write(
        &cap,
        capture().replace(
            r#""files":["tests/test_misc.py"],"functions":[["tests/test_misc.py","test_ok"]]"#,
            r#""files":["gen/data.txt","tests/test_misc.py"],"functions":[["tests/test_misc.py","test_ok"]]"#,
        ),
    )
    .unwrap();
    let repo = Repo::discover(&f.root).unwrap();
    let meta = MapMeta {
        repo: "acme/app".into(),
        base_commit: repo.rev_parse("HEAD").unwrap(),
        engine: "test".into(),
        ..MapMeta::default()
    };
    let graph = ingest(&[cap], repo.root(), meta).unwrap();
    fs::write(&f.map, graph.encode().unwrap().0).unwrap();

    assert!(!run_names(&select(&f)).contains(&"tests/test_misc.py::test_ok".to_string()));
    fs::write(f.root.join("gen/data.txt"), "v2").unwrap();
    let res = select(&f);
    assert!(run_names(&res).contains(&"tests/test_misc.py::test_ok".to_string()));
    assert!(
        res.report
            .changed
            .contains(&"file:gen/data.txt".to_string()),
        "{:?}",
        res.report.changed
    );
    verify_ok(&f, &res);
}

const CALC_JS: &str = "export const RATE = 2;
export function add(a, b) {
  return a + b;
}
export const mul = (a, b) => {
  return a * b * RATE;
};
";

/// A JavaScript repository recorded by a line-reporting probe (ADR 013):
/// `test_add` executed only `add`, `test_mul` executed `mul` and the module
/// top level. Lines are mapped to entities at ingest.
fn setup_js() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("test")).unwrap();
    fs::write(root.join("src/calc.js"), CALC_JS).unwrap();
    fs::write(
        root.join("test/calc.test.js"),
        "test('add', () => {});\ntest('mul', () => {});\n",
    )
    .unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "base"]);
    let header = r#"{"type":"header","v":1,"probe":"js-probe/0.0.1","capabilities":[],"language":"javascript"}"#;
    let t_add = r#"{"type":"test","name":"test/calc.test.js::add","path":"test/calc.test.js","qual":"add","params":"","outcome":"passed","duration_ms":100,"files":[],"functions":[],"packages":[],"flags":[],"lines":[["src/calc.js",[[3,3]]],["test/calc.test.js",[[1,1]]]]}"#;
    let t_mul = r#"{"type":"test","name":"test/calc.test.js::mul","path":"test/calc.test.js","qual":"mul","params":"","outcome":"passed","duration_ms":100,"files":[],"functions":[],"packages":[],"flags":[],"lines":[["src/calc.js",[[1,1],[5,7]]],["test/calc.test.js",[[2,2]]]]}"#;
    let root_rec = r#"{"type":"root","files":[],"functions":[],"packages":[]}"#;
    let cap = dir.path().join("cap.jsonl");
    fs::write(&cap, [header, t_add, t_mul, root_rec].join("\n")).unwrap();
    let repo = Repo::discover(&root).unwrap();
    let meta = MapMeta {
        repo: "acme/web".into(),
        base_commit: repo.rev_parse("HEAD").unwrap(),
        engine: "test".into(),
        ..MapMeta::default()
    };
    let graph = ingest(&[cap], repo.root(), meta).unwrap();
    let map = dir.path().join("map.qmap");
    fs::write(&map, graph.encode().unwrap().0).unwrap();
    let key = dir.path().join("key.hex");
    fs::write(&key, Ed25519Signer::generate().unwrap().seed_hex()).unwrap();
    (dir, root, map, key)
}

fn select_js(root: &Path, map: &Path, key: &Path) -> Vec<String> {
    let repo = Repo::discover(root).unwrap();
    let res = run_select(
        &repo,
        &SelectOptions {
            map: map.to_path_buf(),
            key: Some(key.to_path_buf()),
            repo_id: "acme/web".into(),
            ..SelectOptions::default()
        },
    )
    .unwrap();
    run_names(&res)
}

#[test]
fn javascript_lines_map_to_functions() {
    let (_d, root, map, key) = setup_js();
    let graph = FootprintGraph::decode(&fs::read(&map).unwrap()).unwrap().0;
    assert!(
        graph
            .find_node(quod_model::NodeKind::Function, "src/calc.js::add")
            .is_some()
    );
    assert!(
        graph
            .find_node(quod_model::NodeKind::Function, "src/calc.js::mul")
            .is_some()
    );

    // A body edit in `add` runs only the test that executed it.
    fs::write(root.join("src/calc.js"), CALC_JS.replace("a + b", "b + a")).unwrap();
    assert_eq!(select_js(&root, &map, &key), vec!["test/calc.test.js::add"]);

    // A module-level edit runs the tests that evaluated the module's top level.
    fs::write(
        root.join("src/calc.js"),
        CALC_JS.replace("RATE = 2", "RATE = 3"),
    )
    .unwrap();
    assert_eq!(select_js(&root, &map, &key), vec!["test/calc.test.js::mul"]);

    // A dependency manifest without a delta parser runs everything (R006).
    fs::write(root.join("src/calc.js"), CALC_JS).unwrap();
    fs::write(root.join("package-lock.json"), "{}\n").unwrap();
    assert_eq!(
        select_js(&root, &map, &key),
        vec!["test/calc.test.js::add", "test/calc.test.js::mul"]
    );
}

/// Module-state widening (ADR 014), reproducing the date-fns escape: the
/// second test only reads the cache that the first test filled through
/// `calc`. A change to `calc` must re-run it.
#[test]
fn memoised_module_state_widens_to_the_whole_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    fs::create_dir_all(root.join("src")).unwrap();
    let src = "const cache = {};
export function lookup(k) {
  if (k in cache) return cache[k];
  return calc(k);
}
function calc(k) {
  return (cache[k] = k * 2);
}
export function other() {
  return 1;
}
";
    fs::write(root.join("src/memo.js"), src).unwrap();
    fs::write(
        root.join("src/plain.js"),
        "export function a() {\n  return 1;\n}\nexport function b() {\n  return 2;\n}\n",
    )
    .unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "base"]);
    let header =
        r#"{"type":"header","v":1,"probe":"js-probe/0.0.1","capabilities":[],"ecosystem":"npm"}"#;
    let first = r#"{"type":"test","name":"t.test.js::first","path":"t.test.js","qual":"first","params":"","outcome":"passed","duration_ms":1,"files":[],"functions":[],"packages":[],"flags":[],"positions":[["src/memo.js",[[2,7],[6,0]]]]}"#;
    let second = r#"{"type":"test","name":"t.test.js::second","path":"t.test.js","qual":"second","params":"","outcome":"passed","duration_ms":1,"files":[],"functions":[],"packages":[],"flags":[],"positions":[["src/memo.js",[[2,7]]],["src/plain.js",[[1,7]]]]}"#;
    let root_rec = r#"{"type":"root","files":[],"functions":[],"packages":[]}"#;
    let cap = dir.path().join("cap.jsonl");
    fs::write(&cap, [header, first, second, root_rec].join("\n")).unwrap();
    let repo = Repo::discover(&root).unwrap();
    let meta = MapMeta {
        repo: "acme/web".into(),
        base_commit: repo.rev_parse("HEAD").unwrap(),
        engine: "test".into(),
        ..MapMeta::default()
    };
    let graph = ingest(&[cap], repo.root(), meta).unwrap();
    let map = dir.path().join("map.qmap");
    fs::write(&map, graph.encode().unwrap().0).unwrap();
    let key = dir.path().join("key.hex");
    fs::write(&key, Ed25519Signer::generate().unwrap().seed_hex()).unwrap();

    // `second` never ran `calc`, but read what it cached: it re-runs.
    fs::write(root.join("src/memo.js"), src.replace("k * 2", "k * 3")).unwrap();
    assert_eq!(
        select_js(&root, &map, &key),
        vec!["t.test.js::first", "t.test.js::second"]
    );

    // A stateless file stays function-precise: `b` ran in no test.
    fs::write(root.join("src/memo.js"), src).unwrap();
    fs::write(
        root.join("src/plain.js"),
        "export function a() {\n  return 1;\n}\nexport function b() {\n  return 3;\n}\n",
    )
    .unwrap();
    assert!(select_js(&root, &map, &key).is_empty());
}

// Verdict (architecture v5) --------------------------------------------------

fn verdict(f: &Fixture, results: &[(&str, &str)]) -> quod_cli::VerdictOutput {
    let file = f.root.parent().unwrap().join("results.jsonl");
    let lines: Vec<String> = results
        .iter()
        .map(|(n, o)| {
            format!(r#"{{"type":"result","name":"{n}","outcome":"{o}","duration_ms":1}}"#)
        })
        .collect();
    fs::write(&file, lines.join("\n")).unwrap();
    let repo = Repo::discover(&f.root).unwrap();
    quod_cli::run_verdict(
        &repo,
        &SelectOptions {
            map: f.map.clone(),
            head: None,
            policy: None,
            key: Some(f.key.clone()),
            advisory: false,
            repo_id: "acme/app".into(),
            python: None,
            fingerprint_cmd: None,
        },
        &quod_cli::VerdictOptions {
            results: vec![file],
            author: quod_verdict::AuthorKind::Agent,
            agent: Some("test-agent".into()),
        },
    )
    .unwrap()
}

fn rules(v: &quod_verdict::Verdict) -> Vec<(String, String)> {
    v.findings
        .iter()
        .map(|f| (f.rule.clone(), f.subject.clone()))
        .collect()
}

/// Untested change, flake proof and a signed, verifiable envelope.
#[test]
fn verdict_finds_untested_changes_and_clears_unrelated_failures() {
    let f = setup();
    // `charge` is executed by test_charge; `refund` by no test.
    fs::write(
        f.root.join("app/billing.py"),
        BILLING.replace("x * 2", "x * 3").replace("-x", "0 - x"),
    )
    .unwrap();
    let out = verdict(
        &f,
        &[
            ("tests/test_pay.py::test_charge", "passed"),
            ("tests/test_misc.py::test_broken", "passed"),
            // Nothing test_ok depends on changed: not this change's fault.
            ("tests/test_misc.py::test_ok", "failed"),
        ],
    );
    let v = &out.verdict;
    assert_eq!(v.status, quod_verdict::Status::Pass, "{}", out.card);
    let r = rules(v);
    assert!(
        r.contains(&("U001".into(), "app/billing.py::refund".into())),
        "{r:?}"
    );
    assert!(
        !r.iter().any(|(_, s)| s == "app/billing.py::charge"),
        "{r:?}"
    );
    assert!(
        r.contains(&("F001".into(), "tests/test_misc.py::test_ok".into())),
        "{r:?}"
    );
    assert_eq!(v.obligations.failed_not_caused, 1);
    assert!(out.card.contains("Untested changes"), "{}", out.card);

    // The envelope verifies under the CI key and nothing else.
    let st = quod_verdict::verify(&out.envelope, &f.signer.key_id()).unwrap();
    assert_eq!(st.predicate, *v);
    let other = Ed25519Signer::generate().unwrap();
    assert!(quod_verdict::verify(&out.envelope, &other.key_id()).is_err());
    let mut tampered = out.envelope.clone();
    let mut body = quod_verdict::b64::decode(&tampered.payload).unwrap();
    let s = String::from_utf8(body.clone())
        .unwrap()
        .replace("\"fail\"", "\"pass\"")
        .replace("\"pass\"", "\"blocked\"");
    body = s.into_bytes();
    tampered.payload = quod_verdict::b64::encode(&body);
    assert!(quod_verdict::verify(&tampered, &f.signer.key_id()).is_err());
}

/// A failure in a test whose footprint the change touched is never cleared.
#[test]
fn verdict_fails_when_a_dependent_test_fails() {
    let f = setup();
    fs::write(
        f.root.join("app/billing.py"),
        BILLING.replace("x * 2", "x * 3"),
    )
    .unwrap();
    let out = verdict(
        &f,
        &[
            ("tests/test_pay.py::test_charge", "failed"),
            ("tests/test_misc.py::test_broken", "passed"),
        ],
    );
    assert_eq!(
        out.verdict.status,
        quod_verdict::Status::Fail,
        "{}",
        out.card
    );
    assert!(!rules(&out.verdict).iter().any(|(r, _)| r == "F001"));
}

/// An obligated test that didn't run blocks the Verdict.
#[test]
fn verdict_blocks_when_an_obligation_is_not_met() {
    let f = setup();
    fs::write(
        f.root.join("app/billing.py"),
        BILLING.replace("x * 2", "x * 3"),
    )
    .unwrap();
    let out = verdict(&f, &[("tests/test_misc.py::test_broken", "passed")]);
    assert_eq!(
        out.verdict.status,
        quod_verdict::Status::Blocked,
        "{}",
        out.card
    );
    assert!(
        rules(&out.verdict).contains(&("O001".into(), "tests/test_pay.py::test_charge".into()))
    );
}

/// A test edited together with the code it executes is flagged for review.
#[test]
fn verdict_flags_tests_edited_with_the_code_they_guard() {
    let f = setup();
    fs::write(
        f.root.join("app/billing.py"),
        BILLING.replace("x * 2", "x * 3"),
    )
    .unwrap();
    fs::write(
        f.root.join("tests/test_pay.py"),
        "def test_charge():\n    assert True\n",
    )
    .unwrap();
    let out = verdict(
        &f,
        &[
            ("tests/test_pay.py::test_charge", "passed"),
            ("tests/test_misc.py::test_broken", "passed"),
        ],
    );
    let r = rules(&out.verdict);
    assert!(
        r.contains(&("I001".into(), "tests/test_pay.py::test_charge".into())),
        "{r:?}"
    );
    assert_eq!(out.verdict.status, quod_verdict::Status::Pass);
    assert!(out.card.contains("Test integrity"), "{}", out.card);
}

/// The agent tools over MCP: advisory selection on the working tree.
#[test]
fn mcp_server_answers_agent_tools() {
    use serde_json::{Value, json};
    let f = setup();
    fs::write(
        f.root.join("app/billing.py"),
        BILLING.replace("x * 2", "x * 3"),
    )
    .unwrap();
    let ctx = quod_cli::mcp::McpContext {
        repo: Repo::discover(&f.root).unwrap(),
        map: f.map.clone(),
        policy: None,
        repo_id: "acme/app".into(),
    };
    let input = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "tests_for", "arguments": {}}}),
        json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "why", "arguments": {"test": "tests/test_misc.py::test_ok"}}}),
        json!({"jsonrpc": "2.0", "id": 5, "method": "nope"}),
    ]
    .iter()
    .map(Value::to_string)
    .collect::<Vec<_>>()
    .join("\n");
    let mut out = Vec::new();
    quod_cli::mcp::serve(&ctx, input.as_bytes(), &mut out).unwrap();
    let replies: Vec<Value> = String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(replies.len(), 5, "notifications get no reply");
    assert_eq!(replies[0]["result"]["serverInfo"]["name"], "quod");
    let names: Vec<&str> = replies[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["tests_for", "why", "culprit", "check"]);
    let text = |i: usize| -> Value {
        serde_json::from_str(replies[i]["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    };
    let plan = text(2);
    assert_eq!(plan["advisory"], true);
    let obligated: Vec<&str> = plan["obligated"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["test"].as_str().unwrap())
        .collect();
    assert!(
        obligated.contains(&"tests/test_pay.py::test_charge"),
        "{plan}"
    );
    assert!(
        !obligated.contains(&"tests/test_misc.py::test_ok"),
        "{plan}"
    );
    assert_eq!(text(3)["decision"], "skip");
    assert_eq!(replies[4]["error"]["code"], -32601);
}

/// The independent verifier accepts an honest Verdict (with its map and
/// certificates) and rejects signed forgeries: a false Flake-proof claim and
/// a status that doesn't follow from the findings.
#[test]
fn independent_verifier_checks_verdicts() {
    use quod_verify::{VerdictCheck, verify_verdict};
    let f = setup();
    fs::write(
        f.root.join("app/billing.py"),
        BILLING.replace("x * 2", "x * 3"),
    )
    .unwrap();
    let out = verdict(
        &f,
        &[
            ("tests/test_pay.py::test_charge", "passed"),
            ("tests/test_misc.py::test_broken", "passed"),
            ("tests/test_misc.py::test_ok", "failed"),
        ],
    );
    let (graph, root) = FootprintGraph::decode(&fs::read(&f.map).unwrap()).unwrap();
    let certs = select(&f).certificates;
    let cfg = VerifyConfig {
        trusted_keys: vec![f.signer.key_id()],
        ..VerifyConfig::default()
    };
    let check = VerdictCheck {
        cfg: &cfg,
        map: Some((&graph, root)),
        certs: Some(&certs),
    };
    let json = serde_json::to_string(&out.envelope).unwrap();
    let r = verify_verdict(&json, &check);
    assert!(r.ok(), "{:?}", r.violations);

    // Forgeries, sealed with the real key: only the content checks catch them.
    let forge = |edit: &dyn Fn(&mut quod_verdict::Verdict)| -> Vec<String> {
        let mut v = out.verdict.clone();
        edit(&mut v);
        let env = quod_verdict::seal(&quod_verdict::statement(v), Some(&f.signer)).unwrap();
        verify_verdict(&serde_json::to_string(&env).unwrap(), &check)
            .violations
            .into_iter()
            .map(|v| v.reason)
            .collect()
    };
    // test_charge executes the changed `charge`: it can't be "not caused".
    let reasons = forge(&|v| {
        v.findings.push(quod_verdict::Finding {
            kind: quod_verdict::Kind::FlakeProof,
            severity: quod_verdict::Severity::Info,
            rule: "F001".into(),
            subject: "tests/test_pay.py::test_charge".into(),
            detail: String::new(),
        });
        v.obligations.failed += 1;
        v.obligations.failed_not_caused += 1;
    });
    assert!(
        reasons
            .iter()
            .any(|r| r.contains("in the test's footprint")),
        "{reasons:?}"
    );
    // A failure nobody cleared, reported as a pass.
    let reasons = forge(&|v| v.obligations.failed += 1);
    assert!(reasons.iter().any(|r| r.contains("status")), "{reasons:?}");
    // Unsigned or wrongly keyed envelopes.
    let other = VerifyConfig {
        trusted_keys: vec![Ed25519Signer::generate().unwrap().key_id()],
        ..VerifyConfig::default()
    };
    let r = verify_verdict(
        &json,
        &VerdictCheck {
            cfg: &other,
            map: None,
            certs: None,
        },
    );
    assert!(!r.ok());
}

// Culprit (architecture v5, F5) ----------------------------------------------

fn commit_all(root: &Path, msg: &str) -> String {
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", msg]);
    Repo::discover(root).unwrap().rev_parse("HEAD").unwrap()
}

fn culprit(f: &Fixture, good: &str, bad: &str, failed: &[&str]) -> quod_cli::CulpritReport {
    quod_cli::run_culprit(
        &Repo::discover(&f.root).unwrap(),
        &quod_cli::CulpritOptions {
            map: f.map.clone(),
            good: good.into(),
            bad: bad.into(),
            policy: None,
            failed: failed.iter().map(|s| s.to_string()).collect(),
            python: None,
            fingerprint_cmd: None,
        },
    )
    .unwrap()
}

/// Names the one commit that changed what the failing test executes, rules
/// out the rest, and calls a failure no commit could explain "not code".
#[test]
fn culprit_names_the_commit_that_changed_what_the_test_runs() {
    use quod_verdict::CulpritOutcome;
    let f = setup();
    let repo = Repo::discover(&f.root).unwrap();
    let good = repo.rev_parse("HEAD").unwrap();
    fs::write(
        f.root.join("app/billing.py"),
        BILLING.replace("-x", "0 - x"),
    )
    .unwrap();
    let c1 = commit_all(&f.root, "refund tweak");
    fs::write(
        f.root.join("app/billing.py"),
        BILLING.replace("-x", "0 - x").replace("x * 2", "x * 3"),
    )
    .unwrap();
    let c2 = commit_all(&f.root, "charge more");
    fs::write(f.root.join("NOTES.md"), "notes\n").unwrap();
    let c3 = commit_all(&f.root, "notes");

    let r = culprit(
        &f,
        &good,
        &c3,
        &[
            "tests/test_pay.py::test_charge",
            "tests/test_misc.py::test_ok",
        ],
    );
    assert_eq!(r.commits.len(), 3);
    let charge = &r.findings[0];
    assert_eq!(charge.outcome, CulpritOutcome::Culprit, "{charge:?}");
    assert_eq!(charge.suspects.len(), 1);
    assert_eq!(charge.suspects[0].commit, c2);
    assert!(
        charge.suspects[0]
            .shared
            .iter()
            .any(|s| s.contains("charge")),
        "{charge:?}"
    );
    assert_eq!(charge.cleared, 2);
    assert_eq!(
        r.findings[1].outcome,
        CulpritOutcome::NotCode,
        "{:?}",
        r.findings[1]
    );

    // A map older than the green commit still works while the drift doesn't
    // touch the test's footprint (c1 changed only `refund`)...
    let r = culprit(&f, &c1, &c3, &["tests/test_pay.py::test_charge"]);
    assert_eq!(r.findings[0].outcome, CulpritOutcome::Culprit);
    // ...and refuses to claim anything once it does (c2 changed `charge`).
    let r = culprit(&f, &c2, &c3, &["tests/test_pay.py::test_charge"]);
    assert_eq!(
        r.findings[0].outcome,
        CulpritOutcome::Unknown,
        "{:?}",
        r.findings[0]
    );
}

/// A change in the root scope (conftest) can affect every test: it is a
/// suspect for all of them.
#[test]
fn culprit_keeps_root_scope_changes_as_suspects() {
    use quod_verdict::CulpritOutcome;
    let f = setup();
    let good = Repo::discover(&f.root).unwrap().rev_parse("HEAD").unwrap();
    fs::write(f.root.join("conftest.py"), "import os\n").unwrap();
    let c1 = commit_all(&f.root, "conftest");
    fs::write(
        f.root.join("app/billing.py"),
        BILLING.replace("x * 2", "x * 3"),
    )
    .unwrap();
    let c2 = commit_all(&f.root, "charge");
    let r = culprit(
        &f,
        &good,
        &c2,
        &[
            "tests/test_misc.py::test_ok",
            "tests/test_pay.py::test_charge",
        ],
    );
    let ok = &r.findings[0];
    assert_eq!(ok.outcome, CulpritOutcome::Culprit, "{ok:?}");
    assert_eq!(ok.suspects[0].commit, c1);
    let charge = &r.findings[1];
    assert_eq!(charge.outcome, CulpritOutcome::Suspects, "{charge:?}");
    assert_eq!(
        charge
            .suspects
            .iter()
            .map(|s| s.commit.as_str())
            .collect::<Vec<_>>(),
        [c1.as_str(), c2.as_str()]
    );
}

/// Without a map (none pulled yet) the agent hears "run everything", never
/// a skip, and tools that need a map say how to get one.
#[test]
fn mcp_without_a_map_runs_everything() {
    use serde_json::{Value, json};
    let f = setup();
    let ctx = quod_cli::mcp::McpContext {
        repo: Repo::discover(&f.root).unwrap(),
        map: f.root.join("no-such-map.qmap"),
        policy: None,
        repo_id: "acme/app".into(),
    };
    let input = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "tests_for", "arguments": {}}}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "why", "arguments": {"test": "tests/test_misc.py::test_ok"}}}),
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "check", "arguments": {"results": "r.jsonl"}}}),
    ]
    .iter()
    .map(Value::to_string)
    .collect::<Vec<_>>()
    .join("\n");
    let mut out = Vec::new();
    quod_cli::mcp::serve(&ctx, input.as_bytes(), &mut out).unwrap();
    let replies: Vec<Value> = String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let body = |i: usize| -> Value {
        serde_json::from_str(replies[i]["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    };
    assert_eq!(body(0)["run_everything"], true);
    assert_eq!(replies[0]["result"]["isError"], false);
    assert_eq!(body(1)["decision"], "run");
    assert_eq!(replies[2]["result"]["isError"], true);
    assert!(body(2)["error"].as_str().unwrap().contains("quod map pull"));
}

/// Pulled maps live in the git directory, so installing one doesn't make
/// the working tree look changed (a `.quod/` file would trigger R004).
#[test]
fn pulled_map_path_is_invisible_to_working_tree_diffs() {
    let f = setup();
    let repo = Repo::discover(&f.root).unwrap();
    let p = quod_cli::pull::default_map_path(&repo).unwrap();
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::copy(&f.map, &p).unwrap();
    let head = repo.rev_parse("HEAD").unwrap();
    assert!(
        repo.changed_files(&head, None).unwrap().is_empty(),
        "{} shows up as a change",
        p.display()
    );
}
