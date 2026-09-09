//! Track 3b-1: a module's own pane.
//!
//! A tier-1 module gets two row surfaces, and until now only one of them existed. The
//! rail's rows were rendered, clicked and routed back to the module; a pane spawned with
//! `host.panes.spawn { kind: "module" }` logged "not rendered yet" and opened nothing.
//!
//! These tests drive the real thing: rows go in through [`crate::module_ui::rows::set`]
//! exactly as `host.rows.set { target: "pane" }` puts them there, out through
//! [`crate::viewpane::model_for`] exactly as `paneview::pane_item` reads them, and into a
//! live `ViewPane` — so what is asserted is what a module author would see.

#![allow(unused_imports)]
use super::*;

use avada_core::module::Row;
use avada_core::rights::ModuleId;
use avada_core::tools::kind::{ModulePaneRef, PaneKind};

/// The first-party marketplace, which is the module this surface was built for.
fn market() -> PaneKind {
    PaneKind::Module(ModulePaneRef::new("bshuler/avada-marketplace", "market", None).unwrap())
}

/// One row as a module sends it. `data` is deliberately non-empty: it is the module's
/// private payload, it must survive in the store, and it must never reach Slint.
fn row(id: &str, label: &str, detail: &str, depth: u8, disclosure: Option<bool>) -> Row {
    Row {
        id: id.into(),
        label: label.into(),
        detail: detail.into(),
        depth,
        expandable: disclosure.is_some(),
        expanded: disclosure.unwrap_or(false),
        icon: None,
        marks: Vec::new(),
        data: serde_json::json!({ "secret": "module business" }),
    }
}

/// Put `rows` on the module's pane surface and install the pane the way `pane_item` does,
/// through the same `model_for` the app calls. Returns the pane's uid.
fn install(w: &crate::AppWindow, rows: Vec<Row>) -> String {
    let kind = market();
    let m = kind.module().unwrap();
    crate::module_ui::rows::set(&m.id, &m.surface, rows);

    let uid = "pane-market";
    let model = crate::viewpane::model_for(uid, &kind, None, 0);
    w.set_panes(
        std::rc::Rc::new(slint::VecModel::from(vec![crate::PaneItem {
            title: "the pane".into(),
            x: 8.0,
            y: 40.0,
            w: 600.0,
            h: 500.0,
            visible: true,
            focused: true,
            kind: kind.ui_kind(),
            is_view: kind.is_view(),
            view_title: crate::viewpane::view_title(&kind, None).into(),
            view_rows: model,
            view_sel_lo: -1,
            view_sel_hi: -1,
            font_px: crate::prefs::DEFAULT_FONT_PX,
            ..Default::default()
        }]))
        .into(),
    );
    uid.into()
}

/// The whole point of the track: rows a module sent for a pane are drawn in that pane.
/// Before this they were dropped on the floor with a `tracing::debug!`, so "the module
/// spoke" and "the user can see it" were different claims.
#[test]
fn a_module_pane_draws_the_rows_the_module_sent() {
    ui(|| {
        let w = window();
        install(
            &w,
            vec![
                row("git", "avada-git", "1.4.0", 0, Some(true)),
                row("git/desc", "Git status in the rail", "", 1, None),
                row("files", "avada-files", "0.9.2", 0, Some(false)),
            ],
        );

        // Each row is a named list item — the module's label, not a line number.
        let entry = only(&w, "avada-git", AccessibleRole::ListItem);
        assert_eq!(
            entry.accessible_description().as_deref(),
            Some("1.4.0"),
            "the trailing column is the module's detail, described rather than named"
        );
        assert_eq!(
            entry.accessible_expandable(),
            Some(true),
            "a row the module marked expandable discloses"
        );
        assert_eq!(entry.accessible_expanded(), Some(true));

        let folded = only(&w, "avada-files", AccessibleRole::ListItem);
        assert_eq!(folded.accessible_expanded(), Some(false));

        // A leaf is not a disclosure, and a row with no detail describes nothing.
        let leaf = only(&w, "Git status in the rail", AccessibleRole::ListItem);
        assert_eq!(leaf.accessible_expandable(), Some(false));
        assert_eq!(leaf.accessible_description().as_deref(), Some(""));
    });
}

/// The pane header names the module and surface. There is no file behind a module pane,
/// so the path-shortening every other view title uses has nothing to shorten — an empty
/// header would have been the silent result.
#[test]
fn a_module_pane_is_titled_by_its_module_and_surface() {
    assert_eq!(
        crate::viewpane::view_title(&market(), None),
        "avada-marketplace · market"
    );
}

/// Every row goes back to the module, disclosure or not — the module owns the fold, so
/// there is no such thing as an inert row on this surface. `activatable` is what the
/// `TouchArea` consults, and it is decided Rust-side.
#[test]
fn every_module_row_is_activatable_because_the_module_owns_the_gesture() {
    ui(|| {
        let w = window();
        let uid = install(
            &w,
            vec![
                row("a", "expandable", "", 0, Some(false)),
                row("b", "a leaf", "", 0, None),
            ],
        );
        for i in 0..2 {
            let r = crate::viewpane::row_at(&uid, i).expect("the row is in the model");
            assert_eq!(r.role, crate::viewpane::role::MODULE_ROW);
            assert!(r.activatable(), "row {i} must reach the module");
        }
    });
}

/// Clicking row *n* must activate row *n*. The callback carries an index, so a pane that
/// opens something on every click and a pane that opens the right thing are different
/// claims — the same reason the file browser has this test.
#[test]
fn clicking_a_module_row_activates_the_row_it_names() {
    ui(|| {
        let w = window();
        install(
            &w,
            vec![
                row("git", "avada-git", "", 0, None),
                row("files", "avada-files", "", 0, None),
                row("hyperpane", "avada-hyperpane", "", 0, None),
            ],
        );

        let saw = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        {
            let saw = saw.clone();
            w.on_pane_view_activate(move |pane, row| saw.borrow_mut().push((pane, row)));
        }

        click(&w, &only(&w, "avada-files", AccessibleRole::ListItem));
        assert_eq!(*saw.borrow(), vec![(0, 1)]);
    });
}

/// A module row is not a line of a file. The viewer names its rows "Line 7: …" so two
/// identical lines stay distinct; doing that to a module's label would rename every row
/// the module drew, and `only(…, "avada-git")` above would find nothing.
#[test]
fn a_module_row_is_not_announced_as_a_numbered_line() {
    ui(|| {
        let w = window();
        install(&w, vec![row("git", "avada-git", "1.4.0", 0, None)]);
        assert!(
            by_role(&w, "Line 1.4.0: avada-git", AccessibleRole::ListItem).is_empty(),
            "the module's detail is not a line number"
        );
    });
}

/// The module's opaque `data` is its private business: it goes into the store, it comes
/// back out when the row is activated, and it never becomes a string in the UI tree.
/// A payload leaking into an accessible label would be both a privacy defect and a way
/// for a module to forge another module's row name.
#[test]
fn the_modules_opaque_payload_never_reaches_the_ui() {
    ui(|| {
        let w = window();
        let uid = install(&w, vec![row("git", "avada-git", "1.4.0", 0, None)]);
        let r = crate::viewpane::row_at(&uid, 0).unwrap();
        assert!(
            !format!("{r:?}").contains("module business"),
            "the view row carries only what it draws"
        );
        assert!(by_role(&w, "module business", AccessibleRole::ListItem).is_empty());
    });
}
