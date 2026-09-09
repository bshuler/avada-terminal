//! Track V5l: the left panel's tab tree — adopting an orphaned session, and dragging a pane
//! between and within tab groups.
//!
//! Drag-and-drop here is not one gesture but two collaborating ones: the *source* row
//! publishes where the pointer is, and every tab group independently hit-tests that y against
//! its own rectangle and reports back. Only on release does the source decide what happened —
//! another group is a move, its own group is a reorder. That indirection is why these are the
//! easiest callbacks in the app to wire backwards, and why both directions are tested here
//! with two groups on screen rather than one.

#![allow(unused_imports)]
use super::*;

fn pane_row(uid: &str, title: &str) -> crate::LeftPaneRow {
    crate::LeftPaneRow {
        uid: uid.into(),
        title: title.into(),
        ..Default::default()
    }
}

/// Two tab groups, three panes in the first and one in the second — enough that a reorder has
/// somewhere to go and a move has somewhere to land.
fn install_tree(w: &crate::AppWindow) {
    install_modes(w);
    let tab = |title: &str, active: bool, panes: Vec<crate::LeftPaneRow>| crate::LeftTabRow {
        title: title.into(),
        active,
        panes: std::rc::Rc::new(slint::VecModel::from(panes)).into(),
        ..Default::default()
    };
    w.global::<crate::LeftPanelAdapter>().set_tabs(
        std::rc::Rc::new(slint::VecModel::from(vec![
            tab(
                "Tab 1",
                true,
                vec![
                    pane_row("p1", "zsh"),
                    pane_row("p2", "notes.md"),
                    pane_row("p3", "vim"),
                ],
            ),
            tab("Tab 2", false, vec![pane_row("p4", "htop")]),
        ]))
        .into(),
    );
    settle();
}

/// The pane rows of the tree, in order across both groups.
fn rows(w: &crate::AppWindow) -> Vec<ElementHandle> {
    by_label(
        w,
        "Focus this pane · drag to reorder or move it to another tab",
    )
}

/// Press a row, drag the pointer to an absolute y, and release.
///
/// Two separate gates stand between a synthetic press and the row's own `moved` handler, and
/// both are Slint's, not ours. The tree is mounted inside a `Flickable`, which (a) answers a
/// left press with `DelayForwarding(100ms)` — the inner item does not see the press at all
/// until that timer fires — and (b) intercepts every move for the first 500ms after the press
/// if it travels more than 8px in a scrollable direction, which is exactly what a drag is.
/// Advancing mock time past both thresholds while the pointer is still parked on the press
/// point clears the way: the press reaches the row, and every later move is forwarded rather
/// than eaten as a flick. Real users clear the same gates simply by being slow.
///
/// After that the row's own 4px slop applies — it only starts reporting once the pointer has
/// travelled far enough to not be a click — so the pointer is walked in two steps, the first
/// of which spends the slop.
fn drag_row_to(w: &crate::AppWindow, row: &ElementHandle, target_y: f32) {
    let p = row.absolute_position();
    let s = row.size();
    let x = p.x + s.width / 2.0;
    let y0 = p.y + s.height / 2.0;
    let at = |y: f32| LogicalPosition::new(x, y);
    w.window()
        .dispatch_event(WindowEvent::PointerMoved { position: at(y0) });
    w.window().dispatch_event(WindowEvent::PointerPressed {
        position: at(y0),
        button: PointerEventButton::Left,
    });
    i_slint_backend_testing::mock_elapsed_time(600);
    let mid = y0 + if target_y > y0 { 8.0 } else { -8.0 };
    w.window()
        .dispatch_event(WindowEvent::PointerMoved { position: at(mid) });
    settle();
    w.window().dispatch_event(WindowEvent::PointerMoved {
        position: at(target_y),
    });
    settle();
    w.window().dispatch_event(WindowEvent::PointerReleased {
        position: at(target_y),
        button: PointerEventButton::Left,
    });
    settle();
}

/// Dropping a pane on a *different* group moves it: the source tab and pane, then the
/// destination tab and the slot within it. Four indices, and the two pairs are trivially
/// swappable — which would move the wrong pane into the tab it came from.
#[test]
fn dropping_a_pane_on_another_tab_group_moves_it_there() {
    ui(|| {
        let w = window();
        install_tree(&w);
        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::<(i32, i32, i32, i32)>::new()));
        let seen = log.clone();
        w.global::<crate::LeftPanelAdapter>()
            .on_move_pane(move |a, b, c, d| seen.borrow_mut().push((a, b, c, d)));

        let all = rows(&w);
        assert_eq!(all.len(), 4, "three panes in Tab 1, one in Tab 2");
        // Row 3 is Tab 2's only pane; aim at its middle, which is inside Tab 2's rectangle.
        let target = all[3].absolute_position().y + all[3].size().height / 2.0;
        drag_row_to(&w, &all[1], target);

        let got = log.borrow();
        assert_eq!(got.len(), 1, "one drop, one move; got {got:?}");
        assert_eq!(
            (got[0].0, got[0].1, got[0].2),
            (0, 1, 1),
            "pane 1 of tab 0 lands in tab 1"
        );
        assert!(got[0].3 >= 0, "and at a real slot, not a sentinel");
    });
}

/// Dropping a pane back on its *own* group is a reorder, not a move. The distinction is
/// resolved at release time from whichever group last reported itself hovered, so a tree that
/// treated every drop as a move would tear the pane out of its tab and reinsert it — losing
/// the tab's focus and, with a single-tab workspace, doing nothing visible at all while the
/// callback Rust actually listens for never fires.
#[test]
fn dropping_a_pane_back_on_its_own_group_reorders_it() {
    ui(|| {
        let w = window();
        install_tree(&w);
        let moved = std::rc::Rc::new(std::cell::Cell::new(0));
        let m = moved.clone();
        let lp = w.global::<crate::LeftPanelAdapter>();
        lp.on_move_pane(move |_, _, _, _| m.set(m.get() + 1));
        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::<(i32, i32, i32)>::new()));
        let seen = log.clone();
        lp.on_reorder_pane(move |a, b, c| seen.borrow_mut().push((a, b, c)));

        let all = rows(&w);
        // From the first pane down onto the third — still inside Tab 1's rectangle.
        let target = all[2].absolute_position().y + all[2].size().height / 2.0;
        drag_row_to(&w, &all[0], target);

        let got = log.borrow();
        assert_eq!(got.len(), 1, "one drop, one reorder; got {got:?}");
        assert_eq!(
            (got[0].0, got[0].1),
            (0, 0),
            "tab 0's pane 0 is the one moving"
        );
        assert!(got[0].2 > 0, "and it lands below where it started");
        assert_eq!(
            moved.get(),
            0,
            "a same-group drop is never a cross-tab move"
        );
    });
}

/// An orphaned session is one whose window is gone but whose shell is still running. Adopting
/// it is addressed by **uid**, because the list is rebuilt from a scan of live processes: an
/// index would name a different session the moment another one exits.
#[test]
fn adopting_an_orphaned_session_names_it_by_uid() {
    ui(|| {
        let w = window();
        install_modes(&w);
        let lp = w.global::<crate::LeftPanelAdapter>();
        lp.set_detached(
            std::rc::Rc::new(slint::VecModel::from(vec![
                crate::LeftSessionRow {
                    uid: "s1".into(),
                    label: "build".into(),
                    detail: "idle".into(),
                    live: 0.0,
                },
                crate::LeftSessionRow {
                    uid: "s2".into(),
                    label: "deploy".into(),
                    detail: "running".into(),
                    live: 1.0,
                },
            ]))
            .into(),
        );
        settle();

        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        let seen = log.clone();
        lp.on_adopt_session(move |uid| seen.borrow_mut().push(uid.to_string()));

        let tip = "Adopt this orphaned session into the current tab — it keeps running, scrollback and all";
        let found = by_label(&w, tip);
        assert_eq!(found.len(), 2, "one adopt target per orphaned session");
        click(&w, &found[1]);

        assert_eq!(
            *log.borrow(),
            vec!["s2".to_string()],
            "the second row adopts the second session"
        );
    });
}
