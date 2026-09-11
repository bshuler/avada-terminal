//! Projecting the node tree onto the host's tier-1 row vocabulary.
//!
//! This is deliberately the only file that knows what a [`Row`] looks like. Everything
//! above it works in [`Node`]s, which are testable without the SDK; everything below it is
//! the wire. Keeping the seam here is what lets `app.rs` be tested by asserting on state
//! rather than on JSON.

use crate::tree::{Node, NodeKind};
use avada_module_sdk::rail::Row;
use serde_json::{json, Value};
use std::collections::BTreeSet;

/// The rail entry id this module contributes; must match `avada.toml`.
pub const ENTRY: &str = "workspace";

/// The mark the host draws as selection. Selection is a *mark*, not a field on
/// `host.rows.set`: the host owns the list, and a module that could set selection could
/// yank the human's cursor out from under them mid-scroll.
pub const MARK_SELECTED: &str = "selected";
/// The mark for a row that reports rather than names something openable.
pub const MARK_NOTE: &str = "note";
/// The mark for a file that failed to parse.
pub const MARK_BROKEN: &str = "broken";

/// Project one node into a row.
pub fn row_of(node: &Node, expanded: bool, selected: bool) -> Row {
    let mut marks = vec![node.kind.mark().to_string()];
    if selected {
        marks.push(MARK_SELECTED.to_string());
    }
    if node.kind == NodeKind::Note {
        marks.push(MARK_NOTE.to_string());
        if node.id.starts_with("broken:") {
            marks.push(MARK_BROKEN.to_string());
        }
    }
    Row {
        id: node.id.clone(),
        label: node.label.clone(),
        detail: node.detail.clone().unwrap_or_default(),
        depth: node.depth,
        expandable: node.expandable,
        expanded: node.expandable && expanded,
        icon: None,
        marks,
        // A note carries a null payload, which is what makes it inert: the host echoes
        // `data` back on activation, and a row with nothing to echo cannot ask this module
        // to do anything.
        data: if node.activatable() {
            payload(node)
        } else {
            Value::Null
        },
    }
}

/// The opaque payload the host echoes back on `module.row.activate`.
///
/// It carries the *whole* address rather than just the row id, so activation never has to
/// re-derive where a row came from. A row the human clicks after the tree was rebuilt
/// still names the pane it named when it was drawn.
fn payload(node: &Node) -> Value {
    let mut data = json!({ "id": node.id, "kind": node.kind.mark() });
    if let Some(file) = &node.file {
        data["file"] = json!(file.to_string_lossy());
    }
    if let Some(addr) = node.addr {
        data["addr"] = json!({
            "window": addr.window,
            "group": addr.group,
            "pane": addr.pane,
        });
    }
    data
}

/// Project the whole tree.
pub fn rows(nodes: &[Node], expanded: &BTreeSet<String>, selected: Option<&str>) -> Vec<Row> {
    nodes
        .iter()
        .map(|n| row_of(n, expanded.contains(&n.id), Some(n.id.as_str()) == selected))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PaneAddr;
    use crate::tree::Section;
    use std::path::PathBuf;

    fn node(id: &str, kind: NodeKind) -> Node {
        Node {
            id: id.to_string(),
            depth: 1,
            kind,
            label: "label".into(),
            detail: None,
            expandable: false,
            file: None,
            addr: None,
        }
    }

    #[test]
    fn a_note_carries_no_payload_so_it_cannot_be_activated() {
        let r = row_of(&node("section:project:empty", NodeKind::Note), false, false);
        assert_eq!(r.data, Value::Null);
        assert!(r.marks.contains(&MARK_NOTE.to_string()));
        assert!(!r.marks.contains(&MARK_BROKEN.to_string()));
    }

    #[test]
    fn a_broken_file_row_is_marked_broken_as_well_as_note() {
        let r = row_of(
            &node("broken:/ws/x.avada.json", NodeKind::Note),
            false,
            false,
        );
        assert!(r.marks.contains(&MARK_BROKEN.to_string()));
    }

    #[test]
    fn a_pane_row_carries_its_file_and_its_address() {
        let mut n = node("ws:/ws/a.json/w0/g0/p1", NodeKind::Pane);
        n.file = Some(PathBuf::from("/ws/a.json"));
        n.addr = Some(PaneAddr {
            window: Some(0),
            group: Some(1),
            pane: 2,
        });
        let r = row_of(&n, false, false);
        assert_eq!(r.data["file"], json!("/ws/a.json"));
        assert_eq!(r.data["addr"], json!({"window":0,"group":1,"pane":2}));
        assert_eq!(r.data["kind"], json!("pane"));
    }

    #[test]
    fn expansion_is_only_reported_for_rows_that_have_children() {
        let mut n = node("section:project", NodeKind::Section(Section::Project));
        let expanded = BTreeSet::from(["section:project".to_string()]);
        // Nothing inside: the header must not claim to be an open container, or the host
        // draws a disclosure triangle that does nothing when clicked.
        assert!(!row_of(&n, expanded.contains(&n.id), false).expanded);
        n.expandable = true;
        assert!(row_of(&n, expanded.contains(&n.id), false).expanded);
    }

    #[test]
    fn selection_travels_as_a_mark_and_only_on_the_selected_row() {
        let nodes = [
            node("a", NodeKind::Workspace),
            node("b", NodeKind::Workspace),
        ];
        let rows = rows(&nodes, &BTreeSet::new(), Some("b"));
        assert!(!rows[0].marks.contains(&MARK_SELECTED.to_string()));
        assert!(rows[1].marks.contains(&MARK_SELECTED.to_string()));
    }

    #[test]
    fn a_missing_detail_is_the_empty_string_not_a_null() {
        // `Row::detail` is a plain String on the wire; sending null would fail the host's
        // own deserialisation rather than showing a blank second line.
        let r = row_of(&node("x", NodeKind::Workspace), false, false);
        assert_eq!(r.detail, "");
    }
}
