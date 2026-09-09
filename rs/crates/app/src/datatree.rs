//! Data viewer pane (docs/viewer-panes-plan.md, track V1): a JSON document as a collapsible
//! tree of rows. Owned by the V1 track.
//!
//! Two halves. [`tree_rows`] is the pure projection: text in, one [`ViewRow`] per visible
//! node out, in document order. The rest is the one thing in a view pane that is *not* a
//! function of the file — which containers the human has folded. That lives here, keyed by
//! pane uid, and is folded into `viewpane::Fingerprint` through [`generation`] so a toggle
//! is a cache miss rather than a stale tree.
//!
//! **Row contract (role 18, [`viewpane::role::DATA_NODE`]).** `text` is what a copy yields:
//! the key alone on a container (`deps`), `key: value` on a scalar (`name: "serde"`); the
//! disclosure glyph is decoration and never enters it. `indent` is the depth, `check` the
//! disclosure state — `-1` a scalar, `0` a collapsed container, `1` an expanded one — the
//! same three-state int a task box uses, because `ui/types.slint` is frozen and the row
//! struct Slint sees cannot grow a field of its own this wave. `detail` is the child count a
//! collapsed container shows (`3 keys`, `12 items`). `node` is the stable path
//! (`$.deps.serde`, `$[0].name`) the toggle command names, and `markup` is `text` again with
//! the value inked by type from the palette. A container is activatable (its `path` is the
//! file) so a click reaches `pane-view-activate`; a scalar is inert and selects.
//!
//! **JSON only.** `serde_json` is in the tree; YAML and TOML parsers are not, and the wave
//! adds no dependencies. A `.yaml`/`.toml` file keeps the plain viewer until they land.
//!
//! **Order.** `serde_json::Value` sorts object keys unless the `preserve_order` feature is
//! on, and it is not. A viewer that reorders a document misquotes it, so the tree has its
//! own [`Node`] with a hand-written visitor: `visit_map` hands entries over in document
//! order whatever the feature says, and the error still names a line and column.
//!
//! **Collapse state does not persist across restarts.** It is view state; the file may have
//! changed underneath it, and a stale path set is a bug for no proportionate gain.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::path::Path;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};

use crate::theme::UiPalette;
use crate::viewpane::{role, ViewRow, MAX_LINES};

/// Containers at this depth and deeper start collapsed, so a large file opens instantly and
/// the first screen is its shape rather than its leaves. The top level and its direct
/// children open; `package.json`'s `dependencies` is readable without a click, a lockfile's
/// per-package detail is one click away.
pub const EXPAND_DEPTH: i32 = 2;

/// One parsed value, in document order.
#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    Null,
    Bool(bool),
    /// Kept as text: the viewer prints numbers, it never computes with them.
    Number(String),
    Str(String),
    Array(Vec<Node>),
    /// `Vec`, not a map, so two reads print the keys in the order the author wrote them.
    Object(Vec<(String, Node)>),
}

impl Node {
    fn is_container(&self) -> bool {
        matches!(self, Node::Array(_) | Node::Object(_))
    }

    /// The child count a collapsed container shows beside its key.
    fn kids_label(&self) -> String {
        match self {
            Node::Object(kv) => plural(kv.len(), "key"),
            Node::Array(items) => plural(items.len(), "item"),
            _ => String::new(),
        }
    }

    /// A scalar as the viewer prints it — quoted for a string, bare otherwise.
    fn scalar_text(&self) -> String {
        match self {
            Node::Null => "null".into(),
            Node::Bool(b) => b.to_string(),
            Node::Number(n) => n.clone(),
            Node::Str(s) => format!("{s:?}"),
            Node::Array(_) | Node::Object(_) => String::new(),
        }
    }

    /// The palette token a scalar is inked with. Strings borrow the string colour a source
    /// pane uses, numbers its number colour, so a JSON file and the code that reads it agree.
    fn ink(&self, p: &UiPalette) -> Option<u32> {
        match self {
            Node::Str(_) => Some(p.ok),
            Node::Number(_) => Some(p.warn),
            Node::Bool(_) => Some(p.accent),
            Node::Null => Some(p.faint),
            Node::Array(_) | Node::Object(_) => None,
        }
    }
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

/// Parses one document. The error names the line and column the way a compiler would,
/// with `line_base` added so a JSONL record's error points into the file, not the record.
fn parse(text: &str, line_base: usize) -> Result<Node, String> {
    serde_json::from_str::<Node>(text).map_err(|e| {
        // serde_json's Display appends ` at line L column C`; the position is reported
        // in the file's own numbering, so the suffix is split off and rebuilt.
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
fn child_path(parent: &str, key: &str) -> String {
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
/// `file` is what a container row activates; `flipped` is the set of nodes whose disclosure
/// differs from the depth default (see [`EXPAND_DEPTH`]); the palette inks the values.
///
/// `Err` is the parse failure as the NOTICE the pane should show in place of a tree. An
/// empty file is an `Err` too — "Empty file" — the same words the plain viewer uses.
///
/// At most [`MAX_LINES`] node rows come back, plus one NOTICE saying how many were cut;
/// the walk counts what it skips rather than building it.
pub fn tree_rows(
    text: &str,
    jsonl: bool,
    file: &Path,
    flipped: &BTreeSet<String>,
    palette: &UiPalette,
) -> Result<Vec<ViewRow>, String> {
    if text.trim().is_empty() {
        return Err("Empty file".into());
    }
    let mut w = Walk {
        rows: Vec::new(),
        cut: 0,
        file,
        flipped,
        palette,
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
            // The document's own braces are not a node anyone folds: its members are
            // the top level, so the first row is the first key, not a lone `{`.
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
    let mut rows = w.rows;
    if w.cut > 0 {
        rows.push(ViewRow {
            role: role::NOTICE,
            text: format!("… {} more rows not shown", w.cut),
            ..ViewRow::default()
        });
    }
    Ok(rows)
}

struct Walk<'a> {
    rows: Vec<ViewRow>,
    /// Visible rows past the cap, counted rather than built.
    cut: usize,
    file: &'a Path,
    flipped: &'a BTreeSet<String>,
    palette: &'a UiPalette,
}

impl Walk<'_> {
    /// Whether a container at `depth` with path `node` is open: the depth default, flipped
    /// once per toggle. An XOR set rather than a collapsed set so the state of a file with
    /// 40,000 deep containers is the handful the human touched, not the 40,000 they did not.
    fn expanded(&self, depth: i32, node: &str) -> bool {
        (depth < EXPAND_DEPTH) != self.flipped.contains(node)
    }

    fn push(&mut self, row: ViewRow) {
        if self.rows.len() >= MAX_LINES {
            self.cut += 1;
        } else {
            self.rows.push(row);
        }
    }

    fn visit(&mut self, key: &str, node: &Node, depth: i32, path: &str) {
        let esc = |s: &str| crate::highlight::plain_markup(s, self.palette);
        if node.is_container() {
            let open = self.expanded(depth, path);
            self.push(ViewRow {
                role: role::DATA_NODE,
                text: key.to_string(),
                detail: node.kids_label(),
                // Activatable: the click has to reach Rust to become a toggle.
                path: self.file.to_path_buf(),
                indent: depth,
                check: i32::from(open),
                node: path.to_string(),
                markup: esc(key),
                ..ViewRow::default()
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
        let value = node.scalar_text();
        let inked = match node.ink(self.palette) {
            Some(argb) => format!(
                "<font color=\"#{:06x}\">{}</font>",
                argb & 0x00ff_ffff,
                esc(&value)
            ),
            None => esc(&value),
        };
        self.push(ViewRow {
            role: role::DATA_NODE,
            text: format!("{key}: {value}"),
            indent: depth,
            check: -1,
            node: path.to_string(),
            markup: format!("{}: {inked}", esc(key)),
            ..ViewRow::default()
        });
    }
}

// ---- Per-pane disclosure state.

/// What one pane has folded, and how many times it has been touched. The counter is what
/// the projection cache keys on: two toggles of the same node return the set to where it
/// was, but the rows in between were different, and a counter never collides.
#[derive(Default)]
struct Folds {
    gen: u64,
    flipped: BTreeSet<String>,
}

thread_local! {
    static FOLDS: RefCell<HashMap<String, Folds>> = RefCell::new(HashMap::new());
}

/// Flip node `node` of pane `uid`. Returns the pane's new [`generation`].
pub fn toggle(uid: &str, node: &str) -> u64 {
    FOLDS.with(|f| {
        let mut f = f.borrow_mut();
        let folds = f.entry(uid.to_string()).or_default();
        if !folds.flipped.remove(node) {
            folds.flipped.insert(node.to_string());
        }
        folds.gen += 1;
        folds.gen
    })
}

/// The nodes of pane `uid` whose disclosure differs from the depth default.
pub fn flipped(uid: &str) -> BTreeSet<String> {
    FOLDS.with(|f| {
        f.borrow()
            .get(uid)
            .map(|x| x.flipped.clone())
            .unwrap_or_default()
    })
}

/// How many toggles pane `uid` has seen; 0 for a pane never touched. Part of
/// `viewpane::Fingerprint`, so a toggle re-flattens the tree.
pub fn generation(uid: &str) -> u64 {
    FOLDS.with(|f| f.borrow().get(uid).map_or(0, |x| x.gen))
}

/// Drop pane `uid`'s folds. Called from `State::forget_pane_runtime`, the one path every
/// closed, detached or retired pane passes through — a deeply expanded tree holds a node
/// path per open branch, and a pane that is gone will never read them again.
pub fn forget(uid: &str) {
    FOLDS.with(|f| {
        f.borrow_mut().remove(uid);
    });
}

/// `Command::ViewToggleNode`: flip `node` of the active tab's pane `idx`. Only a data pane
/// has nodes; any other kind, or an index off the end, is ignored rather than recorded
/// against a pane that will never read it. Marks the UI dirty so the next build re-projects.
pub fn toggle_pane(state: &mut crate::state::State, idx: usize, node: &str) {
    let uid = state
        .active_tab()
        .panes
        .get(idx)
        .filter(|p| matches!(p.kind, avada_core::tools::PaneKind::Data))
        .map(|p| p.uid.clone());
    let Some(uid) = uid else {
        return;
    };
    toggle(&uid, node);
    state.dirty = true;
}

/// What an activated row of a view pane becomes when it is a data node: the toggle for
/// pane `pane` naming the row's `node`. `None` for every other role, so `app.rs` falls
/// through to the listing and file-open branches it already has. The decision lives here
/// rather than in the window's `pane-view-activate` closure so it is a function a test
/// can call: the closure needs a real `App`, and no test builds one.
#[tracing::instrument(level = "debug", ret)]
pub fn activate(pane: usize, row: &ViewRow) -> Option<crate::command::Command> {
    (row.role == role::DATA_NODE && row.activatable())
        .then(|| crate::command::Command::ViewToggleNode(pane, row.node.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn pal() -> UiPalette {
        crate::theme::ui_palette(0)
    }

    fn rows(text: &str) -> Vec<ViewRow> {
        tree_rows(
            text,
            false,
            Path::new("/x/a.json"),
            &BTreeSet::new(),
            &pal(),
        )
        .expect("parses")
    }

    fn folded(text: &str, flipped: &[&str]) -> Vec<ViewRow> {
        let set: BTreeSet<String> = flipped.iter().map(|s| s.to_string()).collect();
        tree_rows(text, false, Path::new("/x/a.json"), &set, &pal()).expect("parses")
    }

    /// `(text, indent, check, node)` — the four things a row is.
    fn shape(rows: &[ViewRow]) -> Vec<(String, i32, i32, String)> {
        rows.iter()
            .map(|r| (r.text.clone(), r.indent, r.check, r.node.clone()))
            .collect()
    }

    #[test]
    fn an_object_flattens_to_one_row_per_member_in_document_order() {
        let got = rows(r#"{"zeta": 1, "alpha": "a", "mid": null, "ok": true}"#);
        assert_eq!(
            shape(&got),
            vec![
                ("zeta: 1".to_string(), 0, -1, "$.zeta".to_string()),
                ("alpha: \"a\"".to_string(), 0, -1, "$.alpha".to_string()),
                ("mid: null".to_string(), 0, -1, "$.mid".to_string()),
                ("ok: true".to_string(), 0, -1, "$.ok".to_string()),
            ],
            "keys in the order the author wrote them, not sorted"
        );
        assert!(got.iter().all(|r| r.role == role::DATA_NODE));
        assert!(
            got.iter().all(|r| !r.activatable()),
            "a scalar has nothing to open or fold"
        );
    }

    #[test]
    fn nesting_indents_and_a_container_is_a_foldable_row() {
        let got = rows(r#"{"deps": {"serde": "1", "tokio": "1"}, "name": "x"}"#);
        assert_eq!(
            shape(&got),
            vec![
                ("deps".to_string(), 0, 1, "$.deps".to_string()),
                (
                    "serde: \"1\"".to_string(),
                    1,
                    -1,
                    "$.deps.serde".to_string()
                ),
                (
                    "tokio: \"1\"".to_string(),
                    1,
                    -1,
                    "$.deps.tokio".to_string()
                ),
                ("name: \"x\"".to_string(), 0, -1, "$.name".to_string()),
            ]
        );
        assert!(
            got[0].activatable(),
            "a container row has to reach the click"
        );
        assert_eq!(got[0].path, PathBuf::from("/x/a.json"));
        assert_eq!(got[0].detail, "2 keys");
    }

    #[test]
    fn arrays_index_their_items() {
        let got = rows(r#"{"list": [10, [20], {"k": 1}]}"#);
        assert_eq!(
            shape(&got),
            vec![
                ("list".to_string(), 0, 1, "$.list".to_string()),
                ("0: 10".to_string(), 1, -1, "$.list[0]".to_string()),
                ("1".to_string(), 1, 1, "$.list[1]".to_string()),
                ("0: 20".to_string(), 2, -1, "$.list[1][0]".to_string()),
                ("2".to_string(), 1, 1, "$.list[2]".to_string()),
                ("k: 1".to_string(), 2, -1, "$.list[2].k".to_string()),
            ]
        );
        assert_eq!(got[0].detail, "3 items");
        assert_eq!(got[2].detail, "1 item");
    }

    #[test]
    fn a_top_level_array_or_scalar_still_has_rows() {
        assert_eq!(
            shape(&rows("[1, 2]")),
            vec![
                ("0: 1".to_string(), 0, -1, "$[0]".to_string()),
                ("1: 2".to_string(), 0, -1, "$[1]".to_string()),
            ]
        );
        assert_eq!(
            shape(&rows("\"just a string\"")),
            vec![("$: \"just a string\"".to_string(), 0, -1, "$".to_string())]
        );
    }

    #[test]
    fn empty_containers_are_rows_with_nothing_under_them() {
        let got = rows(r#"{"a": {}, "b": []}"#);
        assert_eq!(
            shape(&got),
            vec![
                ("a".to_string(), 0, 1, "$.a".to_string()),
                ("b".to_string(), 0, 1, "$.b".to_string()),
            ]
        );
        assert_eq!(got[0].detail, "0 keys");
        assert_eq!(got[1].detail, "0 items");
    }

    #[test]
    fn a_key_that_is_not_a_bare_word_is_bracketed_so_paths_cannot_collide() {
        let got = rows(r#"{"a.b": {"c": 1}, "a": {"b": {"c": 2}}, "": 0, "x y": 1}"#);
        let nodes: Vec<&str> = got.iter().map(|r| r.node.as_str()).collect();
        assert_eq!(
            nodes,
            vec![
                "$[\"a.b\"]",
                "$[\"a.b\"].c",
                "$.a",
                "$.a.b",
                "$.a.b.c",
                "$[\"\"]",
                "$[\"x y\"]",
            ]
        );
    }

    #[test]
    fn containers_below_the_depth_threshold_start_collapsed() {
        let doc = r#"{"a": {"b": {"c": {"d": 1}}}}"#;
        let got = rows(doc);
        assert_eq!(
            shape(&got),
            vec![
                ("a".to_string(), 0, 1, "$.a".to_string()),
                ("b".to_string(), 1, 1, "$.a.b".to_string()),
                ("c".to_string(), 2, 0, "$.a.b.c".to_string()),
            ],
            "depth 2 is folded and its subtree is not built"
        );
        assert_eq!(got[2].detail, "1 key", "a folded row says what it hides");
    }

    #[test]
    fn a_collapsed_container_hides_exactly_its_subtree_and_nothing_after_it() {
        let doc = r#"{"a": {"x": 1, "y": {"z": 2}}, "b": 3}"#;
        let open = rows(doc);
        assert_eq!(open.len(), 5);
        let got = folded(doc, &["$.a"]);
        assert_eq!(
            shape(&got),
            vec![
                ("a".to_string(), 0, 0, "$.a".to_string()),
                ("b: 3".to_string(), 0, -1, "$.b".to_string()),
            ],
            "`b` follows `a` in the file and must survive folding `a`"
        );
        // Flipping a deep one opens it: the set is an XOR against the depth default.
        let deep = folded(r#"{"a": {"b": {"c": {"d": 1}}}}"#, &["$.a.b.c"]);
        assert_eq!(deep.len(), 4);
        assert_eq!(deep[2].check, 1);
        assert_eq!(deep[3].text, "d: 1");
    }

    #[test]
    fn node_paths_are_stable_across_a_toggle() {
        let doc = r#"{"a": {"x": 1}, "b": {"y": 2}, "c": 3}"#;
        let before = rows(doc);
        let after = folded(doc, &["$.a"]);
        // Row indices moved (`b` went from 2 to 1); paths did not.
        assert_eq!(before[2].node, "$.b");
        assert_eq!(after[1].node, "$.b");
        assert_eq!(before[4].node, "$.c");
        assert_eq!(after[3].node, "$.c");
    }

    #[test]
    fn jsonl_is_one_tree_per_line_and_errors_name_the_file_line() {
        let text = "{\"a\": 1}\n\n{\"b\": [1, 2]}\n";
        let got = tree_rows(
            text,
            true,
            Path::new("/x/a.jsonl"),
            &BTreeSet::new(),
            &pal(),
        )
        .expect("parses");
        assert_eq!(
            shape(&got),
            vec![
                ("1".to_string(), 0, 1, "$1".to_string()),
                ("a: 1".to_string(), 1, -1, "$1.a".to_string()),
                ("3".to_string(), 0, 1, "$3".to_string()),
                ("b".to_string(), 1, 1, "$3.b".to_string()),
                ("0: 1".to_string(), 2, -1, "$3.b[0]".to_string()),
                ("1: 2".to_string(), 2, -1, "$3.b[1]".to_string()),
            ],
            "the key of a record is its line number, and a blank line is skipped"
        );
        let err = tree_rows(
            "{\"a\": 1}\n{\"b\": }\n",
            true,
            Path::new("/x/a.jsonl"),
            &BTreeSet::new(),
            &pal(),
        )
        .expect_err("line 2 is broken");
        assert!(
            err.contains("line 2"),
            "the file's line, not the record's: {err}"
        );
    }

    #[test]
    fn a_malformed_file_names_the_line_and_never_panics() {
        let err = tree_rows(
            "{\n  \"a\": 1,\n  \"b\": ,\n}",
            false,
            Path::new("/x/a.json"),
            &BTreeSet::new(),
            &pal(),
        )
        .expect_err("not JSON");
        assert!(err.starts_with("Not valid JSON: "), "{err}");
        assert!(err.contains("line 3"), "{err}");
        assert!(
            !err.contains(" at line "),
            "the position is spoken once, in the file's numbering: {err}"
        );
    }

    #[test]
    fn an_empty_file_is_the_same_notice_the_plain_viewer_gives() {
        for text in ["", "   \n\n"] {
            let err = tree_rows(
                text,
                false,
                Path::new("/x/a.json"),
                &BTreeSet::new(),
                &pal(),
            )
            .expect_err("nothing to parse");
            assert_eq!(err, "Empty file");
        }
    }

    #[test]
    fn a_forty_thousand_key_object_stays_under_the_cap() {
        let mut doc = String::from("{");
        for i in 0..40_000 {
            if i > 0 {
                doc.push(',');
            }
            doc.push_str(&format!("\"k{i}\": {i}"));
        }
        doc.push('}');
        let got = rows(&doc);
        assert_eq!(
            got.len(),
            MAX_LINES + 1,
            "the cap, plus the row that says so"
        );
        assert_eq!(got[MAX_LINES].role, role::NOTICE);
        assert_eq!(
            got[MAX_LINES].text,
            format!("… {} more rows not shown", 40_000 - MAX_LINES)
        );
        assert_eq!(
            got[MAX_LINES - 1].text,
            format!("k{}: {}", MAX_LINES - 1, MAX_LINES - 1)
        );
    }

    #[test]
    fn values_are_inked_by_type_and_a_palette_switch_re_inks_them() {
        let doc = r#"{"s": "x", "n": 1.5, "b": false, "z": null, "o": {}}"#;
        let mocha = rows(doc);
        let p = pal();
        let ink = |argb: u32| format!("<font color=\"#{:06x}\">", argb & 0x00ff_ffff);
        assert!(mocha[0].markup.contains(&ink(p.ok)), "{}", mocha[0].markup);
        assert!(
            mocha[1].markup.contains(&ink(p.warn)),
            "{}",
            mocha[1].markup
        );
        assert!(
            mocha[2].markup.contains(&ink(p.accent)),
            "{}",
            mocha[2].markup
        );
        assert!(
            mocha[3].markup.contains(&ink(p.faint)),
            "{}",
            mocha[3].markup
        );
        assert!(
            !mocha[4].markup.contains("<font"),
            "a container's key is plain ink: {}",
            mocha[4].markup
        );
        // `text` is the verbatim pair; the markup is the same pair with colour.
        assert_eq!(mocha[0].text, "s: \"x\"");
        assert_eq!(mocha[1].text, "n: 1.5");

        let latte = tree_rows(
            doc,
            false,
            Path::new("/x/a.json"),
            &BTreeSet::new(),
            &crate::theme::ui_palette(3),
        )
        .unwrap();
        assert_eq!(
            latte[0].text, mocha[0].text,
            "the value itself must not move"
        );
        assert_ne!(latte[0].markup, mocha[0].markup, "a palette switch re-inks");
    }

    #[test]
    fn markup_escapes_what_markdown_would_otherwise_eat() {
        let got = rows(r#"{"*k*": "[a](b)"}"#);
        assert_eq!(got[0].text, "*k*: \"[a](b)\"");
        assert!(got[0].markup.contains("\\*k\\*"), "{}", got[0].markup);
        assert!(
            got[0].markup.contains("\\[a\\]\\(b\\)"),
            "{}",
            got[0].markup
        );
    }

    #[test]
    fn a_toggle_flips_the_set_and_bumps_the_generation() {
        let uid = "view-datatree-toggle-test";
        forget(uid);
        assert_eq!(generation(uid), 0);
        assert!(flipped(uid).is_empty());

        assert_eq!(toggle(uid, "$.a"), 1);
        assert_eq!(flipped(uid).into_iter().collect::<Vec<_>>(), vec!["$.a"]);
        // Toggling it back empties the set but not the counter: the rows in between were
        // different, and the cache has to know.
        assert_eq!(toggle(uid, "$.a"), 2);
        assert!(flipped(uid).is_empty());
        assert_eq!(generation(uid), 2);

        forget(uid);
        assert_eq!(generation(uid), 0);
    }
    /// `Command::ViewToggleNode` from the dispatcher down: the arm has to reach this
    /// pane's folds, and the UI has to be told to rebuild. Deleting the arm in
    /// `command::dispatch` fails this at compile time; wiring it to the wrong pane or
    /// forgetting `dirty` fails it here.
    #[test]
    fn the_toggle_command_folds_the_active_tabs_pane_and_marks_the_ui_dirty() {
        use crate::command::{dispatch, Command};
        use avada_core::session_manager::SessionManager;
        use avada_core::tools::PaneKind;
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let m = SessionManager::new(tx);
        let mut st = crate::state::State::new(crate::theme::load_font(1.0));
        let uid = st
            .add_pane_opts(
                &m,
                crate::state::NewPaneOpts {
                    kind: Some(PaneKind::Data),
                    cwd: Some("/repo/package.json".into()),
                    ..Default::default()
                },
            )
            .expect("data pane added");
        forget(&uid);
        let idx = st
            .active_tab()
            .panes
            .iter()
            .position(|p| p.uid == uid)
            .expect("the pane is in the active tab");
        st.dirty = false;

        dispatch(&mut st, Command::ViewToggleNode(idx, "$.deps".into()), &m);
        assert_eq!(generation(&uid), 1);
        assert_eq!(
            flipped(&uid).into_iter().collect::<Vec<_>>(),
            vec!["$.deps"]
        );
        assert!(st.dirty, "the pane has to rebuild to show the fold");

        // An index off the end is ignored, not recorded against nothing.
        st.dirty = false;
        dispatch(&mut st, Command::ViewToggleNode(99, "$.deps".into()), &m);
        assert_eq!(generation(&uid), 1);
        assert!(!st.dirty);

        // A pane of another kind has no nodes: the toggle is dropped, not recorded
        // against a uid that will never read it back.
        let plain = st
            .add_pane_opts(
                &m,
                crate::state::NewPaneOpts {
                    kind: Some(PaneKind::FileViewer),
                    cwd: Some("/repo/README".into()),
                    ..Default::default()
                },
            )
            .expect("viewer pane added");
        forget(&plain);
        let plain_idx = st
            .active_tab()
            .panes
            .iter()
            .position(|p| p.uid == plain)
            .expect("the viewer pane is in the active tab");
        st.dirty = false;
        dispatch(
            &mut st,
            Command::ViewToggleNode(plain_idx, "$.deps".into()),
            &m,
        );
        assert_eq!(generation(&plain), 0, "a viewer pane records no folds");
        assert!(!st.dirty);
        forget(&uid);
        forget(&plain);
    }

    /// The window's `pane-view-activate` closure asks [`activate`] what a clicked row
    /// means. A container node is the toggle for that pane, naming the node's path — not
    /// the row index, which the fold is about to move. Anything else is `None`, so the
    /// listing and file-open branches after it still get their turn.
    #[test]
    fn an_activated_container_row_is_the_toggle_for_its_pane_and_nothing_else_is() {
        use crate::command::Command;
        let rows = tree_rows(
            "{\"deps\": {\"serde\": \"1\"}, \"name\": \"x\"}",
            false,
            Path::new("/repo/package.json"),
            &BTreeSet::new(),
            &pal(),
        )
        .expect("parses");
        assert!(
            matches!(activate(3, &rows[0]), Some(Command::ViewToggleNode(3, ref n)) if n == "$.deps"),
            "{:?}",
            activate(3, &rows[0])
        );
        // A scalar is inert; a listing's directory row is somebody else's command.
        assert!(
            activate(3, &rows[2]).is_none(),
            "{:?}",
            activate(3, &rows[2])
        );
        let dir = ViewRow {
            role: role::DIR,
            path: PathBuf::from("/repo/src"),
            ..ViewRow::default()
        };
        assert!(activate(0, &dir).is_none(), "{:?}", activate(0, &dir));
    }
}
