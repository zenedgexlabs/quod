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
//!
//! # Header splitting
//!
//! With [`DiffOptions::split_headers`], an undecorated `def` contributes to
//! the enclosing stream only what the enclosing scope actually *evaluates*:
//! the binding (name, `async`), type parameters, default values that aren't
//! side-effect free, and annotations unless the module has
//! `from __future__ import annotations`. Parameter names, side-effect-free
//! defaults (literals, plain names, displays of those) and lazy annotations
//! only change the function object itself. Their effect is visible only to
//! code that calls the function (it runs, so the probe records it) or
//! introspects it (`inspect.signature`, `typing.get_type_hints`, which a
//! probe with the `py.introspection` capability records as a use).
//!
//! A decorated `def` keeps its whole header in the enclosing stream, because
//! the decorator receives the function object at definition time and may
//! inspect it. Without the option, headers are never split, which is what a
//! map recorded by an older probe requires.

use std::collections::BTreeMap;

use quod_hash::{Digest, Domain, hash_domain};
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

/// How to compute Python deltas.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiffOptions {
    /// Attribute parameter names, side-effect-free defaults and lazy
    /// annotations of undecorated `def`s to the function rather than the
    /// enclosing scope (see the module docs). Only sound when the footprint
    /// map was recorded by a probe that records signature introspection.
    pub split_headers: bool,
}

struct Analysis {
    module: Digest,
    entities: BTreeMap<String, Digest>,
    had_error: bool,
}

/// Per-file analysis context.
struct Cx<'a> {
    src: &'a [u8],
    split_headers: bool,
    /// `from __future__ import annotations`: annotations are stored as
    /// strings, never evaluated at definition time.
    lazy_annotations: bool,
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

fn has_future_annotations(root: Node<'_>, src: &[u8]) -> bool {
    let mut cursor = root.walk();
    root.children(&mut cursor).any(|c| {
        let mut cur = c.walk();
        c.kind() == "future_import_statement"
            && c.children_by_field_name("name", &mut cur)
                .any(|n| &src[n.byte_range()] == b"annotations")
    })
}

fn analyze(src: &[u8], opts: DiffOptions) -> Result<Analysis, ChangeError> {
    let src = normalize(src);
    let tree = parser()?
        .parse(&src, None)
        .ok_or_else(|| ChangeError::Parser("parse returned no tree".into()))?;
    let root = tree.root_node();
    let cx = Cx {
        src: &src,
        split_headers: opts.split_headers,
        lazy_annotations: has_future_annotations(root, &src),
    };
    let mut entities = BTreeMap::new();
    let mut module = String::new();
    emit(root, &cx, "", &mut module, &mut entities);
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
    cx: &Cx<'_>,
    scope: &str,
    out: &mut String,
    entities: &mut BTreeMap<String, Digest>,
) {
    let kind = node.kind();
    if kind == "comment" {
        return;
    }
    if is_definition(kind) {
        emit_definition(node, cx, scope, out, entities);
        return;
    }
    if node.child_count() == 0 {
        emit_leaf(&cx.src[node.byte_range()], out);
        return;
    }
    out.push('(');
    out.push_str(kind);
    out.push(' ');
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        emit(child, cx, scope, out, entities);
    }
    out.push_str(") ");
}

/// Builtin names: looking one up can't raise. Any other plain name can
/// (`NameError` when it isn't bound yet, which fails the module's import).
#[rustfmt::skip]
const BUILTIN_NAMES: &[&str] = &[
    "ArithmeticError", "AssertionError", "AttributeError", "BaseException", "BytesWarning",
    "DeprecationWarning", "Ellipsis", "EOFError", "Exception", "FutureWarning",
    "ImportError", "IndexError", "KeyError", "LookupError", "MemoryError", "NotImplemented",
    "NotImplementedError", "OSError", "OverflowError", "RecursionError", "RuntimeError",
    "RuntimeWarning", "StopAsyncIteration", "StopIteration", "SyntaxError", "SystemError",
    "TimeoutError", "TypeError", "UnicodeError", "UserWarning", "ValueError",
    "Warning", "ZeroDivisionError", "abs", "all", "any", "ascii", "bin", "bool", "bytearray",
    "bytes", "callable", "chr", "classmethod", "complex", "dict", "dir", "divmod", "enumerate",
    "filter", "float", "format", "frozenset", "getattr", "hasattr", "hash", "hex", "id",
    "int", "isinstance", "issubclass", "iter", "len", "list", "map", "max", "memoryview",
    "min", "next", "object", "oct", "open", "ord", "pow", "print", "property", "range",
    "repr", "reversed", "round", "set", "setattr", "slice", "sorted", "staticmethod", "str",
    "sum", "super", "tuple", "type", "vars", "zip",
];

/// True if evaluating `node` can't run code or raise: literals, builtin
/// names, and tuple/list/set/dict displays of those. Other names can raise
/// `NameError`. Attribute access, calls, subscripts and operators on
/// non-literals can run code (descriptors, `__getattr__`, operator
/// overloads), so they aren't.
fn side_effect_free(node: Node<'_>, src: &[u8]) -> bool {
    fn named(n: Node<'_>) -> Vec<Node<'_>> {
        let mut c = n.walk();
        n.named_children(&mut c)
            .filter(|ch| ch.kind() != "comment")
            .collect()
    }
    match node.kind() {
        "integer" | "float" | "true" | "false" | "none" | "ellipsis" => true,
        "identifier" => {
            BUILTIN_NAMES.contains(&String::from_utf8_lossy(&src[node.byte_range()]).as_ref())
        }
        // Plain strings only: an f-string's interpolations are expressions.
        "string" => named(node).iter().all(|c| c.kind() != "interpolation"),
        "concatenated_string" => named(node).into_iter().all(|c| side_effect_free(c, src)),
        "parenthesized_expression" => named(node).into_iter().all(|c| side_effect_free(c, src)),
        "unary_operator" => node
            .child_by_field_name("argument")
            .is_some_and(|a| matches!(a.kind(), "integer" | "float")),
        "tuple" | "list" | "set" | "dictionary" => {
            named(node).into_iter().all(|c| side_effect_free(c, src))
        }
        "pair" => named(node).into_iter().all(|c| side_effect_free(c, src)),
        _ => false,
    }
}

/// The part of an undecorated `def` header the enclosing scope evaluates.
fn effective_function_header(
    node: Node<'_>,
    cx: &Cx<'_>,
    scope: &str,
    entities: &mut BTreeMap<String, Digest>,
) -> String {
    let mut out = String::from("(function_binding ");
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "async" => out.push_str("async "),
            "identifier"
                if Some(child.id()) == node.child_by_field_name("name").map(|n| n.id()) =>
            {
                emit_leaf(&cx.src[child.byte_range()], &mut out);
            }
            // PEP 695 type parameters create objects at definition time.
            "type_parameter" => emit(child, cx, scope, &mut out, entities),
            _ => {}
        }
    }
    if let Some(params) = node.child_by_field_name("parameters") {
        let mut pc = params.walk();
        for p in params.named_children(&mut pc) {
            if let Some(v) = p.child_by_field_name("value") {
                if !side_effect_free(v, cx.src) {
                    out.push_str("(default ");
                    emit(v, cx, scope, &mut out, entities);
                    out.push_str(") ");
                }
            }
            if let Some(t) = p.child_by_field_name("type") {
                if !cx.lazy_annotations {
                    out.push_str("(annotation ");
                    emit(t, cx, scope, &mut out, entities);
                    out.push_str(") ");
                }
            }
        }
    }
    if let Some(r) = node.child_by_field_name("return_type") {
        if !cx.lazy_annotations {
            out.push_str("(returns ");
            emit(r, cx, scope, &mut out, entities);
            out.push_str(") ");
        }
    }
    out
}

fn emit_definition(
    node: Node<'_>,
    cx: &Cx<'_>,
    scope: &str,
    out: &mut String,
    entities: &mut BTreeMap<String, Digest>,
) {
    let kind = node.kind();
    let name = node
        .child_by_field_name("name")
        .map(|n| String::from_utf8_lossy(&cx.src[n.byte_range()]).into_owned())
        .unwrap_or_else(|| "<anonymous>".into());
    let qual = format!("{scope}{name}");
    let body = node.child_by_field_name("body");

    // Full header, part of the entity's own stream.
    let mut header = String::new();
    header.push('(');
    header.push_str(kind);
    header.push(' ');
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if Some(child.id()) != body.map(|b| b.id()) {
            emit(child, cx, scope, &mut header, entities);
        }
    }

    // Enclosing scope: what it evaluates of the header, plus a placeholder
    // for the body.
    let decorated = node
        .parent()
        .is_some_and(|p| p.kind() == "decorated_definition");
    if cx.split_headers && kind == "function_definition" && !decorated {
        out.push_str(&effective_function_header(node, cx, scope, entities));
    } else {
        out.push_str(&header);
    }
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
        emit(b, cx, &inner_scope, &mut own, entities);
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

/// Computes the delta between `old` and `new`, without header splitting.
/// `None` means the file is absent on that side (added or deleted).
pub fn diff(old: Option<&[u8]>, new: Option<&[u8]>) -> Result<SourceDelta, ChangeError> {
    diff_with(old, new, DiffOptions::default())
}

/// [`diff`] with explicit options.
pub fn diff_with(
    old: Option<&[u8]>,
    new: Option<&[u8]>,
    opts: DiffOptions,
) -> Result<SourceDelta, ChangeError> {
    let a = old.map(|s| analyze(s, opts)).transpose()?;
    let b = new.map(|s| analyze(s, opts)).transpose()?;
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

    fn ds(old: &str, new: &str) -> SourceDelta {
        diff_with(
            Some(old.as_bytes()),
            Some(new.as_bytes()),
            DiffOptions {
                split_headers: true,
            },
        )
        .unwrap()
    }

    /// From conformance (click): adding a keyword parameter with a constant
    /// default to a method changed the class entity, which the conftest's
    /// import put in the root scope, so every test re-ran.
    #[test]
    fn split_headers_keep_parameters_and_constant_defaults_local() {
        let old = "class C:\n    def m(self, a=1, b=None):\n        return a\n";
        for new in [
            old.replace("a=1", "a=2"),
            old.replace("b=None", "b=None, c=()"),
            old.replace("b=None", "bb=None"),
            old.replace("a=1", "a=-1"),
            old.replace("b=None", "b={'k': (1, 'x')}"),
            old.replace("b=None", "b=int"),
        ] {
            let split = ds(old, &new);
            assert!(!split.module_changed, "{new}");
            assert_eq!(split.entities, vec!["C.m"], "{new}");
            // Without splitting, the class body changes too.
            assert_eq!(d(old, &new).entities, vec!["C", "C.m"], "{new}");
        }
        let module_level = ds("def f(a=1):\n    pass\n", "def f(a=2):\n    pass\n");
        assert!(!module_level.module_changed);
        assert_eq!(module_level.entities, vec!["f"]);
    }

    #[test]
    fn split_headers_drop_only_lazy_annotations() {
        let lazy = "from __future__ import annotations\nclass C:\n    def m(self, x: int) -> C:\n        return self\n";
        let changed = lazy
            .replace("-> C", "-> t.Self")
            .replace("x: int", "x: str");
        assert_eq!(ds(lazy, &changed).entities, vec!["C.m"]);

        // Without the future import, annotations are evaluated by the
        // class body.
        let eager = lazy.replace("from __future__ import annotations\n", "");
        let eager_changed = changed.replace("from __future__ import annotations\n", "");
        assert_eq!(ds(&eager, &eager_changed).entities, vec!["C", "C.m"]);
        // A future import of something else doesn't make annotations lazy.
        let other = eager.replace("class C", "from __future__ import division\nclass C");
        let other_changed =
            eager_changed.replace("class C", "from __future__ import division\nclass C");
        assert_eq!(ds(&other, &other_changed).entities, vec!["C", "C.m"]);
    }

    #[test]
    fn split_headers_keep_evaluated_parts_in_the_enclosing_scope() {
        let base = "def f(a=1):\n    pass\n";
        for new in [
            "def f(a=make()):\n    pass\n",
            "def f(a=x.y):\n    pass\n",
            "def f(a=x[0]):\n    pass\n",
            "def f(a=f\"{x}\"):\n    pass\n",
            "def f(a=1 + x):\n    pass\n",
            "def f(a=lambda: 1):\n    pass\n",
            // A plain name can be unbound at definition time: NameError
            // fails the module's import, which every importer observes.
            "def f(a=SENTINEL):
    pass
",
            "def f(a=(1, SENTINEL)):
    pass
",
            "async def f(a=1):\n    pass\n",
            "def g(a=1):\n    pass\n",
        ] {
            assert!(ds(base, new).module_changed, "{new}");
        }
        // A changed non-constant default stays an enclosing-scope change.
        assert!(
            ds(
                "def f(a=make(1)):\n    pass\n",
                "def f(a=make(2)):\n    pass\n"
            )
            .module_changed
        );
        // Decorators receive the function object and may inspect its
        // signature, so a decorated header is never split.
        let dec = ds("@d\ndef f(a=1):\n    pass\n", "@d\ndef f(a=2):\n    pass\n");
        assert!(dec.module_changed);
        // Adding or removing a method changes the class namespace.
        let added = ds(
            "class C:\n    def m(self):\n        pass\n",
            "class C:\n    def m(self):\n        pass\n    def n(self):\n        pass\n",
        );
        assert_eq!(added.entities, vec!["C", "C.n"]);
        // Class headers (bases, keywords) are always evaluated.
        assert!(ds("class C(A):\n    pass\n", "class C(B):\n    pass\n").module_changed);
    }
}
