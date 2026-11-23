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

use sc_hash::{Digest, Domain, hash_domain};
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
    let module_changed = match (&a, &b) {
        (Some(x), Some(y)) => x.module != y.module || parse_error,
        _ => true,
    };
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
    })
}

/// Maps executed source lines to entities, for probes that report lines.
#[derive(Clone, Debug, Default)]
pub struct LineMap {
    /// Per 1-based line, the innermost entity containing it (index into
    /// `names`). Index 0 is line 1.
    by_line: Vec<Option<u32>>,
    names: Vec<String>,
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
        };
        map.walk(tree.root_node(), &cx, "");
        Ok(map)
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
            // Outer definitions are visited first, so inner ones overwrite.
            let (start, end) = (node.start_position().row, node.end_position().row);
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
}
