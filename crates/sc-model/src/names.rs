//! Canonical node names: the contract between the probes, which record
//! nodes, and the policy, which looks them up after a change.
//!
//! Every producer and consumer must build names with these functions. A
//! mismatch would make a changed entity invisible, so a test that should run
//! would be skipped.

use crate::graph::Ecosystem;

/// Name of a repository file node: the `/`-separated path relative to the
/// repository root.
pub fn file(rel_path: &str) -> String {
    rel_path.replace('\\', "/")
}

/// Name of a function or class node: `<file>::<co_qualname>`, e.g.
/// `src/app.py::Client.get` or `src/app.py::outer.<locals>.inner`.
pub fn function(rel_path: &str, qualname: &str) -> String {
    format!("{}::{qualname}", file(rel_path))
}

/// Name of an external package node: `<ecosystem>:<normalised name>`, e.g.
/// `pypi:requests`. The name must already be normalised (PEP 503 for PyPI).
pub fn package(ecosystem: Ecosystem, normalized_name: &str) -> String {
    let eco = match ecosystem {
        Ecosystem::Pypi => "pypi",
        Ecosystem::Npm => "npm",
    };
    format!("{eco}:{normalized_name}")
}

/// Name of an environment-variable node.
pub fn env_var(name: &str) -> String {
    name.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_stable() {
        assert_eq!(file(r"src\app.py"), "src/app.py");
        assert_eq!(function("src/app.py", "C.m"), "src/app.py::C.m");
        assert_eq!(package(Ecosystem::Pypi, "requests"), "pypi:requests");
    }
}
