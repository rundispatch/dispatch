//! Declaration extraction and contract fingerprints for Rust and Python.
//!
//! A declaration's `sig_fp` changes when its contract changes; `full_fp`
//! changes when anything but comments and whitespace changes. Both are
//! SHA-256 over the declaration's leaf tokens (comments dropped, each token
//! followed by one space, MISSING nodes hashed by kind), preceded by its
//! attributes (Rust) or decorators (Python). `sig_fp` leaves out function
//! bodies, and leaves out the member declarations of traits and classes, which
//! are symbols of their own. Each declaration is hashed once: a trait or class
//! `full_fp` folds in its members' `full_fp` values instead of re-hashing them.
//!
//! tree-sitter recovers from syntax errors and can silently drop or re-parent
//! declarations, so `FileSymbols::has_error` covers the whole file.

use std::collections::{BTreeSet, HashMap};

use anyhow::{Context, Result, anyhow};
use sha2::{Digest, Sha256};
use tree_sitter::{Node, Parser};

const DISPLAY_MAX_CHARS: usize = 240;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Rust,
    Python,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SymbolKind {
    Function,
    Method,
    Struct,
    Enum,
    Trait,
    TypeAlias,
    Const,
    Class,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolDecl {
    /// Qualified name, e.g. `Point::new`, `<Shape as Area>::area`, `Point.norm`.
    pub name: String,
    pub kind: SymbolKind,
    pub sig_fp: String,
    pub full_fp: String,
    /// 1-based inclusive span, starting at the first attribute or decorator.
    pub start_line: u32,
    pub end_line: u32,
    /// Signature text without attributes or body, whitespace-collapsed.
    pub display: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileSymbols {
    pub decls: Vec<SymbolDecl>,
    /// The parse tree contains an ERROR or MISSING node. The declaration table
    /// cannot be trusted for the whole file, even for clean-looking entries.
    pub has_error: bool,
}

pub fn lang_for_path(path: &str) -> Option<Lang> {
    if path.ends_with(".rs") {
        Some(Lang::Rust)
    } else if path.ends_with(".py") {
        Some(Lang::Python)
    } else {
        None
    }
}

pub fn extract(lang: Lang, source: &[u8]) -> Result<FileSymbols> {
    let tree = parse(lang, source)?;
    let root = tree.root_node();
    let mut decls = Vec::new();
    match lang {
        Lang::Rust => rust_scope(root, source, "", false, &mut decls, None),
        Lang::Python => py_scope(root, source, "", &mut decls, None),
    }
    Ok(FileSymbols {
        decls,
        has_error: root.has_error(),
    })
}

/// Names of identifiers, type identifiers and field (method-call) identifiers
/// that start on a line inside one of the 1-based inclusive `line_ranges`.
/// Scoped paths yield each segment because each segment is its own identifier.
pub fn identifiers_in(
    lang: Lang,
    source: &[u8],
    line_ranges: &[(u32, u32)],
) -> Result<BTreeSet<String>> {
    let tree = parse(lang, source)?;
    let overlaps = |n: Node| {
        let (a, b) = (
            n.start_position().row as u32 + 1,
            n.end_position().row as u32 + 1,
        );
        line_ranges.iter().any(|&(lo, hi)| a <= hi && lo <= b)
    };
    let mut names = BTreeSet::new();
    let mut cur = tree.root_node().walk();
    loop {
        let n = cur.node();
        if overlaps(n) {
            if matches!(
                n.kind(),
                "identifier" | "type_identifier" | "field_identifier"
            ) {
                let text = String::from_utf8_lossy(&source[n.byte_range()]).into_owned();
                if !text.is_empty() {
                    names.insert(text);
                }
            }
            if cur.goto_first_child() {
                continue;
            }
        }
        while !cur.goto_next_sibling() {
            if !cur.goto_parent() {
                return Ok(names);
            }
        }
    }
}

fn parse(lang: Lang, source: &[u8]) -> Result<tree_sitter::Tree> {
    let language = match lang {
        Lang::Rust => tree_sitter_rust::LANGUAGE,
        Lang::Python => tree_sitter_python::LANGUAGE,
    };
    let mut parser = Parser::new();
    parser
        .set_language(&language.into())
        .context("load tree-sitter grammar")?;
    parser
        .parse(source, None)
        .ok_or_else(|| anyhow!("tree-sitter produced no tree"))
}

/// Whether a Python function whose header changed from `old` to `new` still
/// accepts every call the old header accepted. Both must be `def` headers with
/// the same name, `async`-ness and return annotation; every old parameter is
/// unchanged and in place; every added parameter is optional (it has a
/// default, or is `*args`, `**kwargs` or the bare `*` before keyword-only
/// parameters with defaults). Anything that cannot be parsed or proven,
/// including a header cut short for display, is not compatible.
pub fn python_call_compatible(old: &str, new: &str) -> bool {
    struct Header {
        is_async: bool,
        name: String,
        returns: Option<String>,
        params: Vec<(String, String)>,
    }
    fn header(text: &str) -> Option<Header> {
        if text.chars().count() >= DISPLAY_MAX_CHARS {
            return None;
        }
        let source = format!("{text}\n    pass\n");
        let tree = parse(Lang::Python, source.as_bytes()).ok()?;
        let root = tree.root_node();
        let def = root.named_child(0)?;
        if root.has_error() || def.kind() != "function_definition" {
            return None;
        }
        let text_of = |node: Node| node.utf8_text(source.as_bytes()).ok().map(str::to_owned);
        let params = def.child_by_field_name("parameters")?;
        let mut cursor = params.walk();
        let params = params
            .named_children(&mut cursor)
            .map(|param| Some((param.kind().to_owned(), text_of(param)?)))
            .collect::<Option<Vec<_>>>()?;
        Some(Header {
            is_async: def.child(0).is_some_and(|first| first.kind() == "async"),
            name: text_of(def.child_by_field_name("name")?)?,
            returns: def.child_by_field_name("return_type").and_then(text_of),
            params,
        })
    }
    let (Some(old), Some(new)) = (header(old), header(new)) else {
        return false;
    };
    old.is_async == new.is_async
        && old.name == new.name
        && old.returns == new.returns
        && new.params.len() >= old.params.len()
        && new.params[..old.params.len()] == old.params[..]
        && new.params[old.params.len()..].iter().all(|(kind, _)| {
            matches!(
                kind.as_str(),
                "default_parameter"
                    | "typed_default_parameter"
                    | "list_splat_pattern"
                    | "dictionary_splat_pattern"
                    | "keyword_separator"
            )
        })
}

/// A declaration already emitted by a nested scope, so its trait or class can
/// exclude it from `sig_fp` and fold in its `full_fp`.
struct Member {
    attr_ids: Vec<usize>,
    item_id: usize,
    full_fp: String,
}

/// Nodes treated specially while hashing one declaration.
#[derive(Default)]
struct Skips {
    /// Excluded from `sig_fp` only (function bodies).
    sig_only: Vec<usize>,
    /// Excluded from `sig_fp`; `full_fp` gets the mapped text instead (empty
    /// text contributes nothing).
    nested: HashMap<usize, String>,
}

impl Skips {
    fn from_members(members: &[Member]) -> Self {
        let mut nested = HashMap::new();
        for m in members {
            for id in &m.attr_ids {
                nested.insert(*id, String::new());
            }
            nested.insert(m.item_id, m.full_fp.clone());
        }
        Skips {
            sig_only: Vec::new(),
            nested,
        }
    }
}

struct Fp {
    sig: Sha256,
    full: Sha256,
    sig_on: bool,
}

impl Fp {
    fn token(&mut self, t: &[u8]) {
        self.full.update(t);
        self.full.update(b" ");
        if self.sig_on {
            self.sig.update(t);
            self.sig.update(b" ");
        }
    }
}

/// Feeds the leaf tokens of `root` into `fp`. Iterative so pathological nesting
/// (long operator chains) cannot overflow the stack. Comments are tested before
/// descending because Rust doc comments are not leaves.
fn feed(root: Node, src: &[u8], skips: &Skips, fp: &mut Fp) {
    let mut cur = root.walk();
    let mut sig_off_at: Option<usize> = None;
    loop {
        let n = cur.node();
        let mut descend = false;
        if n.kind().ends_with("comment") {
        } else if let Some(sub) = skips.nested.get(&n.id()) {
            if !sub.is_empty() {
                fp.full.update(sub.as_bytes());
                fp.full.update(b" ");
            }
        } else if skips.sig_only.contains(&n.id()) {
            if fp.sig_on {
                fp.sig_on = false;
                sig_off_at = Some(n.id());
            }
            descend = true;
        } else if n.child_count() == 0 {
            if n.is_missing() {
                fp.token(format!("\u{1}MISSING:{}", n.kind()).as_bytes());
            } else {
                fp.token(&src[n.byte_range()]);
            }
        } else {
            descend = true;
        }
        if descend && cur.goto_first_child() {
            continue;
        }
        loop {
            if sig_off_at == Some(cur.node().id()) {
                fp.sig_on = true;
                sig_off_at = None;
            }
            if cur.goto_next_sibling() {
                break;
            }
            if !cur.goto_parent() {
                return;
            }
        }
    }
}

fn collapse(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn field_text(n: Node, field: &str, src: &[u8]) -> Option<String> {
    let text = collapse(&src[n.child_by_field_name(field)?.byte_range()]);
    (!text.is_empty()).then_some(text)
}

struct Shape<'a> {
    name: String,
    kind: SymbolKind,
    /// Attribute siblings preceding the item (Rust); empty for Python.
    attrs: &'a [Node<'a>],
    /// Node hashed as the declaration (Python: the `decorated_definition`).
    item: Node<'a>,
    /// Node whose text starts `display` (Python: the def, without decorators).
    display_from: Node<'a>,
    /// Where `display` stops; `None` shows the whole declaration.
    display_to: Option<usize>,
    skips: Skips,
}

fn build(s: Shape, src: &[u8]) -> (SymbolDecl, Member) {
    let mut fp = Fp {
        sig: Sha256::new(),
        full: Sha256::new(),
        sig_on: true,
    };
    for a in s.attrs {
        feed(*a, src, &s.skips, &mut fp);
    }
    feed(s.item, src, &s.skips, &mut fp);
    let (sig_fp, full_fp) = (
        hex::encode(fp.sig.finalize()),
        hex::encode(fp.full.finalize()),
    );
    let end = s.display_to.unwrap_or(s.display_from.end_byte());
    let start = s.display_from.start_byte();
    let mut display = collapse(&src[start..end.max(start)]);
    if let Some((cut, _)) = display.char_indices().nth(DISPLAY_MAX_CHARS) {
        display.truncate(cut);
    }
    let first = s.attrs.first().copied().unwrap_or(s.item);
    let member = Member {
        attr_ids: s.attrs.iter().map(|a| a.id()).collect(),
        item_id: s.item.id(),
        full_fp: full_fp.clone(),
    };
    let decl = SymbolDecl {
        name: s.name,
        kind: s.kind,
        sig_fp,
        full_fp,
        start_line: first.start_position().row as u32 + 1,
        end_line: s.item.end_position().row as u32 + 1,
        display,
    };
    (decl, member)
}

/// Emits declarations found directly in `container`: the source file, a `mod`
/// body, an `impl` body or a `trait` body (`member` is true for the last two).
fn rust_scope(
    container: Node,
    src: &[u8],
    prefix: &str,
    member: bool,
    out: &mut Vec<SymbolDecl>,
    mut members: Option<&mut Vec<Member>>,
) {
    let mut walker = container.walk();
    let mut attrs: Vec<Node> = Vec::new();
    for ch in container.named_children(&mut walker) {
        let kind = ch.kind();
        if kind == "attribute_item" {
            attrs.push(ch);
            continue;
        }
        if kind.ends_with("comment") {
            continue;
        }
        let name = field_text(ch, "name", src).map(|n| format!("{prefix}{n}"));
        let body = ch.child_by_field_name("body");
        let mut shape = None;
        match (kind, name) {
            ("function_item" | "function_signature_item", Some(name)) => {
                let mut skips = Skips::default();
                skips.sig_only.extend(body.map(|b| b.id()));
                shape = Some((
                    name,
                    if member {
                        SymbolKind::Method
                    } else {
                        SymbolKind::Function
                    },
                    body.map(|b| b.start_byte()),
                    skips,
                ));
            }
            ("struct_item" | "union_item", Some(name)) => {
                shape = Some((name, SymbolKind::Struct, None, Skips::default()));
            }
            ("enum_item", Some(name)) => {
                shape = Some((name, SymbolKind::Enum, None, Skips::default()));
            }
            ("type_item", Some(name)) => {
                shape = Some((name, SymbolKind::TypeAlias, None, Skips::default()));
            }
            ("const_item" | "static_item", Some(name)) => {
                shape = Some((name, SymbolKind::Const, None, Skips::default()));
            }
            ("trait_item", Some(name)) => {
                let mut inner = Vec::new();
                let mut kids = Vec::new();
                if let Some(b) = body {
                    rust_scope(
                        b,
                        src,
                        &format!("{name}::"),
                        true,
                        &mut inner,
                        Some(&mut kids),
                    );
                }
                let (decl, m) = build(
                    Shape {
                        name,
                        kind: SymbolKind::Trait,
                        attrs: &attrs,
                        item: ch,
                        display_from: ch,
                        display_to: body.map(|b| b.start_byte()),
                        skips: Skips::from_members(&kids),
                    },
                    src,
                );
                out.push(decl);
                out.append(&mut inner);
                if let Some(ms) = members.as_deref_mut() {
                    ms.push(m);
                }
            }
            ("impl_item", _) => {
                if let (Some(b), Some(ty)) = (body, impl_type(ch, src)) {
                    let owner = match field_text(ch, "trait", src) {
                        Some(tr) => format!("<{prefix}{ty} as {tr}>::"),
                        None => format!("{prefix}{ty}::"),
                    };
                    rust_scope(b, src, &owner, true, out, None);
                }
            }
            ("mod_item", Some(name)) => {
                if let Some(b) = body {
                    rust_scope(b, src, &format!("{name}::"), false, out, None);
                }
            }
            _ => {}
        }
        if let Some((name, kind, display_to, skips)) = shape {
            let (decl, m) = build(
                Shape {
                    name,
                    kind,
                    attrs: &attrs,
                    item: ch,
                    display_from: ch,
                    display_to,
                    skips,
                },
                src,
            );
            out.push(decl);
            if let Some(ms) = members.as_deref_mut() {
                ms.push(m);
            }
        }
        attrs.clear();
    }
}

/// The `impl` target with generic arguments stripped (`Point<T>` -> `Point`).
fn impl_type(imp: Node, src: &[u8]) -> Option<String> {
    let mut t = imp.child_by_field_name("type")?;
    while t.kind() == "generic_type" {
        t = t.child_by_field_name("type")?;
    }
    let text = collapse(&src[t.byte_range()]);
    (!text.is_empty()).then_some(text)
}

/// Emits declarations found directly in the module or a class body. Module
/// constants are only ALL_CAPS assignments; defs nested in functions and
/// declarations inside compound statements are ignored.
fn py_scope(
    container: Node,
    src: &[u8],
    prefix: &str,
    out: &mut Vec<SymbolDecl>,
    mut members: Option<&mut Vec<Member>>,
) {
    let mut walker = container.walk();
    for ch in container.named_children(&mut walker) {
        let def = if ch.kind() == "decorated_definition" {
            match ch.child_by_field_name("definition") {
                Some(d) => d,
                None => continue,
            }
        } else {
            ch
        };
        let body = def.child_by_field_name("body");
        let name = field_text(def, "name", src).map(|n| format!("{prefix}{n}"));
        let shape = match (def.kind(), name) {
            ("function_definition", Some(name)) => {
                let kind = if prefix.is_empty() {
                    SymbolKind::Function
                } else {
                    SymbolKind::Method
                };
                let mut skips = Skips::default();
                skips.sig_only.extend(body.map(|b| b.id()));
                Some((name, kind, body.map(|b| b.start_byte()), skips))
            }
            ("class_definition", Some(name)) => {
                let mut inner = Vec::new();
                let mut kids = Vec::new();
                if let Some(b) = body {
                    py_scope(b, src, &format!("{name}."), &mut inner, Some(&mut kids));
                }
                let (decl, m) = build(
                    Shape {
                        name,
                        kind: SymbolKind::Class,
                        attrs: &[],
                        item: ch,
                        display_from: def,
                        display_to: body.map(|b| b.start_byte()),
                        skips: Skips::from_members(&kids),
                    },
                    src,
                );
                out.push(decl);
                out.append(&mut inner);
                if let Some(ms) = members.as_deref_mut() {
                    ms.push(m);
                }
                None
            }
            ("expression_statement", None) if prefix.is_empty() => {
                py_constant(def, src).map(|n| (n, SymbolKind::Const, None, Skips::default()))
            }
            _ => None,
        };
        if let Some((name, kind, display_to, skips)) = shape {
            let (decl, m) = build(
                Shape {
                    name,
                    kind,
                    attrs: &[],
                    item: ch,
                    display_from: def,
                    display_to,
                    skips,
                },
                src,
            );
            out.push(decl);
            if let Some(ms) = members.as_deref_mut() {
                ms.push(m);
            }
        }
    }
}

/// Name of a module-level `NAME = value` / `NAME: T = value` statement when NAME
/// is ALL_CAPS.
fn py_constant(stmt: Node, src: &[u8]) -> Option<String> {
    let assign = stmt.named_child(0).filter(|a| a.kind() == "assignment")?;
    let left = assign.child_by_field_name("left")?;
    if left.kind() != "identifier" {
        return None;
    }
    let name = String::from_utf8_lossy(&src[left.byte_range()]).into_owned();
    let has_letter = name.chars().any(|c| c.is_alphabetic());
    (has_letter && !name.chars().any(|c| c.is_lowercase())).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUST: &str = r#"
const LIMIT: usize = 10;
static NAME: &str = "x";
type Id = u32;

/// Adds numbers.
#[inline]
pub fn add(a: i32, b: i32) -> i32 {
    a + b // sum
}

#[derive(Debug)]
pub struct Point<T> {
    pub x: T,
    pub y: T,
}

enum Shape {
    Circle(f64),
    Rect { w: f64, h: f64 },
}

trait Area {
    fn area(&self) -> f64;
    fn name(&self) -> String { "shape".into() }
}

impl<T> Point<T> {
    pub fn new(x: T, y: T) -> Self { Point { x, y } }
}

impl Area for Shape {
    fn area(&self) -> f64 { 0.0 }
}

impl Other for Shape {
    fn area(&self) -> f64 { 1.0 }
}

mod inner {
    pub fn helper() {}
    impl Shape { pub fn extra(&self) {} }
}
"#;

    const PY: &str = r#"
MAX_SIZE = 10
lower = 1

def add(a: int, b: int) -> int:
    """Add."""
    return a + b


@decorator
def deco(x):
    return x


class Point(Base):
    z = 1

    def __init__(self, x: int, y: int):
        self.x = x
        self.y = y

    def norm(self) -> float:
        def inner():
            return 1
        return (self.x ** 2 + self.y ** 2) ** 0.5

    class Inner:
        def deep(self):
            pass
"#;

    fn ex(lang: Lang, src: &str) -> FileSymbols {
        extract(lang, src.as_bytes()).unwrap()
    }

    fn get<'a>(fs: &'a FileSymbols, name: &str) -> &'a SymbolDecl {
        fs.decls
            .iter()
            .find(|d| d.name == name)
            .unwrap_or_else(|| panic!("missing {name}: {:?}", names(fs)))
    }

    fn names(fs: &FileSymbols) -> Vec<&str> {
        fs.decls.iter().map(|d| d.name.as_str()).collect()
    }

    #[derive(PartialEq, Debug)]
    enum Change {
        None,
        FullOnly,
        Both,
    }

    fn compare(lang: Lang, base: &str, edited: &str, name: &str) -> Change {
        let (a, b) = (ex(lang, base), ex(lang, edited));
        assert!(!b.has_error, "edited source has syntax errors");
        let (a, b) = (get(&a, name), get(&b, name));
        match (a.sig_fp != b.sig_fp, a.full_fp != b.full_fp) {
            (false, false) => Change::None,
            (false, true) => Change::FullOnly,
            (true, true) => Change::Both,
            (true, false) => panic!("sig_fp changed without full_fp"),
        }
    }

    fn rs(edit: (&str, &str), name: &str) -> Change {
        assert!(RUST.contains(edit.0), "edit target missing: {}", edit.0);
        compare(Lang::Rust, RUST, &RUST.replace(edit.0, edit.1), name)
    }

    fn py(edit: (&str, &str), name: &str) -> Change {
        assert!(PY.contains(edit.0), "edit target missing: {}", edit.0);
        compare(Lang::Python, PY, &PY.replace(edit.0, edit.1), name)
    }

    #[test]
    fn python_calls_stay_valid_only_when_additions_are_optional() {
        let holds = |old: &str, new: &str| python_call_compatible(old, new);
        // The trial's case: an added defaulted parameter.
        assert!(holds(
            "def format_user(user):",
            "def format_user(user, brackets=\"()\"):"
        ));
        assert!(holds("def f(a):", "def f(a, b: int = 1):"));
        assert!(holds("def f(a):", "def f(a, *args, **kwargs):"));
        assert!(holds("def f(a):", "def f(a, *, strict=False):"));
        assert!(holds(
            "async def f(a) -> int:",
            "async def f(a, b=2) -> int:"
        ));
        // Anything a caller could notice breaks.
        assert!(!holds("def validate(token):", "def validate(ctx, token):"));
        assert!(!holds("def f(a):", "def f(a, b):"));
        assert!(!holds("def f(a):", "def f(a, *, strict):"));
        assert!(!holds("def f(a, b):", "def f(a):"));
        assert!(!holds("def f(a, b):", "def f(b, a):"));
        assert!(!holds("def f(a, b):", "def f(a, c):"));
        assert!(!holds("def f(a=1):", "def f(a=2):"));
        assert!(!holds("def f(a: int):", "def f(a: str):"));
        assert!(!holds("def f(a) -> int:", "def f(a) -> str:"));
        assert!(!holds("def f(a):", "async def f(a):"));
        assert!(!holds("def f(a):", "def f(a, /):"));
        assert!(!holds("def f(a):", "def g(a):"));
        assert!(!holds("class A:", "class A(Base):"));
        assert!(!holds(
            "def f(a):",
            &format!("def f(a, {}=1):", "x".repeat(300))
        ));
    }

    #[test]
    fn lang_for_path_maps_extensions() {
        assert_eq!(lang_for_path("src/a.rs"), Some(Lang::Rust));
        assert_eq!(lang_for_path("a/b.py"), Some(Lang::Python));
        assert_eq!(lang_for_path("a.ts"), None);
        assert_eq!(lang_for_path("Makefile"), None);
        assert_eq!(lang_for_path("a.rs.bak"), None);
    }

    #[test]
    fn rust_declarations_and_qualified_names() {
        let fs = ex(Lang::Rust, RUST);
        assert!(!fs.has_error);
        assert_eq!(
            names(&fs),
            [
                "LIMIT",
                "NAME",
                "Id",
                "add",
                "Point",
                "Shape",
                "Area",
                "Area::area",
                "Area::name",
                "Point::new",
                "<Shape as Area>::area",
                "<Shape as Other>::area",
                "inner::helper",
                "inner::Shape::extra",
            ]
        );
        assert_eq!(get(&fs, "add").kind, SymbolKind::Function);
        assert_eq!(get(&fs, "Point::new").kind, SymbolKind::Method);
        assert_eq!(get(&fs, "Area::area").kind, SymbolKind::Method);
        assert_eq!(get(&fs, "Area").kind, SymbolKind::Trait);
        assert_eq!(get(&fs, "Point").kind, SymbolKind::Struct);
        assert_eq!(get(&fs, "Shape").kind, SymbolKind::Enum);
        assert_eq!(get(&fs, "Id").kind, SymbolKind::TypeAlias);
        assert_eq!(get(&fs, "LIMIT").kind, SymbolKind::Const);
        assert_eq!(get(&fs, "NAME").kind, SymbolKind::Const);
        // Trait impls for different traits do not collide; bodies differ too.
        assert_ne!(
            get(&fs, "<Shape as Area>::area").full_fp,
            get(&fs, "<Shape as Other>::area").full_fp
        );
    }

    #[test]
    fn rust_spans_display_and_generic_trait_impls() {
        let fs = ex(Lang::Rust, RUST);
        let add = get(&fs, "add");
        // Span starts at the attribute; the doc comment is not part of the item.
        assert_eq!((add.start_line, add.end_line), (7, 10));
        assert_eq!(add.display, "pub fn add(a: i32, b: i32) -> i32");
        assert_eq!(
            get(&fs, "Point::new").display,
            "pub fn new(x: T, y: T) -> Self"
        );
        assert_eq!(get(&fs, "Area").display, "trait Area");
        let src = "impl From<A> for T { fn from(a: A) -> T { T } }\nimpl From<B> for T { fn from(b: B) -> T { T } }\n";
        let fs = ex(Lang::Rust, src);
        assert_eq!(names(&fs), ["<T as From<A>>::from", "<T as From<B>>::from"]);
    }

    #[test]
    fn display_is_capped() {
        let long = format!("pub fn f({}) {{}}", "a: i32, ".repeat(100));
        let fs = ex(Lang::Rust, &long);
        assert_eq!(fs.decls[0].display.chars().count(), DISPLAY_MAX_CHARS);
    }

    #[test]
    fn rust_body_only_edit() {
        assert_eq!(
            rs(("a + b // sum", "a.wrapping_add(b)"), "add"),
            Change::FullOnly
        );
        assert_eq!(
            rs(("Point { x, y }", "Point { y, x }"), "Point::new"),
            Change::FullOnly
        );
        // Unrelated declarations stay identical.
        assert_eq!(
            rs(("a + b // sum", "a.wrapping_add(b)"), "Point::new"),
            Change::None
        );
    }

    #[test]
    fn rust_comment_doc_and_whitespace_edits_change_nothing() {
        let edited = RUST
            .replace("a + b // sum", "/* new */ a   +\n\n        b // different")
            .replace("/// Adds numbers.", "/// Totally different\n// extra")
            .replace("pub x: T,", "pub   x:\n    T, // field");
        let (a, b) = (ex(Lang::Rust, RUST), ex(Lang::Rust, &edited));
        for d in &a.decls {
            let e = get(&b, &d.name);
            assert_eq!(
                (&d.sig_fp, &d.full_fp),
                (&e.sig_fp, &e.full_fp),
                "{}",
                d.name
            );
        }
    }

    #[test]
    fn rust_signature_edits_change_sig() {
        assert_eq!(
            rs(("a: i32, b: i32", "a: i64, b: i32"), "add"),
            Change::Both
        );
        assert_eq!(
            rs(("b: i32) -> i32", "b: i32) -> i64"), "add"),
            Change::Both
        );
        assert_eq!(rs(("pub fn add", "pub(crate) fn add"), "add"), Change::Both);
        assert_eq!(rs(("pub fn add", "fn add"), "add"), Change::Both);
        assert_eq!(rs(("fn add(", "fn add<X: Copy>("), "add"), Change::Both);
        assert_eq!(
            rs(
                ("-> i32 {\n    a", "-> i32 where i32: Copy {\n    a"),
                "add"
            ),
            Change::Both
        );
        assert_eq!(
            rs(
                ("fn area(&self) -> f64;", "fn area(&self) -> f32;"),
                "Area::area"
            ),
            Change::Both
        );
        assert_eq!(
            rs(
                (
                    "fn area(&self) -> f64 { 0.0 }",
                    "fn area(&mut self) -> f64 { 0.0 }"
                ),
                "<Shape as Area>::area"
            ),
            Change::Both
        );
    }

    #[test]
    fn rust_attributes_affect_fingerprints() {
        assert_eq!(rs(("#[inline]", "#[inline(always)]"), "add"), Change::Both);
        assert_eq!(rs(("#[inline]\n", ""), "add"), Change::Both);
        assert_eq!(
            rs(("#[derive(Debug)]", "#[derive(Debug, Clone)]"), "Point"),
            Change::Both
        );
        // A doc comment between the attribute and the item does not detach it.
        let src = "#[inline]\n/// doc\nfn f() {}\n";
        let with = ex(Lang::Rust, src);
        let without = ex(Lang::Rust, "fn f() {}\n");
        assert_ne!(with.decls[0].sig_fp, without.decls[0].sig_fp);
    }

    #[test]
    fn rust_type_edits_change_sig() {
        assert_eq!(
            rs(("pub y: T,", "pub y: T,\n    pub z: T,"), "Point"),
            Change::Both
        );
        assert_eq!(rs(("pub y: T,", ""), "Point"), Change::Both);
        assert_eq!(rs(("pub y: T,", "pub y: u8,"), "Point"), Change::Both);
        assert_eq!(
            rs(("Circle(f64),", "Circle(f64),\n    Tri(f64),"), "Shape"),
            Change::Both
        );
        assert_eq!(rs(("type Id = u32;", "type Id = u64;"), "Id"), Change::Both);
        assert_eq!(rs(("= 10;", "= 11;"), "LIMIT"), Change::Both);
        assert_eq!(rs(("&str = \"x\"", "&str = \"y\""), "NAME"), Change::Both);
    }

    #[test]
    fn rust_trait_sig_ignores_members_and_full_covers_them() {
        // Default method body edit: trait full changes, sig does not.
        assert_eq!(
            rs(("\"shape\".into()", "\"figure\".into()"), "Area"),
            Change::FullOnly
        );
        assert_eq!(
            rs(("\"shape\".into()", "\"figure\".into()"), "Area::name"),
            Change::FullOnly
        );
        // Member signature change is a member symbol change and reaches trait full.
        let (a, b) = (
            ex(Lang::Rust, RUST),
            ex(
                Lang::Rust,
                &RUST.replace("fn area(&self) -> f64;", "fn area(&self) -> f32;"),
            ),
        );
        assert_ne!(get(&a, "Area").full_fp, get(&b, "Area").full_fp);
        // Trait header change.
        assert_eq!(
            rs(("trait Area {", "trait Area: Sized {"), "Area"),
            Change::Both
        );
    }

    #[test]
    fn rust_ignores_nested_items_macros_and_non_symbol_items() {
        let src = "fn outer() { fn inner() {} struct S; let c = || 1; }\nmacro_rules! m { () => { fn made() {} } }\nuse std::fmt;\nmod decl_only;\nextern \"C\" { fn c(); }\n";
        let fs = ex(Lang::Rust, src);
        assert!(!fs.has_error);
        assert_eq!(names(&fs), ["outer"]);
    }

    #[test]
    fn python_declarations_and_qualified_names() {
        let fs = ex(Lang::Python, PY);
        assert!(!fs.has_error);
        assert_eq!(
            names(&fs),
            [
                "MAX_SIZE",
                "add",
                "deco",
                "Point",
                "Point.__init__",
                "Point.norm",
                "Point.Inner",
                "Point.Inner.deep",
            ]
        );
        assert_eq!(get(&fs, "MAX_SIZE").kind, SymbolKind::Const);
        assert_eq!(get(&fs, "add").kind, SymbolKind::Function);
        assert_eq!(get(&fs, "Point").kind, SymbolKind::Class);
        assert_eq!(get(&fs, "Point.norm").kind, SymbolKind::Method);
        let deco = get(&fs, "deco");
        assert_eq!((deco.start_line, deco.end_line), (10, 12));
        assert_eq!(deco.display, "def deco(x):");
        assert_eq!(get(&fs, "Point").display, "class Point(Base):");
        assert_eq!(get(&fs, "add").display, "def add(a: int, b: int) -> int:");
    }

    #[test]
    fn python_constants_need_all_caps_at_module_level() {
        let src = "A_1 = 1\nb = 2\nCamel = 3\n_ = 4\n__ALL__: list = []\nX, Y = 1, 2\nclass C:\n    LIMIT = 1\n";
        let fs = ex(Lang::Python, src);
        assert_eq!(names(&fs), ["A_1", "__ALL__", "C"]);
    }

    #[test]
    fn python_body_comment_and_whitespace_edits() {
        assert_eq!(py(("** 0.5", "** 0.50"), "Point.norm"), Change::FullOnly);
        // Class sig ignores method bodies; class full folds them in.
        assert_eq!(py(("** 0.5", "** 0.50"), "Point"), Change::FullOnly);
        assert_eq!(
            py(("return a + b", "return b + a"), "add"),
            Change::FullOnly
        );
        assert_eq!(py(("return a + b", "return b + a"), "Point"), Change::None);
        // Nested defs are not symbols but are part of the enclosing full_fp.
        assert_eq!(py(("return 1", "return 2"), "Point.norm"), Change::FullOnly);
        let edited = PY
            .replace("z = 1", "z   =   1  # note\n    # c")
            .replace("return a + b", "return  a  +  b  # sum")
            .replace("MAX_SIZE = 10", "MAX_SIZE   =   10  # cap");
        let (a, b) = (ex(Lang::Python, PY), ex(Lang::Python, &edited));
        for d in &a.decls {
            let e = get(&b, &d.name);
            assert_eq!(
                (&d.sig_fp, &d.full_fp),
                (&e.sig_fp, &e.full_fp),
                "{}",
                d.name
            );
        }
    }

    #[test]
    fn python_signature_decorator_and_class_edits() {
        assert_eq!(
            py(("a: int, b: int", "a: float, b: int"), "add"),
            Change::Both
        );
        assert_eq!(py(("-> int:", "-> float:"), "add"), Change::Both);
        assert_eq!(py(("@decorator", "@other"), "deco"), Change::Both);
        assert_eq!(py(("@decorator\n", ""), "deco"), Change::Both);
        assert_eq!(py(("Point(Base)", "Point(Other)"), "Point"), Change::Both);
        assert_eq!(py(("z = 1", "z = 2"), "Point"), Change::Both);
        assert_eq!(
            py(("x: int, y: int", "x: int"), "Point.__init__"),
            Change::Both
        );
        assert_eq!(
            py(("MAX_SIZE = 10", "MAX_SIZE = 11"), "MAX_SIZE"),
            Change::Both
        );
        // Method signature change does not alter the class sig.
        assert_eq!(
            py(
                ("def norm(self) -> float", "def norm(self) -> int"),
                "Point"
            ),
            Change::FullOnly
        );
    }

    #[test]
    fn python_decorated_method_is_one_symbol_with_decorator() {
        let base = "class C:\n    @property\n    def v(self):\n        return 1\n";
        let edited = base.replace("@property", "@cached");
        assert_eq!(compare(Lang::Python, base, &edited, "C.v"), Change::Both);
        let fs = ex(Lang::Python, base);
        assert_eq!(names(&fs), ["C", "C.v"]);
        assert_eq!(get(&fs, "C.v").start_line, 2);
    }

    #[test]
    fn syntax_errors_set_has_error_for_whole_file() {
        let bad = RUST.replace("a + b // sum", "a + + ) b {{");
        assert!(ex(Lang::Rust, &bad).has_error);
        let sig = RUST.replace(
            "pub fn add(a: i32, b: i32) -> i32 {",
            "pub fn add(a: i32, b: i32 -> i32 {",
        );
        assert!(ex(Lang::Rust, &sig).has_error);
        assert!(ex(Lang::Python, &PY.replace("return a + b", "return a + (")).has_error);
        assert!(ex(Lang::Rust, "fn ok() {}").decls.len() == 1);
        // Declarations found are still returned.
        let fs = ex(Lang::Rust, "fn ok() {}\nfn bad( {\n");
        assert!(fs.has_error);
        assert!(names(&fs).contains(&"ok"));
    }

    #[test]
    fn missing_nodes_are_hashed() {
        // `fn f() {}` vs the same with a MISSING `;`-style recovery must not be
        // indistinguishable from the clean form.
        let clean = ex(Lang::Rust, "const X: u8 = 1;");
        let broken = ex(Lang::Rust, "const X: u8 = 1");
        assert!(broken.has_error);
        assert_ne!(clean.decls[0].full_fp, broken.decls[0].full_fp);
    }

    #[test]
    fn identifiers_respect_line_ranges() {
        let src = "fn alpha() {\n    beta(gamma::delta(1));\n    let x: Epsilon = obj.zeta();\n}\nfn omega() { theta(); }\n";
        let ids = |r: &[(u32, u32)]| identifiers_in(Lang::Rust, src.as_bytes(), r).unwrap();
        let line2: Vec<_> = ids(&[(2, 2)]).into_iter().collect();
        assert_eq!(line2, ["beta", "delta", "gamma"]);
        let line3: Vec<_> = ids(&[(3, 3)]).into_iter().collect();
        assert_eq!(line3, ["Epsilon", "obj", "x", "zeta"]);
        let multi = ids(&[(1, 1), (5, 5)]);
        assert!(multi.contains("alpha") && multi.contains("omega") && multi.contains("theta"));
        assert!(!multi.contains("beta") && !multi.contains("zeta"));
        assert!(ids(&[]).is_empty());
        assert!(ids(&[(9, 12)]).is_empty());
        let py = "def f():\n    a.b(c)\n\ndef g():\n    h(d)\n";
        let got = identifiers_in(Lang::Python, py.as_bytes(), &[(5, 5)]).unwrap();
        assert_eq!(got.into_iter().collect::<Vec<_>>(), ["d", "h"]);
    }

    /// This thread's CPU time: what extraction itself costs, however many
    /// other processes compete for the CPU.
    #[cfg(unix)]
    fn thread_cpu() -> std::time::Duration {
        let mut now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: writes only the timespec passed in.
        assert_eq!(
            unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut now) },
            0
        );
        std::time::Duration::new(now.tv_sec as u64, now.tv_nsec as u32)
    }

    #[cfg(unix)]
    #[test]
    fn extract_orchestrator_is_fast_enough() {
        let src =
            std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/src/orchestrator.rs")).unwrap();
        let start = thread_cpu();
        let fs = extract(Lang::Rust, &src).unwrap();
        let elapsed = thread_cpu() - start;
        assert!(!fs.has_error);
        assert!(fs.decls.len() > 50);
        assert!(elapsed.as_millis() < 500, "took {elapsed:?}");
    }

    #[cfg(unix)]
    #[test]
    fn one_megabyte_files_complete() {
        let rust: String = (0..8000)
            .map(|i| format!("/// doc {i}\n#[inline]\npub fn function_{i}(a: u32, b: &str) -> Vec<u32> {{\n    let x = a + {i};\n    vec![x; 4]\n}}\n\n"))
            .collect();
        let python: String = (0..12000)
            .map(|i| format!("@decorator\ndef function_{i}(a: int, b: str) -> list:\n    x = a + {i}\n    return [x] * 4\n\n"))
            .collect();
        for (lang, src, count) in [(Lang::Rust, rust, 8000), (Lang::Python, python, 12000)] {
            assert!(src.len() > 900_000, "{lang:?} {}", src.len());
            let start = thread_cpu();
            let fs = extract(lang, src.as_bytes()).unwrap();
            let elapsed = thread_cpu() - start;
            assert!(!fs.has_error);
            assert_eq!(fs.decls.len(), count);
            assert!(elapsed.as_secs() < 5, "{lang:?} took {elapsed:?}");
        }
    }

    #[test]
    fn very_deep_expressions_do_not_overflow_the_stack() {
        let expr = vec!["1"; 20_000].join(" + ");
        let src = format!("fn f() -> u64 {{ {expr} }}\nfn g() {{}}\n");
        let fs = extract(Lang::Rust, src.as_bytes()).unwrap();
        assert_eq!(names(&fs), ["f", "g"]);
        let ids = identifiers_in(Lang::Rust, src.as_bytes(), &[(1, 1)]).unwrap();
        assert!(ids.contains("f"));
    }
}
