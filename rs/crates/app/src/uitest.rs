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
