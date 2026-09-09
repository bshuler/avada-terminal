//! Track H4: the tier-5 module grid pane, driven through the real component tree.
//!
//! `src/gridpane.rs` has unit tests for the flattening, but the claim they cannot make is
//! the one that matters: that a frame a module sent is *on screen*. `GridPane` is drawn as
//! a sibling over `ModulePlaceholder`, both instantiated for `kind == 10`, with a single
//! `present` flag deciding which one the user sees — exactly the shape that can be right
//! in Rust, right in the `.slint`, and still render nothing because the adapter was never
//! bound or the run's `uid` filter never matched.
//!
//! So these push a frame in the way `host.grid.set` does, project it the way
//! `paneview::pane_item` does, and then read the text back out of the tree.

#![allow(unused_imports)]
use super::*;

use avada_core::module::grid::{GridCursor, GridFrame, GridLine, GridSpan};
use avada_core::rights::ModuleId;
use avada_core::tools::kind::{ModulePaneRef, PaneKind};
use slint::Model;

const UID: &str = "pane-editor";

fn editor() -> PaneKind {
    PaneKind::Module(ModulePaneRef::new("bshuler/avada-editor", "editor", None).unwrap())
}

/// One line, as one unstyled span. The styling has its own unit tests; what is under test
/// here is whether the characters reach a `Text`.
fn line(text: &str) -> GridLine {
    GridLine {
        spans: vec![GridSpan::plain(text)],
    }
}

/// Push `frame` at the module the way `host.grid.set` does, project it the way
/// `paneview::pane_item` does, and hang the resulting pane in the window.
fn install(w: &crate::AppWindow, frame: GridFrame) {
    let kind = editor();
    let m = kind.module().unwrap();
    crate::module_ui::grid::set_frame(&m.id, frame);
    crate::gridpane::project(UID, m, 8.0, 16.0, 13.0);
    crate::gridpane::attach(w);

    w.set_panes(
        std::rc::Rc::new(slint::VecModel::from(vec![crate::PaneItem {
            uid: UID.into(),
            title: "the editor".into(),
            x: 8.0,
            y: 40.0,
            w: 700.0,
            h: 500.0,
            visible: true,
            focused: true,
            kind: kind.ui_kind(),
            is_view: kind.is_view(),
            view_title: crate::viewpane::view_title(&kind, None).into(),
            font_px: crate::prefs::DEFAULT_FONT_PX,
            ..Default::default()
        }]))
        .into(),
    );
    settle();
}

/// Leave the shared UI thread's stores as they were found: `ui()` runs every test on one
/// long-lived thread, and these stores are thread-local.
fn clean() {
    crate::gridpane::forget(UID);
    crate::module_ui::grid::forget(&ModuleId::new("bshuler/avada-editor").unwrap());
}

/// The frame is drawn, and the module's status line with it. Both are read back by their
/// accessible label, which is the only handle on a `Text` that has no name of its own.
#[test]
fn a_frame_a_module_painted_is_on_screen_line_for_line() {
    ui(|| {
        let w = window();
        install(
            &w,
            GridFrame {
                surface: "editor".into(),
                cols: 80,
                rows: 24,
                lines: vec![line("fn main() {"), line("    hello()"), line("}")],
                cursor: Some(GridCursor {
                    line: 1,
                    col: 4,
                    ..Default::default()
                }),
                status: "NORMAL  main.rs  1:5".into(),
            },
        );

        for text in ["fn main() {", "    hello()", "}"] {
            assert_eq!(
                by_role(&w, text, AccessibleRole::Text).len(),
                1,
                "the module painted {text:?} and exactly one element shows it"
            );
        }
        only(&w, "NORMAL  main.rs  1:5", AccessibleRole::Text);
        clean();
    });
}

/// A run is placed by multiplying its cell coordinates by the metrics the layout pass
/// sized the pane with. Nothing in the `.slint` knows what a cell is, so an off-by-one in
/// that multiplication is invisible to every Rust test — but not to a reader of the tree.
#[test]
fn a_run_lands_at_the_cell_it_names() {
    ui(|| {
        let w = window();
        install(
            &w,
            GridFrame {
                surface: "editor".into(),
                cols: 80,
                rows: 24,
                lines: vec![line("top"), line("second")],
                ..Default::default()
            },
        );

        let top = only(&w, "top", AccessibleRole::Text).absolute_position();
        let second = only(&w, "second", AccessibleRole::Text).absolute_position();
        assert_eq!(top.x, second.x, "both start at column zero");
        assert_eq!(
            second.y - top.y,
            16.0,
            "one line apart is one cell height apart, not one text height"
        );
        clean();
    });
}

/// The other half of `present`: a module that has never painted, or that has just died,
/// must leave the placeholder showing rather than an empty black rectangle. `forget`
/// produces an empty frame at a *later* revision, which is the case the revision counter
/// alone would get wrong.
#[test]
fn a_surface_that_has_not_painted_shows_the_placeholder_and_not_a_blank_grid() {
    ui(|| {
        let w = window();
        // Never painted: no frame at all, so the projection has nothing to show.
        let kind = editor();
        let m = kind.module().unwrap();
        crate::gridpane::project(UID, m, 8.0, 16.0, 13.0);
        assert!(!crate::gridpane::row(UID).expect("a row exists").present);

        // Painted, then lost.
        install(
            &w,
            GridFrame {
                surface: "editor".into(),
                cols: 80,
                rows: 24,
                lines: vec![line("alive")],
                ..Default::default()
            },
        );
        only(&w, "alive", AccessibleRole::Text);

        crate::module_ui::grid::forget(&m.id);
        crate::gridpane::project(UID, m, 8.0, 16.0, 13.0);
        crate::gridpane::attach(&w);
        settle();
        assert!(
            !crate::gridpane::row(UID).expect("a row exists").present,
            "a dead module's pane is not a live one"
        );
        assert!(
            by_role(&w, "alive", AccessibleRole::Text).is_empty(),
            "the last frame it managed is gone from the tree, not frozen there"
        );
        clean();
    });
}

/// One editor, two stores — the visible half of it. A tier-5 surface's actions are
/// appended to the *existing* keybindings page as extra categories, so this asserts on the
/// model the real `resync` builds rather than on a hand-filled one: the category is named
/// for the module and surface, each row shows the preset's chord as `<kbd>` parts, and an
/// action the preset does not bind shows as unbound rather than absent.
#[test]
fn a_module_surface_becomes_a_category_on_the_app_s_own_keybindings_page() {
    use avada_core::module::grid::{DeclareKeymap, GridAction, KeymapPreset};

    ui(|| {
        let m = ModuleId::new("bshuler/avada-editor").unwrap();
        crate::module_ui::grid::set_keymap(
            &m,
            DeclareKeymap {
                surface: "editor".into(),
                actions: vec![
                    GridAction {
                        id: "move.left".into(),
                        label: "Move left".into(),
                    },
                    GridAction {
                        id: "file.save".into(),
                        label: "Save".into(),
                    },
                ],
                presets: vec![KeymapPreset {
                    name: "helix".into(),
                    label: "Helix".into(),
                    // `file.save` is deliberately unbound by this preset.
                    bindings: [("ctrl+h".to_string(), "move.left".to_string())]
                        .into_iter()
                        .collect(),
                }],
                default_preset: Some("helix".into()),
            },
        );

        let w = window();
        let ui_models = crate::paneview::Ui::new();
        ui_models.attach(&w);
        let mut state = crate::state::State::new(crate::theme::load_font(1.0));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mgr = avada_core::session_manager::SessionManager::new(tx);
        crate::paneview::resync(&mut state, &w, &ui_models, (1280.0, 800.0), 1.0, &mgr);

        let rows: Vec<(String, String, Vec<String>)> = ui_models
            .keybindings
            .iter()
            .filter(|k| k.category.contains("avada-editor"))
            .map(|k| {
                (
                    k.category.to_string(),
                    k.label.to_string(),
                    k.parts.iter().map(|p| p.to_string()).collect(),
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                (
                    "bshuler/avada-editor · editor".to_string(),
                    "Move left".to_string(),
                    vec!["Ctrl".to_string(), "H".to_string()],
                ),
                (
                    "bshuler/avada-editor · editor".to_string(),
                    "Save".to_string(),
                    vec![],
                ),
            ],
            "a declared action the preset does not bind is a visible unbound row, not a \
             missing one — it is the row the user clicks to bind it"
        );

        crate::module_ui::grid::forget(&m);
    });
}
