//! Track 3b-2: a module's own document pane, the reader half of tier 5.
//!
//! A [grid](gridpane) is the writer half — a rectangle of cells the module owns and the
//! reader edits. A *document* takes nothing back: the module ships parsed [`Block`]s over
//! `host.doc.set`, the host typesets them, and no keystroke returns. So the claim these
//! tests make that no Rust test can is that a document a module authored is *on screen* —
//! `project_doc` has unit tests for the block→row mapping, but not for whether those rows
//! reach a live `ViewPane`, nor that they stay inert once they do.
//!
//! Blocks go in through [`crate::module_ui::doc::set`] exactly as `host.doc.set` puts them
//! there, out through [`crate::viewpane::model_for`] exactly as `paneview::pane_item` reads
//! them, and into a real `ViewPane` — the same path a markdown module author would drive.

#![allow(unused_imports)]
use super::*;

use avada_core::module::doc::{Block, Cell, Doc};
use avada_core::module::Row;
use avada_core::rights::ModuleId;
use avada_core::tools::kind::{ModulePaneRef, PaneKind};

const UID: &str = "pane-preview";

/// A markdown module's preview surface — the module this reader was built for.
fn preview() -> PaneKind {
    PaneKind::Module(ModulePaneRef::new("bshuler/avada-markdown", "preview", None).unwrap())
}

/// Typeset `blocks` on the module's document surface the way `host.doc.set` does, project
/// them the way `paneview::pane_item` does, and hang the resulting pane in the window.
fn install_doc(w: &crate::AppWindow, blocks: Vec<Block>) {
    let kind = preview();
    let m = kind.module().unwrap();
    crate::module_ui::doc::set(&m.id, Doc::new(&m.surface, blocks));
    install_pane(w, &kind);
}

/// Build the pane's model through the same `model_for` the app calls, and set it — shared
/// by the doc path and the precedence test, which installs rows before a doc.
fn install_pane(w: &crate::AppWindow, kind: &PaneKind) {
    let model = crate::viewpane::model_for(UID, kind, None, 0);
    w.set_panes(
        std::rc::Rc::new(slint::VecModel::from(vec![crate::PaneItem {
            uid: UID.into(),
            title: "the preview".into(),
            x: 8.0,
            y: 40.0,
            w: 600.0,
            h: 500.0,
            visible: true,
            focused: true,
            kind: kind.ui_kind(),
            is_view: kind.is_view(),
            view_title: crate::viewpane::view_title(kind, None).into(),
            view_rows: model,
            view_sel_lo: -1,
            view_sel_hi: -1,
            font_px: crate::prefs::DEFAULT_FONT_PX,
            ..Default::default()
        }]))
        .into(),
    );
    settle();
}

/// One module row, so the precedence test can put a tier-2 surface under the doc. `data` is
/// the module's private payload and must never reach the tree — the same fixture shape
/// `modulepane` uses.
fn row(id: &str, label: &str) -> Row {
    Row {
        id: id.into(),
        label: label.into(),
        detail: String::new(),
        depth: 0,
        expandable: false,
        expanded: false,
        icon: None,
        marks: Vec::new(),
        data: serde_json::json!({ "secret": "module business" }),
    }
}

/// Leave the shared UI thread's stores as they were found: `ui()` runs every test on one
/// long-lived thread and these stores are thread-local, so a doc left in the store would
/// win over the next test's rows by precedence.
fn clean() {
    crate::module_ui::doc::forget(&ModuleId::new("bshuler/avada-markdown").unwrap());
    crate::module_ui::rows::forget(&ModuleId::new("bshuler/avada-markdown").unwrap());
}

/// The track's whole point: blocks a module typeset are drawn, structure and all. Each
/// block becomes a list-item labelled by its own text, and the projected role is the one
/// the block asked for — a heading is a heading, not a paragraph.
#[test]
fn a_document_a_module_typeset_reaches_the_pane() {
    ui(|| {
        let w = window();
        install_doc(
            &w,
            vec![
                Block::Heading {
                    level: 1,
                    text: "Release notes".into(),
                },
                Block::Prose {
                    text: "The grace window is fourteen days.".into(),
                },
                Block::Bullet {
                    depth: 0,
                    marker: "-".into(),
                    check: -1,
                    text: "Panels are modules now.".into(),
                },
            ],
        );

        // Every rendered block is a named list item — its text, the label a screen reader
        // (or this test) has on a row that has no name of its own.
        only(&w, "Release notes", AccessibleRole::ListItem);
        only(
            &w,
            "The grace window is fourteen days.",
            AccessibleRole::ListItem,
        );
        only(&w, "Panels are modules now.", AccessibleRole::ListItem);

        // The projected roles are what `project_doc` promised, read back off the live model.
        assert_eq!(
            crate::viewpane::row_at(UID, 0).unwrap().role,
            crate::viewpane::role::H1,
            "a level-1 heading projects to H1"
        );
        assert_eq!(
            crate::viewpane::row_at(UID, 1).unwrap().role,
            crate::viewpane::role::PROSE
        );
        assert_eq!(
            crate::viewpane::row_at(UID, 2).unwrap().role,
            crate::viewpane::role::BULLET
        );
        clean();
    });
}

/// A document row is inert, and that is the tier-5-reader contract: the module owns the
/// text and the host owns the pixels, so there is no gesture to send back. A module *row*
/// (tier 2) is activatable because the module owns the fold; a doc block never is. Getting
/// this wrong would hand the reader a clickable paragraph that reported a row index to a
/// module with no handler for it.
#[test]
fn a_document_row_is_inert_because_the_host_owns_the_pixels() {
    ui(|| {
        let w = window();
        install_doc(
            &w,
            vec![
                Block::Heading {
                    level: 2,
                    text: "Heading".into(),
                },
                Block::Prose {
                    text: "A paragraph.".into(),
                },
            ],
        );

        for i in 0..2 {
            let r = crate::viewpane::row_at(UID, i).expect("the row is in the model");
            assert!(
                !r.activatable(),
                "doc row {i} is a reader's row and must not route a click back to the module"
            );
            assert_ne!(
                r.role,
                crate::viewpane::role::MODULE_ROW,
                "a doc block is never a tier-2 module row"
            );
        }
        clean();
    });
}

/// One surface, two possible sources: a doc (`host.doc.set`) and rows (`host.rows.set`).
/// They are alternatives, and the doc is the richer statement, so once a module has
/// typeset one it wins. This is the branch in `rows_for_pane` — proved end-to-end by
/// installing rows first, seeing them, then a doc, and seeing the doc replace them.
#[test]
fn a_doc_takes_precedence_over_rows_on_one_surface() {
    ui(|| {
        let w = window();
        let kind = preview();
        let m = kind.module().unwrap();

        // Rows first: the tier-2 surface renders the module's own row.
        crate::module_ui::rows::set(&m.id, &m.surface, vec![row("a", "a module row")]);
        install_pane(&w, &kind);
        only(&w, "a module row", AccessibleRole::ListItem);

        // Now the module typesets a document on the same surface. Rebuilt through the same
        // `model_for`, the pane shows the doc and the row is gone.
        crate::module_ui::doc::set(
            &m.id,
            Doc::new(
                &m.surface,
                vec![Block::Heading {
                    level: 1,
                    text: "the document wins".into(),
                }],
            ),
        );
        install_pane(&w, &kind);
        only(&w, "the document wins", AccessibleRole::ListItem);
        assert!(
            by_role(&w, "a module row", AccessibleRole::ListItem).is_empty(),
            "a typeset document replaces the rows on its surface, it does not stack under them"
        );
        clean();
    });
}

/// When the host says the module is gone, its last word must not stay on screen. `forget`
/// clears the document, and because the revision moves the pane's cache re-projects to
/// nothing — the same path `RailEvent::Gone` drives.
#[test]
fn a_gone_module_empties_its_document_pane() {
    ui(|| {
        let w = window();
        let kind = preview();
        let m = kind.module().unwrap();

        install_doc(
            &w,
            vec![Block::Heading {
                level: 1,
                text: "still here".into(),
            }],
        );
        only(&w, "still here", AccessibleRole::ListItem);

        // The module dies. Its document is forgotten, and the rebuilt pane is blank.
        crate::module_ui::doc::forget(&m.id);
        install_pane(&w, &kind);
        assert!(
            by_role(&w, "still here", AccessibleRole::ListItem).is_empty(),
            "a dead module's document is off the screen, not frozen on it"
        );
        clean();
    });
}
