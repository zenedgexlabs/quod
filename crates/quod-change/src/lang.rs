//! Language registry and the generic tree-sitter entity extractor (ADR 013).
//!
//! Each supported language has a declarative [`Spec`]:
//!
//! - which syntax nodes define entities, and how each is named;
//! - which child is the body: the part that runs only when the entity runs;
//! - the test-skip markers;
//! - whether the language is *static*.
//!
//! The extractor serves two purposes, and the shared code is what keeps them
//! consistent:
//!
//! - [`diff`] reports which entities changed between two versions of a file,
//!   like `python::diff` (an entity's stream is its header plus body; the
//!   enclosing stream sees the header plus a placeholder);
//! - [`LineMap`] maps executed source lines from a probe to the innermost
//!   entity containing them. A line outside every entity is module-level.
//!
//! Python changes are diffed by [`crate::python`] (ADR 011 header splitting).
//! Python also has a spec here, used only for line mapping. Its entity names
//! follow `co_qualname`, as `python.rs` does.

use std::collections::BTreeMap;

use quod_hash::{Digest, Domain, hash_domain};
use tree_sitter::{Node, Parser};

use crate::ChangeError;
use crate::python::SourceDelta;

/// A language with an entity spec.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Language {
    /// Python (`.py`).
    Python,
    /// JavaScript (`.js`, `.mjs`, `.cjs`, `.jsx`).
    JavaScript,
    /// TypeScript (`.ts`, `.mts`, `.cts`).
    TypeScript,
    /// TypeScript with JSX (`.tsx`).
    Tsx,
    /// Go (`.go`).
    Go,
    /// Rust (`.rs`).
    Rust,
    /// Java (`.java`).
    Java,
}

/// How a definition node is named.
#[derive(Clone, Copy, Debug)]
enum Name {
    /// The text of a field (usually `name`).
    Field(&'static str),
    /// `const f = () => …`: the declarator's `name`, only when its `value` is
    /// a function or class expression.
    Declarator,
    /// Go method: `Recv.Name`, the receiver's base type plus the method name.
    GoMethod,
    /// Rust `impl Type` / `impl Trait for Type`.
    RustImpl,
}

/// Whether nested definitions are named with the language's function
/// separator (inner functions) or with `.` (members of a container).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scope {
    Function,
    Container,
}

#[derive(Clone, Copy, Debug)]
struct DefRule {
    kind: &'static str,
    name: Name,
    scope: Scope,
}

/// A language's entity spec.
#[derive(Debug)]
pub struct Spec {
    defs: &'static [DefRule],
    /// Separator between a function and a definition nested in it.
    fn_sep: &'static str,
    /// Node kinds that are comments.
    comments: &'static [&'static str],
    /// Substrings counted as skip markers in test files (rule R011).
    pub skip_markers: &'static [&'static str],
    /// Module-level declarations can change behaviour without executing a
    /// line (compile-time constants, types, generics, macros). Probes for
    /// static languages must report the test unit's source closure in
    /// `files` (ADR 013).
    pub is_static: bool,
}

const fn rule(kind: &'static str, name: Name, scope: Scope) -> DefRule {
    DefRule { kind, name, scope }
}

const JS_FUNCTION_VALUES: &[&str] = &[
    "arrow_function",
    "function_expression",
    "function",
    "generator_function",
    "class",
];

const JS_DEFS: &[DefRule] = &[
    rule("function_declaration", Name::Field("name"), Scope::Function),
    rule(
        "generator_function_declaration",
        Name::Field("name"),
        Scope::Function,
    ),
    rule("class_declaration", Name::Field("name"), Scope::Container),
    rule(
        "abstract_class_declaration",
        Name::Field("name"),
        Scope::Container,
    ),
    rule("method_definition", Name::Field("name"), Scope::Function),
    rule("variable_declarator", Name::Declarator, Scope::Function),
];

const JS_SKIPS: &[&str] = &[
    ".skip(",
    "xit(",
    "xtest(",
    "xdescribe(",
    ".todo(",
    ".skipIf(",
];

static PYTHON: Spec = Spec {
    defs: &[
        rule("function_definition", Name::Field("name"), Scope::Function),
        rule("class_definition", Name::Field("name"), Scope::Container),
    ],
    fn_sep: ".<locals>.",
    comments: &["comment"],
    skip_markers: &[
        "mark.skip",
        "mark.xfail",
        "pytest.skip(",
        "pytest.xfail(",
        "unittest.skip",
    ],
    is_static: false,
};

static JAVASCRIPT: Spec = Spec {
    defs: JS_DEFS,
    fn_sep: ".",
    comments: &["comment", "html_comment"],
    skip_markers: JS_SKIPS,
    is_static: false,
};

static GO: Spec = Spec {
    defs: &[
        rule("function_declaration", Name::Field("name"), Scope::Function),
        rule("method_declaration", Name::GoMethod, Scope::Function),
    ],
    fn_sep: ".",
    comments: &["comment"],
    skip_markers: &["t.Skip(", "t.Skipf(", "t.SkipNow(", "b.Skip("],
    is_static: true,
};

static RUST: Spec = Spec {
    defs: &[
        rule("function_item", Name::Field("name"), Scope::Function),
        rule("impl_item", Name::RustImpl, Scope::Container),
        rule("trait_item", Name::Field("name"), Scope::Container),
        rule("mod_item", Name::Field("name"), Scope::Container),
    ],
    fn_sep: ".",
    comments: &["line_comment", "block_comment"],
    skip_markers: &["#[ignore"],
    is_static: true,
};

static JAVA: Spec = Spec {
    defs: &[
        rule("class_declaration", Name::Field("name"), Scope::Container),
        rule(
            "interface_declaration",
            Name::Field("name"),
            Scope::Container,
        ),
        rule("enum_declaration", Name::Field("name"), Scope::Container),
        rule("record_declaration", Name::Field("name"), Scope::Container),
        rule(
            "annotation_type_declaration",
            Name::Field("name"),
            Scope::Container,
        ),
        rule("method_declaration", Name::Field("name"), Scope::Function),
        rule(
            "constructor_declaration",
            Name::Field("name"),
            Scope::Function,
        ),
        rule(
            "compact_constructor_declaration",
            Name::Field("name"),
            Scope::Function,
        ),
    ],
    fn_sep: ".",
    comments: &["line_comment", "block_comment"],
    skip_markers: &["@Disabled", "@Ignore", "Assumptions.assume", "assumeTrue("],
    is_static: true,
};

impl Language {
    /// The language of a repository path, by extension.
    pub fn from_path(path: &str) -> Option<Self> {
        let ext = path.rsplit_once('.').map(|(_, e)| e)?;
        Some(match ext {
            "py" => Self::Python,
            "js" | "mjs" | "cjs" | "jsx" => Self::JavaScript,
            "ts" | "mts" | "cts" => Self::TypeScript,
            "tsx" => Self::Tsx,
            "go" => Self::Go,
            "rs" => Self::Rust,
            "java" => Self::Java,
            _ => return None,
        })
    }

    /// JavaScript or TypeScript (ES module semantics).
    pub fn is_js_family(self) -> bool {
        matches!(self, Self::JavaScript | Self::TypeScript | Self::Tsx)
    }

    /// The language's entity spec.
    pub fn spec(self) -> &'static Spec {
        match self {
            Self::Python => &PYTHON,
            Self::JavaScript | Self::TypeScript | Self::Tsx => &JAVASCRIPT,
            Self::Go => &GO,
            Self::Rust => &RUST,
            Self::Java => &JAVA,
        }
    }

    fn grammar(self) -> tree_sitter::Language {
        match self {
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Self::Go => tree_sitter_go::LANGUAGE.into(),
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Java => tree_sitter_java::LANGUAGE.into(),
        }
    }
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

fn parse(lang: Language, src: &[u8]) -> Result<tree_sitter::Tree, ChangeError> {
    let mut p = Parser::new();
    p.set_language(&lang.grammar())
        .map_err(|e| ChangeError::Parser(e.to_string()))?;
    p.parse(src, None)
        .ok_or_else(|| ChangeError::Parser("parse returned no tree".into()))
}

struct Cx<'a> {
    src: &'a [u8],
    spec: &'static Spec,
}

impl Cx<'_> {
    fn text(&self, n: Node<'_>) -> String {
        String::from_utf8_lossy(&self.src[n.byte_range()]).into_owned()
    }

    /// If `node` defines an entity: its name, scope kind and body.
    fn definition<'t>(&self, node: Node<'t>) -> Option<(String, Scope, Option<Node<'t>>)> {
        let rule = self.spec.defs.iter().find(|r| r.kind == node.kind())?;
        let field = |n: Node<'t>, f: &str| n.child_by_field_name(f);
        let (name, body) = match rule.name {
            Name::Field(f) => (self.text(field(node, f)?), field(node, "body")),
            Name::Declarator => {
                let value = field(node, "value")?;
                if !JS_FUNCTION_VALUES.contains(&value.kind()) {
                    return None;
                }
                (self.text(field(node, "name")?), field(value, "body"))
            }
            Name::GoMethod => {
                let recv = field(node, "receiver")?;
                let mut ty = String::new();
                let mut stack = vec![recv];
                while let Some(n) = stack.pop() {
                    if n.kind() == "type_identifier" {
                        ty = self.text(n);
                        break;
                    }
                    let mut c = n.walk();
                    let children: Vec<_> = n.named_children(&mut c).collect();
                    stack.extend(children.into_iter().rev());
                }
                (
                    format!("{ty}.{}", self.text(field(node, "name")?)),
                    field(node, "body"),
                )
            }
            Name::RustImpl => {
                let ty = self.text(field(node, "type")?);
                let name = match field(node, "trait") {
                    Some(t) => format!("<{ty} as {}>", self.text(t)),
                    None => ty,
                };
                (name, field(node, "body"))
            }
        };
        // A definition without a body (`mod foo;`, an abstract method, an
        // overload signature) isn't an entity: it is part of its scope.
        let body = body?;
        Some((name, rule.scope, Some(body)))
    }

    fn inner_scope(&self, qual: &str, scope: Scope) -> String {
        match scope {
            Scope::Function => format!("{qual}{}", self.spec.fn_sep),
            Scope::Container => format!("{qual}."),
        }
    }
}

fn emit_leaf(text: &[u8], out: &mut String) {
    let s = String::from_utf8_lossy(text);
    out.push_str(&s.len().to_string());
    out.push(':');
    out.push_str(&s);
    out.push(' ');
}

fn emit(
    node: Node<'_>,
    cx: &Cx<'_>,
    scope: &str,
    out: &mut String,
    entities: &mut BTreeMap<String, Digest>,
) {
    if cx.spec.comments.contains(&node.kind()) {
        return;
    }
    if let Some((name, kind, body)) = cx.definition(node) {
        let qual = format!("{scope}{name}");
        let body_id = body.map(|b| b.id());
        // Header: everything except the body, part of both streams.
        let mut header = format!("({} ", node.kind());
        emit_except(node, cx, scope, body_id, &mut header, entities);
        out.push_str(&header);
        out.push_str("<body:");
        out.push_str(&qual);
        out.push_str(">) ");
        let mut own = header;
        if let Some(b) = body {
            emit(b, cx, &cx.inner_scope(&qual, kind), &mut own, entities);
        }
        own.push(')');
        let h = hash_domain(Domain::Entity, own.as_bytes());
        entities
            .entry(qual)
            .and_modify(|prev| {
                let mut buf = prev.as_bytes().to_vec();
                buf.extend_from_slice(h.as_bytes());
                *prev = hash_domain(Domain::Entity, &buf);
            })
            .or_insert(h);
        return;
    }
    if node.child_count() == 0 {
        emit_leaf(&cx.src[node.byte_range()], out);
        return;
    }
    out.push('(');
    out.push_str(node.kind());
    out.push(' ');
    emit_except(node, cx, scope, None, out, entities);
    out.push_str(") ");
}

/// Emits `node`'s children, skipping the subtree with id `skip` (a body).
/// For a declarator, the body sits inside the `value` child, so the value
/// is emitted around it.
fn emit_except(
    node: Node<'_>,
    cx: &Cx<'_>,
    scope: &str,
    skip: Option<usize>,
    out: &mut String,
    entities: &mut BTreeMap<String, Digest>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if Some(child.id()) == skip {
            continue;
        }
        let contains_skip = skip.is_some_and(|s| contains(child, s));
        if contains_skip {
            out.push('(');
            out.push_str(child.kind());
            out.push(' ');
            emit_except(child, cx, scope, skip, out, entities);
            out.push_str(") ");
        } else {
            emit(child, cx, scope, out, entities);
        }
    }
}

fn contains(node: Node<'_>, id: usize) -> bool {
    let mut c = node.walk();
    let children: Vec<_> = node.children(&mut c).collect();
    children
        .into_iter()
        .any(|ch| ch.id() == id || contains(ch, id))
}

struct Analysis {
    module: Digest,
    entities: BTreeMap<String, Digest>,
    had_error: bool,
}

fn analyze(lang: Language, src: &[u8]) -> Result<Analysis, ChangeError> {
    let src = normalize(src);
    let tree = parse(lang, &src)?;
    let cx = Cx {
        src: &src,
        spec: lang.spec(),
    };
    let mut entities = BTreeMap::new();
    let mut module = String::new();
    emit(tree.root_node(), &cx, "", &mut module, &mut entities);
    Ok(Analysis {
        module: hash_domain(Domain::Entity, module.as_bytes()),
        entities,
        had_error: tree.root_node().has_error(),
    })
}

fn count_skip_markers(spec: &Spec, src: Option<&[u8]>) -> u32 {
    let Some(src) = src else { return 0 };
    let text = String::from_utf8_lossy(src);
    spec.skip_markers
        .iter()
        .map(|m| text.matches(m).count() as u32)
        .sum()
}

/// Entity-level delta between two versions of a source file in `lang`.
/// `None` means the file is absent on that side. Python uses
/// [`crate::python::diff_with`] instead.
pub fn diff(
    lang: Language,
    old: Option<&[u8]>,
    new: Option<&[u8]>,
) -> Result<SourceDelta, ChangeError> {
    let a = old.map(|s| analyze(lang, s)).transpose()?;
    let b = new.map(|s| analyze(lang, s)).transpose()?;
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
    let mut module_changed = match (&a, &b) {
        (Some(x), Some(y)) => x.module != y.module || parse_error,
        _ => true,
    };
    // ADR 020: new top-level function declarations, and nothing else at
    // module level. The caller still checks the repository for code that
    // could observe the module's set of names.
    let mut added = Vec::new();
    if module_changed && !parse_error && lang.is_js_family() {
        if let (Some(x), Some(old), Some(new)) = (&a, old, new) {
            if let Some((names, stripped)) = js_added_functions(lang, old, new)? {
                let y = analyze(lang, &stripped)?;
                if !y.had_error && y.module == x.module {
                    module_changed = false;
                    added = names;
                }
            }
        }
    }
    let removed = ea
        .keys()
        .filter(|k| !eb.contains_key(*k))
        .cloned()
        .collect();
    let spec = lang.spec();
    Ok(SourceDelta {
        module_changed,
        entities,
        parse_error,
        removed,
        skip_markers: (count_skip_markers(spec, old), count_skip_markers(spec, new)),
        added,
    })
}

/// True for byte `c` that can be part of a JavaScript identifier (ASCII).
fn js_ident_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$'
}

/// True if `name` occurs in `src` as a whole identifier-like word: in code,
/// comments or strings alike (so `obj[name]`, `"name"` and forward
/// references all count).
fn js_word_occurs(src: &[u8], name: &str) -> bool {
    let n = name.as_bytes();
    if n.is_empty() || src.len() < n.len() {
        return false;
    }
    (0..=src.len() - n.len()).any(|i| {
        &src[i..i + n.len()] == n
            && (i == 0 || !js_ident_byte(src[i - 1]))
            && src.get(i + n.len()).is_none_or(|&c| !js_ident_byte(c))
    })
}

/// Names of added functions, and the new source without them.
type Added = (Vec<String>, Vec<u8>);

/// Top-level function declarations in `new` (plain or `export`ed, not
/// `export default`) whose names don't occur anywhere in `old`, with `new`
/// minus those statements. `None` if there are none.
///
/// Declaring a function runs no code (declarations are hoisted) and only
/// binds a name nothing at `old` could refer to; an ES module can't import
/// a name that doesn't exist (a link error). Whether the rest of the module
/// is unchanged is decided by comparing streams, so a removal that changes
/// how the remaining code parses is never exempt.
fn js_added_functions(
    lang: Language,
    old: &[u8],
    new: &[u8],
) -> Result<Option<Added>, ChangeError> {
    const DECLS: [&str; 2] = ["function_declaration", "generator_function_declaration"];
    let tree = parse(lang, new)?;
    let root = tree.root_node();
    let mut cut: Vec<(usize, usize)> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    let mut cursor = root.walk();
    for stmt in root.named_children(&mut cursor) {
        let decl = if DECLS.contains(&stmt.kind()) {
            Some(stmt)
        } else if stmt.kind() == "export_statement" {
            let mut c = stmt.walk();
            let default = stmt.children(&mut c).any(|ch| ch.kind() == "default");
            stmt.child_by_field_name("declaration")
                .filter(|d| !default && DECLS.contains(&d.kind()))
        } else {
            None
        };
        let Some(decl) = decl else { continue };
        let Some(name) = decl.child_by_field_name("name") else {
            continue;
        };
        let name = String::from_utf8_lossy(&new[name.byte_range()]).into_owned();
        if js_word_occurs(old, &name) {
            continue;
        }
        cut.push((stmt.start_byte(), stmt.end_byte()));
        names.push(name);
    }
    if cut.is_empty() {
        return Ok(None);
    }
    let mut stripped = Vec::with_capacity(new.len());
    let mut at = 0;
    for (s, e) in cut {
        stripped.extend_from_slice(&new[at..s]);
        stripped.push(b'\n');
        at = e;
    }
    stripped.extend_from_slice(&new[at..]);
    names.sort();
    names.dedup();
    Ok(Some((names, stripped)))
}

/// A definition's (start, end, name index); positions are 0-based
/// (row, column).
type Span = ((usize, usize), (usize, usize), u32);

/// Leaf values a read-only table may hold: immutable literals, tuples of
/// them, and `Decimal(...)` / `Fraction(...)` of literals when the file
/// imports those names from the standard library (`numbers` lists them).
fn py_immutable_literal(n: Node<'_>, numbers: &[&str], src: &[u8]) -> bool {
    match n.kind() {
        "string" | "concatenated_string" | "integer" | "float" | "true" | "false" | "none" => true,
        "unary_operator" => {
            let mut c = n.walk();
            n.named_children(&mut c)
                .all(|ch| matches!(ch.kind(), "integer" | "float"))
        }
        "tuple" | "parenthesized_expression" => {
            let mut c = n.walk();
            n.named_children(&mut c)
                .filter(|ch| ch.kind() != "comment")
                .all(|ch| py_immutable_literal(ch, numbers, src))
        }
        "call" => {
            let callee = n
                .child_by_field_name("function")
                .map(|f| std::str::from_utf8(&src[f.byte_range()]).unwrap_or(""))
                .unwrap_or("");
            let Some(args) = n.child_by_field_name("arguments") else {
                return false;
            };
            let mut c = args.walk();
            numbers.contains(&callee)
                && args.kind() == "argument_list"
                && args
                    .named_children(&mut c)
                    .filter(|ch| ch.kind() != "comment")
                    .all(|ch| {
                        matches!(ch.kind(), "string" | "integer" | "float" | "unary_operator")
                            && py_immutable_literal(ch, numbers, src)
                    })
        }
        _ => false,
    }
}

/// Constructors of immutable numbers the file imports from the standard
/// library: `Decimal` / `Fraction` (`from decimal import Decimal`), or
/// `decimal.Decimal` / `fractions.Fraction` (`import decimal`). An alias or
/// a module-level redefinition doesn't count.
fn py_number_constructors(root: Node<'_>, src: &[u8]) -> Vec<&'static str> {
    let text = |n: Node<'_>| std::str::from_utf8(&src[n.byte_range()]).unwrap_or("");
    let mut out = Vec::new();
    let mut c = root.walk();
    for stmt in root.named_children(&mut c) {
        let t = text(stmt).split_whitespace().collect::<Vec<_>>().join(" ");
        for (module, name, qualified) in [
            ("decimal", "Decimal", "decimal.Decimal"),
            ("fractions", "Fraction", "fractions.Fraction"),
        ] {
            let from = format!("from {module} import ");
            match stmt.kind() {
                "import_from_statement" if t.starts_with(&from) => {
                    if t[from.len()..]
                        .trim_matches(|ch| ch == '(' || ch == ')')
                        .split(',')
                        .any(|w| w.trim() == name)
                    {
                        out.push(name);
                    }
                }
                "import_statement" if t == format!("import {module}") => out.push(qualified),
                _ => {}
            }
        }
    }
    // A module-level rebinding (`Decimal = ...`, `def Decimal`, `class
    // Decimal`) means the name isn't the standard library's.
    let mut c = root.walk();
    let rebound: Vec<String> = root
        .named_children(&mut c)
        .filter_map(|stmt| match stmt.kind() {
            "function_definition" | "class_definition" => {
                stmt.child_by_field_name("name").map(|n| text(n).to_owned())
            }
            "expression_statement" => stmt
                .named_child(0)
                .filter(|a| a.kind() == "assignment")
                .and_then(|a| a.child_by_field_name("left"))
                .map(|l| text(l).to_owned()),
            _ => None,
        })
        .collect();
    out.retain(|n| !rebound.iter().any(|r| r == n));
    out
}

/// A module-level `NAME = <list/dict/set display of immutable literals>`
/// that no code in the file can mutate: a constant table, not state.
///
/// Every use of NAME must be read-only: an index read (not an assignment
/// or `del` target), the iterable of a loop or comprehension, a comparison
/// (`in`) or condition, an argument to a pure builtin, a non-mutating
/// method call, a rebinding, or the default value of a parameter that is
/// itself only read in its function. Anything else (passing it elsewhere,
/// returning or aliasing it, mutating methods, `global`) might mutate it.
/// Mutation from other files stays the documented cross-file assumption.
fn py_read_only_table(assign: Node<'_>, root: Node<'_>, src: &[u8]) -> bool {
    let text = |n: Node<'_>| std::str::from_utf8(&src[n.byte_range()]).unwrap_or("");
    // Only module-level simple names (`NAME = ...` directly under the module).
    let module_level = assign
        .parent()
        .is_some_and(|p| p.kind() == "expression_statement" && p.parent() == Some(root));
    let (Some(left), Some(right)) = (
        assign.child_by_field_name("left"),
        assign.child_by_field_name("right"),
    ) else {
        return false;
    };
    if !module_level || left.kind() != "identifier" {
        return false;
    }
    let numbers = py_number_constructors(root, src);
    let leaf = |n: Node<'_>| py_immutable_literal(n, &numbers, src);
    let elements_immutable = match right.kind() {
        "list" | "set" => {
            let mut c = right.walk();
            right
                .named_children(&mut c)
                .filter(|ch| ch.kind() != "comment")
                .all(leaf)
        }
        "dictionary" => {
            let mut c = right.walk();
            right
                .named_children(&mut c)
                .filter(|ch| ch.kind() != "comment")
                .all(|pair| {
                    pair.kind() == "pair"
                        && pair.child_by_field_name("key").is_some_and(leaf)
                        && pair.child_by_field_name("value").is_some_and(leaf)
                })
        }
        _ => false,
    };
    elements_immutable && py_only_read(root, text(left), src, 2)
}

/// True if every use of `name` under `scope` is read-only (see
/// [`py_read_only_table`]). A use as a parameter's default value is
/// read-only if that parameter is only read in its function's body,
/// followed through at most `depth` such defaults.
fn py_only_read(scope: Node<'_>, name: &str, src: &[u8], depth: u32) -> bool {
    const PURE_CALLS: &[&str] = &[
        "len",
        "sorted",
        "list",
        "tuple",
        "set",
        "frozenset",
        "dict",
        "sum",
        "min",
        "max",
        "any",
        "all",
        "enumerate",
        "zip",
        "iter",
        "reversed",
        "repr",
        "str",
        "bool",
        "isinstance",
        "hash",
        "print",
        "map",
        "filter",
    ];
    const READ_METHODS: &[&str] = &[
        "get",
        "keys",
        "values",
        "items",
        "index",
        "count",
        "copy",
        "issubset",
        "issuperset",
        "union",
        "intersection",
        "difference",
        "symmetric_difference",
        "isdisjoint",
    ];
    let text = |n: Node<'_>| std::str::from_utf8(&src[n.byte_range()]).unwrap_or("");
    let read_only_use = |id: Node<'_>| -> bool {
        let Some(p) = id.parent() else { return false };
        let is = |field: &str| p.child_by_field_name(field) == Some(id);
        match p.kind() {
            // Rebinding the name (the definition, or a later assignment).
            "assignment" if is("left") => true,
            "subscript" if is("value") => match p.parent() {
                Some(pp) => {
                    let target = matches!(pp.kind(), "assignment" | "augmented_assignment")
                        && pp.child_by_field_name("left") == Some(p);
                    !(target || pp.kind() == "delete_statement")
                }
                None => true,
            },
            "for_statement" | "for_in_clause" => is("right"),
            "comparison_operator"
            | "not_operator"
            | "if_statement"
            | "while_statement"
            | "elif_clause"
            | "assert_statement"
            | "expression_statement" => true,
            "argument_list" => p
                .parent()
                .filter(|c| c.kind() == "call")
                .and_then(|c| c.child_by_field_name("function"))
                .is_some_and(|f| f.kind() == "identifier" && PURE_CALLS.contains(&text(f))),
            "attribute" if is("object") => {
                let method = p.child_by_field_name("attribute").map(text).unwrap_or("");
                match p.parent() {
                    Some(c) if c.kind() == "call" => READ_METHODS.contains(&method),
                    // Attribute read without a call (`NAME.__class__`).
                    _ => true,
                }
            }
            // `obj.NAME` or `f(NAME=...)`: a different name.
            // (`f(x=NAME)` passes the table on, which may mutate it.)
            "attribute" => true,
            "keyword_argument" => is("name"),
            // `def f(x=NAME)`: every call without x hands NAME to f, so f's
            // uses of x must be read-only too (the mutable-default problem).
            "default_parameter" | "typed_default_parameter" if is("value") => {
                let param = p.child_by_field_name("name").map(text).unwrap_or("");
                let body = p
                    .parent()
                    .and_then(|ps| ps.parent())
                    .filter(|f| f.kind() == "function_definition")
                    .and_then(|f| f.child_by_field_name("body"));
                depth > 0
                    && !param.is_empty()
                    && body.is_some_and(|b| py_only_read(b, param, src, depth - 1))
            }
            _ => false,
        }
    };
    let mut stack = vec![scope];
    while let Some(n) = stack.pop() {
        if matches!(n.kind(), "global_statement" | "nonlocal_statement")
            && text(n)
                .split_whitespace()
                .any(|w| w.trim_matches(',') == name)
        {
            return false;
        }
        if n.kind() == "identifier" && text(n) == name && !read_only_use(n) {
            return false;
        }
        let mut c = n.walk();
        stack.extend(n.children(&mut c));
    }
    true
}

/// `__name__`-style identifier.
fn is_dunder(name: &str) -> bool {
    let name = name.trim();
    name.len() > 4
        && name.starts_with("__")
        && name.ends_with("__")
        && name[2..name.len() - 2]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn kind_in(n: Option<Node<'_>>, kinds: &[&str]) -> bool {
    n.is_some_and(|n| kinds.contains(&n.kind()))
}

/// Mutable state at module or class level (see [`LineMap::module_state`]).
/// A syntactic, conservative-by-construction check per language:
///
/// - JavaScript/TypeScript: `let`/`var` at the top level; `const` bound to an
///   object, array or `new` expression; static class fields with such
///   values;
/// - Python: module- or class-level assignments of dict/list/set displays,
///   comprehensions or `dict()`/`set()`/`defaultdict()`-style calls, and any
///   `global` statement;
/// - Go: package-level `var`; Rust: `static`, `thread_local!`; Java: `static`
///   fields that aren't `final` primitives or strings.
fn has_module_state(lang: Language, root: Node<'_>, src: &[u8]) -> bool {
    const JS_MUTABLE: &[&str] = &["object", "array", "new_expression"];
    const PY_MUTABLE: &[&str] = &[
        "dictionary",
        "list",
        "set",
        "dictionary_comprehension",
        "list_comprehension",
        "set_comprehension",
    ];
    const PY_CALLS: &[&str] = &[
        "dict",
        "list",
        "set",
        "defaultdict",
        "OrderedDict",
        "Counter",
        "deque",
        "WeakValueDictionary",
        "WeakKeyDictionary",
        "WeakSet",
        "ChainMap",
    ];
    let text = |n: Node<'_>| std::str::from_utf8(&src[n.byte_range()]).unwrap_or("");
    // (node, depth, inside a Python function)
    let mut stack = vec![(root, 0usize, false)];
    while let Some((n, depth, in_fn)) = stack.pop() {
        let found = match lang {
            Language::JavaScript | Language::TypeScript | Language::Tsx => match n.kind() {
                "lexical_declaration" | "variable_declaration" if depth <= 2 => {
                    let mut c = n.walk();
                    let kw = n.children(&mut c).next().map(text).unwrap_or("");
                    kw != "const" || {
                        let mut c = n.walk();
                        let decls: Vec<_> = n.named_children(&mut c).collect();
                        decls
                            .iter()
                            .any(|d| kind_in(d.child_by_field_name("value"), JS_MUTABLE))
                    }
                }
                "field_definition" | "public_field_definition" => {
                    let mut c = n.walk();
                    let is_static = n.children(&mut c).any(|ch| ch.kind() == "static");
                    is_static && kind_in(n.child_by_field_name("value"), JS_MUTABLE)
                }
                _ => false,
            },
            Language::Python => match n.kind() {
                "global_statement" => true,
                // Dunder targets (`__all__`, `__slots__`, `__match_args__`)
                // are declarations the language and tools read, not state
                // code mutates between calls.
                "assignment"
                    if !in_fn
                        && depth <= 6
                        && !n
                            .child_by_field_name("left")
                            .is_some_and(|l| is_dunder(text(l))) =>
                {
                    let right = n.child_by_field_name("right");
                    let mutable = kind_in(right, PY_MUTABLE)
                        || right.is_some_and(|r| {
                            r.kind() == "call"
                                && r.child_by_field_name("function").is_some_and(|f| {
                                    let name = text(f).rsplit('.').next().unwrap_or("");
                                    PY_CALLS.contains(&name)
                                })
                        });
                    mutable && !py_read_only_table(n, root, src)
                }
                _ => false,
            },
            Language::Go => n.kind() == "var_declaration" && depth <= 1,
            Language::Rust => {
                matches!(n.kind(), "static_item")
                    || (n.kind() == "macro_invocation" && text(n).starts_with("thread_local!"))
            }
            Language::Java => {
                n.kind() == "field_declaration" && {
                    let t = text(n);
                    let head = t.split('=').next().unwrap_or(t);
                    head.contains("static")
                        && !(head.contains("final")
                            && n.child_by_field_name("type").is_some_and(|ty| {
                                matches!(
                                    ty.kind(),
                                    "integral_type" | "floating_point_type" | "boolean_type"
                                ) || text(ty) == "String"
                            }))
                }
            }
        };
        if found {
            return true;
        }
        // Function bodies are excluded: their locals aren't shared state
        // (except Python's `global`, found anywhere).
        let is_function = matches!(
            n.kind(),
            "function_declaration"
                | "function_expression"
                | "arrow_function"
                | "method_definition"
                | "generator_function_declaration"
                | "function_item"
                | "method_declaration"
                | "constructor_declaration"
        ) || (n.kind() == "function_definition" && lang != Language::Python);
        if is_function {
            continue;
        }
        let in_fn = in_fn || n.kind() == "function_definition";
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            stack.push((ch, depth + 1, in_fn));
        }
    }
    false
}

/// Maps executed source lines to entities, for probes that report lines.
#[derive(Clone, Debug, Default)]
pub struct LineMap {
    /// Per 1-based line, the innermost entity containing it (index into
    /// `names`). Index 0 is line 1.
    by_line: Vec<Option<u32>>,
    names: Vec<String>,
    /// The file keeps mutable state across calls at module (or class) level:
    /// a cache, a registry, a counter. See [`LineMap::module_state`].
    module_state: bool,
    /// Definition spans in pre-order: ((start row, col), (end row, col),
    /// name index), rows and columns 0-based.
    spans: Vec<Span>,
}

impl LineMap {
    /// Builds the map for one file.
    pub fn new(lang: Language, src: &[u8]) -> Result<Self, ChangeError> {
        let src = normalize(src);
        let tree = parse(lang, &src)?;
        let cx = Cx {
            src: &src,
            spec: lang.spec(),
        };
        let lines = src.iter().filter(|b| **b == b'\n').count() + 1;
        let mut map = Self {
            by_line: vec![None; lines],
            names: Vec::new(),
            module_state: has_module_state(lang, tree.root_node(), &src),
            spans: Vec::new(),
        };
        map.walk(tree.root_node(), &cx, "");
        Ok(map)
    }

    /// Every entity of the file.
    pub fn entities(&self) -> &[String] {
        &self.names
    }

    /// True if the file keeps mutable state at module or class level. A
    /// function that reads that state can depend on a *different* function
    /// of the file that ran earlier, possibly in another test: a memoised
    /// result is read without re-running what computed it. Ingest therefore
    /// widens a test that touched such a file to every entity of the file.
    /// State shared *across* files stays a documented assumption.
    pub fn module_state(&self) -> bool {
        self.module_state
    }

    fn walk(&mut self, node: Node<'_>, cx: &Cx<'_>, scope: &str) {
        let mut next_scope = None;
        if let Some((name, kind, _)) = cx.definition(node) {
            let qual = format!("{scope}{name}");
            let idx = match self.names.iter().position(|n| *n == qual) {
                Some(i) => i as u32,
                None => {
                    self.names.push(qual.clone());
                    (self.names.len() - 1) as u32
                }
            };
            let (sp, ep) = (node.start_position(), node.end_position());
            self.spans
                .push(((sp.row, sp.column), (ep.row, ep.column), idx));
            // Outer definitions are visited first, so inner ones overwrite.
            let (start, end) = (sp.row, ep.row);
            for l in start..=end.min(self.by_line.len().saturating_sub(1)) {
                self.by_line[l] = Some(idx);
            }
            next_scope = Some(cx.inner_scope(&qual, kind));
        }
        let scope = next_scope.as_deref().unwrap_or(scope);
        let mut c = node.walk();
        let children: Vec<_> = node.children(&mut c).collect();
        for ch in children {
            self.walk(ch, cx, scope);
        }
    }

    /// The innermost entity containing the position (1-based line, 0-based
    /// column), or `None` for module-level code. Probes that know where
    /// each executed function starts report these positions, which also
    /// tells apart several definitions on one line.
    pub fn entity_at(&self, line: u32, col: u32) -> Option<&str> {
        let pos = (usize::try_from(line).ok()?.checked_sub(1)?, col as usize);
        // Pre-order: a later span containing the position is nested in an
        // earlier one, so the last match is the innermost.
        self.spans
            .iter()
            .rev()
            .find(|(s, e, _)| *s <= pos && pos < *e)
            .map(|(_, _, i)| self.names[*i as usize].as_str())
    }

    /// The innermost entity containing 1-based `line`, or `None` for
    /// module-level code (or a line past the end of the file).
    pub fn entity(&self, line: u32) -> Option<&str> {
        let i = usize::try_from(line).ok()?.checked_sub(1)?;
        self.by_line
            .get(i)
            .copied()
            .flatten()
            .map(|n| self.names[n as usize].as_str())
    }

    /// Entity names and whether module-level code ran, for inclusive
    /// 1-based line ranges.
    pub fn resolve(&self, ranges: &[(u32, u32)]) -> (Vec<&str>, bool) {
        let mut seen = vec![false; self.names.len()];
        let mut module = false;
        for &(a, b) in ranges {
            for l in a.max(1)..=b {
                match self.by_line.get(l as usize - 1) {
                    Some(Some(i)) => seen[*i as usize] = true,
                    Some(None) => module = true,
                    None => break,
                }
            }
        }
        let names = self
            .names
            .iter()
            .zip(seen)
            .filter(|(_, s)| *s)
            .map(|(n, _)| n.as_str())
            .collect();
        (names, module)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(lang: Language, old: &str, new: &str) -> SourceDelta {
        diff(lang, Some(old.as_bytes()), Some(new.as_bytes())).unwrap()
    }

    fn names(lang: Language, src: &str) -> Vec<String> {
        let m = LineMap::new(lang, src.as_bytes()).unwrap();
        let mut v = m.names.clone();
        v.sort();
        v
    }

    #[test]
    fn languages_by_extension() {
        assert_eq!(Language::from_path("a/b.ts"), Some(Language::TypeScript));
        assert_eq!(Language::from_path("x.tsx"), Some(Language::Tsx));
        assert_eq!(Language::from_path("x.mjs"), Some(Language::JavaScript));
        assert_eq!(Language::from_path("main.go"), Some(Language::Go));
        assert_eq!(Language::from_path("lib.rs"), Some(Language::Rust));
        assert_eq!(Language::from_path("A.java"), Some(Language::Java));
        assert_eq!(Language::from_path("x.rb"), None);
        assert_eq!(Language::from_path("Makefile"), None);
        assert!(Language::Go.spec().is_static && !Language::JavaScript.spec().is_static);
    }

    const JS: &str = "import { x } from './x.js';
export function add(a, b) {
  return a + b;
}
const mul = (a, b) => {
  return a * b;
};
class Calc extends Base {
  total = 0;
  run(n) {
    return add(n, 1);
  }
  static make() { return new Calc(); }
}
const LIMIT = 10;
";

    #[test]
    fn javascript_entities_and_locality() {
        assert_eq!(
            names(Language::JavaScript, JS),
            vec!["Calc", "Calc.make", "Calc.run", "add", "mul"]
        );
        let body = d(Language::JavaScript, JS, &JS.replace("a + b", "a - b"));
        assert!(!body.module_changed);
        assert_eq!(body.entities, vec!["add"]);
        let arrow = d(Language::JavaScript, JS, &JS.replace("a * b", "b * a"));
        assert_eq!(
            (arrow.module_changed, arrow.entities),
            (false, vec!["mul".to_string()])
        );
        let method = d(
            Language::JavaScript,
            JS,
            &JS.replace("add(n, 1)", "add(n, 2)"),
        );
        assert_eq!(
            (method.module_changed, method.entities),
            (false, vec!["Calc.run".to_string()])
        );
        // A class field runs at construction: it is the class entity.
        let field = d(
            Language::JavaScript,
            JS,
            &JS.replace("total = 0", "total = 1"),
        );
        assert_eq!(
            (field.module_changed, field.entities),
            (false, vec!["Calc".to_string()])
        );
        // Module-level code and headers run when the module is evaluated.
        for new in [
            JS.replace("LIMIT = 10", "LIMIT = 11"),
            JS.replace("extends Base", "extends Other"),
            JS.replace("add(a, b)", "add(a, b = 1)"),
            JS.replace("'./x.js'", "'./y.js'"),
        ] {
            assert!(d(Language::JavaScript, JS, &new).module_changed, "{new}");
        }
        // Formatting and comments are not changes.
        let fmt = JS
            .replace("return a + b;", "return a+b; // sum")
            .replace("  run(n)", "  /* x */ run(n)");
        let none = d(Language::JavaScript, JS, &fmt);
        assert!(!none.module_changed && none.entities.is_empty(), "{none:?}");
    }

    /// ADR 020 (luxon, E8): a new top-level function is not a module change.
    #[test]
    fn added_functions_are_not_module_changes() {
        for (new, name) in [
            (
                format!("{JS}export function snap(v) {{\n  return Math.round(v);\n}}\n"),
                "snap",
            ),
            (format!("function helper() {{}}\n{JS}"), "helper"),
            (
                JS.replace("const LIMIT", "function* gen() {}\nconst LIMIT"),
                "gen",
            ),
            (
                format!("{JS}export async function load() {{ await 0; }}\n"),
                "load",
            ),
        ] {
            let delta = d(Language::JavaScript, JS, &new);
            assert!(!delta.module_changed, "{new}");
            assert_eq!(delta.added, vec![name.to_owned()], "{new}");
            assert_eq!(delta.entities, vec![name.to_owned()], "{new}");
        }
        let ts = "export const a = 1;\n";
        let ts_new =
            "export const a = 1;\nexport function f(x: number): number {\n  return x;\n}\n";
        assert!(!d(Language::TypeScript, ts, ts_new).module_changed);

        // The name occurs at base (here in a string, as in `obj["later"]`):
        // something may have looked it up.
        let base = format!("{JS}const k = 'later';\n");
        let delta = d(
            Language::JavaScript,
            &base,
            &format!("{base}function later() {{}}\n"),
        );
        assert!(delta.module_changed && delta.added.is_empty(), "{delta:?}");

        // Still module changes:
        for new in [
            // Another module-level change comes with it.
            JS.replace("const LIMIT = 10;", "const LIMIT = 10;\nconst k = 'later';")
                + "function later() {}\n",
            // Default export: changes what `import x from` gets.
            format!("{JS}export default function main() {{}}\n"),
            // Anything else at module level changed too.
            JS.replace("LIMIT = 10", "LIMIT = 11") + "function extra() {}\n",
            // Not a declaration: a const arrow function runs an initializer.
            format!("{JS}export const fresh = () => 1;\n"),
        ] {
            let delta = d(Language::JavaScript, JS, &new);
            assert!(delta.module_changed, "{new}");
        }
        // Removing a function is a module change (an importer of it breaks).
        let without = JS.replace("export function add(a, b) {\n  return a + b;\n}\n", "");
        assert!(d(Language::JavaScript, JS, &without).module_changed);
    }

    #[test]
    fn typescript_and_tsx() {
        let ts = "interface P { a: number }
export class S {
  constructor(private n: number) {}
  get(): number {
    return this.n;
  }
}
export const f = (p: P): number => {
  return p.a;
};
";
        assert_eq!(
            names(Language::TypeScript, ts),
            vec!["S", "S.constructor", "S.get", "f"]
        );
        let body = d(
            Language::TypeScript,
            ts,
            &ts.replace("this.n;", "this.n + 1;"),
        );
        assert_eq!(
            (body.module_changed, body.entities),
            (false, vec!["S.get".to_string()])
        );
        // Interfaces are erased at runtime, but count as module-level (conservative).
        assert!(
            d(
                Language::TypeScript,
                ts,
                &ts.replace("a: number", "a: string")
            )
            .module_changed
        );
        let tsx = "export function View() {
  return <div>hi</div>;
}
";
        let ch = d(Language::Tsx, tsx, &tsx.replace("hi", "bye"));
        assert_eq!(
            (ch.module_changed, ch.entities),
            (false, vec!["View".to_string()])
        );
    }

    const GO_SRC: &str = "package calc

const Limit = 10

type T struct{ n int }

func Add(a, b int) int {
	return a + b
}

func (t *T) Get() int {
	return t.n
}
";

    #[test]
    fn go_functions_methods_and_static_declarations() {
        assert_eq!(names(Language::Go, GO_SRC), vec!["Add", "T.Get"]);
        let body = d(Language::Go, GO_SRC, &GO_SRC.replace("a + b", "a - b"));
        assert_eq!(
            (body.module_changed, body.entities),
            (false, vec!["Add".to_string()])
        );
        let m = d(
            Language::Go,
            GO_SRC,
            &GO_SRC.replace("return t.n", "return t.n * 2"),
        );
        assert_eq!(m.entities, vec!["T.Get"]);
        // Constants and types change behaviour without executing: module-level.
        assert!(
            d(
                Language::Go,
                GO_SRC,
                &GO_SRC.replace("Limit = 10", "Limit = 11")
            )
            .module_changed
        );
        assert!(
            d(
                Language::Go,
                GO_SRC,
                &GO_SRC.replace("n int }", "n int64 }")
            )
            .module_changed
        );
        // A signature is the header.
        assert!(
            d(
                Language::Go,
                GO_SRC,
                &GO_SRC.replace("Add(a, b int) int", "Add(a, b int64) int64")
            )
            .module_changed
        );
    }

    const RS: &str = "const N: u32 = 3;

pub fn twice(x: u32) -> u32 {
    x * 2
}

struct S;

impl S {
    fn get(&self) -> u32 {
        N
    }
}

impl Display for S {
    fn fmt(&self, f: &mut Formatter) -> Result {
        write!(f, \"s\")
    }
}

mod inner {
    pub fn h() {}
}

mod decl;
";

    #[test]
    fn rust_items() {
        assert_eq!(
            names(Language::Rust, RS),
            vec![
                "<S as Display>",
                "<S as Display>.fmt",
                "S",
                "S.get",
                "inner",
                "inner.h",
                "twice"
            ]
        );
        let body = d(Language::Rust, RS, &RS.replace("x * 2", "x * 3"));
        assert_eq!(
            (body.module_changed, body.entities),
            (false, vec!["twice".to_string()])
        );
        let m = d(
            Language::Rust,
            RS,
            &RS.replace("write!(f, \"s\")", "write!(f, \"t\")"),
        );
        assert_eq!(m.entities, vec!["<S as Display>.fmt"]);
        assert!(d(Language::Rust, RS, &RS.replace("N: u32 = 3", "N: u32 = 4")).module_changed);
    }

    const JAVA_SRC: &str = "package p;

public class Calc {
  static final int LIMIT = 10;
  private int n;
  public Calc(int n) {
    this.n = n;
  }
  public int add(int a) {
    return n + a;
  }
  public int add(int a, int b) {
    return a + b;
  }
}
";

    #[test]
    fn java_classes_methods_and_overloads() {
        assert_eq!(
            names(Language::Java, JAVA_SRC),
            vec!["Calc", "Calc.Calc", "Calc.add"]
        );
        // Overloads share a name and combine: either change is a change to it.
        let second = d(
            Language::Java,
            JAVA_SRC,
            &JAVA_SRC.replace("return a + b", "return a - b"),
        );
        assert_eq!(
            (second.module_changed, second.entities),
            (false, vec!["Calc.add".to_string()])
        );
        // A static final constant is the class entity (javac also inlines it
        // into callers: the static closure covers that, ADR 013).
        let k = d(
            Language::Java,
            JAVA_SRC,
            &JAVA_SRC.replace("LIMIT = 10", "LIMIT = 11"),
        );
        assert_eq!(
            (k.module_changed, k.entities),
            (false, vec!["Calc".to_string()])
        );
    }

    #[test]
    fn line_map_resolves_innermost_entities() {
        let m = LineMap::new(Language::JavaScript, JS.as_bytes()).unwrap();
        assert_eq!(m.entity(1), None); // import: module level
        assert_eq!(m.entity(3), Some("add"));
        assert_eq!(m.entity(6), Some("mul"));
        assert_eq!(m.entity(9), Some("Calc")); // class field
        assert_eq!(m.entity(11), Some("Calc.run"));
        assert_eq!(m.entity(15), None);
        assert_eq!(m.entity(999), None);
        // Positions tell apart definitions sharing a line.
        assert_eq!(m.entity_at(5, 12), Some("mul")); // the arrow's parameters
        assert_eq!(m.entity_at(5, 6), Some("mul"));
        assert_eq!(m.entity_at(5, 0), None); // `const`: module level
        let one = LineMap::new(
            Language::JavaScript,
            b"class A { m() { return 1; } n() { return 2; } }
",
        )
        .unwrap();
        assert_eq!(one.entity_at(1, 10), Some("A.m"));
        assert_eq!(one.entity_at(1, 28), Some("A.n"));
        assert_eq!(one.entity_at(1, 0), Some("A"));
        let (ents, module) = m.resolve(&[(1, 1), (9, 12)]);
        assert_eq!(ents, vec!["Calc", "Calc.run"]);
        assert!(module);

        let py = "import os

class C:
    def m(self):
        def inner():
            return 1
        return inner()
";
        let pm = LineMap::new(Language::Python, py.as_bytes()).unwrap();
        assert_eq!(pm.entity(6), Some("C.m.<locals>.inner"));
        assert_eq!(pm.entity(4), Some("C.m"));
        assert_eq!(pm.entity(1), None);
    }

    #[test]
    fn crlf_and_skip_markers() {
        let crlf = JS.replace('\n', "\r\n");
        let none = d(Language::JavaScript, JS, &crlf);
        assert!(!none.module_changed && none.entities.is_empty());
        let t = "test('a', () => {});\n";
        let skipped = d(Language::JavaScript, t, "test.skip('a', () => {});\n");
        assert_eq!(skipped.skip_markers, (0, 1));
        let go = d(
            Language::Go,
            "package p\n",
            "package p\nfunc TestX(t *testing.T) { t.Skip(\"x\") }\n",
        );
        assert_eq!(go.skip_markers, (0, 1));
    }

    fn stateful(lang: Language, src: &str) -> bool {
        LineMap::new(lang, src.as_bytes()).unwrap().module_state()
    }

    /// From conformance (date-fns `tzOffset`): a module-level cache filled by
    /// `calcOffset` is read by later tests that never run `calcOffset`.
    #[test]
    fn module_state_detection() {
        use Language::*;
        for (lang, src) in [
            (
                TypeScript,
                "const offsetCache: Record<string, number> = {};\nexport function f() {}\n",
            ),
            (
                JavaScript,
                "let count = 0;\nexport function inc() { return ++count; }\n",
            ),
            (JavaScript, "var seen;\n"),
            (JavaScript, "export const registry = new Map();\n"),
            (TypeScript, "class C {\n  static cache = new Map();\n}\n"),
            (
                Python,
                "_cache = {}\ndef f(x):\n    return _cache.setdefault(x, x)\n",
            ),
            (
                Python,
                "from collections import defaultdict\nseen = defaultdict(list)\n",
            ),
            (Python, "class C:\n    instances = []\n"),
            // An export list doesn't hide a real table next to it.
            (
                Python,
                "__all__ = ['f']\n_table = [1, 2, 3]\ndef f():\n    _table.append(4)\n",
            ),
            // A literal table that is written, deleted from, passed or returned.
            (Python, "_t = {'a': 1}\ndef f(k):\n    _t[k] = 2\n"),
            (Python, "_t = [1]\ndef f():\n    del _t[0]\n"),
            (Python, "_t = [1]\ndef f(g):\n    g(_t)\n"),
            (Python, "_t = [1]\ndef f():\n    return _t\n"),
            (Python, "_t = [1]\ndef f(g):\n    g(x=_t)\n"),
            (Python, "_t = [1]\ndef f():\n    x = _t\n    x.append(2)\n"),
            // Mutable elements: iteration could hand them out.
            (
                Python,
                "_t = [[1], [2]]\ndef f():\n    for row in _t:\n        row.append(0)\n",
            ),
            (Python, "n = 0\ndef inc():\n    global n\n    n += 1\n"),
            // A table as a parameter's default: the function must only read it.
            (Python, "_t = {'a': 1}\ndef f(k, t=_t):\n    t[k] = 2\n"),
            (Python, "_t = {'a': 1}\ndef f(t=_t):\n    return t\n"),
            (Python, "_t = {'a': 1}\ndef f(g, t=_t):\n    g(t)\n"),
            (Python, "_t = [1]\ndef f(t=_t):\n    t.append(2)\n"),
            (Python, "_t = [1]\ndef f(t: list = _t):\n    t.append(2)\n"),
            // Decimal(...) leaves: only the standard library's, of literals.
            (
                Python,
                "from money import Decimal\n_t = {'a': Decimal('1')}\n",
            ),
            (
                Python,
                "from decimal import Decimal\nDecimal = list\n_t = {'a': Decimal('1')}\n",
            ),
            (
                Python,
                "from decimal import Decimal\nx = '1'\n_t = {'a': Decimal(x)}\n",
            ),
            (
                Python,
                "from decimal import Decimal as D\n_t = {'a': D('1')}\n",
            ),
            (Go, "package p\n\nvar cache = map[string]int{}\n"),
            (Rust, "static COUNT: AtomicUsize = AtomicUsize::new(0);\n"),
            (
                Java,
                "class C { static Map<String, Integer> cache = new HashMap<>(); }\n",
            ),
        ] {
            assert!(stateful(lang, src), "{lang:?}: {src}");
        }
        for (lang, src) in [
            (
                TypeScript,
                "export const RATE = 2;\nconst NAME = 'x';\nexport function f() { let local = {}; return local; }\n",
            ),
            (
                JavaScript,
                "const re = /a+/;\nexport const add = (a, b) => a + b;\n",
            ),
            (
                Python,
                "RATE = 2\nNAME = 'x'\ndef f():\n    cache = {}\n    return cache\n",
            ),
            // Constant tables: immutable literals, only read.
            (
                Python,
                "_TESTS = [(2047, (2,)), (1373653, (2, 3))]\n_NAMES = {'a': 1, 'b': -2}\ndef f(n):\n    for lim, bases in _TESTS:\n        if n < lim:\n            return bases\n    return _NAMES.get('a'), len(_TESTS), 'a' in _NAMES, _NAMES['b']\n",
            ),
            // From the demo repository: a rate table of Decimals, passed as a
            // default argument that the function only indexes.
            (
                Python,
                "from decimal import ROUND_HALF_EVEN, Decimal\n\nRATES: dict[str, Decimal] = {\n    \"USD\": Decimal(\"1\"),\n    \"EUR\": Decimal(\"0.92\"),\n}\n\n\ndef convert(cents, frm, to, rates: dict[str, Decimal] = RATES):\n    try:\n        usd = Decimal(cents) / rates[frm]\n        return (usd * rates[to]).quantize(Decimal(\"1\"), rounding=ROUND_HALF_EVEN)\n    except KeyError as e:\n        raise ValueError(e.args[0]) from None\n",
            ),
            (
                Python,
                "import decimal\nfrom fractions import Fraction\n_t = [decimal.Decimal('1.5'), Fraction(1, 3), Fraction(-2)]\ndef f(i, t=_t):\n    return t[i] if i < len(t) else None\n",
            ),
            // Declarations, not state: export lists, slots, match args.
            (
                Python,
                "__all__ = ['f', 'g']\nclass C:\n    __slots__ = ['a']\n    __match_args__ = ('a',)\ndef f():\n    pass\n",
            ),
            (
                Go,
                "package p\n\nconst Limit = 10\n\nfunc f() { m := map[string]int{}; _ = m }\n",
            ),
            (
                Java,
                "class C { static final int LIMIT = 10; static final String NAME = \"x\"; }\n",
            ),
        ] {
            assert!(!stateful(lang, src), "{lang:?}: {src}");
        }
    }
}
