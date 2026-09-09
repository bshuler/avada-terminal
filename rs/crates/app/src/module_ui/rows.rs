//! The rows a module projects into a pane of its own (contract `host.rows.set` with
//! `target = "pane"`), and their projection into [`crate::viewpane`]'s row model.
//!
//! This is the pane-side twin of [`crate::leftpanel::ModuleRail`]'s row store, and it is a
//! separate store on purpose: a module's rail contributions and its pane contributions are
//! separate manifest namespaces, so the same id can name both (the first-party marketplace
//! uses `marketplace` for each). One map keyed by that id would have the two surfaces
//! overwrite each other.
//!
//! Thread-local, like [`crate::datatree`]'s fold store, for the same reason: it is UI state
//! belonging to the window's thread, and every reader — the host event fold-in, the
//! projection, the click — already runs there.

use avada_core::module::Row;
use avada_core::rights::ModuleId;
use std::cell::RefCell;
use std::collections::HashMap;

use crate::viewpane::{role, ViewRow};

/// One surface's rows plus the counter [`crate::viewpane`]'s cache watches.
#[derive(Default)]
struct Surface {
    rows: Vec<Row>,
    /// Bumped on every replacement. The projection has no other way to notice: the rows
    /// are not a file, so there is no mtime to compare.
    revision: u64,
}

thread_local! {
    static SURFACES: RefCell<HashMap<String, Surface>> = RefCell::new(HashMap::new());
}

/// The store key. The same `<owner/repo>#<id>` shape the rail uses, so the two stores are
/// read the same way even though they never share an entry.
fn key(module: &ModuleId, surface: &str) -> String {
    crate::leftpanel::entry_key(module, surface)
}

/// Replace `surface`'s rows. A module sends the whole list every time, so this replaces
/// rather than merges — there is no partial update in contract 1.
pub fn set(module: &ModuleId, surface: &str, rows: Vec<Row>) {
    SURFACES.with(|s| {
        let mut s = s.borrow_mut();
        let e = s.entry(key(module, surface)).or_default();
        e.rows = rows;
        e.revision += 1;
    });
}

/// What `surface` last projected; empty when it has never spoken.
pub fn rows(module: &ModuleId, surface: &str) -> Vec<Row> {
    SURFACES.with(|s| {
        s.borrow()
            .get(&key(module, surface))
            .map(|e| e.rows.clone())
            .unwrap_or_default()
    })
}

/// How many times `surface` has been replaced. Zero for a surface that has never spoken,
/// which is also the value every non-module pane reports — an unspoken surface and a file
/// pane are equally "nothing here but the file", so they may share the number.
pub fn generation(module: &ModuleId, surface: &str) -> u64 {
    SURFACES.with(|s| {
        s.borrow()
            .get(&key(module, surface))
            .map_or(0, |e| e.revision)
    })
}

/// Drop every surface belonging to `module`. Called when the host says the module is gone:
/// its panes must empty rather than keep showing a dead module's last word as if it were
/// live.
pub fn forget(module: &ModuleId) {
    let prefix = format!("{}#", module.as_str());
    SURFACES.with(|s| {
        let mut s = s.borrow_mut();
        for k in s
            .keys()
            .filter(|k| k.starts_with(&prefix))
            .cloned()
            .collect::<Vec<_>>()
        {
            // Cleared, not removed: the pane is still open and its projection must see a
            // *new* revision, or the cache would hand back the rows the module left behind.
            if let Some(e) = s.get_mut(&k) {
                e.rows.clear();
                e.revision += 1;
            }
        }
    });
}

/// The dim trailing column for a row: its marks, then whatever detail the module wrote.
///
/// Marks and detail are separate fields on the wire and one column on screen, and the join
/// happens here because everything the view renders is decided in Rust (see the module docs
/// of [`crate::viewpane`]). An unknown mark rides along verbatim — the contract says the
/// host ignores marks it does not know, and showing the word is the cheapest way to ignore
/// one without hiding it.
fn trailing(row: &Row) -> String {
    let marks = row.marks.join(" · ");
    match (marks.is_empty(), row.detail.is_empty()) {
        (true, _) => row.detail.clone(),
        (false, true) => marks,
        (false, false) => format!("{marks} · {}", row.detail),
    }
}

/// `surface`'s rows as the view's rows. Every one is activatable — a module row's meaning
/// belongs to the module, so the host cannot decide that a click on one does nothing.
pub fn view_rows(module: &ModuleId, surface: &str) -> Vec<ViewRow> {
    rows(module, surface)
        .into_iter()
        .map(|r| ViewRow {
            role: role::MODULE_ROW,
            detail: trailing(&r),
            indent: i32::from(r.depth),
            check: match (r.expandable, r.expanded) {
                (false, _) => -1,
                (true, false) => 0,
                (true, true) => 1,
            },
            node: r.id,
            text: r.label,
            ..ViewRow::default()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> ModuleId {
        ModuleId::new(s).expect("a valid module id")
    }

    fn row(id: &str, expandable: bool, expanded: bool) -> Row {
        Row {
            id: id.into(),
            label: id.to_uppercase(),
            detail: String::new(),
            depth: 1,
            expandable,
            expanded,
            icon: None,
            marks: vec![],
            data: serde_json::Value::Null,
        }
    }

    /// The whole reason this store is not the rail's: the marketplace names both of its
    /// surfaces `marketplace`, and the two must not be one another.
    #[test]
    fn a_surface_is_per_module_and_per_id() {
        let (a, b) = (id("bshuler/avada-marketplace"), id("acme/avada-files"));
        set(&a, "marketplace", vec![row("one", false, false)]);
        set(
            &b,
            "marketplace",
            vec![row("two", false, false), row("three", false, false)],
        );
        set(&a, "other", vec![]);
        assert_eq!(rows(&a, "marketplace").len(), 1);
        assert_eq!(rows(&b, "marketplace").len(), 2);
        assert!(rows(&a, "other").is_empty());
        assert!(rows(&a, "never-spoken").is_empty());
    }

    /// The revision IS the cache key: rows that changed without it moving would leave the
    /// pane showing the previous list forever.
    #[test]
    fn every_replacement_moves_the_revision_and_a_gone_module_empties_its_panes() {
        let m = id("bshuler/avada-git");
        assert_eq!(generation(&m, "log"), 0, "an unspoken surface is at zero");
        set(&m, "log", vec![row("a", false, false)]);
        let first = generation(&m, "log");
        assert!(first > 0);
        // Same rows, said again: still a new revision. The store cannot tell an idempotent
        // resend from a real change without comparing, and the pane redrawing is cheaper
        // than the pane being wrong.
        set(&m, "log", vec![row("a", false, false)]);
        assert!(generation(&m, "log") > first);

        let before = generation(&m, "log");
        forget(&m);
        assert!(rows(&m, "log").is_empty(), "a dead module keeps no rows");
        assert!(
            generation(&m, "log") > before,
            "and the emptying is itself a change the projection must see"
        );
    }

    #[test]
    fn a_module_row_carries_its_id_depth_and_disclosure_into_the_view() {
        let m = id("bshuler/avada-marketplace");
        let mut leaf = row("ripgrep", false, false);
        leaf.marks = vec!["installed".into(), "update".into()];
        leaf.detail = "1.2.0".into();
        let mut open = row("all", true, true);
        open.depth = 0;
        set(&m, "browse", vec![leaf, open, row("some", true, false)]);

        let got = view_rows(&m, "browse");
        assert_eq!(got.len(), 3);
        assert!(got.iter().all(|r| r.role == role::MODULE_ROW));
        assert!(
            got.iter().all(ViewRow::activatable),
            "the module decides what a row means, so the host may not call one inert"
        );
        assert_eq!(
            got[0].node, "ripgrep",
            "the id the module named, not the index"
        );
        assert_eq!(got[0].text, "RIPGREP");
        assert_eq!(
            got[0].detail, "installed · update · 1.2.0",
            "marks and detail share one column"
        );
        assert_eq!(got[0].indent, 1);
        assert_eq!(
            [got[0].check, got[1].check, got[2].check],
            [-1, 1, 0],
            "flat, open, folded"
        );
    }

    #[test]
    fn the_trailing_column_holds_whichever_halves_exist() {
        let mut r = row("x", false, false);
        assert_eq!(trailing(&r), "");
        r.detail = "1.2.0".into();
        assert_eq!(trailing(&r), "1.2.0");
        r.marks = vec!["installed".into()];
        assert_eq!(trailing(&r), "installed · 1.2.0");
        r.detail = String::new();
        assert_eq!(trailing(&r), "installed");
    }
}
