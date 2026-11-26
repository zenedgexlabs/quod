//! Section-level deltas for configuration files.
//!
//! `pyproject.toml`, `setup.cfg`, `tox.ini` and `pytest.ini` mix test-runner
//! settings with unrelated tooling (linters, docs, packaging metadata). This
//! module reports *which* sections changed, so policy can tell a `tool.ruff`
//! edit from a `tool.pytest` edit. Anything unparseable is
//! [`ConfigDelta::Unparseable`], which policy must treat as fail-closed.

use std::collections::BTreeMap;

/// Recognised configuration formats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigKind {
    /// `pyproject.toml`.
    PyProject,
    /// INI-style: `setup.cfg`, `tox.ini`, `pytest.ini`.
    Ini,
    /// `package.json` (any directory): sections are top-level fields.
    PackageJson,
    /// `tsconfig*.json` (any directory, JSON with comments): sections are
    /// `compilerOptions.<option>` and the other top-level fields.
    TsConfig,
}

/// Which sections of a configuration file changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigDelta {
    /// Changed section keys, sorted. For TOML these are dotted paths: two
    /// levels under `tool` and `project` (`tool.ruff`,
    /// `project.dependencies`) and one level elsewhere (`build-system`,
    /// `dependency-groups`). For INI they are section names. For
    /// `tsconfig*.json`, `compilerOptions.<option>` or a top-level field.
    Sections(Vec<String>),
    /// One side couldn't be parsed. This must fail closed.
    Unparseable(String),
}

/// Identifies a configuration file by its file name (in any directory).
pub fn config_kind(path: &str) -> Option<ConfigKind> {
    match path.rsplit('/').next().unwrap_or(path) {
        "pyproject.toml" => Some(ConfigKind::PyProject),
        "setup.cfg" | "tox.ini" | "pytest.ini" => Some(ConfigKind::Ini),
        "package.json" => Some(ConfigKind::PackageJson),
        n if n.starts_with("tsconfig") && n.ends_with(".json") => Some(ConfigKind::TsConfig),
        _ => None,
    }
}

/// Compares two versions of a configuration file. `None` means absent.
pub fn delta(kind: ConfigKind, old: Option<&[u8]>, new: Option<&[u8]>) -> ConfigDelta {
    let parse = |b: Option<&[u8]>| -> Result<BTreeMap<String, String>, String> {
        let Some(b) = b else {
            return Ok(BTreeMap::new());
        };
        let text = std::str::from_utf8(b)
            .map_err(|_| "not UTF-8".to_string())?
            .replace("\r\n", "\n");
        match kind {
            ConfigKind::PyProject => toml_sections(&text),
            ConfigKind::Ini => Ok(ini_sections(&text)),
            ConfigKind::PackageJson => json_fields(&text),
            ConfigKind::TsConfig => tsconfig_fields(&text),
        }
    };
    let (a, b) = match (parse(old), parse(new)) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => return ConfigDelta::Unparseable(e),
    };
    let mut keys: Vec<String> = a
        .keys()
        .chain(b.keys())
        .filter(|k| a.get(*k) != b.get(*k))
        .cloned()
        .collect();
    keys.sort();
    keys.dedup();
    ConfigDelta::Sections(keys)
}

fn toml_sections(text: &str) -> Result<BTreeMap<String, String>, String> {
    let doc: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| format!("invalid TOML: {e}"))?;
    let mut out = BTreeMap::new();
    for (k, v) in &doc {
        match (k.as_str(), v) {
            ("tool" | "project", toml::Value::Table(t)) => {
                for (k2, v2) in t {
                    out.insert(format!("{k}.{k2}"), crate::lock::canonical(v2));
                }
            }
            _ => {
                out.insert(k.clone(), crate::lock::canonical(v));
            }
        }
    }
    Ok(out)
}

fn json_fields(text: &str) -> Result<BTreeMap<String, String>, String> {
    let v: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?;
    let obj = v
        .as_object()
        .ok_or_else(|| "package.json is not an object".to_string())?;
    // serde_json's map is ordered by key, so `to_string` is canonical.
    Ok(obj
        .iter()
        .map(|(k, v)| (k.clone(), v.to_string()))
        .collect())
}

/// `tsconfig.json` fields: each compiler option on its own, other
/// top-level fields whole. The file is JSON with comments and trailing
/// commas, as `tsc` accepts.
fn tsconfig_fields(text: &str) -> Result<BTreeMap<String, String>, String> {
    let v: serde_json::Value = serde_json::from_str(&strip_jsonc(text))
        .map_err(|e| format!("invalid tsconfig JSON: {e}"))?;
    let obj = v
        .as_object()
        .ok_or_else(|| "tsconfig is not an object".to_string())?;
    let mut out = BTreeMap::new();
    for (k, v) in obj {
        match (k.as_str(), v) {
            ("compilerOptions", serde_json::Value::Object(opts)) => {
                for (o, ov) in opts {
                    out.insert(format!("compilerOptions.{o}"), ov.to_string());
                }
            }
            _ => {
                out.insert(k.clone(), v.to_string());
            }
        }
    }
    Ok(out)
}

/// JSON with comments to JSON: drops `//` and `/* */` comments and commas
/// before `}` or `]`, outside strings.
fn strip_jsonc(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    let mut in_str = false;
    while i < b.len() {
        let c = b[i];
        if in_str {
            out.push(c);
            if c == b'\\' && i + 1 < b.len() {
                out.push(b[i + 1]);
                i += 2;
                continue;
            }
            in_str = c != b'"';
            i += 1;
            continue;
        }
        match c {
            b'"' => {
                in_str = true;
                out.push(c);
                i += 1;
            }
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            b',' => {
                // A trailing comma: the next significant byte closes.
                let mut j = i + 1;
                loop {
                    while j < b.len() && b[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    if b.get(j) == Some(&b'/') && b.get(j + 1) == Some(&b'/') {
                        while j < b.len() && b[j] != b'\n' {
                            j += 1;
                        }
                    } else if b.get(j) == Some(&b'/') && b.get(j + 1) == Some(&b'*') {
                        j += 2;
                        while j + 1 < b.len() && !(b[j] == b'*' && b[j + 1] == b'/') {
                            j += 1;
                        }
                        j += 2;
                    } else {
                        break;
                    }
                }
                if !matches!(b.get(j), Some(b'}' | b']')) {
                    out.push(c);
                }
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_default()
}

/// Splits INI text into sections with whitespace- and comment-normalised
/// bodies. Lines before the first header belong to the `""` section.
fn ini_sections(text: &str) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    let mut current = String::new();
    for raw in text.lines() {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            current = trimmed[1..trimmed.len() - 1].trim().to_owned();
            out.entry(current.clone()).or_default();
            continue;
        }
        let body = out.entry(current.clone()).or_default();
        body.push_str(&trimmed.split_whitespace().collect::<Vec<_>>().join(" "));
        body.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(d: ConfigDelta) -> Vec<String> {
        match d {
            ConfigDelta::Sections(k) => k,
            ConfigDelta::Unparseable(e) => panic!("unparseable: {e}"),
        }
    }

    #[test]
    fn kinds() {
        assert_eq!(config_kind("pyproject.toml"), Some(ConfigKind::PyProject));
        assert_eq!(
            config_kind("examples/x/pyproject.toml"),
            Some(ConfigKind::PyProject)
        );
        assert_eq!(config_kind("tox.ini"), Some(ConfigKind::Ini));
        assert_eq!(config_kind("ruff.toml"), None);
    }

    #[test]
    fn pyproject_sections() {
        let old = "[project]\nname = \"x\"\nversion = \"1\"\nurls = { a = \"b\" }\n\n[tool.ruff]\nline-length = 88\n\n[tool.pytest.ini_options]\naddopts = \"-q\"\n";
        let lint = old.replace("88", "100");
        assert_eq!(
            keys(delta(
                ConfigKind::PyProject,
                Some(old.as_bytes()),
                Some(lint.as_bytes())
            )),
            vec!["tool.ruff"]
        );
        let both = lint
            .replace("-q", "-ra")
            .replace("version = \"1\"", "version = \"2\"");
        assert_eq!(
            keys(delta(
                ConfigKind::PyProject,
                Some(old.as_bytes()),
                Some(both.as_bytes())
            )),
            vec!["project.version", "tool.pytest", "tool.ruff"]
        );
        let reformatted = old.replace("line-length = 88", "line-length=88");
        assert!(
            keys(delta(
                ConfigKind::PyProject,
                Some(old.as_bytes()),
                Some(reformatted.as_bytes())
            ))
            .is_empty()
        );
    }

    #[test]
    fn ini_sections_and_comments() {
        let old = "[flake8]\nmax-line-length = 88\n\n[tool:pytest]\naddopts = -q\n";
        let lint = "# comment\n[flake8]\nmax-line-length = 100\n\n[tool:pytest]\naddopts =  -q\n";
        assert_eq!(
            keys(delta(
                ConfigKind::Ini,
                Some(old.as_bytes()),
                Some(lint.as_bytes())
            )),
            vec!["flake8"]
        );
    }

    #[test]
    fn tsconfig_options() {
        assert_eq!(config_kind("tsconfig.json"), Some(ConfigKind::TsConfig));
        assert_eq!(
            config_kind("web/tsconfig.build.json"),
            Some(ConfigKind::TsConfig)
        );
        let old = "{\n  // comment, with \"quotes\"\n  \"compilerOptions\": {\n    \"target\": \"ESNext\", /* x */\n    \"moduleResolution\": \"Node\",\n    \"paths\": { \"@/*\": [\"src/*\"] },\n  },\n  \"include\": [\"src\", \"http://x\"],\n}\n";
        let new = old.replace("    \"moduleResolution\": \"Node\",\n", "");
        assert_eq!(
            keys(delta(
                ConfigKind::TsConfig,
                Some(old.as_bytes()),
                Some(new.as_bytes())
            )),
            vec!["compilerOptions.moduleResolution"]
        );
        let inc = old.replace("\"src\",", "\"lib\",");
        assert_eq!(
            keys(delta(
                ConfigKind::TsConfig,
                Some(old.as_bytes()),
                Some(inc.as_bytes())
            )),
            vec!["include"]
        );
        assert!(matches!(
            delta(ConfigKind::TsConfig, Some(b"{"), Some(b"{}")),
            ConfigDelta::Unparseable(_)
        ));
    }

    #[test]
    fn invalid_toml_is_unparseable() {
        assert!(matches!(
            delta(ConfigKind::PyProject, Some(b"[x"), Some(b"")),
            ConfigDelta::Unparseable(_)
        ));
    }
}
