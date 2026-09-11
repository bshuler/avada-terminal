//! Turning the tree model into the host's tier-1 [`Row`] list.
//!
//! A tier-1 module draws nothing: it hands the host a flat list of rows and the host
//! decides what a folder icon looks like on this platform, in this theme, at this DPI.
//! Everything the module wants to say about a row therefore has to fit into `icon`,
//! `marks` and `data` — three vocabularies the contract fixes.

use crate::app::State;
use crate::tree::{FileRow, KIND_DIR};
use avada_module_sdk::rail::Row;
use serde_json::json;

/// The rail entry id, matching `avada.toml`.
pub const ENTRY: &str = "files";

/// Row id of the "go up one directory" row.
pub const UP_ROW: &str = "up";
/// Row id of the root header row.
pub const ROOT_ROW: &str = "root";

/// The mark the host scrolls to. `host.rows.set` carries no selection field on purpose
/// (see `docs/module-contract.md` §10.1); a mark is how a module says "this one".
pub const MARK_SELECTED: &str = "selected";
/// A dotfile. The tree lists them — hiding them would be lying about what is on disk —
/// but the host may dim them.
pub const MARK_HIDDEN: &str = "hidden";

/// The whole row list for the current state.
pub fn rows(state: &State) -> Vec<Row> {
    let Some(root) = state.root.as_ref() else {
        return vec![note_row("no-root", "No workspace open")];
    };
    let mut out = Vec::with_capacity(state.rows.len() + 2);

    // `..` first, so the way out of a root that guessed too narrowly is always the row
    // your eye lands on. Absent at the filesystem root, where it would do nothing.
    if let Some(parent) = root.parent().filter(|p| *p != root.as_path()) {
        out.push(Row {
            id: UP_ROW.into(),
            label: "..".into(),
            detail: parent.display().to_string(),
            depth: 0,
            expandable: false,
            expanded: false,
            icon: Some("folder".into()),
            marks: Vec::new(),
            data: json!({ "kind": "up", "path": parent.display().to_string() }),
        });
    }

    // The root is a row rather than a caption because it is the drop target for "collapse
    // everything" and because the tree below it then has an honest depth.
    out.push(Row {
        id: ROOT_ROW.into(),
        label: crate::tree::name_of(root),
        detail: root.display().to_string(),
        depth: 0,
        expandable: true,
        expanded: true,
        icon: Some("folder-open".into()),
        marks: Vec::new(),
        data: json!({ "kind": "dir", "path": root.display().to_string(), "root": true }),
    });

    let mut note = 0usize;
    for r in &state.rows {
        out.push(row_of(state, r, &mut note));
    }
    out
}

fn row_of(state: &State, r: &FileRow, note: &mut usize) -> Row {
    if !r.activatable() {
        *note += 1;
        let mut row = note_row(&format!("note-{note}"), &r.label);
        row.depth = (r.depth + 1).clamp(0, u8::MAX as i32) as u8;
        return row;
    }
    let is_dir = r.kind == KIND_DIR;
    let mut marks = Vec::new();
    if state.selected.as_deref() == Some(r.path.as_path()) {
        marks.push(MARK_SELECTED.to_string());
    }
    if r.label.starts_with('.') {
        marks.push(MARK_HIDDEN.to_string());
    }
    Row {
        // The path is the identity. An index would move under a click the moment a
        // directory above it was expanded.
        id: r.path.display().to_string(),
        label: r.label.clone(),
        detail: r.detail.clone(),
        depth: (r.depth + 1).clamp(0, u8::MAX as i32) as u8,
        expandable: is_dir,
        expanded: r.expanded,
        icon: Some(
            match (is_dir, r.expanded) {
                (true, true) => "folder-open",
                (true, false) => "folder",
                _ => "file",
            }
            .into(),
        ),
        marks,
        data: json!({
            "kind": if is_dir { "dir" } else { "file" },
            "path": r.path.display().to_string(),
        }),
    }
}

/// An inert message row: no data, so `module.row.activate` never even reaches the app.
fn note_row(id: &str, label: &str) -> Row {
    Row {
        id: id.into(),
        label: label.into(),
        detail: String::new(),
        depth: 0,
        expandable: false,
        expanded: false,
        icon: None,
        marks: Vec::new(),
        data: serde_json::Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::tree::fake::FakeFs;
    use std::path::{Path, PathBuf};

    fn app(paths: &[(&str, bool)], root: &str) -> (App, FakeFs) {
        let mut fs = FakeFs::tree(paths);
        let mut app = App::new();
        app.set_workspace(Some(PathBuf::from(root)));
        app.activate(&mut fs);
        (app, fs)
    }

    #[test]
    fn the_list_leads_with_the_way_out_and_the_root_it_is_showing() {
        let (app, _) = app(&[("/w/a.txt", false)], "/w");
        let r = rows(&app.state);
        assert_eq!(r[0].id, UP_ROW);
        assert_eq!(r[0].label, "..");
        assert_eq!(r[1].id, ROOT_ROW);
        assert_eq!(r[1].label, "w");
        assert!(r[1].expanded);
        // Children sit one level under the root row they belong to.
        assert_eq!(r[2].label, "a.txt");
        assert_eq!(r[2].depth, 1);
    }

    #[test]
    fn the_filesystem_root_offers_no_way_further_up() {
        let (app, _) = app(&[("/a.txt", false)], "/");
        let r = rows(&app.state);
        assert_eq!(r[0].id, ROOT_ROW);
    }

    #[test]
    fn a_row_carries_its_path_and_kind_for_the_gesture_that_comes_back() {
        let (app, _) = app(&[("/w/src/main.rs", false)], "/w");
        let r = rows(&app.state);
        let dir = r.iter().find(|x| x.label == "src").unwrap();
        assert_eq!(dir.data["kind"], "dir");
        assert_eq!(dir.data["path"], "/w/src");
        assert!(dir.expandable);
        assert_eq!(dir.icon.as_deref(), Some("folder"));
    }

    #[test]
    fn an_expanded_directory_says_so_in_the_icon_as_well_as_the_flag() {
        let mut fs = FakeFs::tree(&[("/w/src/main.rs", false)]);
        let mut app = App::new();
        app.set_workspace(Some(PathBuf::from("/w")));
        app.activate(&mut fs);
        app.toggle(&mut fs, Path::new("/w/src"));
        let r = rows(&app.state);
        let dir = r.iter().find(|x| x.label == "src").unwrap();
        assert!(dir.expanded);
        assert_eq!(dir.icon.as_deref(), Some("folder-open"));
    }

    #[test]
    fn a_dotfile_is_listed_and_marked_rather_than_hidden() {
        let (app, _) = app(&[("/w/.env", false)], "/w");
        let r = rows(&app.state);
        let dot = r.iter().find(|x| x.label == ".env").unwrap();
        assert!(dot.marks.contains(&MARK_HIDDEN.to_string()));
    }

    #[test]
    fn the_selected_row_is_the_one_the_host_will_scroll_to() {
        let mut fs = FakeFs::tree(&[("/w/a.txt", false), ("/w/b.txt", false)]);
        let mut app = App::new();
        app.set_workspace(Some(PathBuf::from("/w")));
        app.activate(&mut fs);
        app.reveal(&mut fs, Path::new("/w/b.txt"), None, None);
        let r = rows(&app.state);
        let marked: Vec<&str> = r
            .iter()
            .filter(|x| x.marks.iter().any(|m| m == MARK_SELECTED))
            .map(|x| x.label.as_str())
            .collect();
        assert_eq!(marked, vec!["b.txt"], "exactly one row may be selected");
    }

    #[test]
    fn a_note_row_carries_no_data_so_it_cannot_be_activated() {
        let (app, _) = app(&[("/w", true)], "/w");
        let r = rows(&app.state);
        let note = r.last().unwrap();
        assert_eq!(note.label, "Empty directory");
        assert!(note.data.is_null());
        assert!(!note.expandable);
    }

    #[test]
    fn without_a_workspace_the_list_says_so_instead_of_being_empty() {
        let app = App::new();
        let r = rows(&app.state);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].label, "No workspace open");
    }
}
