//! Track F1: the file explorer, after it stopped being part of the app.
//!
//! The left panel used to draw a `FILES` mode of its own — a mode index, a
//! `LeftFileRow` model, six `on_files_*` callbacks and a `filetree.rs` behind them. All of
//! it now lives in `bshuler/avada-files`, a module that says `host.rail.register` and then
//! `host.rows.set`, and the panel draws whatever comes back.
//!
//! That is a much bigger promise than "the rows still render": everything the built-in
//! mode did — opening a directory, clicking a file, the filter box, the reveal that scrolls
//! to a row, the right-click menu — now has to survive a process boundary and arrive
//! through a generic row list that knows nothing about files. These tests drive that list
//! from a fake `bshuler/avada-files` (`RailEvent`s, exactly what `module::Host` hands the
//! app) through the same `fill_rail` projection the real resync uses. A failure here is a
//! feature that a human can no longer reach with a mouse, not a property that failed to
//! store a string.
#![allow(unused_imports)]
use super::*;

use crate::leftpanel::{entry_key, ModuleRail};
use crate::paneview::fill_rail;
use avada_core::module::{RailEntry, RailEvent, Row};
use avada_core::rights::ModuleId;

fn files_module() -> ModuleId {
    ModuleId::new("bshuler/avada-files").expect("a valid owner/repo module id")
}

/// The entry the module registers. Built through serde because `tier` is `UiTier`, a type
/// the app crate deliberately cannot name — this is the literal `host.rail.register`
/// payload. Tier 1 is the row list, which is the only surface a file tree needs.
fn files_entry() -> RailEntry {
    serde_json::from_value(serde_json::json!({
        "id": "files", "label": "Files", "tier": 1, "order": 0
    }))
    .expect("a rail entry")
}

/// One row as `avada-files` projects it: the absolute path is the row id AND rides in
/// `data.path`, which is what lets the host open its own menu over it (§10.6).
fn row(path: &str, label: &str, depth: u8, dir: bool, expanded: bool, marks: &[&str]) -> Row {
    Row {
        id: path.into(),
        label: label.into(),
        detail: String::new(),
        depth,
        expandable: dir,
        expanded,
        icon: None,
        marks: marks.iter().map(|m| m.to_string()).collect(),
        data: serde_json::json!({
            "kind": if dir { "dir" } else { "file" },
            "path": path,
        }),
    }
}

/// A small project as the module would first paint it: an open `src`, one file inside it,
/// a folded `target`, a dotfile the module dimmed, and a README.
fn a_project() -> Vec<Row> {
    vec![
        row("/proj/src", "src", 0, true, true, &[]),
        row("/proj/src/main.rs", "main.rs", 1, false, false, &[]),
        row("/proj/target", "target", 0, true, false, &[]),
        row("/proj/.env", ".env", 0, false, false, &["hidden"]),
        row("/proj/README.md", "README.md", 0, false, false, &[]),
    ]
}

/// Put the panel exactly where a live `bshuler/avada-files` leaves it: registered,
/// projected, active, and drawn.
fn install(w: &crate::AppWindow, rows: Vec<Row>) -> String {
    let module = files_module();
    let mut rail = ModuleRail::default();
    rail.apply(RailEvent::Registered {
        module: module.clone(),
        entries: vec![files_entry()],
    });
    rail.apply(RailEvent::Rows {
        module: module.clone(),
        entry: "files".into(),
        rows,
    });
    let key = entry_key(&module, "files");
    assert!(rail.activate(&key), "the entry the module just registered");
    install_modes(w);
    fill_rail(w, &rail);
    key
}

/// THE regression this file exists for: everything the deleted `FILES` mode drew is on
/// screen again, reached the same way, with nothing in the app that knows it is a file.
#[test]
fn a_files_module_puts_its_entry_on_the_strip_and_a_project_in_the_panel() {
    ui(|| {
        let w = window();
        install(&w, a_project());

        // The strip. The built-in mode's glyph is gone, so if the module's button is not
        // here there is no way into the explorer at all.
        let button = only(&w, "Files", AccessibleRole::Button);
        let size = button.size();
        assert!(
            size.width > 0.0 && size.height > 0.0,
            "the entry laid out to {}x{} — nothing can click it",
            size.width,
            size.height
        );

        let src = only(&w, "src", AccessibleRole::Button);
        let main = only(&w, "main.rs", AccessibleRole::Button);
        let target = only(&w, "target", AccessibleRole::Button);
        let readme = only(&w, "README.md", AccessibleRole::Button);

        assert!(
            src.absolute_position().y < main.absolute_position().y
                && main.absolute_position().y < target.absolute_position().y
                && target.absolute_position().y < readme.absolute_position().y,
            "the rows keep the order the module flattened them in — a tree that reorders \
             itself is not a tree"
        );
        assert_eq!(src.accessible_expanded(), Some(true), "src is open");
        assert_eq!(target.accessible_expanded(), Some(false), "target is not");
        assert_eq!(
            readme.accessible_expandable(),
            Some(false),
            "a file does not open"
        );
        // Measured on the *label*, not on the button. The button is the row's `TouchArea`
        // and it deliberately spans the full width — the whole row is clickable, and a
        // screen reader should say so — which means every row's button starts at x = 0
        // whatever its depth. The indent lives on the content inside it, so the label is
        // the only element whose position can tell a nested row from a top-level one.
        let label_x = |l: &str| only(&w, l, AccessibleRole::Text).absolute_position().x;
        assert!(
            label_x("main.rs") > label_x("src"),
            "depth 1 has to be drawn indented under depth 0, or the tree reads as a flat \
             list of unrelated names"
        );
        assert_eq!(
            only(&w, ".env", AccessibleRole::Button)
                .accessible_value()
                .as_deref(),
            Some("hidden"),
            "a dimmed row is still listed and still announced"
        );
    });
}

/// Clicking a file is the explorer's whole point. A directory toggles and a file opens,
/// and the two must not send the same thing — the module tells them apart by the gesture
/// alone, exactly as `RailRowView` decides it from `expandable`.
#[test]
fn clicking_a_directory_toggles_and_clicking_a_file_opens() {
    ui(|| {
        let w = window();
        let key = install(&w, a_project());

        let got = std::rc::Rc::new(std::cell::RefCell::new(
            Vec::<(String, String, String)>::new(),
        ));
        {
            let got = got.clone();
            w.global::<crate::RailAdapter>()
                .on_row_activate(move |key, row, gesture| {
                    got.borrow_mut()
                        .push((key.to_string(), row.to_string(), gesture.to_string()));
                });
        }

        click(&w, &only(&w, "target", AccessibleRole::Button));
        click(&w, &only(&w, "main.rs", AccessibleRole::Button));
        assert_eq!(
            got.borrow().as_slice(),
            [
                (key.clone(), "/proj/target".into(), "toggle".into()),
                (key, "/proj/src/main.rs".into(), "open".into()),
            ],
            "the row id is the module's own — a path here, but the host never reads it as one"
        );
    });
}

/// The query box. The built-in mode owned a `TextInput` and a `files_query` field; a tier-1
/// module has no way to draw a text field at all, so the panel draws one for it and sends
/// `rail.query`. If the box is missing or its typing goes nowhere, recursive find — the one
/// feature of the old explorer with no other route to it — is gone.
#[test]
fn the_filter_box_is_drawn_for_a_tier_one_entry_and_typing_reaches_rust() {
    ui(|| {
        let w = window();
        install(&w, a_project());

        let box_ = only(&w, "Filter Files", AccessibleRole::TextInput);
        let size = box_.size();
        assert!(
            size.width > 0.0 && size.height > 0.0,
            "the filter box laid out to {}x{} — nothing can type into it",
            size.width,
            size.height
        );

        let got = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        {
            let got = got.clone();
            w.global::<crate::RailAdapter>()
                .on_query_changed(move |q| got.borrow_mut().push(q.to_string()));
        }

        // Click first: a box nothing focused would swallow every keystroke, which is a
        // failure mode a direct property write would hide completely.
        click(&w, &box_);
        for ch in ["m", "a"] {
            w.window()
                .dispatch_event(slint::platform::WindowEvent::KeyPressed {
                    text: slint::SharedString::from(ch),
                });
            w.window()
                .dispatch_event(slint::platform::WindowEvent::KeyReleased {
                    text: slint::SharedString::from(ch),
                });
        }
        assert_eq!(
            got.borrow().as_slice(),
            ["m".to_string(), "ma".to_string()],
            "each keystroke sends the WHOLE query: `rail.query` is a statement of what the \
             box now says, not a stream of keys the module has to reassemble"
        );
        assert_eq!(
            w.global::<crate::RailAdapter>().get_query(),
            "ma",
            "and Rust can read it back, so a reveal can clear it"
        );
    });
}

/// A module that never asked for a filter still gets the box, because `RailEntry` has no
/// `filterable` field and the SDK is frozen this wave — but a NON-tier-1 entry must not,
/// since it draws its own surface and a stray text field would sit on top of it.
#[test]
fn a_tier_two_entry_gets_no_filter_box() {
    ui(|| {
        let w = window();
        let module = ModuleId::new("acme/avada-graph").expect("a module id");
        let mut rail = ModuleRail::default();
        rail.apply(RailEvent::Registered {
            module: module.clone(),
            entries: vec![serde_json::from_value(serde_json::json!({
                "id": "graph", "label": "Graph", "tier": 2, "order": 0
            }))
            .expect("a rail entry")],
        });
        assert!(rail.activate(&entry_key(&module, "graph")));
        install_modes(&w);
        fill_rail(&w, &rail);

        assert!(
            !w.global::<crate::RailAdapter>().get_filterable(),
            "the box is a tier-1 affordance"
        );
        assert!(
            by_role(&w, "Filter Graph", AccessibleRole::TextInput).is_empty(),
            "and it must not be drawn over a surface the module owns"
        );
    });
}

/// Reveal-in-files. A link says `path:line`, the module walks its tree, expands the
/// ancestors and marks the row `selected` — and the panel has to actually MOVE, because a
/// project's tree is far longer than the panel is tall and a highlight nobody can see is
/// the same as no reveal at all.
#[test]
fn the_row_a_module_marks_selected_is_scrolled_into_view() {
    ui(|| {
        let w = window();
        let deep = |marks: &[&str]| {
            (0..80)
                .map(|i| {
                    let path = format!("/proj/f{i:02}.rs");
                    let m = if i == 60 { marks } else { &[][..] };
                    row(&path, &format!("f{i:02}.rs"), 0, false, false, m)
                })
                .collect::<Vec<_>>()
        };

        install(&w, deep(&[]));
        assert!(
            by_role(&w, "f60.rs", AccessibleRole::Button).is_empty(),
            "the fixture is pointless unless the row starts below the fold"
        );

        install(&w, deep(&["selected"]));
        let rail = w.global::<crate::RailAdapter>();
        assert!(
            rail.get_scroll_y() > 0.0,
            "Rust measures the rows above the mark; a zero offset means it did not find it"
        );

        // The assignment above moved the viewport; the ListView only re-instantiates the
        // rows it can now see on the next frame.
        settle();
        let found = only(&w, "f60.rs", AccessibleRole::Button);
        let y = found.absolute_position().y;
        assert!(
            (0.0..800.0).contains(&y),
            "the revealed row has to be ON SCREEN, not merely instantiated: y={y}"
        );
        assert_eq!(
            found.accessible_item_selected(),
            Some(true),
            "and it has to be the one announced as selected"
        );
    });
}

/// The right-click. A tier-1 module has no surface to hang a popup on, so the HOST draws
/// the row menu from the row's own `data.path` (§10.6) — which is how the explorer's whole
/// "Open in…" list survived the move without the module shipping a single menu row.
///
/// Driven all the way through: a real right-click, through the real callback, into a real
/// `State`, ending at the real menu builder.
#[test]
fn right_clicking_a_row_opens_the_hosts_own_file_menu_over_its_path() {
    ui(|| {
        let w = window();
        install(&w, a_project());

        let got = std::rc::Rc::new(std::cell::RefCell::new(
            Vec::<(String, String, f32, f32)>::new(),
        ));
        {
            let got = got.clone();
            w.global::<crate::RailAdapter>()
                .on_row_context(move |key, row, x, y| {
                    got.borrow_mut()
                        .push((key.to_string(), row.to_string(), x, y));
                });
        }

        let readme = only(&w, "README.md", AccessibleRole::Button);
        right_click(&w, &readme);

        let got = got.borrow();
        let (_, row_id, x, y) = got.first().expect("the right-click reached Rust").clone();
        assert_eq!(row_id, "/proj/README.md");
        let pos = readme.absolute_position();
        let size = readme.size();
        assert!(
            (x - (pos.x + size.width / 2.0)).abs() < 2.0
                && (y - (pos.y + size.height / 2.0)).abs() < 2.0,
            "the menu has to open AT the pointer, so the coordinates must survive the \
             callback: got ({x}, {y}) for a row at {pos:?}"
        );

        // The other half: those coordinates and that row, through `State`, must produce
        // the host's file menu — not an empty one, and not the module's own.
        let mut st = crate::state::State::new(crate::theme::load_font(1.0));
        st.apply_rail_event(RailEvent::Registered {
            module: files_module(),
            entries: vec![files_entry()],
        });
        st.apply_rail_event(RailEvent::Rows {
            module: files_module(),
            entry: "files".into(),
            rows: a_project(),
        });
        let key = entry_key(&files_module(), "files");
        st.rail.activate(&key);
        let _ = st.take_rail_requests();

        st.rail_context(&key, "/proj/README.md", x, y);
        let menu = st.ctx.as_ref().expect("the host opened its own row menu");
        assert!(
            menu.entries.iter().any(|e| e.label == "Open in Terminal"),
            "the module ships no menu; this list is the app's, unchanged: {:?}",
            menu.entries.iter().map(|e| &e.label).collect::<Vec<_>>()
        );
        assert_eq!(
            st.take_rail_requests().len(),
            1,
            "and the module still hears the gesture — the menu is in ADDITION to it"
        );
    });
}

/// The deletion itself. `LeftPanelAdapter` carried a `files` model, a `files-root`, a
/// `files-query` and six callbacks; every one of them is a way for the app to grow a second
/// file explorer back. The adapter is generated from the `.slint`, so the source is the
/// only place this can be checked — and checking it here means a merge that resurrects one
/// fails a test rather than passing review.
#[test]
fn no_files_member_is_left_on_the_left_panel_adapter() {
    let src = include_str!("../../ui/types.slint");
    let adapter = src
        .split("export global LeftPanelAdapter")
        .nth(1)
        .expect("the adapter is declared")
        .split("\nexport global ")
        .next()
        .expect("and ends");
    for line in adapter.lines() {
        let decl = line.trim();
        if !(decl.starts_with("in property")
            || decl.starts_with("out property")
            || decl.starts_with("in-out property")
            || decl.starts_with("callback"))
        {
            continue;
        }
        assert!(
            !decl.contains("files") || decl.contains("git-commit-files"),
            "the built-in explorer is a module now; this member is a way to grow it back: \
             {decl}"
        );
    }
    assert!(
        !src.contains("LeftFileRow"),
        "and its row struct must be gone with it"
    );
}
