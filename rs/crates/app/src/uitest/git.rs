//! Track G1: the git working tree, after it stopped being part of the app.
//!
//! The left panel used to draw a `GIT` mode of its own — a mode index, a `LeftGitRow`
//! model, seven `on_git_*` callbacks and a `gitpanel.rs` that shelled out to
//! `git status --porcelain=v2` behind them. The porcelain stayed (it is the host's git
//! service now, behind the `git.read` capability); everything ABOVE it left for
//! `bshuler/avada-git`, a module that asks `host.git.status` and answers with rows.
//!
//! What has to survive that move is not "the rows still render". It is every gesture the
//! built-in mode offered: the sections a human reads the working tree by, the click that
//! opened a changed file, and — the one verb a module is not allowed to perform itself —
//! the diff, which spawns a process. These tests drive the panel from a fake
//! `bshuler/avada-git` (`RailEvent`s, exactly what `module::Host` hands the app) through
//! the same `fill_rail` projection the real resync uses, and then push the right-click the
//! rest of the way through a real `State` into the real menu builder.
#![allow(unused_imports)]
use super::*;

use crate::leftpanel::{entry_key, ModuleRail};
use crate::paneview::{fill_rail, LEFT_MODE_RAIL, LEFT_MODE_WORKSPACE};
use avada_core::module::{RailEntry, RailEvent, Row};
use avada_core::rights::ModuleId;

fn git_module() -> ModuleId {
    ModuleId::new("bshuler/avada-git").expect("a valid owner/repo module id")
}

/// The entry the module registers. Built through serde because `tier` is `UiTier`, a type
/// the app crate deliberately cannot name — this is the literal `host.rail.register`
/// payload. Tier 1 is the row list, which is all a status listing ever needed.
fn git_entry() -> RailEntry {
    serde_json::from_value(serde_json::json!({
        "id": "git", "label": "Git", "tier": 1, "order": 0
    }))
    .expect("a rail entry")
}

/// A section heading — Staged, Changed, Untracked — as the module projects it. It carries
/// no `path`, which is precisely how the host knows there is no file menu to draw over it.
fn section(title: &str) -> Row {
    Row {
        id: format!("section:{title}"),
        label: title.into(),
        detail: String::new(),
        depth: 0,
        expandable: false,
        expanded: false,
        icon: None,
        marks: vec![],
        data: serde_json::json!({ "kind": "section" }),
    }
}

/// One changed file. `data.path` is what lets the host open its own menu over the row
/// (§10.6); `data.git` is the extra fact only this module knows — which repository, and
/// which revision the row was listed FROM. A null `rev` means the working tree.
fn file_row(root: &str, path: &str, label: &str, code: &str, rev: Option<&str>) -> Row {
    Row {
        id: path.into(),
        label: label.into(),
        detail: code.into(),
        depth: 1,
        expandable: false,
        expanded: false,
        icon: None,
        marks: vec![],
        data: serde_json::json!({
            "kind": "file",
            "path": path,
            "code": code,
            "git": {
                "root": root,
                "rev": rev,
                "short": rev.map(|r| r.chars().take(7).collect::<String>()),
            },
        }),
    }
}

/// A working tree with something in each of the three sections, which is what the built-in
/// mode drew and what a human opens the panel for.
fn a_working_tree() -> Vec<Row> {
    vec![
        section("Staged"),
        file_row("/proj", "/proj/src/main.rs", "main.rs", "M", None),
        section("Changed"),
        file_row("/proj", "/proj/README.md", "README.md", "M", None),
        section("Untracked"),
        file_row("/proj", "/proj/notes.txt", "notes.txt", "?", None),
    ]
}

/// Put the panel exactly where a live `bshuler/avada-git` leaves it: registered,
/// projected, active, and drawn.
fn install(w: &crate::AppWindow, rows: Vec<Row>) -> String {
    let module = git_module();
    let mut rail = ModuleRail::default();
    rail.apply(RailEvent::Registered {
        module: module.clone(),
        entries: vec![git_entry()],
    });
    rail.apply(RailEvent::Rows {
        module: module.clone(),
        entry: "git".into(),
        rows,
    });
    let key = entry_key(&module, "git");
    assert!(rail.activate(&key), "the entry the module just registered");
    install_modes(w);
    fill_rail(w, &rail);
    w.global::<crate::LeftPanelAdapter>()
        .set_mode(LEFT_MODE_RAIL);
    key
}

/// THE regression this file exists for: everything the deleted `GIT` mode drew is on
/// screen again, reached the same way, with nothing in the app that knows it is git.
#[test]
fn a_git_module_puts_its_entry_on_the_strip_and_the_working_tree_in_the_panel() {
    ui(|| {
        let w = window();
        install(&w, a_working_tree());

        // The strip. The built-in mode's glyph is gone, so if the module's button is not
        // here there is no way into the working tree at all.
        let button = only(&w, "Git", AccessibleRole::Button);
        let size = button.size();
        assert!(
            size.width > 0.0 && size.height > 0.0,
            "the entry laid out to {}x{} — nothing can click it",
            size.width,
            size.height
        );

        let y = |l: &str| only(&w, l, AccessibleRole::Button).absolute_position().y;
        assert!(
            y("Staged") < y("main.rs")
                && y("main.rs") < y("Changed")
                && y("Changed") < y("README.md")
                && y("README.md") < y("Untracked")
                && y("Untracked") < y("notes.txt"),
            "each file has to stay under the heading that says what git will do with it — \
             a staged file listed under Untracked is a lie about the index"
        );

        // Measured on the *label*, not on the button: the button is the row's `TouchArea`
        // and deliberately spans the full width, so every row starts at the same x whatever
        // its depth. The indent lives on the content inside it.
        let label_x = |l: &str| only(&w, l, AccessibleRole::Text).absolute_position().x;
        assert!(
            label_x("main.rs") > label_x("Staged"),
            "the files are drawn indented under their heading, or the list reads as six \
             unrelated names"
        );
        assert_eq!(
            only(&w, "README.md", AccessibleRole::Button)
                .accessible_description()
                .as_deref(),
            Some("M"),
            "git's status letter is the whole point of the row and must be announced"
        );
    });
}

/// Clicking a changed file is the working tree's whole point. `RailRowView` tells the
/// gesture apart by `expandable` alone, so a status row — which never expands — has to
/// arrive as `open`, not `toggle`.
#[test]
fn clicking_a_changed_file_reaches_the_module_as_an_open() {
    ui(|| {
        let w = window();
        let key = install(&w, a_working_tree());

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

        click(&w, &only(&w, "README.md", AccessibleRole::Button));
        assert_eq!(
            got.borrow().as_slice(),
            [(key, "/proj/README.md".into(), "open".into())],
            "the row id is the module's own — a path here, but the host never reads it as one"
        );
    });
}

/// The diff. It is the one verb of the old panel a module cannot perform for itself:
/// `git show` is a command, and no module may spawn one — `host.panes.spawn` takes a kind
/// and a path, deliberately never a command line. So the module states a FACT on the row —
/// which repository, which revision — and the host offers the verb.
///
/// Driven all the way through: a real right-click, through the real callback, into a real
/// `State`, ending at the real menu builder.
#[test]
fn right_clicking_a_commit_row_offers_the_hosts_diff_for_that_revision() {
    ui(|| {
        let w = window();
        let rev = "abc1234def5678";
        let rows = vec![
            section("Commit"),
            file_row("/proj", "/proj/src/main.rs", "main.rs", "M", Some(rev)),
        ];
        install(&w, rows.clone());

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

        let main = only(&w, "main.rs", AccessibleRole::Button);
        right_click(&w, &main);

        let got = got.borrow();
        let (_, row_id, x, y) = got.first().expect("the right-click reached Rust").clone();
        assert_eq!(row_id, "/proj/src/main.rs");
        let pos = main.absolute_position();
        let size = main.size();
        assert!(
            (x - (pos.x + size.width / 2.0)).abs() < 2.0
                && (y - (pos.y + size.height / 2.0)).abs() < 2.0,
            "the menu has to open AT the pointer, so the coordinates must survive the \
             callback: got ({x}, {y}) for a row at {pos:?}"
        );

        // The other half: those coordinates and that row, through `State`, must produce the
        // host's file menu WITH the diff the row's `data.git` asked for.
        let mut st = crate::state::State::new(crate::theme::load_font(1.0));
        st.apply_rail_event(RailEvent::Registered {
            module: git_module(),
            entries: vec![git_entry()],
        });
        st.apply_rail_event(RailEvent::Rows {
            module: git_module(),
            entry: "git".into(),
            rows,
        });
        let key = entry_key(&git_module(), "git");
        st.rail.activate(&key);
        let _ = st.take_rail_requests();

        st.rail_context(&key, "/proj/src/main.rs", x, y);
        let menu = st.ctx.as_ref().expect("the host opened its own row menu");
        let labels = menu
            .entries
            .iter()
            .map(|e| e.label.clone())
            .collect::<Vec<_>>();
        assert!(
            labels.iter().any(|l| l == "Show Diff in abc1234"),
            "the module said which revision this row came from; the host owes it the diff \
             verb, named after that revision: {labels:?}"
        );
        assert!(
            labels.iter().any(|l| l == "Open in Terminal"),
            "and the ordinary file menu is still there — the diff is in ADDITION to it: \
             {labels:?}"
        );
        assert_eq!(
            st.take_rail_requests().len(),
            1,
            "the module still hears the gesture too"
        );
    });
}

/// A row that claims no revision gets no diff. Every module's rows go through this same
/// `rail_context`, so a host that offered the verb unconditionally would put "Show Diff" on
/// a row from the file explorer, in a directory that is not a repository at all.
#[test]
fn a_row_that_claims_no_revision_gets_the_plain_file_menu() {
    ui(|| {
        let mut st = crate::state::State::new(crate::theme::load_font(1.0));
        let module = ModuleId::new("bshuler/avada-files").expect("a module id");
        let entry: RailEntry = serde_json::from_value(serde_json::json!({
            "id": "files", "label": "Files", "tier": 1, "order": 0
        }))
        .expect("a rail entry");
        st.apply_rail_event(RailEvent::Registered {
            module: module.clone(),
            entries: vec![entry],
        });
        st.apply_rail_event(RailEvent::Rows {
            module: module.clone(),
            entry: "files".into(),
            rows: vec![Row {
                id: "/proj/README.md".into(),
                label: "README.md".into(),
                detail: String::new(),
                depth: 0,
                expandable: false,
                expanded: false,
                icon: None,
                marks: vec![],
                data: serde_json::json!({ "kind": "file", "path": "/proj/README.md" }),
            }],
        });
        let key = entry_key(&module, "files");
        st.rail.activate(&key);

        st.rail_context(&key, "/proj/README.md", 10.0, 10.0);
        let menu = st.ctx.as_ref().expect("the host opened its own row menu");
        let labels = menu
            .entries
            .iter()
            .map(|e| e.label.clone())
            .collect::<Vec<_>>();
        assert!(
            labels.iter().any(|l| l == "Open in Terminal"),
            "the ordinary menu is what a row without a revision gets: {labels:?}"
        );
        assert!(
            !labels.iter().any(|l| l.starts_with("Show Diff")),
            "nothing on this row says it came from a repository: {labels:?}"
        );
    });
}

/// The deletion itself. `LeftPanelAdapter` carried a `git` model, a `git-branch`, a
/// `git-commit` and seven callbacks; every one of them is a way for the app to grow a
/// second git panel back. The adapter is generated from the `.slint`, so the source is the
/// only place this can be checked — and checking it here means a merge that resurrects one
/// fails a test rather than passing review.
#[test]
fn no_git_member_is_left_on_the_left_panel_adapter() {
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
            !decl.contains("git"),
            "the working tree is a module now; this member is a way to grow it back: {decl}"
        );
    }
    assert!(
        !src.contains("LeftGitRow"),
        "and its row struct must be gone with it"
    );
}
