//! Function-level Python deltas.
//!
//! Each `def` and `class` is an *entity*, named by its Python `co_qualname`
//! (`f`, `C.method`, `outer.<locals>.inner`), which is the name the probe
//! records at runtime. An entity's hash covers its header and body. A nested
//! definition's body is replaced by a placeholder, because it is a separate
//! code object.
//!
//! What runs in the *enclosing* scope stays in the enclosing scope's stream:
//! decorators, default values and annotations are evaluated when the `def`
//! statement executes. So changing a default argument changes the module (or
//! class) entity, not only the function. That is what Python actually does.
//!
//! Streams are structural (node kinds plus leaf text), not token lists, so a
//! statement moved in or out of a block by re-indentation is a change, while
//! whitespace, comments and line breaks inside brackets are not. Line endings
//! are normalised CRLF→LF, matching Python's universal-newline source reading.

use std::collections::BTreeMap;

use sc_hash::{Digest, Domain, hash_domain};
use tree_sitter::{Node, Parser};

use crate::ChangeError;

/// The delta between two versions of a Python file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceDelta {
    /// Module-level code changed: imports, top-level statements, decorators,
    /// defaults, definitions added or removed, or the file was added, deleted
    /// or unparseable. Every test that imported the module is affected.
    pub module_changed: bool,
    /// Qualified names of entities whose own code changed, was added or was
    /// removed, sorted.
    pub entities: Vec<String>,
    /// True if either version failed to parse cleanly. The delta is then the
    /// conservative "everything changed".
    pub parse_error: bool,
    /// Entities present at base and absent at head (a subset of `entities`),
    /// sorted. Used by the test-integrity rules to flag deleted tests.
    pub removed: Vec<String>,
    /// Number of skip/xfail markers at base and at head (`pytest.mark.skip`,
    /// `skipif`, `xfail`, `pytest.skip(`, `unittest.skip`). An increase in a
    /// test file is an integrity finding.
    pub skip_markers: (u32, u32),
}

const SKIP_MARKERS: [&str; 5] = [
    "mark.skip",
    "mark.xfail",
    "pytest.skip(",
    "pytest.xfail(",
    "unittest.skip",
];

fn count_skip_markers(src: Option<&[u8]>) -> u32 {
    let Some(src) = src else { return 0 };
    let text = String::from_utf8_lossy(src);
    SKIP_MARKERS
        .iter()
        .map(|m| text.matches(m).count() as u32)
        .sum()
}

struct Analysis {
    module: Digest,
    entities: BTreeMap<String, Digest>,
    had_error: bool,
}

fn parser() -> Result<Parser, ChangeError> {
    let mut p = Parser::new();
    p.set_language(&tree_sitter_python::LANGUAGE.into())
        .map_err(|e| ChangeError::Parser(e.to_string()))?;
    Ok(p)
}

fn normalize(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len());
    let mut i = 0;
    while i < src.len() {
        if src[i] == b'\r' && src.get(i + 1) == Some(&b'\n') {
            i += 1;
            continue;
        }
        out.push(src[i]);
        i += 1;
    }
    out
}

fn analyze(src: &[u8]) -> Result<Analysis, ChangeError> {
    let src = normalize(src);
    let tree = parser()?
        .parse(&src, None)
        .ok_or_else(|| ChangeError::Parser("parse returned no tree".into()))?;
    let root = tree.root_node();
    let mut entities = BTreeMap::new();
    let mut module = String::new();
    emit(root, &src, "", &mut module, &mut entities);
    Ok(Analysis {
        module: hash_domain(Domain::Entity, module.as_bytes()),
        entities,
        had_error: root.has_error(),
    })
}

fn is_definition(kind: &str) -> bool {
    kind == "function_definition" || kind == "class_definition"
}

fn emit_leaf(text: &[u8], out: &mut String) {
    let s = String::from_utf8_lossy(text);
    out.push_str(&s.len().to_string());
    out.push(':');
    out.push_str(&s);
    out.push(' ');
}

/// Serializes `node` into `out`. Definitions are emitted as header plus body
/// placeholder, and their own streams are recorded in `entities`.
fn emit(
    node: Node<'_>,
    src: &[u8],
    scope: &str,
    out: &mut String,
    entities: &mut BTreeMap<String, Digest>,
) {
    let kind = node.kind();
    if kind == "comment" {
        return;
    }
    if is_definition(kind) {
        emit_definition(node, src, scope, out, entities);
        return;
    }
    if node.child_count() == 0 {
        emit_leaf(&src[node.byte_range()], out);
        return;
    }
    out.push('(');
    out.push_str(kind);
    out.push(' ');
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        emit(child, src, scope, out, entities);
    }
    out.push_str(") ");
}

fn emit_definition(
    node: Node<'_>,
    src: &[u8],
    scope: &str,
    out: &mut String,
    entities: &mut BTreeMap<String, Digest>,
) {
    let kind = node.kind();
    let name = node
        .child_by_field_name("name")
        .map(|n| String::from_utf8_lossy(&src[n.byte_range()]).into_owned())
        .unwrap_or_else(|| "<anonymous>".into());
    let qual = format!("{scope}{name}");
    let body = node.child_by_field_name("body");

    // Header, shared by the enclosing stream and the entity's own stream.
    let mut header = String::new();
    header.push('(');
    header.push_str(kind);
    header.push(' ');
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if Some(child.id()) != body.map(|b| b.id()) {
            emit(child, src, scope, &mut header, entities);
        }
    }

    // Enclosing scope: header plus a placeholder for the body.
    out.push_str(&header);
    out.push_str("<body:");
    out.push_str(&qual);
    out.push_str(">) ");

    // Entity stream: header plus body, with nested definitions in the inner scope.
    let inner_scope = if kind == "function_definition" {
        format!("{qual}.<locals>.")
    } else {
        format!("{qual}.")
    };
    let mut own = header;
    if let Some(b) = body {
        emit(b, src, &inner_scope, &mut own, entities);
    }
    own.push(')');
    let own_hash = hash_domain(Domain::Entity, own.as_bytes());
    // The same qualified name can be defined several times in one scope
    // (if/else or try/except branches, @overload stubs, redefinitions). At
    // runtime they all share one co_qualname, so the entity must cover every
    // definition, in source order. Otherwise a change to an earlier one is
    // masked by a later one.
    entities
        .entry(qual)
        .and_modify(|prev| {
            let mut buf = prev.as_bytes().to_vec();
            buf.extend_from_slice(own_hash.as_bytes());
            *prev = hash_domain(Domain::Entity, &buf);
        })
        .or_insert(own_hash);
}

/// Computes the delta between `old` and `new`. `None` means the file is
/// absent on that side (added or deleted).
pub fn diff(old: Option<&[u8]>, new: Option<&[u8]>) -> Result<SourceDelta, ChangeError> {
    let a = old.map(analyze).transpose()?;
    let b = new.map(analyze).transpose()?;
    let parse_error =
        a.as_ref().is_some_and(|x| x.had_error) || b.as_ref().is_some_and(|x| x.had_error);

    let empty = BTreeMap::new();
    let ea = a.as_ref().map_or(&empty, |x| &x.entities);
    let eb = b.as_ref().map_or(&empty, |x| &x.entities);
    let mut entities: Vec<String> = ea
        .iter()
        .filter(|(k, v)| eb.get(*k) != Some(*v))
        .map(|(k, _)| k.clone())
        .chain(eb.keys().filter(|k| !ea.contains_key(*k)).cloned())
        .collect();
    entities.sort();
    entities.dedup();

    let module_changed = match (&a, &b) {
        (Some(x), Some(y)) => x.module != y.module || parse_error,
        _ => true,
    };
    let removed: Vec<String> = ea
        .keys()
        .filter(|k| !eb.contains_key(*k))
        .cloned()
        .collect();
    Ok(SourceDelta {
        module_changed,
        entities,
        parse_error,
        removed,
        skip_markers: (count_skip_markers(old), count_skip_markers(new)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(old: &str, new: &str) -> SourceDelta {
        diff(Some(old.as_bytes()), Some(new.as_bytes())).unwrap()
    }

    #[test]
    fn formatting_and_comments_are_not_changes() {
        let old = "def f(a, b):\n    return a + b\n";
        let new = "def f(a,\n      b):  # comment\n    # another\n    return a+b\n";
        let delta = d(old, new);
        assert!(!delta.module_changed);
        assert!(delta.entities.is_empty());
    }

    #[test]
    fn body_change_is_local_to_the_function() {
        let old = "import os\n\ndef f():\n    return 1\n\ndef g():\n    return 2\n";
        let new = "import os\n\ndef f():\n    return 1\n\ndef g():\n    return 3\n";
        let delta = d(old, new);
        assert!(!delta.module_changed);
        assert_eq!(delta.entities, vec!["g"]);
    }

    #[test]
    fn reindentation_that_moves_a_statement_is_a_change() {
        let old = "def f(xs):\n    for x in xs:\n        a(x)\n    b()\n";
        let new = "def f(xs):\n    for x in xs:\n        a(x)\n        b()\n";
        assert_eq!(d(old, new).entities, vec!["f"]);
    }

    #[test]
    fn method_change_is_local_to_the_method() {
        let old = "class C:\n    X = 1\n    def m(self):\n        return 1\n    def n(self):\n        return 2\n";
        let new = "class C:\n    X = 1\n    def m(self):\n        return 9\n    def n(self):\n        return 2\n";
        let delta = d(old, new);
        assert!(!delta.module_changed);
        assert_eq!(delta.entities, vec!["C.m"]);
    }

    #[test]
    fn class_attribute_change_is_the_class_entity() {
        let old = "class C:\n    X = 1\n    def m(self):\n        return 1\n";
        let new = "class C:\n    X = 2\n    def m(self):\n        return 1\n";
        assert_eq!(d(old, new).entities, vec!["C"]);
    }

    #[test]
    fn defaults_and_decorators_belong_to_the_enclosing_scope() {
        let old = "@cache\ndef f(a=1):\n    return a\n";
        let new = "@cache\ndef f(a=2):\n    return a\n";
        let delta = d(old, new);
        assert!(
            delta.module_changed,
            "default values are evaluated at module level"
        );
        assert_eq!(delta.entities, vec!["f"]);

        let dec = d("@a\ndef f():\n    pass\n", "@b\ndef f():\n    pass\n");
        assert!(dec.module_changed);
    }

    /// Regression for a conformance escape (jinja `async_variant`): two
    /// `is_async` definitions in if/else branches share a qualified name, and a
    /// change to the first one was masked by the second.
    #[test]
    fn duplicate_qualnames_cover_every_definition() {
        let old = "def deco(flag):\n    if flag:\n        def is_async(a):\n            return a[0]\n    else:\n        def is_async(a):\n            return a[2]\n    return is_async\n";
        let first = old.replacen("a[0]", "a[1]", 1);
        let delta = d(old, &first);
        assert!(!delta.module_changed);
        assert_eq!(delta.entities, vec!["deco.<locals>.is_async"]);

        let second = old.replacen("a[2]", "a[3]", 1);
        assert_eq!(d(old, &second).entities, vec!["deco.<locals>.is_async"]);

        // try/except redefinition at module level
        let old_mod = "try:\n    def f():\n        return 1\nexcept ImportError:\n    def f():\n        return 2\n";
        assert_eq!(
            d(old_mod, &old_mod.replacen("return 1", "return 9", 1)).entities,
            vec!["f"]
        );
    }

    #[test]
    fn nested_functions_use_locals_qualnames() {
        let old = "def outer():\n    def inner():\n        return 1\n    return inner\n";
        let new = "def outer():\n    def inner():\n        return 2\n    return inner\n";
        let delta = d(old, new);
        assert!(!delta.module_changed);
        assert_eq!(delta.entities, vec!["outer.<locals>.inner"]);
    }

    #[test]
    fn added_and_removed_definitions() {
        let delta = d("def f():\n    pass\n", "def g():\n    pass\n");
        assert!(delta.module_changed);
        assert_eq!(delta.entities, vec!["f", "g"]);
    }

    #[test]
    fn crlf_is_not_a_change() {
        let lf = "def f():\n    return '''a\nb'''\n";
        let crlf = lf.replace('\n', "\r\n");
        let delta = d(lf, &crlf);
        assert!(!delta.module_changed);
        assert!(delta.entities.is_empty());
    }

    #[test]
    fn parse_errors_are_conservative() {
        let delta = d("def f():\n    return 1\n", "def f(:\n    return 1\n");
        assert!(delta.parse_error);
        assert!(delta.module_changed);
    }

    #[test]
    fn added_and_deleted_files() {
        let added = diff(None, Some(b"def f():\n    pass\n")).unwrap();
        assert!(added.module_changed);
        assert_eq!(added.entities, vec!["f"]);
        let deleted = diff(Some(b"def f():\n    pass\n"), None).unwrap();
        assert!(deleted.module_changed);
        assert_eq!(deleted.entities, vec!["f"]);
    }
}
