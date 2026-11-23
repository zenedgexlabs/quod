//! Lockfile deltas: which locked packages changed.
//!
//! Supported: `uv.lock`, `poetry.lock`, PEP 751 `pylock*.toml`, and pinned
//! `requirements*.txt`. Each package gets a fingerprint over its entire
//! locked entry (version, source, hashes, dependency metadata), so any change
//! to how a package is resolved shows up. Anything that can't be parsed is
//! [`LockDelta::Unparseable`], which policy must treat as fail-closed.

use std::collections::BTreeMap;

use sc_hash::{Digest, Domain, hash_domain};

/// Recognised lockfile formats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockKind {
    /// `uv.lock`.
    Uv,
    /// `poetry.lock`.
    Poetry,
    /// PEP 751 `pylock.toml` / `pylock.*.toml`.
    Pylock,
    /// `requirements*.txt`.
    Requirements,
    /// npm `package-lock.json` / `npm-shrinkwrap.json` (lockfile v1 to v3).
    Npm,
}

impl LockKind {
    /// The package ecosystem the lockfile pins: `"pypi"` or `"npm"`.
    pub fn ecosystem(self) -> &'static str {
        match self {
            Self::Npm => "npm",
            _ => "pypi",
        }
    }
}

/// A package pin on one side of a delta.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PkgPin {
    /// Locked version (empty if the lockfile has none, e.g. an unpinned
    /// requirement).
    pub version: String,
    /// Fingerprint of the full locked entry.
    pub fingerprint: Digest,
}

/// One package whose lock entry changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageChange {
    /// PEP 503 normalised name. `__options__` stands for global options in a
    /// requirements file (index URLs, includes, editable installs).
    pub name: String,
    /// Pin at base (`None` if added).
    pub old: Option<PkgPin>,
    /// Pin at head (`None` if removed).
    pub new: Option<PkgPin>,
}

/// The result of comparing two versions of a lockfile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LockDelta {
    /// Packages whose entry changed, sorted by name. Empty means the
    /// lockfile changed only cosmetically.
    Packages(Vec<PackageChange>),
    /// One side could not be parsed. This must fail closed.
    Unparseable(String),
}

/// Identifies a lockfile by its file name.
pub fn lock_kind(path: &str) -> Option<LockKind> {
    let file = path.rsplit('/').next().unwrap_or(path);
    match file {
        "uv.lock" => Some(LockKind::Uv),
        "poetry.lock" => Some(LockKind::Poetry),
        f if f == "pylock.toml" || (f.starts_with("pylock.") && f.ends_with(".toml")) => {
            Some(LockKind::Pylock)
        }
        f if f.starts_with("requirements") && f.ends_with(".txt") => Some(LockKind::Requirements),
        "package-lock.json" | "npm-shrinkwrap.json" => Some(LockKind::Npm),
        _ => None,
    }
}

/// PEP 503 name normalisation.
pub fn normalize_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut sep = false;
    for c in name.chars() {
        if matches!(c, '-' | '_' | '.') {
            sep = true;
        } else {
            if sep && !out.is_empty() {
                out.push('-');
            }
            sep = false;
            out.extend(c.to_lowercase());
        }
    }
    out
}

/// Compares two versions of a lockfile.
pub fn delta(kind: LockKind, old: Option<&[u8]>, new: Option<&[u8]>) -> LockDelta {
    let parse_side = |b: Option<&[u8]>| -> Result<BTreeMap<String, PkgPin>, String> {
        match b {
            None => Ok(BTreeMap::new()),
            Some(b) => {
                let text =
                    std::str::from_utf8(b).map_err(|_| "lockfile is not UTF-8".to_string())?;
                parse(kind, &text.replace("\r\n", "\n"))
            }
        }
    };
    let (a, b) = match (parse_side(old), parse_side(new)) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => return LockDelta::Unparseable(e),
    };
    let mut names: Vec<&String> = a.keys().chain(b.keys()).collect();
    names.sort();
    names.dedup();
    let changes = names
        .into_iter()
        .filter_map(|n| {
            let (o, w) = (a.get(n), b.get(n));
            (o != w).then(|| PackageChange {
                name: n.clone(),
                old: o.cloned(),
                new: w.cloned(),
            })
        })
        .collect();
    LockDelta::Packages(changes)
}

fn parse(kind: LockKind, text: &str) -> Result<BTreeMap<String, PkgPin>, String> {
    match kind {
        LockKind::Uv | LockKind::Poetry => parse_toml(text, "package"),
        LockKind::Pylock => parse_toml(text, "packages"),
        LockKind::Requirements => parse_requirements(text),
        LockKind::Npm => parse_npm(text),
    }
}

/// npm lockfiles. v2/v3 list every installed copy under `packages`, keyed
/// by its `node_modules/...` path. v1 nests them under `dependencies`. A
/// name installed at several paths (different versions for different
/// dependents) is one package whose fingerprint covers every copy. The
/// root entry (`""`) is the project itself, whose manifest is
/// `package.json`.
fn parse_npm(text: &str) -> Result<BTreeMap<String, PkgPin>, String> {
    let v: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("invalid package-lock JSON: {e}"))?;
    let mut copies: BTreeMap<String, Vec<(String, String, String)>> = BTreeMap::new();
    if let Some(pkgs) = v.get("packages").and_then(|p| p.as_object()) {
        for (path, entry) in pkgs {
            if path.is_empty() {
                continue;
            }
            let name = entry
                .get("name")
                .and_then(|n| n.as_str())
                .map(str::to_owned)
                .or_else(|| path.rsplit_once("node_modules/").map(|(_, n)| n.to_owned()))
                // Workspace members are listed by their directory.
                .unwrap_or_else(|| path.clone());
            let version = entry
                .get("version")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_owned();
            copies
                .entry(name)
                .or_default()
                .push((path.clone(), version, entry.to_string()));
        }
    } else if let Some(deps) = v.get("dependencies").and_then(|d| d.as_object()) {
        fn walk(
            prefix: &str,
            deps: &serde_json::Map<String, serde_json::Value>,
            out: &mut BTreeMap<String, Vec<(String, String, String)>>,
        ) {
            for (name, entry) in deps {
                let path = format!("{prefix}node_modules/{name}");
                let version = entry
                    .get("version")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_owned();
                let mut flat = entry.clone();
                if let Some(o) = flat.as_object_mut() {
                    o.remove("dependencies");
                }
                out.entry(name.clone()).or_default().push((
                    path.clone(),
                    version,
                    flat.to_string(),
                ));
                if let Some(nested) = entry.get("dependencies").and_then(|d| d.as_object()) {
                    walk(&format!("{path}/"), nested, out);
                }
            }
        }
        walk("", deps, &mut copies);
    } else {
        return Err("package-lock has neither `packages` nor `dependencies`".into());
    }
    Ok(copies
        .into_iter()
        .map(|(name, mut list)| {
            list.sort();
            let mut versions: Vec<&str> = list.iter().map(|(_, v, _)| v.as_str()).collect();
            versions.dedup();
            let joined: String = list
                .iter()
                .map(|(p, _, e)| format!("{p}\u{0}{e}\u{0}"))
                .collect();
            let pin = PkgPin {
                version: versions.join(","),
                fingerprint: hash_domain(Domain::Blob, joined.as_bytes()),
            };
            (name, pin)
        })
        .collect())
}

pub(crate) fn canonical(v: &toml::Value) -> String {
    let mut s = String::new();
    canon(v, &mut s);
    s
}

fn canon(v: &toml::Value, out: &mut String) {
    match v {
        toml::Value::Table(t) => {
            out.push('{');
            // toml::Table is ordered by key, so this is canonical.
            for (k, v) in t {
                out.push_str(&format!("{}:{k}=", k.len()));
                canon(v, out);
                out.push(',');
            }
            out.push('}');
        }
        toml::Value::Array(a) => {
            out.push('[');
            for v in a {
                canon(v, out);
                out.push(',');
            }
            out.push(']');
        }
        toml::Value::String(s) => out.push_str(&format!("s{}:{s}", s.len())),
        other => out.push_str(&other.to_string()),
    }
}

fn parse_toml(text: &str, array_key: &str) -> Result<BTreeMap<String, PkgPin>, String> {
    let doc: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| format!("invalid TOML: {e}"))?;
    let entries = match doc.get(array_key) {
        None => return Ok(BTreeMap::new()),
        Some(toml::Value::Array(a)) => a,
        Some(_) => return Err(format!("`{array_key}` is not an array")),
    };
    // A name may appear more than once (forked resolutions); fold them together.
    let mut grouped: BTreeMap<String, (Vec<String>, Vec<String>)> = BTreeMap::new();
    for e in entries {
        let t = e.as_table().ok_or("package entry is not a table")?;
        let name = t
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or("package entry without a name")?;
        let version = t
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let mut s = String::new();
        canon(e, &mut s);
        let slot = grouped.entry(normalize_name(name)).or_default();
        slot.0.push(version);
        slot.1.push(s);
    }
    Ok(grouped
        .into_iter()
        .map(|(name, (mut versions, mut canons))| {
            versions.sort();
            versions.dedup();
            canons.sort();
            let pin = PkgPin {
                version: versions.join(","),
                fingerprint: hash_domain(Domain::Blob, canons.join("\n").as_bytes()),
            };
            (name, pin)
        })
        .collect())
}

fn parse_requirements(text: &str) -> Result<BTreeMap<String, PkgPin>, String> {
    // Join backslash continuations.
    let joined = text.replace("\\\n", " ");
    let mut pkgs: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut options: Vec<String> = Vec::new();
    for raw in joined.lines() {
        let line = match raw.find(" #") {
            Some(i) => &raw[..i],
            None if raw.trim_start().starts_with('#') => "",
            None => raw,
        };
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            continue;
        }
        if line.starts_with('-')
            || line.contains("://")
            || line.starts_with('.')
            || line.starts_with('/')
        {
            // Includes, editable installs, index options, URLs and local paths
            // can't be resolved here. They go under a pseudo-package, so any
            // change to them is visible and fails closed.
            options.push(line);
            continue;
        }
        let end = line
            .find(['[', '<', '>', '=', '!', '~', ';', '@', ' '])
            .unwrap_or(line.len());
        let name = normalize_name(&line[..end]);
        if name.is_empty() {
            return Err(format!("cannot parse requirement line {line:?}"));
        }
        // Fingerprint the normalised name plus the rest of the line, so
        // `Requests==1` and `requests==1` are the same pin.
        let canonical = format!("{name}{}", &line[end..]);
        pkgs.entry(name).or_default().push(canonical);
    }
    let mut out: BTreeMap<String, PkgPin> = pkgs
        .into_iter()
        .map(|(name, mut lines)| {
            lines.sort();
            let version = lines
                .iter()
                .filter_map(|l| l.split("==").nth(1))
                .map(|v| v.split([' ', ';']).next().unwrap_or("").to_owned())
                .collect::<Vec<_>>()
                .join(",");
            (
                name,
                PkgPin {
                    version,
                    fingerprint: hash_domain(Domain::Blob, lines.join("\n").as_bytes()),
                },
            )
        })
        .collect();
    if !options.is_empty() {
        options.sort();
        out.insert(
            "__options__".into(),
            PkgPin {
                version: String::new(),
                fingerprint: hash_domain(Domain::Blob, options.join("\n").as_bytes()),
            },
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(d: &LockDelta) -> Vec<String> {
        match d {
            LockDelta::Packages(p) => p.iter().map(|c| c.name.clone()).collect(),
            LockDelta::Unparseable(e) => panic!("unparseable: {e}"),
        }
    }

    #[test]
    fn detects_kinds() {
        assert_eq!(lock_kind("uv.lock"), Some(LockKind::Uv));
        assert_eq!(lock_kind("svc/poetry.lock"), Some(LockKind::Poetry));
        assert_eq!(lock_kind("pylock.dev.toml"), Some(LockKind::Pylock));
        assert_eq!(
            lock_kind("requirements-dev.txt"),
            Some(LockKind::Requirements)
        );
        assert_eq!(lock_kind("pyproject.toml"), None);
    }

    #[test]
    fn normalizes_names() {
        assert_eq!(normalize_name("Foo_Bar.baz"), "foo-bar-baz");
        assert_eq!(normalize_name("a--b"), "a-b");
    }

    #[test]
    fn uv_lock_version_and_hash_changes() {
        let old = r#"
version = 1
[[package]]
name = "requests"
version = "2.31.0"
source = { registry = "https://pypi.org/simple" }
sdist = { url = "x", hash = "sha256:aaa" }

[[package]]
name = "idna"
version = "3.6"
source = { registry = "https://pypi.org/simple" }
"#;
        let new = old
            .replace("2.31.0", "2.32.0")
            .replace("sha256:aaa", "sha256:bbb");
        let d = delta(LockKind::Uv, Some(old.as_bytes()), Some(new.as_bytes()));
        assert_eq!(names(&d), vec!["requests"]);

        let only_hash = old.replace("sha256:aaa", "sha256:ccc");
        let d = delta(
            LockKind::Uv,
            Some(old.as_bytes()),
            Some(only_hash.as_bytes()),
        );
        assert_eq!(
            names(&d),
            vec!["requests"],
            "a re-published artifact is a change"
        );

        let reordered = r#"
version = 1
[[package]]
name = "idna"
version = "3.6"
source = { registry = "https://pypi.org/simple" }

[[package]]
name = "requests"
version = "2.31.0"
source = { registry = "https://pypi.org/simple" }
sdist = { url = "x", hash = "sha256:aaa" }
"#;
        let d = delta(
            LockKind::Uv,
            Some(old.as_bytes()),
            Some(reordered.as_bytes()),
        );
        assert!(names(&d).is_empty(), "reordering is cosmetic");
    }

    #[test]
    fn requirements_pins_and_options() {
        let old = "# deps\nRequests==2.31.0 --hash=sha256:aaa\nidna==3.6\n";
        let new = "# deps changed comment\nrequests==2.31.0 --hash=sha256:aaa\nidna==3.7\n--index-url https://mirror/simple\n";
        let d = delta(
            LockKind::Requirements,
            Some(old.as_bytes()),
            Some(new.as_bytes()),
        );
        assert_eq!(names(&d), vec!["__options__", "idna"]);
        if let LockDelta::Packages(p) = &d {
            assert_eq!(p[1].new.as_ref().unwrap().version, "3.7");
        }
    }

    #[test]
    fn added_lockfile_lists_every_package() {
        let new = "[[package]]\nname = \"a\"\nversion = \"1\"\n[[package]]\nname = \"b\"\nversion = \"2\"\n";
        let d = delta(LockKind::Poetry, None, Some(new.as_bytes()));
        assert_eq!(names(&d), vec!["a", "b"]);
    }

    #[test]
    fn invalid_toml_is_unparseable() {
        let d = delta(LockKind::Uv, Some(b"[[package]\nname="), Some(b""));
        assert!(matches!(d, LockDelta::Unparseable(_)));
    }

    const NPM_V3: &str = r#"{
  "name": "app", "lockfileVersion": 3, "requires": true,
  "packages": {
    "": { "name": "app", "dependencies": { "left-pad": "^1.3.0", "@scope/util": "^2.0.0" } },
    "node_modules/left-pad": { "version": "1.3.0", "integrity": "sha512-aaa" },
    "node_modules/@scope/util": { "version": "2.0.0", "integrity": "sha512-bbb" },
    "node_modules/@scope/util/node_modules/left-pad": { "version": "1.1.0", "integrity": "sha512-ccc" }
  }
}"#;

    #[test]
    fn npm_lockfile_deltas() {
        assert_eq!(lock_kind("web/package-lock.json"), Some(LockKind::Npm));
        assert_eq!(LockKind::Npm.ecosystem(), "npm");
        let bump = NPM_V3.replace(
            "\"2.0.0\", \"integrity\": \"sha512-bbb\"",
            "\"2.1.0\", \"integrity\": \"sha512-ddd\"",
        );
        let LockDelta::Packages(ch) = delta(
            LockKind::Npm,
            Some(NPM_V3.as_bytes()),
            Some(bump.as_bytes()),
        ) else {
            panic!("unparseable")
        };
        let names: Vec<&str> = ch.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["@scope/util"]);
        assert_eq!(ch[0].new.as_ref().unwrap().version, "2.1.0");
        // A nested copy is part of its name's fingerprint.
        let nested = NPM_V3.replace("sha512-ccc", "sha512-eee");
        let LockDelta::Packages(ch) = delta(
            LockKind::Npm,
            Some(NPM_V3.as_bytes()),
            Some(nested.as_bytes()),
        ) else {
            panic!("unparseable")
        };
        assert_eq!(
            ch.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["left-pad"]
        );
        // Formatting isn't a change.
        let compact: serde_json::Value = serde_json::from_str(NPM_V3).unwrap();
        let LockDelta::Packages(ch) = delta(
            LockKind::Npm,
            Some(NPM_V3.as_bytes()),
            Some(compact.to_string().as_bytes()),
        ) else {
            panic!("unparseable")
        };
        assert!(ch.is_empty());
        assert!(matches!(
            delta(LockKind::Npm, Some(b"{"), Some(b"{}")),
            LockDelta::Unparseable(_)
        ));
    }

    #[test]
    fn npm_lockfile_v1() {
        let v1 = r#"{"lockfileVersion": 1, "dependencies": {"a": {"version": "1.0.0", "dependencies": {"b": {"version": "2.0.0"}}}}}"#;
        let v1b = v1.replace("\"2.0.0\"", "\"2.0.1\"");
        let LockDelta::Packages(ch) =
            delta(LockKind::Npm, Some(v1.as_bytes()), Some(v1b.as_bytes()))
        else {
            panic!("unparseable")
        };
        assert_eq!(
            ch.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["b"]
        );
    }
}
