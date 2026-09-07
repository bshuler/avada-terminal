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
    w.window()
        .set_size(slint::PhysicalSize::new(1280, 800));
    w
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
    let at = LogicalPosition::new(
        pos.x + size.width / 2.0,
        pos.y + size.height / 2.0,
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

/// Put the left panel into the git mode's commit view with one file, the way clicking a
/// hash in a pane's output does.
fn open_commit_view(w: &crate::AppWindow) {
    let lp = w.global::<crate::LeftPanelAdapter>();
    lp.set_open(true);
    lp.set_mode(crate::paneview::LEFT_MODE_GIT);
    lp.set_git_repo(true);
    lp.set_git_commit_open(true);
    lp.set_git_commit_title("the subject".into());
    lp.set_git_commit_meta("abc1234 · T · today".into());
    lp.set_git_commit_files_title("Files · 1".into());
    let files = std::rc::Rc::new(slint::VecModel::from(vec![crate::LeftGitRow {
        path: "sub/a.txt".into(),
        label: "a.txt".into(),
        detail: "sub".into(),
        code: "M".into(),
        selected: false,
    }]));
    lp.set_git_commit_files(files.into());
}

/// Put the left panel into the git mode's working-tree view. `dirty` decides whether git
/// reported anything — a clean tree and a dirty one draw different headers.
fn open_working_tree(w: &crate::AppWindow, dirty: bool) {
    let lp = w.global::<crate::LeftPanelAdapter>();
    lp.set_open(true);
    lp.set_mode(crate::paneview::LEFT_MODE_GIT);
    lp.set_git_repo(true);
    lp.set_git_commit_open(false);
    lp.set_git_head("main".into());
    let rows = if dirty {
        vec![crate::LeftGitRow {
            path: "sub/a.txt".into(),
            label: "a.txt".into(),
            detail: "sub".into(),
            code: "M".into(),
            selected: false,
        }]
    } else {
        Vec::new()
    };
    lp.set_git_changed(std::rc::Rc::new(slint::VecModel::from(rows)).into());
}

/// THE regression this suite exists for: the commit header's diff button must be present,
/// laid out, and hit-testable, and clicking it must reach Rust.
#[test]
fn the_commit_diff_button_is_clickable_and_reaches_rust() {
    ui(|| {
        let w = window();
        open_commit_view(&w);

        let fired = std::rc::Rc::new(std::cell::Cell::new(false));
        {
            let fired = fired.clone();
            w.global::<crate::LeftPanelAdapter>()
                .on_git_commit_diff(move || fired.set(true));
        }

        let found = by_label(&w, "Open a pane with this commit's whole diff");
        assert_eq!(
            found.len(),
            1,
            "the commit header must show exactly one diff button"
        );
        click(&w, &found[0]);
        assert!(
            fired.get(),
            "clicking the diff button must reach LeftPanelAdapter.git-commit-diff"
        );
    });
}

/// Its neighbour, so a failure above can be read as "the diff button specifically" rather
/// than "the header is broken".
#[test]
fn the_commit_back_button_is_clickable_and_reaches_rust() {
    ui(|| {
        let w = window();
        open_commit_view(&w);

        let fired = std::rc::Rc::new(std::cell::Cell::new(false));
        {
            let fired = fired.clone();
            w.global::<crate::LeftPanelAdapter>()
                .on_git_commit_close(move || fired.set(true));
        }

        let found = by_label(&w, "Back to the working tree");
        assert_eq!(found.len(), 1, "the commit header must show a back button");
        click(&w, &found[0]);
        assert!(fired.get(), "clicking back must reach git-commit-close");
    });
}

/// The working tree and the commit are two views of one mode, and the *commit* diff button
/// belongs to the commit only. If it leaked into the working-tree view it would dispatch
/// `GitCommitDiff` with no commit loaded and silently do nothing.
#[test]
fn the_working_tree_view_shows_no_commit_diff_button() {
    ui(|| {
        let w = window();
        open_working_tree(&w, true);
        assert!(
            by_label(&w, "Open a pane with this commit's whole diff").is_empty(),
            "the working-tree view must not offer a commit diff"
        );
    });
}

/// The bug the user reported, at the layer they hit it: they opened the git panel on a
/// dirty tree and there was no diff to press. Before `git-diff` existed this found nothing.
#[test]
fn the_working_tree_diff_button_is_clickable_and_reaches_rust() {
    ui(|| {
        let w = window();
        open_working_tree(&w, true);

        let fired = std::rc::Rc::new(std::cell::Cell::new(false));
        {
            let fired = fired.clone();
            w.global::<crate::LeftPanelAdapter>()
                .on_git_diff(move || fired.set(true));
        }

        let found = by_label(&w, "Open a pane with the working tree's whole diff");
        assert_eq!(
            found.len(),
            1,
            "a dirty working tree must show exactly one diff button"
        );
        click(&w, &found[0]);
        assert!(
            fired.get(),
            "clicking the working-tree diff button must reach LeftPanelAdapter.git-diff"
        );
    });
}

/// `git diff HEAD` on a clean tree opens a pane that prints nothing, which reads as the
/// button being broken. So the button is only there when there is something to show.
#[test]
fn a_clean_working_tree_shows_no_diff_button() {
    ui(|| {
        let w = window();
        open_working_tree(&w, false);
        assert!(
            by_label(&w, "Open a pane with the working tree's whole diff").is_empty(),
            "a clean tree must not offer a diff"
        );
    });
}

// ===== the mode strip =====
//
// Every left-panel feature is reached through this strip, so a strip that does not switch
// makes every mode below it unreachable. It is drawn as three glyph buttons with no text.

/// Publish the three built-in modes the way `paneview` does on every resync.
fn install_modes(w: &crate::AppWindow) {
    let rows = vec![
        crate::LeftModeRow { label: "Workspace".into(), icon: 0, brand: slint::Color::from_rgb_u8(0, 0, 0) },
        crate::LeftModeRow { label: "Files".into(), icon: -1, brand: slint::Color::from_rgb_u8(0, 0, 0) },
        crate::LeftModeRow { label: "Git".into(), icon: -2, brand: slint::Color::from_rgb_u8(0, 0, 0) },
    ];
    let lp = w.global::<crate::LeftPanelAdapter>();
    lp.set_open(true);
    lp.set_modes(std::rc::Rc::new(slint::VecModel::from(rows)).into());
}

/// Clicking Git in the strip must both set `mode` and tell Rust, because entering GIT is
/// what runs `git status` — a strip that only moved the highlight would show a stale tree.
#[test]
fn the_mode_strip_switches_to_git_and_tells_rust() {
    ui(|| {
        let w = window();
        install_modes(&w);
        w.global::<crate::LeftPanelAdapter>().set_mode(crate::paneview::LEFT_MODE_WORKSPACE);

        let saw = std::rc::Rc::new(std::cell::Cell::new(-1));
        {
            let saw = saw.clone();
            w.global::<crate::LeftPanelAdapter>().on_mode_changed(move |m| saw.set(m));
        }

        let found = by_label(&w, "Git");
        assert_eq!(found.len(), 1, "the strip must show exactly one Git button");
        click(&w, &found[0]);

        assert_eq!(
            w.global::<crate::LeftPanelAdapter>().get_mode(),
            crate::paneview::LEFT_MODE_GIT,
            "the click must select the git mode"
        );
        assert_eq!(
            saw.get(),
            crate::paneview::LEFT_MODE_GIT,
            "the click must also reach mode-changed, which is what triggers git status"
        );
    });
}

/// The same for Files, so a failure above reads as "the strip" and not "the git mode".
#[test]
fn the_mode_strip_switches_to_files_and_tells_rust() {
    ui(|| {
        let w = window();
        install_modes(&w);
        w.global::<crate::LeftPanelAdapter>().set_mode(crate::paneview::LEFT_MODE_WORKSPACE);

        let saw = std::rc::Rc::new(std::cell::Cell::new(-1));
        {
            let saw = saw.clone();
            w.global::<crate::LeftPanelAdapter>().on_mode_changed(move |m| saw.set(m));
        }

        let found = by_label(&w, "Files");
        assert_eq!(found.len(), 1, "the strip must show exactly one Files button");
        click(&w, &found[0]);
        assert_eq!(
            w.global::<crate::LeftPanelAdapter>().get_mode(),
            crate::paneview::LEFT_MODE_FILES
        );
        assert_eq!(saw.get(), crate::paneview::LEFT_MODE_FILES);
    });
}

// ===== the git panel's other header buttons =====

/// Refresh sits beside the new diff button and is the older of the two. If adding a second
/// button to that header ever displaced or covered it, this is what says so.
#[test]
fn the_refresh_button_still_works_beside_the_new_diff_button() {
    ui(|| {
        let w = window();
        open_working_tree(&w, true);

        let fired = std::rc::Rc::new(std::cell::Cell::new(false));
        {
            let fired = fired.clone();
            w.global::<crate::LeftPanelAdapter>().on_git_refresh(move || fired.set(true));
        }

        let found = by_label(&w, "Re-run git status");
        assert_eq!(found.len(), 1, "the git header must show one refresh button");
        click(&w, &found[0]);
        assert!(fired.get(), "refresh must still reach git-refresh");
    });
}

/// A section head is the only way to get a collapsed list back, and it is drawn as a label
/// plus a bare chevron Path with no pressable-looking chrome.
#[test]
fn a_git_section_head_collapses_and_reopens_its_list() {
    ui(|| {
        let w = window();
        open_working_tree(&w, true);

        let open = "Collapse the working-tree changes";
        let shut = "Show the working-tree changes";
        assert_eq!(by_label(&w, open).len(), 1, "the Changes head starts expanded");

        click(&w, &by_label(&w, open)[0]);
        assert!(
            by_label(&w, open).is_empty() && by_label(&w, shut).len() == 1,
            "clicking the head must collapse the section"
        );

        click(&w, &by_label(&w, shut)[0]);
        assert_eq!(
            by_label(&w, open).len(),
            1,
            "clicking it again must reopen the section"
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

        let found = by_label(&w, "New pane · Shift-click for shell, command and split options");
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
