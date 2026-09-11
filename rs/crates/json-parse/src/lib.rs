//! The order-preserving JSON tree flattener, shared by the app's built-in data view and
//! the `bshuler/avada-datatree` module.
//!
//! This is the half of a data view that a *module* owns: it turns a file's text into a flat
//! list of visible [`TreeLine`]s, one per node in document order, *given the set of folded
//! containers*. A data tree is interactive where a table or a preview is inert — the human
//! folds and unfolds containers — so the neutral IR is a flattened tree, not the frozen
//! [`Block`](avada_module_sdk) document a table ships. Both the app's built-in view (which
//! maps a line to its `ViewRow`) and the module (which maps a line to a rail-style `Row`)
//! call [`flatten`] with their own fold set and cannot drift, because the fold arithmetic,
//! the child counts, the value classification and the row cap all live here once.
//!
//! What stays with the caller is the paint: inking a value by type from a palette, escaping
//! the markup a UI would otherwise eat, and the path a container row carries so a click
//! reaches the toggle. This crate takes no palette and emits no markup — it names *what* is
//! on each line, never its colour.
//!
//! **Order.** `serde_json::Value` sorts object keys unless the `preserve_order` feature is
//! on, and it is not. A viewer that reorders a document misquotes it, so the tree has its
//! own [`Node`] with a hand-written visitor: `visit_map` hands entries over in document
//! order whatever the feature says, and the error still names a line and column.
//!
//! **JSON only.** `serde_json` is the whole dependency; YAML and TOML parsers are not here.
//! A `.yaml`/`.toml` file keeps the plain viewer until they land.

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use std::collections::BTreeSet;
use std::fmt;

/// Containers at this depth and deeper start collapsed, so a large file opens instantly and
/// the first screen is its shape rather than its leaves. The top level and its direct
/// children open; `package.json`'s `dependencies` is readable without a click, a lockfile's
/// per-package detail is one click away.
///
/// A constant, not a parameter of [`flatten`], on purpose: the built-in view and the module
/// must agree on which containers open by default or their fold sets would mean different
/// things, and a shared constant is an agreement neither caller can break.
pub const EXPAND_DEPTH: i32 = 2;

/// The most node rows a data view builds. A file with more is flattened to the cap and
/// [`Flattened::cut`] says how many visible nodes were left — a fact about the document, so
/// it belongs to the flatten rather than to any pixel.
pub const MAX_LINES: usize = 5_000;

/// One parsed JSON value, in document order.
#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    /// `null`.
    Null,
    /// `true` / `false`.
    Bool(bool),
    /// Kept as text: the viewer prints numbers, it never computes with them.
    Number(String),
    /// A string, unquoted here; [`Node::scalar_text`] adds the quotes when it prints.
    Str(String),
    /// An array, its items in order.
    Array(Vec<Node>),
    /// `Vec`, not a map, so two reads print the keys in the order the author wrote them.
    Object(Vec<(String, Node)>),
}

impl Node {
    /// Whether this node has children a reader can fold under it.
    pub fn is_container(&self) -> bool {
        matches!(self, Node::Array(_) | Node::Object(_))
    }

    /// The child count a collapsed container shows beside its key (`3 keys`, `12 items`).
    /// Empty for a scalar, which has no children to count.
    pub fn kids_label(&self) -> String {
        match self {
            Node::Object(kv) => plural(kv.len(), "key"),
            Node::Array(items) => plural(items.len(), "item"),
            _ => String::new(),
        }
    }

    /// A scalar as the viewer prints it — quoted for a string, bare otherwise. Empty for a
    /// container, whose text is its key alone.
    pub fn scalar_text(&self) -> String {
        match self {
            Node::Null => "null".into(),
            Node::Bool(b) => b.to_string(),
            Node::Number(n) => n.clone(),
            Node::Str(s) => format!("{s:?}"),
            Node::Array(_) | Node::Object(_) => String::new(),
        }
    }

    /// The kind of scalar this is, for a caller that inks a value by its type. `None` for a
    /// container, which is not a value but a heading over its members.
    pub fn value_type(&self) -> Option<ValueType> {
        match self {
            Node::Str(_) => Some(ValueType::Str),
            Node::Number(_) => Some(ValueType::Number),
            Node::Bool(_) => Some(ValueType::Bool),
            Node::Null => Some(ValueType::Null),
            Node::Array(_) | Node::Object(_) => None,
        }
    }
}

/// Which kind of scalar a line holds, so a caller can ink it by type without re-parsing.
/// A container has no `ValueType`; it is a heading, not a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueType {
    /// A JSON string.
    Str,
    /// A JSON number, kept verbatim as text.
    Number,
    /// A JSON boolean.
    Bool,
    /// JSON `null`.
    Null,
}

/// What one visible line is: a foldable container, or a scalar leaf.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LineKind {
    /// A container the reader can fold. `open` is its current disclosure — the depth
    /// default flipped by the caller's fold set — and `kids` is the child count it shows
    /// when collapsed (`3 keys`, `12 items`).
    Container {
        /// Whether the container is expanded; its members follow it only when it is.
        open: bool,
        /// The child count, already pluralised (`1 key`, `12 items`).
        kids: String,
    },
    /// A scalar leaf: its printed `value` and the `ty` a caller inks it by.
    Scalar {
        /// The value as printed — quoted for a string, bare otherwise.
        value: String,
        /// The scalar's type, for inking.
        ty: ValueType,
    },
}

/// One visible node in the flattened tree, in document order.
///
/// `key` is the object key or array index; `depth` is how far it nests; `path` is the stable
/// address (`$.deps.serde`, `$[0].name`) a caller both keys its fold set on and hands to a
/// toggle. `kind` says whether it folds and carries the count or value the row prints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeLine {
    /// The object key or array index this line names.
    pub key: String,
    /// Nesting depth; the top level is `0`.
    pub depth: i32,
    /// The stable path, unique across the document and stable across a toggle.
    pub path: String,
    /// Whether the line folds, and what it prints.
    pub kind: LineKind,
}

/// The flatten's result: the visible lines, and how many more were past the cap.
///
/// `cut` is a count, not the lines themselves — the walk counts what it skips rather than
/// building it, so a forty-thousand-node file costs the cap, not the file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Flattened {
    /// The visible node lines, at most [`MAX_LINES`] of them, in document order.
    pub lines: Vec<TreeLine>,
    /// How many visible nodes were left unbuilt past the cap.
    pub cut: usize,
}

fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

impl<'de> Deserialize<'de> for Node {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Node;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a JSON value")
            }
            fn visit_unit<E: de::Error>(self) -> Result<Node, E> {
                Ok(Node::Null)
            }
            fn visit_none<E: de::Error>(self) -> Result<Node, E> {
                Ok(Node::Null)
            }
            fn visit_bool<E: de::Error>(self, b: bool) -> Result<Node, E> {
                Ok(Node::Bool(b))
            }
            fn visit_i64<E: de::Error>(self, n: i64) -> Result<Node, E> {
                Ok(Node::Number(n.to_string()))
            }
            fn visit_u64<E: de::Error>(self, n: u64) -> Result<Node, E> {
                Ok(Node::Number(n.to_string()))
            }
            fn visit_f64<E: de::Error>(self, n: f64) -> Result<Node, E> {
                Ok(Node::Number(n.to_string()))
            }
            fn visit_str<E: de::Error>(self, s: &str) -> Result<Node, E> {
                Ok(Node::Str(s.to_string()))
            }
            fn visit_string<E: de::Error>(self, s: String) -> Result<Node, E> {
                Ok(Node::Str(s))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Node, A::Error> {
                let mut items = Vec::new();
                while let Some(item) = seq.next_element()? {
                    items.push(item);
                }
                Ok(Node::Array(items))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Node, A::Error> {
                let mut kv = Vec::new();
                while let Some((k, v)) = map.next_entry::<String, Node>()? {
                    kv.push((k, v));
                }
                Ok(Node::Object(kv))
            }
        }
        d.deserialize_any(V)
    }
}

/// Parses one document. The error names the line and column the way a compiler would, with
/// `line_base` added so a JSONL record's error points into the file, not the record.
fn parse(text: &str, line_base: usize) -> Result<Node, String> {
    serde_json::from_str::<Node>(text).map_err(|e| {
        // serde_json's Display appends ` at line L column C`; the position is reported in
        // the file's own numbering, so the suffix is split off and rebuilt.
        let full = e.to_string();
        let msg = full.split(" at line ").next().unwrap_or(&full).to_string();
        format!(
            "Not valid JSON: {msg} (line {}, column {})",
            line_base + e.line(),
            e.column()
        )
    })
}

/// The path of `key` under `parent`. A key that reads as a bare word is joined with a dot,
/// anything else — a dot, a bracket, a space, an empty string — is bracketed and quoted so
/// `$.a.b` and `$["a.b"]` never collide.
pub fn child_path(parent: &str, key: &str) -> String {
    let bare = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-');
    if bare {
        format!("{parent}.{key}")
    } else {
        format!("{parent}[{key:?}]")
    }
}

/// The projection. `text` is the file; `jsonl` reads one document per non-blank line;
/// `flipped` is the set of node paths whose disclosure differs from the depth default
/// (see [`EXPAND_DEPTH`]).
///
/// `Err` is the parse failure, or `"Empty file"`, as the text a caller shows in place of a
/// tree. At most [`MAX_LINES`] lines come back; [`Flattened::cut`] says how many more there
/// were.
#[tracing::instrument(level = "debug", skip_all)]
pub fn flatten(text: &str, jsonl: bool, flipped: &BTreeSet<String>) -> Result<Flattened, String> {
    if text.trim().is_empty() {
        return Err("Empty file".into());
    }
    let mut w = Walk {
        lines: Vec::new(),
        cut: 0,
        flipped,
    };
    if jsonl {
        for (i, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let n = i + 1;
            let node = parse(line, i)?;
            w.visit(&n.to_string(), &node, 0, &format!("${n}"));
        }
    } else {
        let root = parse(text, 0)?;
        match &root {
            // The document's own braces are not a node anyone folds: its members are the
            // top level, so the first line is the first key, not a lone `{`.
            Node::Object(kv) => {
                for (k, v) in kv {
                    w.visit(k, v, 0, &child_path("$", k));
                }
            }
            Node::Array(items) => {
                for (i, v) in items.iter().enumerate() {
                    w.visit(&i.to_string(), v, 0, &format!("$[{i}]"));
                }
            }
            scalar => w.visit("$", scalar, 0, "$"),
        }
    }
    Ok(Flattened {
        lines: w.lines,
        cut: w.cut,
    })
}

struct Walk<'a> {
    lines: Vec<TreeLine>,
    /// Visible lines past the cap, counted rather than built.
    cut: usize,
    flipped: &'a BTreeSet<String>,
}

impl Walk<'_> {
    /// Whether a container at `depth` with path `node` is open: the depth default, flipped
    /// once per toggle. An XOR set rather than a collapsed set so the state of a file with
    /// 40,000 deep containers is the handful the human touched, not the 40,000 they did not.
    fn expanded(&self, depth: i32, node: &str) -> bool {
        (depth < EXPAND_DEPTH) != self.flipped.contains(node)
    }

    fn push(&mut self, line: TreeLine) {
        if self.lines.len() >= MAX_LINES {
            self.cut += 1;
        } else {
            self.lines.push(line);
        }
    }

    fn visit(&mut self, key: &str, node: &Node, depth: i32, path: &str) {
        if node.is_container() {
            let open = self.expanded(depth, path);
            self.push(TreeLine {
                key: key.to_string(),
                depth,
                path: path.to_string(),
                kind: LineKind::Container {
                    open,
                    kids: node.kids_label(),
                },
            });
            if !open {
                return;
            }
            match node {
                Node::Object(kv) => {
                    for (k, v) in kv {
                        self.visit(k, v, depth + 1, &child_path(path, k));
                    }
                }
                Node::Array(items) => {
                    for (i, v) in items.iter().enumerate() {
                        self.visit(&i.to_string(), v, depth + 1, &format!("{path}[{i}]"));
                    }
                }
                _ => {}
            }
            return;
        }
        self.push(TreeLine {
            key: key.to_string(),
            depth,
            path: path.to_string(),
            kind: LineKind::Scalar {
                value: node.scalar_text(),
                // A container is handled above, so `value_type` is always `Some` here.
                ty: node.value_type().expect("a scalar has a value type"),
            },
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(text: &str) -> Flattened {
        flatten(text, false, &BTreeSet::new()).expect("parses")
    }

    fn folded(text: &str, flipped: &[&str]) -> Flattened {
        let set: BTreeSet<String> = flipped.iter().map(|s| s.to_string()).collect();
        flatten(text, false, &set).expect("parses")
    }

    /// `(key, depth, path, printed)` — the shape a line carries, with `printed` the text a
    /// caller builds (`key` for a container, `key: value` for a scalar).
    fn shape(f: &Flattened) -> Vec<(String, i32, String, String)> {
        f.lines
            .iter()
            .map(|l| {
                let printed = match &l.kind {
                    LineKind::Container { .. } => l.key.clone(),
                    LineKind::Scalar { value, .. } => format!("{}: {value}", l.key),
                };
                (l.key.clone(), l.depth, l.path.clone(), printed)
            })
            .collect()
    }

    #[test]
    fn an_object_flattens_to_one_line_per_member_in_document_order() {
        let got = flat(r#"{"zeta": 1, "alpha": "a", "mid": null, "ok": true}"#);
        assert_eq!(
            shape(&got),
            vec![
                ("zeta".to_string(), 0, "$.zeta".to_string(), "zeta: 1".to_string()),
                ("alpha".to_string(), 0, "$.alpha".to_string(), "alpha: \"a\"".to_string()),
                ("mid".to_string(), 0, "$.mid".to_string(), "mid: null".to_string()),
                ("ok".to_string(), 0, "$.ok".to_string(), "ok: true".to_string()),
            ],
            "keys in the order the author wrote them, not sorted"
        );
        assert!(
            got.lines
                .iter()
                .all(|l| matches!(l.kind, LineKind::Scalar { .. })),
            "a scalar has nothing to open or fold"
        );
    }

    #[test]
    fn value_types_classify_every_scalar() {
        let got = flat(r#"{"s": "x", "n": 3, "b": false, "z": null}"#);
        let tys: Vec<ValueType> = got
            .lines
            .iter()
            .filter_map(|l| match &l.kind {
                LineKind::Scalar { ty, .. } => Some(*ty),
                _ => None,
            })
            .collect();
        assert_eq!(
            tys,
            vec![ValueType::Str, ValueType::Number, ValueType::Bool, ValueType::Null]
        );
    }

    #[test]
    fn nesting_indents_and_a_container_is_a_foldable_line() {
        let got = flat(r#"{"deps": {"serde": "1", "tokio": "1"}}"#);
        // deps is depth 0, its members depth 1; deps is a container, open by default.
        assert!(matches!(
            got.lines[0].kind,
            LineKind::Container { open: true, .. }
        ));
        assert_eq!(got.lines[0].depth, 0);
        assert_eq!(got.lines[0].path, "$.deps");
        assert_eq!(got.lines[1].depth, 1);
        assert_eq!(got.lines[1].path, "$.deps.serde");
    }

    #[test]
    fn a_collapsed_container_shows_its_child_count() {
        let got = flat(r#"{"deps": {"a": 1, "b": 2, "c": 3}}"#);
        match &got.lines[0].kind {
            LineKind::Container { kids, .. } => assert_eq!(kids, "3 keys"),
            other => panic!("deps is a container, got {other:?}"),
        }
    }

    #[test]
    fn arrays_index_their_items() {
        let got = flat(r#"{"xs": ["a", "b"]}"#);
        assert_eq!(got.lines[1].key, "0");
        assert_eq!(got.lines[1].path, "$.xs[0]");
        assert_eq!(got.lines[2].key, "1");
        assert_eq!(got.lines[2].path, "$.xs[1]");
    }

    #[test]
    fn a_top_level_array_or_scalar_still_has_lines() {
        // A top-level array's items are the top level — no lone root row — so `[1, 2]`
        // is its two items, not a root plus two.
        let arr = flat("[1, 2]");
        assert_eq!(arr.lines.len(), 2);
        assert_eq!(arr.lines[0].path, "$[0]");
        assert_eq!(arr.lines[1].path, "$[1]");
        // A bare scalar is a single line addressed at the root.
        let scalar = flat("42");
        assert_eq!(scalar.lines.len(), 1);
        assert_eq!(scalar.lines[0].path, "$");
    }

    #[test]
    fn empty_containers_are_lines_with_nothing_under_them() {
        let got = flat(r#"{"a": {}, "b": []}"#);
        assert_eq!(got.lines.len(), 2, "two empty containers, no members");
        match &got.lines[0].kind {
            LineKind::Container { kids, .. } => assert_eq!(kids, "0 keys"),
            other => panic!("got {other:?}"),
        }
        match &got.lines[1].kind {
            LineKind::Container { kids, .. } => assert_eq!(kids, "0 items"),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn a_key_that_is_not_a_bare_word_is_bracketed_so_paths_cannot_collide() {
        let got = flat(r#"{"a.b": 1, "a": {"b": 2}}"#);
        assert_eq!(got.lines[0].path, r#"$["a.b"]"#);
        assert_eq!(got.lines[1].path, "$.a");
        assert_eq!(got.lines[2].path, "$.a.b");
    }

    #[test]
    fn containers_below_the_depth_threshold_start_collapsed() {
        // At EXPAND_DEPTH == 2, depth-0 and depth-1 containers open, depth-2 collapse.
        let got = flat(r#"{"a": {"b": {"c": {"d": 1}}}}"#);
        // a (d0, open) -> b (d1, open) -> c (d2, collapsed): its member d is not shown.
        let paths: Vec<&str> = got.lines.iter().map(|l| l.path.as_str()).collect();
        assert_eq!(paths, vec!["$.a", "$.a.b", "$.a.b.c"]);
        match &got.lines[2].kind {
            LineKind::Container { open, .. } => assert!(!open, "depth-2 container starts collapsed"),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn a_collapsed_container_hides_exactly_its_subtree_and_nothing_after_it() {
        // Fold $.a (open by default); its members vanish but $.z after it stays.
        let got = folded(r#"{"a": {"x": 1, "y": 2}, "z": 3}"#, &["$.a"]);
        let paths: Vec<&str> = got.lines.iter().map(|l| l.path.as_str()).collect();
        assert_eq!(paths, vec!["$.a", "$.z"]);
    }

    #[test]
    fn node_paths_are_stable_across_a_toggle() {
        let open = flat(r#"{"a": {"x": 1}}"#);
        let closed = folded(r#"{"a": {"x": 1}}"#, &["$.a"]);
        assert_eq!(open.lines[0].path, "$.a");
        assert_eq!(closed.lines[0].path, "$.a", "the path a toggle names does not move");
    }

    #[test]
    fn jsonl_is_one_tree_per_line_and_errors_name_the_file_line() {
        // Each non-blank line is its own record, a foldable container named by its line
        // number, with its members beneath it: $1, $1.a, $2, $2.b.
        let got = flatten("{\"a\": 1}\n{\"b\": 2}\n", true, &BTreeSet::new()).expect("parses");
        assert_eq!(got.lines[0].path, "$1");
        assert_eq!(got.lines[0].key, "1");
        assert!(matches!(got.lines[0].kind, LineKind::Container { open: true, .. }));
        assert_eq!(got.lines[1].path, "$1.a");
        assert_eq!(got.lines[2].path, "$2");
        assert_eq!(got.lines[2].key, "2");
        // A bad record names the file line, not the record's own line 1.
        let err = flatten("{\"a\": 1}\nnot json\n", true, &BTreeSet::new()).unwrap_err();
        assert!(err.contains("line 2"), "error names the file line: {err}");
    }

    #[test]
    fn a_malformed_file_names_the_line_and_never_panics() {
        let err = flatten("{\"a\": }", false, &BTreeSet::new()).unwrap_err();
        assert!(err.starts_with("Not valid JSON:"), "got {err}");
        assert!(err.contains("line 1"), "got {err}");
    }

    #[test]
    fn an_empty_file_is_a_notice_not_an_empty_tree() {
        assert_eq!(flatten("   \n  ", false, &BTreeSet::new()).unwrap_err(), "Empty file");
    }

    #[test]
    fn a_forty_thousand_key_object_stays_under_the_cap() {
        let body: String = (0..40_000)
            .map(|i| format!("\"k{i}\": {i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let got = flatten(&format!("{{{body}}}"), false, &BTreeSet::new()).expect("parses");
        assert_eq!(got.lines.len(), MAX_LINES);
        assert_eq!(got.cut, 40_000 - MAX_LINES);
    }
}
