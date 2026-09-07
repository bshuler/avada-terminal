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
            &[("New Pane", "Ctrl+T"), ("Show Diff", ""), ("Preferences", "")],
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
        let rows: Vec<crate::PrefBrowserRow> = [("com.apple.Safari", "Safari"), ("org.mozilla.firefox", "Firefox")]
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
            slint::SharedString::from("hyperpanes"),
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
            ("Project", "hyperpanes"),
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
        click(
            &w,
            &only(&w, "fix the diff button", AccessibleRole::Button),
        );
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
        assert_eq!(got.get(), 1, "the second attachment's × must remove index 1");
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
