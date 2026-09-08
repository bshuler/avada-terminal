//! End-to-end tests that drive the **real Slint component tree**.
//!
//! Everything below `State` had unit tests; the UI layer had none, so a feature could be
//! correct in Rust, correct in the `.slint` source, and still unreachable with a mouse —
//! a control laid out to zero size, covered by a sibling, or rendered behind a `if` that
//! is never true. That is the class of defect these catch.
//!
//! They run on Slint's headless testing backend: a real component tree, real layout, real
//! hit-testing, no window and no window-server connection. So they run in CI, and on a
//! developer's machine they never take focus.
#![cfg(test)]

// One file per Wave 1 track (docs/modules-fanout-plan.md §4); each `use super::*`s the
// helpers below. The orchestrator owns this file and the list; a track owns its file.
mod annotations;
mod datatree;
mod files;
mod git;
mod image;
mod links;
mod matrix;
mod placeholder;
mod rail;
mod rights;
mod table;

use i_slint_backend_testing::ElementHandle;
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition};

/// Run `f` on the one thread that owns the Slint platform, and propagate its result.
///
/// Slint binds a platform — and every window built on it — to the thread that installed
/// it, while `cargo test` runs each test on its own thread. So the UI lives on a single
/// long-lived worker and the tests post work to it. A panic (i.e. a failed assertion) is
/// carried back and re-raised on the test's own thread, so failures still land on the test
/// that caused them instead of killing the worker.
fn ui<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    type Job = Box<dyn FnOnce() + Send>;
    static TX: std::sync::OnceLock<std::sync::mpsc::Sender<Job>> = std::sync::OnceLock::new();
    let tx = TX.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("hp-uitest".into())
            .spawn(move || {
                i_slint_backend_testing::init_integration_test_with_mock_time();
                for job in rx {
                    job();
                }
            })
            .expect("ui test thread");
        tx
    });
    let (done, wait) = std::sync::mpsc::channel();
    tx.send(Box::new(move || {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        let _ = done.send(r);
    }))
    .expect("ui thread alive");
    match wait.recv().expect("ui thread answered") {
        Ok(v) => v,
        Err(p) => std::panic::resume_unwind(p),
    }
}

/// A window sized like a real one, so layout has room to be wrong in the ways it would be
/// wrong for a human. A too-small window would hide a squeeze bug; a huge one would hide a
/// different squeeze bug. This is roughly the default.
fn window() -> crate::AppWindow {
    let w = crate::AppWindow::new().expect("the component tree builds");
    w.window().set_size(slint::PhysicalSize::new(1280, 800));
    w
}

/// Let one frame happen.
///
/// A `ListView` is virtualized: it only instantiates the rows inside its own viewport, and
/// it recomputes that window when the frame is drawn — not when `viewport-y` is assigned.
/// So a test that scrolls (or that changes anything a layout has to settle) and then looks
/// straight away is reading the *previous* frame's tree, where the row it just revealed
/// does not exist yet. The backend runs on mock time, so this costs nothing but the tick.
fn settle() {
    i_slint_backend_testing::mock_elapsed_time(50);
}

/// Click an element the way a mouse would: move onto its centre, press, release. This goes
/// through Slint's own hit-testing, so a control that is zero-sized, clipped away or
/// covered by a sibling will simply not receive it — which is the point.
fn click(w: &crate::AppWindow, el: &ElementHandle) {
    let pos = el.absolute_position();
    let size = el.size();
    assert!(
        size.width > 0.0 && size.height > 0.0,
        "the control laid out to {}x{} — nothing can click it",
        size.width,
        size.height
    );
    let at = LogicalPosition::new(pos.x + size.width / 2.0, pos.y + size.height / 2.0);
    let win = w.window();
    win.dispatch_event(WindowEvent::PointerMoved { position: at });
    win.dispatch_event(WindowEvent::PointerPressed {
        position: at,
        button: PointerEventButton::Left,
    });
    win.dispatch_event(WindowEvent::PointerReleased {
        position: at,
        button: PointerEventButton::Left,
    });
}

/// Press the right button on an element's centre, the way a context-menu gesture does.
/// `TouchArea` reports the button in `pointer-event`, so a menu bound to the right button
/// only opens if the event really carries it — which is why this is not `click()` with a
/// flag.
fn right_click(w: &crate::AppWindow, el: &ElementHandle) {
    let pos = el.absolute_position();
    let size = el.size();
    assert!(
        size.width > 0.0 && size.height > 0.0,
        "the control laid out to {}x{} — nothing can click it",
        size.width,
        size.height
    );
    let at = LogicalPosition::new(pos.x + size.width / 2.0, pos.y + size.height / 2.0);
    let win = w.window();
    win.dispatch_event(WindowEvent::PointerMoved { position: at });
    win.dispatch_event(WindowEvent::PointerPressed {
        position: at,
        button: PointerEventButton::Right,
    });
    win.dispatch_event(WindowEvent::PointerReleased {
        position: at,
        button: PointerEventButton::Right,
    });
}

/// Click an element that lives inside a `PopupWindow`, given the popup's own window-space
/// origin.
///
/// `absolute_position()` walks up with `StopAtPopups`, so an element inside a popup reports
/// coordinates relative to the **popup**, while `dispatch_event` speaks **window**
/// coordinates. Nothing public bridges the two, so the caller supplies the origin — the same
/// arithmetic Slint does when it places the popup. Worth the fiddle: the alternative,
/// `invoke_accessible_default_action()`, proves the callback is wired but would pass just as
/// happily for a swatch laid out to zero size or buried under a sibling.
fn click_in_popup(w: &crate::AppWindow, origin: LogicalPosition, el: &ElementHandle) {
    let pos = el.absolute_position();
    let size = el.size();
    assert!(
        size.width > 0.0 && size.height > 0.0,
        "the control laid out to {}x{} — nothing can click it",
        size.width,
        size.height
    );
    let at = LogicalPosition::new(
        origin.x + pos.x + size.width / 2.0,
        origin.y + pos.y + size.height / 2.0,
    );
    let win = w.window();
    win.dispatch_event(WindowEvent::PointerMoved { position: at });
    win.dispatch_event(WindowEvent::PointerPressed {
        position: at,
        button: PointerEventButton::Left,
    });
    win.dispatch_event(WindowEvent::PointerReleased {
        position: at,
        button: PointerEventButton::Left,
    });
}

/// Every button announcing itself with this label. The glyph buttons in this UI draw a
/// Path and no text, so their accessible label — which is their tooltip sentence — is the
/// only thing that names them, to a screen reader and to this test alike.
fn by_label(w: &crate::AppWindow, label: &str) -> Vec<ElementHandle> {
    ElementHandle::find_by_accessible_label(w, label).collect()
}

// ===== the mode strip =====
//
// Every left-panel feature is reached through this strip, so a strip that does not switch
// makes every mode below it unreachable. It is drawn as glyph buttons with no text.

/// Publish the built-in modes the way `paneview` does on every resync.
///
/// One, not three: the explorer and the git working tree that used to follow it are the
/// `bshuler/avada-files` and `bshuler/avada-git` modules now, and a module reaches the
/// strip through `RailAdapter.entries` instead. The workspace is what is left, so this
/// helper's job is now to give the strip its fixed head for the module buttons to follow.
fn install_modes(w: &crate::AppWindow) {
    let rows = vec![crate::LeftModeRow {
        label: "Workspace".into(),
        icon: 0,
        brand: slint::Color::from_rgb_u8(0, 0, 0),
    }];
    let lp = w.global::<crate::LeftPanelAdapter>();
    lp.set_open(true);
    lp.set_modes(std::rc::Rc::new(slint::VecModel::from(rows)).into());
}

/// The strip has to tell Rust which mode was pressed, not merely move its own highlight:
/// `mode` is `in-out` and written in Slint, so `mode-changed` is the only thing that lets
/// Rust learn a module entry is no longer the one on screen.
#[test]
fn the_mode_strip_selects_a_built_in_and_tells_rust() {
    ui(|| {
        let w = window();
        install_modes(&w);
        // The strip draws itself only when there is somewhere else to go — one built-in
        // and no modules is not a choice. A rail entry is on screen and selected, which is
        // exactly the state this test is about leaving.
        w.global::<crate::RailAdapter>().set_present(true);
        w.global::<crate::LeftPanelAdapter>()
            .set_mode(crate::paneview::LEFT_MODE_RAIL);

        let saw = std::rc::Rc::new(std::cell::Cell::new(-99));
        {
            let saw = saw.clone();
            w.global::<crate::LeftPanelAdapter>()
                .on_mode_changed(move |m| saw.set(m));
        }

        let found = by_label(&w, "Workspace");
        assert_eq!(
            found.len(),
            1,
            "the strip must show exactly one Workspace button"
        );
        click(&w, &found[0]);

        assert_eq!(
            w.global::<crate::LeftPanelAdapter>().get_mode(),
            crate::paneview::LEFT_MODE_WORKSPACE,
            "the click must select the workspace mode"
        );
        assert_eq!(
            saw.get(),
            crate::paneview::LEFT_MODE_WORKSPACE,
            "the click must also reach mode-changed, which is what drops the rail entry"
        );
    });
}

/// Elements by their Slint element id (`Component::name`), for the controls that are not
/// icon buttons and so carry no tooltip — a tab chip is named by the title it draws, which
/// is data, not an affordance.
fn by_id(w: &crate::AppWindow, id: &str) -> Vec<ElementHandle> {
    ElementHandle::find_by_element_id(w, id).collect()
}

// ===== the top bar =====
//
// Everything above the panes: the tab strip, the panel toggle, the window buttons. A tab
// strip that does not switch tabs makes every pane behind the wrong tab unreachable, and
// the window buttons are the only way to minimise or close a frameless window.

/// Publish a tab strip. `active` is the selected index; `system` marks the app-owned tab,
/// which shows no ×.
fn install_tabs(w: &crate::AppWindow, titles: &[&str], active: usize, system: Option<usize>) {
    let rows: Vec<crate::TabItem> = titles
        .iter()
        .enumerate()
        .map(|(i, t)| crate::TabItem {
            title: (*t).into(),
            active: i == active,
            system: Some(i) == system,
        })
        .collect();
    w.set_tabs(std::rc::Rc::new(slint::VecModel::from(rows)).into());
}

/// The ＋ is the only pointer route to a new tab that does not go through a menu.
#[test]
fn the_new_tab_button_is_clickable_and_reaches_rust() {
    ui(|| {
        let w = window();
        install_tabs(&w, &["one"], 0, None);

        let fired = std::rc::Rc::new(std::cell::Cell::new(false));
        {
            let fired = fired.clone();
            w.on_new_tab(move || fired.set(true));
        }

        let found = by_label(&w, "New tab");
        assert_eq!(found.len(), 1, "the strip must show exactly one ＋");
        click(&w, &found[0]);
        assert!(fired.get(), "the ＋ must reach new-tab");
    });
}

/// Clicking a chip must select *that* chip. An off-by-one here is invisible in a
/// screenshot and switches the user to the wrong workspace.
#[test]
fn clicking_a_tab_chip_selects_that_tab() {
    ui(|| {
        let w = window();
        install_tabs(&w, &["one", "two", "three"], 0, None);

        let saw = std::rc::Rc::new(std::cell::Cell::new(-1));
        {
            let saw = saw.clone();
            w.on_select_tab(move |i| saw.set(i));
        }

        let chips = by_id(&w, "TabChip::ta");
        assert_eq!(chips.len(), 3, "one hit area per tab, in strip order");
        click(&w, &chips[2]);
        assert_eq!(saw.get(), 2, "clicking the third chip must select tab 2");
    });
}

/// The × is per-chip, so it carries the same off-by-one risk as selection — with a worse
/// outcome, since closing the wrong tab ends the wrong shells.
#[test]
fn the_tab_close_button_closes_that_tab() {
    ui(|| {
        let w = window();
        install_tabs(&w, &["one", "two", "three"], 1, None);

        let saw = std::rc::Rc::new(std::cell::Cell::new(-1));
        {
            let saw = saw.clone();
            w.on_close_tab(move |i| saw.set(i));
        }

        let found = by_label(&w, "Close this tab");
        assert_eq!(found.len(), 3, "every ordinary tab offers a ×");
        assert!(
            found[1].computed_opacity() > 0.0,
            "the selected tab's × must actually be painted, not merely present"
        );
        click(&w, &found[1]);
        assert_eq!(saw.get(), 1, "the second tab's × must close tab 1");
    });
}

/// The always-on "Hyperpane" tab cannot be closed, so it must not offer a ×: an affordance
/// that does nothing is worse than no affordance.
#[test]
fn the_system_tab_offers_no_close_button() {
    ui(|| {
        let w = window();
        install_tabs(&w, &["Hyperpane", "two", "three"], 1, Some(0));
        assert_eq!(
            by_label(&w, "Close this tab").len(),
            2,
            "the system tab must show no ×, the other two must"
        );
    });
}

/// The toggle is the only pointer route to the left panel — the surface the reported bug
/// lived on. If it stopped reaching Rust, every test above it would still pass.
#[test]
fn the_left_panel_toggle_reaches_rust() {
    ui(|| {
        let w = window();
        install_tabs(&w, &["one"], 0, None);
        w.global::<crate::LeftPanelAdapter>().set_open(false);

        let fired = std::rc::Rc::new(std::cell::Cell::new(false));
        {
            let fired = fired.clone();
            w.global::<crate::LeftPanelAdapter>()
                .on_toggle(move || fired.set(true));
        }

        let found = by_label(
            &w,
            "Show the left panel — workspace tree, library, sets, detached sessions",
        );
        assert_eq!(found.len(), 1, "a closed panel offers one Show button");
        click(&w, &found[0]);
        assert!(fired.get(), "the toggle must reach LeftPanelAdapter.toggle");
    });
}

/// The hamburger is the discoverable route to everything that has no icon of its own —
/// preferences, layouts, new pane. It opens the shared context menu at the app root.
#[test]
fn the_app_menu_button_reaches_rust() {
    ui(|| {
        let w = window();
        install_tabs(&w, &["one"], 0, None);

        let fired = std::rc::Rc::new(std::cell::Cell::new(false));
        {
            let fired = fired.clone();
            w.on_open_app_menu(move |_x, _y| fired.set(true));
        }

        let found = by_label(
            &w,
            "Application menu — new tab or pane, layouts, preferences",
        );
        assert_eq!(found.len(), 1, "one hamburger");
        click(&w, &found[0]);
        assert!(fired.get(), "the hamburger must reach open-app-menu");
    });
}

/// This is a frameless window: these three buttons ARE the title bar. Nothing else can
/// minimise, restore or close it with a mouse.
#[test]
fn the_window_controls_reach_rust() {
    ui(|| {
        let w = window();
        install_tabs(&w, &["one"], 0, None);

        let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::<&'static str>::new()));
        for (label, mark) in [
            ("Minimize the window", "min"),
            ("Maximize the window", "max"),
            ("Close the window", "close"),
        ] {
            let seen = seen.clone();
            match mark {
                "min" => w.on_min_window(move || seen.borrow_mut().push("min")),
                "max" => w.on_max_window(move || seen.borrow_mut().push("max")),
                _ => w.on_close_window(move || seen.borrow_mut().push("close")),
            }
            let found = by_label(&w, label);
            assert_eq!(found.len(), 1, "exactly one {label:?} button");
            click(&w, &found[0]);
        }
        assert_eq!(
            *seen.borrow(),
            vec!["min", "max", "close"],
            "each window button must reach its own callback, and only its own"
        );
    });
}

// ===== the pane header =====
//
// Four buttons in 26px, repeated per pane. They are the only pointer route to zoom,
// fullscreen and closing a pane, and each carries the pane's index — so "it works" and
// "it works on the pane you clicked" are different claims.

/// Publish `n` panes tiled side by side, each big enough that its 26px header is not
/// clipped away (the pane rect clips its children).
fn install_panes(w: &crate::AppWindow, kinds: &[i32]) {
    let rows: Vec<crate::PaneItem> = kinds
        .iter()
        .enumerate()
        .map(|(i, kind)| crate::PaneItem {
            title: format!("pane {i}").into(),
            x: 8.0 + i as f32 * 420.0,
            y: 40.0,
            w: 400.0,
            h: 300.0,
            visible: true,
            focused: i == 0,
            kind: *kind,
            ..Default::default()
        })
        .collect();
    w.set_panes(std::rc::Rc::new(slint::VecModel::from(rows)).into());
}

/// Close carries the index, and closing the wrong pane ends the wrong shell.
#[test]
fn the_pane_close_button_reaches_rust_with_its_own_index() {
    ui(|| {
        let w = window();
        install_panes(&w, &[0, 0]);

        let saw = std::rc::Rc::new(std::cell::Cell::new(-1));
        {
            let saw = saw.clone();
            w.on_pane_close(move |i| saw.set(i));
        }

        let found = by_label(&w, "Close this pane and end its shell");
        assert_eq!(found.len(), 2, "one close button per pane");
        click(&w, &found[1]);
        assert_eq!(saw.get(), 1, "the second pane's × must close pane 1");
    });
}

/// Zoom and fullscreen sit between the mic and the ×, in a header only 26px tall. A layout
/// that squeezed them to nothing would still draw a plausible-looking header.
#[test]
fn the_pane_zoom_and_fullscreen_buttons_reach_rust() {
    ui(|| {
        let w = window();
        install_panes(&w, &[0]);

        let zoomed = std::rc::Rc::new(std::cell::Cell::new(-1));
        let full = std::rc::Rc::new(std::cell::Cell::new(-1));
        {
            let zoomed = zoomed.clone();
            w.on_pane_zoom(move |i| zoomed.set(i));
            let full = full.clone();
            w.on_pane_fullscreen(move |i| full.set(i));
        }

        let z = by_label(&w, "Zoom this pane to fill the tab");
        assert_eq!(z.len(), 1);
        click(&w, &z[0]);
        assert_eq!(zoomed.get(), 0, "zoom must reach pane-zoom(0)");

        let f = by_label(&w, "Fullscreen this pane — the whole window, no chrome");
        assert_eq!(f.len(), 1);
        click(&w, &f[0]);
        assert_eq!(full.get(), 0, "fullscreen must reach pane-fullscreen(0)");
    });
}

/// A view pane (`kind >= 2`) is read-only: there is nowhere for a transcript to be typed,
/// so the microphone must be absent rather than present and inert.
#[test]
fn only_a_terminal_pane_offers_the_microphone() {
    ui(|| {
        let w = window();
        install_panes(&w, &[0, 2]);

        let mic = "Dictate into this pane — record speech, then type the transcript";
        assert_eq!(
            by_label(&w, mic).len(),
            1,
            "the terminal pane offers a mic and the view pane does not"
        );
        // …and the view pane's close button must not promise to end a shell it has not got.
        assert_eq!(by_label(&w, "Close this pane and end its shell").len(), 1);
        assert_eq!(by_label(&w, "Close this pane").len(), 1);
    });
}

/// The mic is a toggle drawn as one button, so "stop" is a different sentence on the same
/// control — the state a listener needs and the only thing that says recording is live.
#[test]
fn a_recording_pane_offers_stop_rather_than_start() {
    ui(|| {
        let w = window();
        w.set_panes(
            std::rc::Rc::new(slint::VecModel::from(vec![crate::PaneItem {
                title: "pane 0".into(),
                x: 8.0,
                y: 40.0,
                w: 400.0,
                h: 300.0,
                visible: true,
                focused: true,
                recording: true,
                ..Default::default()
            }]))
            .into(),
        );

        let fired = std::rc::Rc::new(std::cell::Cell::new(-1));
        {
            let fired = fired.clone();
            w.on_pane_mic(move |i| fired.set(i));
        }

        let stop = "Stop recording — transcribe and type it into this pane";
        let found = by_label(&w, stop);
        assert_eq!(found.len(), 1, "a recording pane must offer Stop");
        click(&w, &found[0]);
        assert_eq!(fired.get(), 0, "the mic must reach pane-mic(0)");
    });
}

// ===== the right-hand rail =====

/// The rail's ＋ is the primary way a pane gets made; the reported bug class is exactly a
/// button that is drawn but wired to nothing.
#[test]
fn the_rail_new_pane_button_reaches_rust() {
    ui(|| {
        let w = window();

        let fired = std::rc::Rc::new(std::cell::Cell::new(false));
        {
            let fired = fired.clone();
            w.on_new_pane(move || fired.set(true));
        }

        let found = by_label(
            &w,
            "New pane · Shift-click for shell, command and split options",
        );
        assert_eq!(found.len(), 1, "the rail shows exactly one ＋");
        click(&w, &found[0]);
        assert!(fired.get(), "the rail ＋ must reach new-pane");
    });
}

/// The projects flyout is the only route to a repo's worktrees, and it opens from a glyph
/// with no text on it.
#[test]
fn the_projects_rail_button_toggles_projects() {
    ui(|| {
        let w = window();
        w.set_sidebar_open(false);

        let fired = std::rc::Rc::new(std::cell::Cell::new(false));
        {
            let fired = fired.clone();
            w.on_toggle_projects(move || fired.set(true));
        }

        let found = by_label(&w, "Projects — open a repo or manage its git worktrees");
        assert_eq!(found.len(), 1, "a closed rail offers one Projects button");
        click(&w, &found[0]);
        assert!(fired.get(), "it must reach toggle-projects");
    });
}

/// The ＋ on the PROJECTS header exists only inside the open flyout, so it is exactly the
/// kind of control that can rot unnoticed behind a collapsed section.
#[test]
fn the_add_project_button_reaches_rust() {
    ui(|| {
        let w = window();
        w.set_sidebar_open(true);

        let fired = std::rc::Rc::new(std::cell::Cell::new(false));
        {
            let fired = fired.clone();
            w.on_open_add_project(move || fired.set(true));
        }

        let found = by_label(&w, "Add a project folder to this list");
        assert_eq!(found.len(), 1, "the open flyout offers one ＋");
        click(&w, &found[0]);
        assert!(fired.get(), "it must reach open-add-project");
    });
}

// ===== the overlays, the dialogs and the context menus =====
//
// Everything mounted at the window root behind `overlay-kind` or `ctx-kind`. None of it
// was reachable from a test until the components carried accessible names — which is the
// same sentence as "none of it was reachable from a screen reader", and the reason
// "Show Diff" could ship as a menu row nothing ever pressed.

use i_slint_backend_testing::AccessibleRole;

/// Controls announcing themselves with this label *and* this role.
///
/// Slint gives every `Text` an automatic `accessible-label` of its own string and the role
/// `text` (`builtins.slint`), so a card's heading and the button repeating that word are
/// both findable by label — only one of them is pressable. The role is what tells them
/// apart. The alternative, stripping the caption's name so the label is unique, would make
/// the screen reader worse in order to make the test easier.
fn by_role(w: &crate::AppWindow, label: &str, role: AccessibleRole) -> Vec<ElementHandle> {
    by_label(w, label)
        .into_iter()
        .filter(|e| e.accessible_role() == Some(role))
        .collect()
}

/// Exactly one control with this label and role, or a failure naming what was found. Every
/// overlay test wants this shape, and "found 0" vs "found 2" are different bugs.
fn only(w: &crate::AppWindow, label: &str, role: AccessibleRole) -> ElementHandle {
    let found = by_role(w, label, role);
    assert_eq!(
        found.len(),
        1,
        "expected exactly one {role:?} named {label:?}, found {}",
        found.len()
    );
    found.into_iter().next().unwrap()
}

/// A plain top-level menu row. `kind: 0` is an item; `-1` is a separator and `>= 2` opens a
/// submenu, which is why the index a row reports is not its position among the *rows*.
fn menu_row(label: &str) -> crate::MenuEntry {
    crate::MenuEntry {
        label: label.into(),
        ..Default::default()
    }
}

/// Post a context menu, the way a right-click on a pane header does.
fn install_menu(w: &crate::AppWindow, entries: Vec<crate::MenuEntry>) {
    w.set_ctx_x(140.0);
    w.set_ctx_y(120.0);
    w.set_ctx_entries(std::rc::Rc::new(slint::VecModel::from(entries)).into());
    w.set_ctx_kind(1);
}

/// The defect the user reported, one layer up from the git panel: a menu row that draws but
/// dispatches nothing. The separator makes this sharper than "something fired" — the row
/// must report its own index in `ctx-entries`, and a menu whose separators were skipped
/// while counting would run the neighbouring command instead.
#[test]
fn a_context_menu_row_reaches_rust_with_its_own_index() {
    ui(|| {
        let w = window();
        install_menu(
            &w,
            vec![
                menu_row("Split Right"),
                crate::MenuEntry {
                    kind: -1,
                    ..Default::default()
                },
                menu_row("Show Diff"),
            ],
        );

        let picked = std::rc::Rc::new(std::cell::Cell::new(-1));
        {
            let picked = picked.clone();
            w.on_ctx_pick(move |i| picked.set(i));
        }

        click(&w, &only(&w, "Show Diff", AccessibleRole::Button));
        assert_eq!(
            picked.get(),
            2,
            "the row must report its index in ctx-entries, separators counted"
        );
    });
}

/// A greyed row must be inert, not merely grey. The menus disable rows that would act on
/// nothing (Show Diff on an untracked file), and a disabled row that still dispatched would
/// be a worse bug than no row at all.
#[test]
fn a_disabled_context_menu_row_cannot_be_clicked() {
    ui(|| {
        let w = window();
        install_menu(
            &w,
            vec![
                menu_row("Close Pane"),
                crate::MenuEntry {
                    label: "Show Diff".into(),
                    disabled: true,
                    ..Default::default()
                },
            ],
        );

        let picked = std::rc::Rc::new(std::cell::Cell::new(-1));
        {
            let picked = picked.clone();
            w.on_ctx_pick(move |i| picked.set(i));
        }

        let row = only(&w, "Show Diff", AccessibleRole::Button);
        assert_eq!(
            row.accessible_enabled(),
            Some(false),
            "a disabled row must say so, not just draw itself faintly"
        );
        click(&w, &row);
        assert_eq!(picked.get(), -1, "a disabled row must dispatch nothing");
    });
}

/// A separator is decoration. If it were a row it would be pickable, and picking it would
/// dispatch an index the Rust side maps to a real command.
#[test]
fn a_context_menu_separator_is_not_a_row() {
    ui(|| {
        let w = window();
        install_menu(
            &w,
            vec![
                menu_row("Close Pane"),
                crate::MenuEntry {
                    kind: -1,
                    ..Default::default()
                },
            ],
        );
        let rows: Vec<_> = ElementHandle::find_by_element_id(&w, "ContextMenu::row").collect();
        assert_eq!(rows.len(), 1, "two entries, one of them a rule, is one row");
    });
}

/// Publish a command palette with `sel` highlighted.
fn install_palette(w: &crate::AppWindow, rows: &[(&str, &str)], sel: i32) {
    let items: Vec<crate::PaletteItem> = rows
        .iter()
        .map(|(t, s)| crate::PaletteItem {
            title: (*t).into(),
            subtitle: (*s).into(),
        })
        .collect();
    w.set_palette(std::rc::Rc::new(slint::VecModel::from(items)).into());
    w.set_palette_sel(sel);
    w.set_overlay_kind(1);
}

/// Clicking a palette row must select it *and then* run it. The order is the whole point:
/// `palette-activate` runs whatever `palette-sel` currently is, so activating before
/// picking would run the row the keyboard cursor happened to be on — the row above.
#[test]
fn clicking_a_palette_row_selects_it_before_running_it() {
    ui(|| {
        let w = window();
        install_palette(
            &w,
            &[
                ("New Pane", "Ctrl+T"),
                ("Show Diff", ""),
                ("Preferences", ""),
            ],
            0,
        );

        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        {
            let log = log.clone();
            w.on_palette_pick(move |i| log.borrow_mut().push(format!("pick {i}")));
        }
        {
            let log = log.clone();
            w.on_palette_activate(move || log.borrow_mut().push("activate".into()));
        }

        click(&w, &only(&w, "Show Diff", AccessibleRole::Button));
        assert_eq!(
            log.borrow().as_slice(),
            ["pick 1".to_string(), "activate".to_string()],
            "a palette row must pick itself, then activate"
        );
    });
}

/// The highlight is the keyboard cursor: it is the row Enter runs. Drawing it as a
/// background tint alone tells a screen reader nothing, and tells this test nothing either.
#[test]
fn the_palette_announces_which_row_is_selected() {
    ui(|| {
        let w = window();
        install_palette(&w, &[("New Pane", ""), ("Show Diff", "")], 1);

        assert_eq!(
            only(&w, "Show Diff", AccessibleRole::Button).accessible_checked(),
            Some(true),
            "the selected row must announce itself as the selected one"
        );
        assert_eq!(
            only(&w, "New Pane", AccessibleRole::Button).accessible_checked(),
            Some(false),
            "…and only that row"
        );
    });
}

/// An empty result set must say so rather than leaving a blank card, which reads as a hung
/// palette.
#[test]
fn an_empty_palette_says_so() {
    ui(|| {
        let w = window();
        install_palette(&w, &[], 0);
        assert!(
            !by_label(&w, "No matching commands").is_empty(),
            "a palette with no matches must show its empty state"
        );
    });
}

/// Put up the close confirmation. `final_close` is the last pane in the window, where the
/// close is irreversible and the "ask me" opt-out is withheld.
fn open_confirm_close(w: &crate::AppWindow, final_close: bool) {
    w.set_cc_title("bash — ~/code/hyperpanes".into());
    w.set_cc_final(final_close);
    w.set_cc_ask(true);
    w.set_overlay_kind(7);
}

/// The confirmation exists to stop an accidental close; a Close button that reached nothing
/// would strand the pane behind a card the user cannot get past.
#[test]
fn the_close_confirmation_confirms() {
    ui(|| {
        let w = window();
        open_confirm_close(&w, false);

        let fired = std::rc::Rc::new(std::cell::Cell::new(false));
        {
            let fired = fired.clone();
            w.on_confirm_close_go(move || fired.set(true));
        }

        click(&w, &only(&w, "Close", AccessibleRole::Button));
        assert!(fired.get(), "Close must reach confirm-close-go");
    });
}

/// The opt-out is a checkbox drawn from a Path and a caption, with no platform checkbox
/// anywhere in it — so its state exists only in what it announces.
#[test]
fn the_close_confirmation_opt_out_toggles_and_reaches_rust() {
    ui(|| {
        let w = window();
        open_confirm_close(&w, false);

        let asked = std::rc::Rc::new(std::cell::Cell::new(true));
        {
            let asked = asked.clone();
            w.on_set_confirm_close(move |on| asked.set(on));
        }

        let box_ = only(&w, "Ask before closing", AccessibleRole::Checkbox);
        assert_eq!(
            box_.accessible_checked(),
            Some(true),
            "the box must start ticked — that is the state it is drawn in"
        );
        click(&w, &box_);
        assert!(
            !asked.get(),
            "clicking a ticked box must ask Rust to switch it off"
        );
    });
}

/// Closing the last pane closes the window, and that one cannot be undone — so the card
/// changes its verb and withholds the "stop asking me" opt-out entirely.
#[test]
fn the_final_close_is_named_differently_and_offers_no_opt_out() {
    ui(|| {
        let w = window();
        open_confirm_close(&w, true);

        assert_eq!(
            by_role(&w, "Close Window", AccessibleRole::Button).len(),
            1,
            "the last-pane close must name the window it is closing"
        );
        assert!(
            by_role(&w, "Ask before closing", AccessibleRole::Checkbox).is_empty(),
            "the irreversible confirmation must not be silenceable"
        );
    });
}

/// The "Open link with…" chooser. Each row is a browser the OS reported; the index is the
/// only thing that gets back to Rust, so a row wired to the wrong one opens the wrong app.
#[test]
fn the_browser_chooser_returns_the_row_that_was_clicked() {
    ui(|| {
        let w = window();
        w.set_ask_url("https://example.com/a".into());
        let rows: Vec<crate::PrefBrowserRow> = [
            ("com.apple.Safari", "Safari"),
            ("org.mozilla.firefox", "Firefox"),
        ]
        .iter()
        .map(|(id, name)| crate::PrefBrowserRow {
            id: (*id).into(),
            name: (*name).into(),
            active: false,
        })
        .collect();
        w.set_ask_browsers(std::rc::Rc::new(slint::VecModel::from(rows)).into());
        w.set_overlay_kind(6);

        let picked = std::rc::Rc::new(std::cell::Cell::new(-1));
        {
            let picked = picked.clone();
            w.on_pick_browser(move |i| picked.set(i));
        }

        click(&w, &only(&w, "Firefox", AccessibleRole::Button));
        assert_eq!(picked.get(), 1, "the second row must report index 1");
    });
}

/// The Add-Project dialog, and the duplicate-label trap in one test: the card's heading and
/// its submit button are both "Add project", so a label-only search finds two elements and
/// only one of them can be pressed.
#[test]
fn the_add_project_dialog_submits() {
    ui(|| {
        let w = window();
        w.set_overlay_kind(4);

        assert_eq!(
            by_label(&w, "Add project").len(),
            3,
            "the heading, the button and the button's own caption all carry the phrase — \
             which is why the role, not the label, is what picks the pressable one"
        );

        let fired = std::rc::Rc::new(std::cell::Cell::new(false));
        {
            let fired = fired.clone();
            w.on_submit_add_project(move |_| fired.set(true));
        }

        click(&w, &only(&w, "Add project", AccessibleRole::Button));
        assert!(fired.get(), "the button must reach submit-add-project");
    });
}

/// An inline validation error must actually appear; a dialog that rejects a path silently
/// looks like a dead button.
#[test]
fn the_add_project_dialog_shows_its_error() {
    ui(|| {
        let w = window();
        w.set_ap_error("that folder does not exist".into());
        w.set_overlay_kind(4);
        assert!(
            !by_label(&w, "that folder does not exist").is_empty(),
            "ap-error must be rendered, not just held"
        );
    });
}

/// Seed the New-Pane dialog. Its colour row indexes `swatches` and its shell dropdown
/// indexes `shells`, so both must be non-empty for the card to be the card a user sees.
fn open_new_pane(w: &crate::AppWindow) {
    w.set_np_swatches(
        std::rc::Rc::new(slint::VecModel::from(vec![
            slint::Color::from_rgb_u8(0xe0, 0x60, 0x60),
            slint::Color::from_rgb_u8(0x60, 0xa0, 0xe0),
        ]))
        .into(),
    );
    w.set_np_shells(
        std::rc::Rc::new(slint::VecModel::from(vec![crate::PrefOption {
            id: 0,
            label: "zsh".into(),
            active: true,
        }]))
        .into(),
    );
    w.set_np_default_idx(0);
    w.set_overlay_kind(3);
}

/// The dialog is the only route to a pane with a chosen shell, command or colour; the ＋
/// beside it makes a default one. If Create reached nothing the whole card would be a form
/// that discards what you typed.
#[test]
fn the_new_pane_dialog_creates() {
    ui(|| {
        let w = window();
        open_new_pane(&w);

        let fired = std::rc::Rc::new(std::cell::Cell::new(false));
        {
            let fired = fired.clone();
            w.on_submit_new_pane(move |_, _, _, _, _, _, _| fired.set(true));
        }

        click(&w, &only(&w, "Create pane", AccessibleRole::Button));
        assert!(fired.get(), "Create pane must reach submit-new-pane");
    });
}

/// Show Frame / Show Dot are dialog-local state — nothing in Rust sees them until submit —
/// so the only observable proof the toggle works is the state it announces afterwards.
#[test]
fn the_new_pane_frame_toggle_flips() {
    ui(|| {
        let w = window();
        open_new_pane(&w);

        let before = only(&w, "Show Frame", AccessibleRole::Checkbox);
        assert_eq!(
            before.accessible_checked(),
            Some(false),
            "the card opens with no colour picked, so the frame starts off"
        );
        click(&w, &before);
        assert_eq!(
            only(&w, "Show Frame", AccessibleRole::Checkbox).accessible_checked(),
            Some(true),
            "clicking the toggle must switch it on"
        );
    });
}

/// The preferences rail. Seven panels behind seven items, and the panel you cannot reach is
/// the panel whose settings may as well not exist.
#[test]
fn the_preferences_rail_switches_panels() {
    ui(|| {
        let w = window();
        w.set_overlay_kind(2);

        for name in [
            "Appearance",
            "Terminal",
            "AI features",
            "Keybindings",
            "General",
            "Tools",
            "Browser",
        ] {
            assert_eq!(
                by_role(&w, name, AccessibleRole::Tab).len(),
                1,
                "the rail must offer {name}"
            );
        }

        assert_eq!(
            only(&w, "Appearance", AccessibleRole::Tab).accessible_checked(),
            Some(true),
            "the card opens on Appearance"
        );
        click(&w, &only(&w, "Keybindings", AccessibleRole::Tab));
        assert_eq!(
            only(&w, "Keybindings", AccessibleRole::Tab).accessible_checked(),
            Some(true),
            "clicking a rail item must select its panel"
        );
        assert_eq!(
            only(&w, "Appearance", AccessibleRole::Tab).accessible_checked(),
            Some(false),
            "…and deselect the one that was showing"
        );
    });
}

/// Done is what commits the appearance draft. A Done that reached nothing would look like
/// preferences that silently forget every change.
#[test]
fn the_preferences_done_button_reaches_rust() {
    ui(|| {
        let w = window();
        w.set_overlay_kind(2);

        let fired = std::rc::Rc::new(std::cell::Cell::new(false));
        {
            let fired = fired.clone();
            w.on_pref_done(move || fired.set(true));
        }

        click(&w, &only(&w, "Done", AccessibleRole::Button));
        assert!(fired.get(), "Done must reach pref-done");
    });
}

// ===========================================================================================
// Block 3 — the New-Goal box and the controls *inside* each preferences panel.
//
// Blocks 1 and 2 reached the frames: the icon strip, the dialogs' verbs, the preferences
// rail. What sat inside a panel was still anonymous — eleven `PrefToggle`s and four
// `FontDropdown`s carried the whole of Preferences with no name between them, because the
// caption is a sibling `Text` and the thing you press is a bare track or a bare rectangle.
// Same for the New-Goal box's category chips and its option lists.
// ===========================================================================================

/// Put up the New-Goal box with its option chips revealed, the way Ctrl+O does.
///
/// `field` is which category the keyboard is on: 0 is the free-text goal, 1-4 are the chips.
/// The chips must be non-empty or the row draws nothing — the box only shows categories it
/// has values for.
fn open_new_goal(w: &crate::AppWindow, field: i32) {
    w.set_overlay_kind(5);
    w.set_goal_options_open(true);
    w.set_goal_field(field);
    w.set_goal_chips(
        std::rc::Rc::new(slint::VecModel::from(vec![
            slint::SharedString::from("avada"),
            slint::SharedString::from("opus"),
            slint::SharedString::from("sonnet"),
            slint::SharedString::from("haiku"),
        ]))
        .into(),
    );
}

/// The New-Goal option list for whichever field is focused.
fn install_goal_menu(w: &crate::AppWindow, rows: &[(&str, &str)], sel: i32) {
    w.set_goal_menu(
        std::rc::Rc::new(slint::VecModel::from(
            rows.iter()
                .map(|(title, subtitle)| crate::PaletteItem {
                    title: (*title).into(),
                    subtitle: (*subtitle).into(),
                    ..Default::default()
                })
                .collect::<Vec<_>>(),
        ))
        .into(),
    );
    w.set_goal_menu_sel(sel);
    w.set_goal_menu_open(true);
}

/// A `PrefOption` list for a dropdown, with `active` on exactly one row.
fn pref_options(rows: &[&str], active: usize) -> slint::ModelRc<crate::PrefOption> {
    std::rc::Rc::new(slint::VecModel::from(
        rows.iter()
            .enumerate()
            .map(|(i, label)| crate::PrefOption {
                id: i as i32,
                label: (*label).into(),
                active: i == active,
            })
            .collect::<Vec<_>>(),
    ))
    .into()
}

/// The four category chips are a tab strip: each one names its category, reads out the value
/// it currently holds, and says whether it is the one the keyboard is on. Before this the row
/// was four unnamed rectangles, so neither a screen reader nor a test could tell "Orch" from
/// "Spec" — the only difference between them is the text drawn inside.
#[test]
fn the_goal_chips_are_a_tab_strip() {
    ui(|| {
        let w = window();
        open_new_goal(&w, 2);

        for (name, value) in [
            ("Project", "avada"),
            ("Orch", "opus"),
            ("Spec", "sonnet"),
            ("Impl", "haiku"),
        ] {
            let chip = only(&w, name, AccessibleRole::Tab);
            assert_eq!(
                chip.accessible_description().as_deref(),
                Some(value),
                "the {name} chip must read out the value it holds"
            );
        }

        assert_eq!(
            only(&w, "Orch", AccessibleRole::Tab).accessible_checked(),
            Some(true),
            "goal-field 2 is the Orch chip"
        );
        assert_eq!(
            only(&w, "Project", AccessibleRole::Tab).accessible_checked(),
            Some(false),
            "…and only that one"
        );
    });
}

/// Clicking a chip must ask Rust to focus *that* chip. `goal-field` is one-based over the
/// chips because 0 is the free-text field, so an off-by-one here silently moves the keyboard
/// to the neighbouring model.
#[test]
fn clicking_a_goal_chip_focuses_that_field() {
    ui(|| {
        let w = window();
        open_new_goal(&w, 0);

        let got = std::rc::Rc::new(std::cell::Cell::new(-1));
        {
            let got = got.clone();
            w.on_goal_field_click(move |i| got.set(i));
        }

        click(&w, &only(&w, "Spec", AccessibleRole::Tab));
        assert_eq!(got.get(), 3, "Spec is chip index 2, i.e. goal-field 3");
    });
}

/// The focused chip's option list. It must report the row that was clicked, and announce
/// which row the keyboard is on — the same list is drawn for history, projects and model
/// tiers, so an index that drifted would pick the wrong model with no visible symptom.
#[test]
fn a_goal_option_row_reports_its_own_index() {
    ui(|| {
        let w = window();
        open_new_goal(&w, 2);
        install_goal_menu(&w, &[("opus", "most capable"), ("sonnet", "faster")], 0);

        assert_eq!(
            only(&w, "sonnet", AccessibleRole::Button).accessible_checked(),
            Some(false),
            "row 1 is not the selected row"
        );
        assert_eq!(
            only(&w, "opus", AccessibleRole::Button).accessible_checked(),
            Some(true),
            "row 0 is"
        );

        let got = std::rc::Rc::new(std::cell::Cell::new(-1));
        {
            let got = got.clone();
            w.on_goal_menu_click(move |i| got.set(i));
        }
        click(&w, &only(&w, "sonnet", AccessibleRole::Button));
        assert_eq!(got.get(), 1, "the second row must report index 1");
    });
}

/// With `goal-field == 0` the same `goal-menu` is the *history* dropdown, drawn by a
/// different branch of the .slint. Two branches rendering one model is exactly where a fix
/// applied to one and not the other hides, so both are exercised.
#[test]
fn the_goal_history_dropdown_is_the_other_branch_of_the_same_list() {
    ui(|| {
        let w = window();
        open_new_goal(&w, 0);
        install_goal_menu(&w, &[("fix the diff button", "2 days ago")], 0);

        let got = std::rc::Rc::new(std::cell::Cell::new(-1));
        {
            let got = got.clone();
            w.on_goal_menu_click(move |i| got.set(i));
        }
        click(&w, &only(&w, "fix the diff button", AccessibleRole::Button));
        assert_eq!(got.get(), 0, "the history row must reach goal-menu-click");
    });
}

/// Each attachment's remove button is named after its file. With two images attached the
/// old shared tooltip produced two controls called "Remove this attachment": ambiguous to
/// read, ambiguous to click, and `assert_eq!(len, 1)` would have failed on both.
#[test]
fn each_attachment_names_the_file_it_removes() {
    ui(|| {
        let w = window();
        w.set_overlay_kind(5);
        w.set_goal_images(
            std::rc::Rc::new(slint::VecModel::from(vec![
                slint::SharedString::from("screenshot.png"),
                slint::SharedString::from("diagram.png"),
            ]))
            .into(),
        );

        let got = std::rc::Rc::new(std::cell::Cell::new(-1));
        {
            let got = got.clone();
            w.on_goal_remove_image(move |i| got.set(i));
        }

        click(
            &w,
            &only(&w, "Remove attachment diagram.png", AccessibleRole::Button),
        );
        assert_eq!(
            got.get(),
            1,
            "the second attachment's × must remove index 1"
        );
    });
}

/// The two switches that decide what a pane looks like. `pref-action` is a pair of ints —
/// kind, then argument — so a switch wired to the wrong kind would silently toggle a
/// different preference, which no per-function test can see.
#[test]
fn the_appearance_switches_reach_rust_with_their_own_kind() {
    ui(|| {
        let w = window();
        w.set_overlay_kind(2);
        w.set_pref_frame(true);
        w.set_pref_dot(false);

        let log: std::rc::Rc<std::cell::RefCell<Vec<(i32, i32)>>> = Default::default();
        {
            let log = log.clone();
            w.on_pref_action(move |kind, arg| log.borrow_mut().push((kind, arg)));
        }

        let frame = only(&w, "Pane frame border", AccessibleRole::Switch);
        assert_eq!(
            frame.accessible_checked(),
            Some(true),
            "the frame switch must show the state it was given"
        );
        click(&w, &frame);

        let dot = only(&w, "Pane color dot", AccessibleRole::Switch);
        assert_eq!(dot.accessible_checked(), Some(false));
        click(&w, &dot);

        assert_eq!(
            *log.borrow(),
            vec![(2, 0), (3, 1)],
            "frame is kind 2 and was on, so it asks for off; dot is kind 3 and was off"
        );
    });
}

/// Every switch in Preferences must be nameable — this is the one assertion that fails when
/// a new one is added without a label, which is how the last eleven got here unnamed.
#[test]
fn every_preferences_panel_names_its_switches() {
    ui(|| {
        let w = window();
        w.set_overlay_kind(2);
        w.set_pref_clickable(true);
        w.set_pref_idle_alert(true);

        // The selected panel is dialog-local state — there is no global to poke — so the
        // test walks the rail the way a user does, which is the more honest route anyway.
        let panels: [(&str, &[&str]); 4] = [
            ("Appearance", &["Pane frame border", "Pane color dot"]),
            ("Terminal", &["Copy on select", "Clickable file paths"]),
            ("AI features", &["Idle glow for AI panes"]),
            ("General", &["Ask before closing a pane or a tab"]),
        ];
        for (panel, switches) in panels {
            click(&w, &only(&w, panel, AccessibleRole::Tab));
            for name in switches {
                assert_eq!(
                    by_role(&w, name, AccessibleRole::Switch).len(),
                    1,
                    "the {panel} panel must offer a switch named {name:?}"
                );
            }
        }
    });
}

/// A dropdown is named by its caption and *valued* by its selection. Naming it after the
/// current font would announce what it holds instead of what it is — and would rename the
/// control every time the user changed it, so no test could address it twice.
#[test]
fn a_preferences_dropdown_is_named_by_its_caption_not_its_value() {
    ui(|| {
        let w = window();
        w.set_overlay_kind(2);
        w.set_pref_families(pref_options(&["Menlo", "JetBrains Mono"], 0));
        w.set_pref_font_label("Menlo".into());

        let dd = only(&w, "Terminal font", AccessibleRole::Combobox);
        assert_eq!(dd.accessible_value().as_deref(), Some("Menlo"));
        assert_eq!(
            dd.accessible_expanded(),
            Some(false),
            "it starts closed, and says so"
        );

        w.set_pref_font_label("JetBrains Mono".into());
        assert_eq!(
            only(&w, "Terminal font", AccessibleRole::Combobox)
                .accessible_value()
                .as_deref(),
            Some("JetBrains Mono"),
            "the value follows the selection; the name does not"
        );
    });
}

// ===========================================================================================
// The sidebar: the projects flyout, its rows, and the menu behind a right-click
// ===========================================================================================

/// Two projects in the sidebar with the flyout open, the way clicking the folder icon does.
/// The first has two worktrees; `history` decides whether it also has agent sessions, which
/// is what gates the Worktrees|History bar, and `segment` which of the two is showing.
fn install_projects(w: &crate::AppWindow, history: bool, segment: i32) {
    w.set_show_sidebar(true);
    w.set_sidebar_open(true);
    let wt = |branch: &str, path: &str, is_main: bool| crate::WorktreeRow {
        branch: branch.into(),
        path: path.into(),
        is_main,
        ..Default::default()
    };
    let worktrees = std::rc::Rc::new(slint::VecModel::from(vec![
        wt("main", "/code/hyperpanes", true),
        wt("wip", "/code/hyperpanes-wip", false),
    ]));
    let sessions = std::rc::Rc::new(slint::VecModel::from(if history {
        vec![crate::ClaudeSessionItem {
            id: "abc123".into(),
            source: "Claude".into(),
            summary: "the show-diff defect".into(),
            when: "2h ago".into(),
            count: 42,
        }]
    } else {
        vec![]
    }));
    let projects = std::rc::Rc::new(slint::VecModel::from(vec![
        crate::ProjectItem {
            name: "avada".into(),
            worktrees: worktrees.into(),
            sessions: sessions.into(),
            has_history: history,
            segment,
            ..Default::default()
        },
        crate::ProjectItem {
            name: "claude-standards".into(),
            ..Default::default()
        },
    ]));
    w.set_projects(projects.into());
}

/// Where project `index`'s right-click menu sits in window coordinates: the `ProjectRow`
/// component it hangs off, plus `ProjectMenu`'s declared `x`/`y` anchor.
fn project_menu_origin(w: &crate::AppWindow, index: usize) -> LogicalPosition {
    let rows: Vec<ElementHandle> =
        ElementHandle::find_by_element_type_name(w, "ProjectRow").collect();
    let p = rows[index].absolute_position();
    LogicalPosition::new(p.x + 10.0, p.y + 26.0)
}

/// A project is a row you can open and a row you can unfold, and it has to say so. Before
/// this the name was a child `Text` — auto-labelled, so the tree *looked* named while the
/// pressable row announced nothing and answered to no query.
#[test]
fn a_project_row_is_a_named_expandable_list_item() {
    ui(|| {
        let w = window();
        install_projects(&w, false, 0);

        let row = only(&w, "avada", AccessibleRole::ListItem);
        assert_eq!(row.accessible_expandable(), Some(true));
        assert_eq!(
            row.accessible_expanded(),
            Some(false),
            "it starts collapsed, and says so"
        );
        assert_eq!(
            by_role(&w, "claude-standards", AccessibleRole::ListItem).len(),
            1,
            "every project is a row, not just the first"
        );
    });
}

/// Opening a project is the sidebar's whole point, and the index it sends is the only thing
/// that decides *which* repo opens. An off-by-one here opens the neighbour, which looks like
/// a working feature until you have two projects.
#[test]
fn clicking_a_project_row_opens_that_project() {
    ui(|| {
        let w = window();
        install_projects(&w, false, 0);
        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::<i32>::new()));
        let seen = log.clone();
        w.on_open_project(move |i| seen.borrow_mut().push(i));

        click(&w, &only(&w, "claude-standards", AccessibleRole::ListItem));
        assert_eq!(
            *log.borrow(),
            vec![1],
            "the second row opens the second project"
        );
    });
}

/// The chevron is the only way to see a project's worktrees. It is also named per project
/// now — four chevrons called "Expand this project" are four controls nothing can tell
/// apart, for a screen reader and for this test alike.
#[test]
fn the_chevron_unfolds_that_project_and_nothing_else() {
    ui(|| {
        let w = window();
        install_projects(&w, false, 0);
        let trash = "Delete this worktree from disk";
        assert!(
            by_label(&w, trash).is_empty(),
            "a collapsed project shows none of its worktrees"
        );

        let tip = "Expand avada — worktrees and Claude history";
        click(&w, &only(&w, tip, AccessibleRole::Button));

        assert_eq!(
            only(&w, "avada", AccessibleRole::ListItem).accessible_expanded(),
            Some(true)
        );
        assert_eq!(
            by_label(&w, trash).len(),
            1,
            "the wip worktree can be deleted"
        );
        assert_eq!(
            by_label(&w, "The main checkout can't be removed").len(),
            1,
            "…and the main checkout says why it cannot"
        );
        assert_eq!(
            by_role(&w, "Collapse avada", AccessibleRole::Button).len(),
            1,
            "the chevron now offers the opposite gesture"
        );
        assert!(
            by_label(&w, "Expand claude-standards — worktrees and Claude history").len() == 1,
            "the other project stayed collapsed"
        );
    });
}

/// Worktrees|History is a two-tab strip. Both pills were unnamed rectangles whose only
/// distinguishing feature was the text drawn inside them, so nothing could say which one was
/// showing — and the segment a click reports is what the controller stores per project.
#[test]
fn the_worktrees_and_history_segments_are_a_tab_strip() {
    ui(|| {
        let w = window();
        install_projects(&w, true, 0);
        click(
            &w,
            &only(
                &w,
                "Expand avada — worktrees and Claude history",
                AccessibleRole::Button,
            ),
        );

        assert_eq!(
            only(&w, "Worktrees", AccessibleRole::Tab).accessible_checked(),
            Some(true)
        );
        assert_eq!(
            only(&w, "History", AccessibleRole::Tab).accessible_checked(),
            Some(false)
        );

        click(&w, &only(&w, "History", AccessibleRole::Tab));
        let ui_state = w.global::<crate::HistoryUi>();
        assert_eq!(
            (ui_state.get_proj(), ui_state.get_segment()),
            (0, 1),
            "the pick names the project it came from and the segment it chose"
        );
    });
}

/// The bar is only drawn for a project that has history at all — otherwise the expanded body
/// is just the worktrees. A bar that appeared with nothing behind it would be a dead control.
#[test]
fn a_project_without_history_gets_no_segmented_bar() {
    ui(|| {
        let w = window();
        install_projects(&w, false, 0);
        click(
            &w,
            &only(
                &w,
                "Expand avada — worktrees and Claude history",
                AccessibleRole::Button,
            ),
        );

        assert!(by_role(&w, "Worktrees", AccessibleRole::Tab).is_empty());
        assert!(by_role(&w, "History", AccessibleRole::Tab).is_empty());
        assert_eq!(
            by_label(&w, "Delete this worktree from disk").len(),
            1,
            "the worktrees show anyway — the bar-less default"
        );
    });
}

/// With the History segment showing, the worktrees give way to sessions. Two `if` branches
/// over one expanded body is exactly where a fix applied to one and not the other hides.
#[test]
fn the_history_segment_swaps_worktrees_for_sessions() {
    ui(|| {
        let w = window();
        install_projects(&w, true, 1);
        click(
            &w,
            &only(
                &w,
                "Expand avada — worktrees and Claude history",
                AccessibleRole::Button,
            ),
        );

        assert_eq!(
            only(&w, "History", AccessibleRole::Tab).accessible_checked(),
            Some(true)
        );
        assert!(
            by_label(&w, "Delete this worktree from disk").is_empty(),
            "the worktree rows are gone"
        );
        assert_eq!(
            by_label(&w, "Resume this Claude session in a new pane").len(),
            1,
            "…and the session is there to resume"
        );
    });
}

/// The right-click menu. Eight 18px squares that differ only by hue are eight identical
/// controls to anything that cannot see them, and the recolour index they send is what picks
/// the colour — so this checks the name AND that the name maps to the right index.
#[test]
fn the_project_menu_names_every_colour_swatch() {
    ui(|| {
        let w = window();
        install_projects(&w, false, 0);
        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::<(i32, i32)>::new()));
        let seen = log.clone();
        w.on_recolor_project(move |i, s| seen.borrow_mut().push((i, s)));

        right_click(&w, &only(&w, "avada", AccessibleRole::ListItem));

        for name in [
            "Red", "Orange", "Green", "Blue", "Purple", "Pink", "Teal", "Yellow",
        ] {
            assert_eq!(
                by_role(&w, name, AccessibleRole::Button).len(),
                1,
                "the {name} swatch names itself"
            );
        }

        click_in_popup(
            &w,
            project_menu_origin(&w, 0),
            &only(&w, "Blue", AccessibleRole::Button),
        );
        assert_eq!(
            *log.borrow(),
            vec![(0, 3)],
            "Blue is palette slot 3 of project 0"
        );
    });
}

/// The destructive row in that menu. A bare "Remove" is one of several in this app; the one
/// that drops a project should say which project.
#[test]
fn the_project_menu_removes_the_project_it_names() {
    ui(|| {
        let w = window();
        install_projects(&w, false, 0);
        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::<i32>::new()));
        let seen = log.clone();
        w.on_remove_project(move |i| seen.borrow_mut().push(i));

        right_click(&w, &only(&w, "claude-standards", AccessibleRole::ListItem));
        assert!(
            by_role(&w, "Remove project avada", AccessibleRole::Button).is_empty(),
            "the menu belongs to the row that opened it"
        );

        click_in_popup(
            &w,
            project_menu_origin(&w, 1),
            &only(
                &w,
                "Remove project claude-standards",
                AccessibleRole::Button,
            ),
        );
        assert_eq!(*log.borrow(), vec![1]);
    });
}

// ===== the view panes =====
//
// The Family B pane body: the file browser, the file viewer and the markdown preview,
// all three rendered by one `ViewRowView` per row. Every gesture this pane has — open a
// file, walk into a directory, select a range of lines — arrives through that row, and
// until now the row was anonymous: the name was drawn by a child `Text`, which Slint
// auto-labels, so the surface looked named while the element carrying the `TouchArea`
// said nothing.

/// A single view pane, laid out large enough that its `ListView` really instantiates the
/// rows (it only builds the visible ones, so a pane too short to show row 3 is a pane
/// where row 3 cannot be found — which would be a test artefact, not a defect).
///
/// `kind` 2 is the file browser, 3 the viewer, 4 the markdown preview; the rows decide
/// what is actually drawn, so this only has to be in Family B's range.
fn install_view_pane(
    w: &crate::AppWindow,
    kind: i32,
    title: &str,
    rows: Vec<crate::PaneViewRow>,
    sel: (i32, i32),
    toast: &str,
) {
    install_view_pane_at(
        w,
        kind,
        title,
        rows,
        sel,
        toast,
        crate::prefs::DEFAULT_FONT_PX,
    );
}

/// The same pane at a chosen font size — what `Ctrl/Cmd+=` and `+-` move through
/// `State::font_zoom`, and what a workspace file persists as `fontSize`. Every pane has
/// always carried it; only the terminal half used to read it.
fn install_view_pane_at(
    w: &crate::AppWindow,
    kind: i32,
    title: &str,
    rows: Vec<crate::PaneViewRow>,
    sel: (i32, i32),
    toast: &str,
    font_px: f32,
) {
    w.set_panes(
        std::rc::Rc::new(slint::VecModel::from(vec![crate::PaneItem {
            title: "the pane".into(),
            x: 8.0,
            y: 40.0,
            w: 600.0,
            h: 500.0,
            visible: true,
            focused: true,
            kind,
            is_view: view_flag(kind),
            view_title: title.into(),
            view_rows: std::rc::Rc::new(slint::VecModel::from(rows)).into(),
            view_sel_lo: sel.0,
            view_sel_hi: sel.1,
            toast: toast.into(),
            font_px,
            ..Default::default()
        }]))
        .into(),
    );
}

/// The `is-view` flag `paneview::pane_item` sends beside `kind`. Two projections of one
/// enum, so they can disagree; the projection is the thing under test, which is why the
/// tests set both rather than letting the helper infer a view from the row roles.
/// [`the_is_view_flag_matches_the_kind_it_claims`] is what keeps this list honest.
fn view_flag(kind: i32) -> bool {
    matches!(kind, 2 | 3 | 4 | 6 | 7 | 8 | 9)
}

/// `PaneItem::is-view` replaced a `kind >= 2 && kind <= 4` range test in the `.slint`, and
/// that range was already wrong — `Browser` sat at 5 only by luck of ordering. This walks
/// the real enum so the flag and the kind can never drift apart again.
#[test]
fn the_is_view_flag_matches_the_kind_it_claims() {
    use avada_core::tools::PaneKind;

    // An exhaustive match over the enum: a variant added without a line in `all` below
    // fails to compile here rather than quietly skipping the assertion.
    fn _covered(k: &PaneKind) -> i32 {
        match k {
            PaneKind::Terminal => 0,
            PaneKind::Tool(_) => 1,
            PaneKind::FileBrowser => 2,
            PaneKind::FileViewer => 3,
            PaneKind::Markdown => 4,
            PaneKind::Browser => 5,
            PaneKind::Code => 6,
            PaneKind::Data => 7,
            PaneKind::Table => 8,
            PaneKind::Image => 9,
            PaneKind::Module(_) => 10,
        }
    }
    let all = [
        PaneKind::Terminal,
        PaneKind::Tool("claude".into()),
        PaneKind::FileBrowser,
        PaneKind::FileViewer,
        PaneKind::Markdown,
        PaneKind::Browser,
        PaneKind::Code,
        PaneKind::Data,
        PaneKind::Table,
        PaneKind::Image,
        PaneKind::from_meta_value("module:acme/avada-files#tree"),
    ];
    for k in all {
        assert_eq!(
            view_flag(k.ui_kind()),
            k.is_view(),
            "{k:?} draws as the wrong family"
        );
    }
}

/// A listing row: `role` 1 is a directory, 2 a file, and `activatable` is decided
/// Rust-side rather than re-derived from the role.
fn listing(role: i32, text: &str, detail: &str, activatable: bool) -> crate::PaneViewRow {
    crate::PaneViewRow {
        role,
        text: text.into(),
        detail: detail.into(),
        activatable,
        ..Default::default()
    }
}

/// A verbatim line of a file: `detail` is the line number the viewer prints in its gutter.
fn line(n: i32, text: &str) -> crate::PaneViewRow {
    crate::PaneViewRow {
        role: 3,
        text: text.into(),
        detail: n.to_string().into(),
        activatable: false,
        ..Default::default()
    }
}

/// The row is a list item with a name, and the size/age column is a description rather
/// than part of that name — otherwise every file in the browser would be called something
/// no caller could predict and no reader could index.
#[test]
fn a_listing_row_is_a_named_selectable_list_item() {
    ui(|| {
        let w = window();
        install_view_pane(
            &w,
            2,
            "code/avada",
            vec![
                listing(1, "src", "", true),
                listing(2, "README.md", "2.1 kB · 3d", false),
            ],
            (-1, -1),
            "",
        );

        let file = only(&w, "README.md", AccessibleRole::ListItem);
        assert_eq!(
            file.accessible_description().as_deref(),
            Some("2.1 kB · 3d"),
            "the trailing column elaborates the name; it is not part of it"
        );
        assert_eq!(
            file.accessible_item_selectable(),
            Some(true),
            "every row is a candidate for the range selection"
        );
        assert_eq!(file.accessible_item_selected(), Some(false));
        // The directory is a row of its own, addressable by the name it draws.
        only(&w, "src", AccessibleRole::ListItem);
    });
}

/// Opening the wrong file is the same class of defect as closing the wrong pane: the
/// callback carries a row index, so "it opens" and "it opens the row you clicked" are
/// different claims.
#[test]
fn clicking_a_listing_row_opens_the_row_it_names() {
    ui(|| {
        let w = window();
        install_view_pane(
            &w,
            2,
            "code/avada",
            vec![
                listing(1, "src", "", true),
                listing(1, "scripts", "", true),
                listing(2, "README.md", "2.1 kB · 3d", false),
            ],
            (-1, -1),
            "",
        );

        let saw = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        {
            let saw = saw.clone();
            w.on_pane_view_activate(move |pane, row| saw.borrow_mut().push((pane, row)));
        }

        click(&w, &only(&w, "scripts", AccessibleRole::ListItem));
        assert_eq!(
            *saw.borrow(),
            vec![(0, 1)],
            "the second row of the first pane, not merely some row of some pane"
        );
    });
}

/// A line of a file cannot be opened, so a plain click on it selects instead. Same row,
/// same gesture, different verb — decided by `activatable`, which is why the row has to
/// be the thing that reports it.
#[test]
fn clicking_an_inert_line_selects_it_rather_than_opening_it() {
    ui(|| {
        let w = window();
        install_view_pane(
            &w,
            3,
            "src/main.rs",
            vec![line(1, "fn main() {"), line(2, "    run();"), line(3, "}")],
            (-1, -1),
            "",
        );

        let opened = std::rc::Rc::new(std::cell::Cell::new(false));
        let picked = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        {
            let opened = opened.clone();
            w.on_pane_view_activate(move |_, _| opened.set(true));
            let picked = picked.clone();
            w.on_pane_view_select(move |pane, row, ext| picked.borrow_mut().push((pane, row, ext)));
        }

        click(
            &w,
            &only(&w, "Line 2:     run();", AccessibleRole::ListItem),
        );
        assert_eq!(
            *picked.borrow(),
            vec![(0, 1, false)],
            "a plain click selects"
        );
        assert!(
            !opened.get(),
            "an inert row must not claim to open anything"
        );
    });
}

/// Two identical lines of a file are two different rows. The viewer already prints the
/// number that tells them apart, so the name says it too — otherwise the second `foo` is
/// unreachable and the first one answers for both.
#[test]
fn the_viewer_numbers_its_lines_so_two_identical_ones_stay_distinct() {
    ui(|| {
        let w = window();
        install_view_pane(
            &w,
            3,
            "src/main.rs",
            vec![line(1, "    }"), line(2, "}"), line(3, "    }")],
            (-1, -1),
            "",
        );

        let picked = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        {
            let picked = picked.clone();
            w.on_pane_view_select(move |_, row, _| picked.borrow_mut().push(row));
        }

        // Both exist, and each resolves to exactly one row.
        click(&w, &only(&w, "Line 1:     }", AccessibleRole::ListItem));
        click(&w, &only(&w, "Line 3:     }", AccessibleRole::ListItem));
        assert_eq!(
            *picked.borrow(),
            vec![0, 2],
            "identical text, different rows — the number is what separates them"
        );
    });
}

/// The selection is a range decided Rust-side and painted here. A highlight is invisible
/// to anything that cannot see, so the rows in the range have to say they are in it.
#[test]
fn the_selected_range_says_which_rows_are_in_it() {
    ui(|| {
        let w = window();
        install_view_pane(
            &w,
            3,
            "src/main.rs",
            vec![
                line(1, "one"),
                line(2, "two"),
                line(3, "three"),
                line(4, "four"),
            ],
            (1, 2),
            "",
        );

        let in_range: Vec<bool> = ["one", "two", "three", "four"]
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let row = only(
                    &w,
                    &format!("Line {}: {t}", i + 1),
                    AccessibleRole::ListItem,
                );
                row.accessible_item_selected() == Some(true)
            })
            .collect();
        assert_eq!(
            in_range,
            vec![false, true, true, false],
            "rows 1 and 2 inclusive, matching sel-lo/sel-hi"
        );
    });
}

/// The pane's transient confirmation. It appears and fades without focus ever moving to
/// it, so a name alone reaches nobody — it has to declare itself live, or a copy from a
/// view pane confirms itself only to people who can see the corner it appears in.
#[test]
fn the_pane_toast_announces_itself_when_it_appears() {
    ui(|| {
        let w = window();
        install_view_pane(
            &w,
            3,
            "src/main.rs",
            vec![line(1, "one")],
            (-1, -1),
            "Copied 12 lines",
        );

        let toast = only(&w, "Copied 12 lines", AccessibleRole::Text);
        assert_eq!(
            toast.accessible_live_region(),
            Some(i_slint_backend_testing::AccessibleLiveness::Polite),
            "a message nothing focuses has to interrupt on its own or not at all"
        );
    });
}

/// No toast, no toast element — an empty confirmation left in the tree would be read out
/// as an empty announcement every time the pane redrew.
#[test]
fn a_quiet_pane_shows_no_toast_at_all() {
    ui(|| {
        let w = window();
        install_view_pane(&w, 3, "src/main.rs", vec![line(1, "one")], (-1, -1), "");
        assert!(
            by_label(&w, "Copied 12 lines").is_empty(),
            "the toast is conditional on having something to say"
        );
    });
}

/// A markdown preview's rows are prose, not filenames, and prose keeps its raw source in
/// `text` beside the parsed `md` — which is what lets one binding name every role. The
/// rows that draw no text of their own say what they are instead of nothing.
#[test]
fn the_markdown_preview_names_its_blocks_including_the_wordless_ones() {
    ui(|| {
        let w = window();
        let md = |role: i32, text: &str| crate::PaneViewRow {
            role,
            text: text.into(),
            activatable: false,
            ..Default::default()
        };
        install_view_pane(
            &w,
            4,
            "README.md",
            vec![
                md(4, "Avada"),
                md(16, "A terminal multiplexer."),
                md(10, ""),
                md(12, ""),
            ],
            (-1, -1),
            "",
        );

        only(&w, "Avada", AccessibleRole::ListItem);
        only(&w, "A terminal multiplexer.", AccessibleRole::ListItem);
        only(&w, "Horizontal rule", AccessibleRole::ListItem);
        only(&w, "Diagram", AccessibleRole::ListItem);
    });
}

/// The breadcrumb says which file or folder the pane is showing. The pane header above it
/// carries the pane's own label, so this line is the target, and the two must not be
/// confused for one another.
#[test]
fn the_breadcrumb_names_the_target_not_the_pane() {
    ui(|| {
        let w = window();
        install_view_pane(&w, 3, "src/main.rs", vec![line(1, "one")], (-1, -1), "");

        only(&w, "src/main.rs", AccessibleRole::Text);
        // …and it is not the pane's own title, which the header draws separately.
        assert!(!by_label(&w, "the pane").is_empty());
    });
}

// ===== zoom in a view pane =====
//
// `Cmd/Ctrl+=`, `+-` and `+0` were always reaching Rust: the chord resolves
// (`keybindings::font_zoom_chords_resolve`), `State::font_zoom` moves the focused pane's
// `font_px` with no pane-kind gate, the toast flashed the new percentage, and a workspace
// file already persisted it as `fontSize`. The one missing hop was here — `ViewPane` never
// read `PaneItem::font-px`, so in a file browser, viewer or markdown preview the chord
// fired, the corner said "114%", and nothing on screen moved. That is precisely the class
// of defect no per-function test can see: every link type-checked, and the feature was
// still dead on the screen the user was looking at.

/// The height of the row that draws `label`, in logical px.
fn row_height(w: &crate::AppWindow, label: &str) -> f32 {
    only(w, label, AccessibleRole::ListItem).size().height
}

/// One listing row, measured at three font sizes. `19px` is the fixed row height the view
/// draws at the default; the assertion is proportionality rather than a magic number, so
/// re-tuning the row does not break the test — only losing the zoom does.
#[test]
fn zoom_grows_and_shrinks_a_view_pane_row() {
    ui(|| {
        let w = window();
        let rows = || vec![listing(2, "README.md", "2.1 kB · 3d", false)];

        install_view_pane_at(&w, 2, "code", rows(), (-1, -1), "", 14.0);
        let base = row_height(&w, "README.md");

        install_view_pane_at(&w, 2, "code", rows(), (-1, -1), "", 28.0);
        let doubled = row_height(&w, "README.md");

        install_view_pane_at(&w, 2, "code", rows(), (-1, -1), "", 8.0);
        let smallest = row_height(&w, "README.md");

        assert!(base > 0.0, "the base row has to exist before it can grow");
        assert!(
            (doubled - base * 2.0).abs() < 0.5,
            "twice the font is twice the row: {base} → {doubled}"
        );
        assert!(
            smallest < base,
            "the minimum font size has to shrink the row: {base} → {smallest}"
        );
    });
}

/// The chord says "any pane", so all three Family B kinds have to answer it — the browser
/// listing, the viewer's verbatim lines and the preview's prose each measure themselves a
/// different way, and only one of the three shares a code path with the others.
#[test]
fn every_view_kind_answers_the_zoom_chord() {
    ui(|| {
        let w = window();
        let prose = crate::PaneViewRow {
            role: 16,
            text: "A terminal multiplexer.".into(),
            ..Default::default()
        };
        let cases: [(i32, crate::PaneViewRow, &str); 3] = [
            (2, listing(2, "README.md", "", false), "README.md"),
            (3, line(1, "fn main() {"), "Line 1: fn main() {"),
            (4, prose, "A terminal multiplexer."),
        ];

        for (kind, row, label) in cases {
            install_view_pane_at(&w, kind, "t", vec![row.clone()], (-1, -1), "", 10.0);
            let small = row_height(&w, label);
            install_view_pane_at(&w, kind, "t", vec![row], (-1, -1), "", 24.0);
            let large = row_height(&w, label);
            assert!(
                large > small,
                "pane kind {kind} ignored the zoom: {small} → {large}"
            );
        }
    });
}

/// Chrome does not scale. A browser's Cmd+ grows the page and leaves the toolbar alone;
/// the breadcrumb that says which file this is, and the toast that says what just
/// happened, are this pane's toolbar.
#[test]
fn the_pane_chrome_holds_its_size_while_the_content_zooms() {
    ui(|| {
        let w = window();
        let rows = || vec![listing(2, "README.md", "", false)];

        install_view_pane_at(
            &w,
            2,
            "src/main.rs",
            rows(),
            (-1, -1),
            "Copied 12 lines",
            14.0,
        );
        let crumb = only(&w, "src/main.rs", AccessibleRole::Text).size();
        let toast = only(&w, "Copied 12 lines", AccessibleRole::Text).size();

        install_view_pane_at(
            &w,
            2,
            "src/main.rs",
            rows(),
            (-1, -1),
            "Copied 12 lines",
            28.0,
        );
        assert_eq!(
            only(&w, "src/main.rs", AccessibleRole::Text).size().height,
            crumb.height,
            "the breadcrumb is chrome, not content"
        );
        assert_eq!(
            only(&w, "Copied 12 lines", AccessibleRole::Text)
                .size()
                .height,
            toast.height,
            "so is the toast"
        );
    });
}

/// A mermaid diagram is laid out in Rust (`src/mermaid.rs`) and arrives here already
/// measured, so scaling the frame around it would crop the drawing rather than magnify it.
/// It keeps its own size deliberately, and the prose around it grows past it — a known
/// limit, pinned here so it stays a decision rather than becoming a regression.
#[test]
fn a_diagram_keeps_the_size_rust_measured_for_it() {
    ui(|| {
        let w = window();
        let diagram = || crate::PaneViewRow {
            role: 12,
            diagram: crate::PaneDiagram {
                w: 400.0,
                h: 120.0,
                ..Default::default()
            },
            ..Default::default()
        };

        install_view_pane_at(&w, 4, "README.md", vec![diagram()], (-1, -1), "", 14.0);
        let base = row_height(&w, "Diagram");
        install_view_pane_at(&w, 4, "README.md", vec![diagram()], (-1, -1), "", 28.0);
        assert_eq!(
            row_height(&w, "Diagram"),
            base,
            "the box was measured before it got here; growing it alone would only crop it"
        );
    });
}

/// A pane whose size was never set reports 0, not the property's default — the default
/// only applies where nothing binds it at all. Read literally that would divide the whole
/// view down to nothing, which is a blank pane rather than a small one.
#[test]
fn an_unsized_pane_renders_unzoomed_rather_than_invisible() {
    ui(|| {
        let w = window();
        install_view_pane_at(
            &w,
            2,
            "code",
            vec![listing(2, "README.md", "", false)],
            (-1, -1),
            "",
            0.0,
        );
        assert!(
            row_height(&w, "README.md") > 0.0,
            "an out-of-range font size means unzoomed, not collapsed"
        );
    });
}

// ===== the source viewer (role 17) =====
//
// A `.rs`/`.ts`/`.py` file opens in `PaneKind::Code`, whose rows carry both the raw line
// (`text`, for copy and the accessible label) and the same line as markdown markup
// (`md`, built by `src/highlight.rs`, carrying a `<font color>` per token). The row must
// stay exactly as tall as a plain viewer row — row N is line N — so the coloured text
// cannot be measured by the row's height the way prose is. It is reached by element id
// instead: `StyledText` is not auto-labelled the way `Text` is.

/// A coloured source line. `markup` is what the highlighter emits; `text` stays verbatim.
fn source(n: i32, text: &str, markup: &str) -> crate::PaneViewRow {
    crate::PaneViewRow {
        role: 17,
        text: text.into(),
        detail: n.to_string().into(),
        md: slint::StyledText::from_markdown(markup)
            .expect("the highlighter's markup has to parse, or the colour is silently lost"),
        ..Default::default()
    }
}

/// The `StyledText` a source row draws its line in. Empty on every other role.
fn source_texts(w: &crate::AppWindow) -> Vec<ElementHandle> {
    ElementHandle::find_by_element_id(w, "ViewRowView::src").collect()
}

/// Role 17 used to fall through `role > 16` into the plain-line branch, which draws
/// `text` in a `Text` — the file would have opened with every colour thrown away and no
/// error anywhere. The two branches are distinguishable because only one of them builds
/// the named `StyledText`.
#[test]
fn a_source_row_draws_the_coloured_line_and_a_plain_one_does_not() {
    ui(|| {
        let w = window();
        let row = || source(1, "fn main() {", "<font color=\"#89b4fa\">fn</font> main");

        install_view_pane_at(&w, 6, "src/main.rs", vec![row()], (-1, -1), "", 14.0);
        assert_eq!(
            source_texts(&w).len(),
            1,
            "a source row has to draw its markup, not its plain text"
        );

        install_view_pane_at(
            &w,
            3,
            "a.log",
            vec![line(1, "fn main() {")],
            (-1, -1),
            "",
            14.0,
        );
        assert!(
            source_texts(&w).is_empty(),
            "a plain viewer line must not reach the source branch"
        );
    });
}

/// Both branches keep the line number in the gutter, and both announce the raw line —
/// the colours are decoration, and a screen reader must not read markup at anyone.
#[test]
fn a_source_row_announces_the_line_it_shows() {
    ui(|| {
        let w = window();
        install_view_pane_at(
            &w,
            6,
            "src/main.rs",
            vec![source(
                42,
                "let x = 1;",
                "<font color=\"#89b4fa\">let</font> x",
            )],
            (-1, -1),
            "",
            14.0,
        );
        only(&w, "Line 42: let x = 1;", AccessibleRole::ListItem);
        only(&w, "42", AccessibleRole::Text);
    });
}

/// The defect `d3808d0` shipped with. Every length in the plain-line branch was a bare
/// literal while the row height was `19px * zoom`, so `Cmd+=` grew the spacing between
/// lines and left the glyphs exactly where they were. The five zoom tests above all
/// passed, because every one of them measured the *row*.
///
/// Measured here as geometry that can only move if the font moved: the gutter's own
/// width, and the gap between the gutter and the text (its width plus the spacing).
#[test]
fn zoom_reaches_a_plain_line_and_not_just_its_row() {
    ui(|| {
        let w = window();
        let rows = || vec![line(1, "fn main() {")];
        let measure = |px: f32| {
            install_view_pane_at(&w, 3, "a.log", rows(), (-1, -1), "", px);
            let gutter = only(&w, "1", AccessibleRole::Text);
            let body = only(&w, "fn main() {", AccessibleRole::Text);
            (
                gutter.size().width,
                body.absolute_position().x - gutter.absolute_position().x,
            )
        };

        let (gutter, gap) = measure(14.0);
        let (gutter2, gap2) = measure(28.0);
        assert!(gutter > 0.0 && gap > 0.0, "the row has to exist first");
        assert!(
            (gutter2 - gutter * 2.0).abs() < 0.5,
            "the line-number gutter ignored the zoom: {gutter} → {gutter2}"
        );
        assert!(
            (gap2 - gap * 2.0).abs() < 0.5,
            "the gutter's width and the spacing after it ignored the zoom: {gap} → {gap2}"
        );
    });
}

/// The same chord on a source pane, where the coloured text *is* measurable: the
/// `StyledText` takes its natural width (a spacer eats the rest of the row), so its
/// width is the glyphs' width and nothing else.
#[test]
fn zoom_reaches_the_glyphs_of_a_source_line() {
    ui(|| {
        let w = window();
        let rows = || vec![source(1, "fn main() {", "fn main")];
        let width = |px: f32| {
            install_view_pane_at(&w, 6, "src/main.rs", rows(), (-1, -1), "", px);
            let found = source_texts(&w);
            assert_eq!(found.len(), 1, "one source row, one coloured line");
            found[0].size().width
        };

        let base = width(14.0);
        let doubled = width(28.0);
        assert!(base > 0.0, "an empty measurement proves nothing");
        assert!(
            (doubled - base * 2.0).abs() < 2.0,
            "twice the font is twice the line: {base} → {doubled}"
        );
    });
}

/// Source has to be monospaced or every alignment in the file is a lie. `ui/viewpanes.slint`
/// imports the bundled JetBrains Mono so `"JetBrains Mono"` resolves as a family name;
/// if that import were dropped the family would silently fall back to the platform's
/// proportional default, which is exactly the kind of quiet regression this asserts away:
/// in a proportional face `iiiiiiiiii` is far narrower than `MMMMMMMMMM`.
#[test]
fn a_source_line_is_drawn_in_a_monospaced_face() {
    ui(|| {
        let w = window();
        let width = |s: &str| {
            install_view_pane_at(
                &w,
                6,
                "src/main.rs",
                vec![source(1, s, s)],
                (-1, -1),
                "",
                14.0,
            );
            source_texts(&w)[0].size().width
        };
        let narrow = width("iiiiiiiiii");
        let wide = width("MMMMMMMMMM");
        assert!(narrow > 0.0, "the line has to be drawn to be measured");
        assert!(
            (wide - narrow).abs() < 1.0,
            "ten narrow glyphs and ten wide ones must occupy the same width: \
             {narrow} vs {wide} — the monospace family did not resolve"
        );
    });
}
