//! Track V5h: the terminal body's pointer layer — selection, link hit-testing, wheel,
//! right-click, and the raw mouse reports a mouse-grabbing app receives.
//!
//! This is the most overloaded surface in the app: one `TouchArea` decides, per event, whether
//! a gesture belongs to the *user* (select, open a link, paste) or to the *program running in
//! the pane* (vim, htop, Claude Code, which set DECSET 1000/1002/1003 and expect raw reports).
//! Getting that split wrong is not a cosmetic bug — it either makes text unselectable or makes
//! a TUI's own mouse handling dead. Every test here installs two panes and drives the **second**,
//! because each callback carries a pane index that a single-pane test cannot falsify.

#![allow(unused_imports)]
use super::*;

/// Two terminal panes, optionally with the second one mouse-grabbed by its program and/or
/// scrolled up off the live edge (which is what reveals the jump-to-bottom HUD).
fn install_bodies(w: &crate::AppWindow, grabs: bool, scroll_offset: i32) {
    let rows: Vec<crate::PaneItem> = (0..2)
        .map(|i| crate::PaneItem {
            uid: format!("u{i}").into(),
            title: format!("pane {i}").into(),
            x: 8.0 + i as f32 * 420.0,
            y: 40.0,
            w: 400.0,
            h: 300.0,
            visible: true,
            focused: i == 0,
            kind: 0,
            font_px: 14.0,
            app_grabs_mouse: grabs && i == 1,
            scroll_offset: if i == 1 { scroll_offset } else { 0 },
            ..Default::default()
        })
        .collect();
    w.set_panes(std::rc::Rc::new(slint::VecModel::from(rows)).into());
    settle();
}

/// The pointer layer of each terminal pane, in strip order.
fn bodies(w: &crate::AppWindow) -> Vec<ElementHandle> {
    let v = by_id(w, "TerminalPane::ta");
    assert_eq!(v.len(), 2, "one pointer layer per terminal pane");
    v
}

/// A point inside an element, given as a fraction of its size. Selection and hit-testing are
/// *positional*, so these tests must aim at named spots rather than always the centre —
/// dragging from the centre to the centre is not a drag.
fn at(el: &ElementHandle, fx: f32, fy: f32) -> LogicalPosition {
    let p = el.absolute_position();
    let s = el.size();
    LogicalPosition::new(p.x + s.width * fx, p.y + s.height * fy)
}

/// A press–drag–release inside one pane must produce the whole selection lifecycle for *that*
/// pane: begin where the button went down, update as the mouse travels, end on release. A
/// widget that reported `update` from the un-pressed move handler would select text on a bare
/// hover; one that skipped `end` would leave the selection latched to the cursor forever.
///
/// The release also resolves as a link click — the two are deliberately stacked on one event,
/// with `selection-end` first so a drag copies before the click can be interpreted.
#[test]
fn dragging_the_terminal_body_begins_updates_and_ends_a_selection() {
    ui(|| {
        let w = window();
        install_bodies(&w, false, 0);

        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        {
            let l = log.clone();
            w.on_pane_selection_begin(move |i, x, y| {
                l.borrow_mut()
                    .push(format!("begin {i} {} {}", x > 0.0, y > 0.0))
            });
            let l = log.clone();
            w.on_pane_selection_update(move |i, x, y| {
                l.borrow_mut()
                    .push(format!("update {i} {} {}", x > 0.0, y > 0.0))
            });
            let l = log.clone();
            w.on_pane_selection_end(move |i| l.borrow_mut().push(format!("end {i}")));
            let l = log.clone();
            w.on_pane_link_activated(move |i, _x, _y, ctrl| {
                l.borrow_mut().push(format!("link {i} ctrl={ctrl}"))
            });
        }

        let b = bodies(&w).swap_remove(1);
        let start = at(&b, 0.25, 0.4);
        let mid = at(&b, 0.55, 0.4);
        let end = at(&b, 0.75, 0.4);
        w.window()
            .dispatch_event(WindowEvent::PointerMoved { position: start });
        w.window().dispatch_event(WindowEvent::PointerPressed {
            position: start,
            button: PointerEventButton::Left,
        });
        w.window()
            .dispatch_event(WindowEvent::PointerMoved { position: mid });
        w.window()
            .dispatch_event(WindowEvent::PointerMoved { position: end });
        w.window().dispatch_event(WindowEvent::PointerReleased {
            position: end,
            button: PointerEventButton::Left,
        });
        settle();

        assert_eq!(
            *log.borrow(),
            vec![
                "begin 1 true true",
                "update 1 true true",
                "update 1 true true",
                "end 1",
                "link 1 ctrl=false",
            ]
        );
    });
}

/// A *bare* hover — no button held — must hit-test for clickable paths, and leaving the pane
/// must retract whatever the hover lit. These ride the move `pointer-event` rather than
/// `moved`, because `moved` only fires while the mouse is grabbed; a hit-test wired to `moved`
/// leaves every path in a shell dead until the user happens to drag over it.
#[test]
fn hovering_the_body_hit_tests_for_links_and_reports_leaving() {
    ui(|| {
        let w = window();
        install_bodies(&w, false, 0);

        let moved = std::rc::Rc::new(std::cell::RefCell::new(Vec::<i32>::new()));
        let exited = std::rc::Rc::new(std::cell::RefCell::new(Vec::<i32>::new()));
        {
            let m = moved.clone();
            w.on_pane_link_moved(move |i, _x, _y| m.borrow_mut().push(i));
            let e = exited.clone();
            w.on_pane_link_exited(move |i| e.borrow_mut().push(i));
        }

        let bs = bodies(&w);
        w.window().dispatch_event(WindowEvent::PointerMoved {
            position: at(&bs[0], 0.5, 0.5),
        });
        settle();
        assert_eq!(*moved.borrow(), vec![0], "hovering pane 0 hit-tests pane 0");
        assert!(
            exited.borrow().is_empty(),
            "still inside — nothing has been left yet"
        );

        w.window().dispatch_event(WindowEvent::PointerMoved {
            position: at(&bs[1], 0.5, 0.5),
        });
        settle();
        assert_eq!(
            *exited.borrow(),
            vec![0],
            "leaving pane 0 must retract pane 0's hover"
        );
        assert_eq!(
            *moved.borrow(),
            vec![0, 1],
            "…and the new pane picks the hit-testing up"
        );
    });
}

/// Right-click in the body is a paste (Windows Terminal's bargain), *unless* the click landed
/// on a link — then the controller claims it for a context menu by returning true. The claim
/// is a return value rather than two separate callbacks precisely so it cannot do both, and
/// this test pins each branch, because the failure mode of the false branch is pasting the
/// clipboard into a shell the user only meant to right-click.
#[test]
fn a_body_right_click_pastes_unless_a_link_claims_it() {
    ui(|| {
        let w = window();
        install_bodies(&w, false, 0);

        let ctx = std::rc::Rc::new(std::cell::Cell::new((-1, false, false)));
        let pasted = std::rc::Rc::new(std::cell::RefCell::new(Vec::<i32>::new()));
        let claim = std::rc::Rc::new(std::cell::Cell::new(false));
        {
            let (c, k) = (ctx.clone(), claim.clone());
            w.on_pane_link_context(move |i, _x, _y, ax, ay| {
                c.set((i, ax > 0.0, ay > 0.0));
                k.get()
            });
            let p = pasted.clone();
            w.on_pane_paste(move |i| p.borrow_mut().push(i));
        }

        let b = bodies(&w).swap_remove(1);
        right_click(&w, &b);
        assert_eq!(
            ctx.get(),
            (1, true, true),
            "the menu anchor must be a window coordinate"
        );
        assert_eq!(
            *pasted.borrow(),
            vec![1],
            "unclaimed, a right-click pastes into that pane"
        );

        claim.set(true);
        right_click(&w, &b);
        assert_eq!(
            *pasted.borrow(),
            vec![1],
            "a claimed right-click must NOT also paste"
        );
    });
}

/// The wheel scrolls that pane's scrollback, carrying the pointer position along (mouse-aware
/// apps need it) and a signed line count. Up is *into history*, so the sign must survive: an
/// inverted wheel is the kind of bug that only shows up in a real terminal.
#[test]
fn the_wheel_scrolls_that_panes_scrollback_in_the_direction_it_turned() {
    ui(|| {
        let w = window();
        install_bodies(&w, false, 0);

        let saw = std::rc::Rc::new(std::cell::RefCell::new(Vec::<(i32, f32)>::new()));
        {
            let s = saw.clone();
            w.on_pane_scroll(move |i, x, y, d| {
                assert!(
                    x > 0.0 && y > 0.0,
                    "a scroll must carry a real pointer position"
                );
                s.borrow_mut().push((i, d));
            });
        }

        let p = at(&bodies(&w)[1], 0.5, 0.5);
        w.window()
            .dispatch_event(WindowEvent::PointerMoved { position: p });
        w.window().dispatch_event(WindowEvent::PointerScrolled {
            position: p,
            delta_x: 0.0,
            delta_y: 40.0,
        });
        w.window().dispatch_event(WindowEvent::PointerScrolled {
            position: p,
            delta_x: 0.0,
            delta_y: -40.0,
        });
        settle();

        assert_eq!(*saw.borrow(), vec![(1, 3.0), (1, -3.0)]);
    });
}

/// When the program in the pane has grabbed the mouse, a plain press stops being a selection
/// and becomes a raw report: kind 0 = down, 1 = move, 2 = up, with the button mapped
/// left/middle → 0/1 and everything else → 2. This is the split that makes vim's visual mode
/// and htop's clickable header work at all.
#[test]
fn a_mouse_grabbing_program_receives_the_raw_pointer_reports() {
    ui(|| {
        let w = window();
        install_bodies(&w, true, 0);

        let rep = std::rc::Rc::new(std::cell::RefCell::new(Vec::<(i32, i32, i32)>::new()));
        let sel = std::rc::Rc::new(std::cell::Cell::new(false));
        {
            let r = rep.clone();
            w.on_pane_pointer_report(move |i, kind, btn, x, y| {
                assert!(
                    x > 0.0 && y > 0.0,
                    "a report must carry the position it happened at"
                );
                r.borrow_mut().push((i, kind, btn));
            });
            let s = sel.clone();
            w.on_pane_selection_begin(move |_, _, _| s.set(true));
        }

        let b = bodies(&w).swap_remove(1);
        let p = at(&b, 0.4, 0.4);
        let q = at(&b, 0.6, 0.4);
        w.window()
            .dispatch_event(WindowEvent::PointerMoved { position: p });
        w.window().dispatch_event(WindowEvent::PointerPressed {
            position: p,
            button: PointerEventButton::Left,
        });
        w.window()
            .dispatch_event(WindowEvent::PointerMoved { position: q });
        w.window().dispatch_event(WindowEvent::PointerReleased {
            position: q,
            button: PointerEventButton::Left,
        });
        settle();

        let got = rep.borrow().clone();
        assert!(
            got.contains(&(1, 0, 0)),
            "a left press must report down/left, got {got:?}"
        );
        assert!(
            got.contains(&(1, 1, 0)),
            "a drag must report moves with the held button"
        );
        assert!(
            got.contains(&(1, 2, 0)),
            "and the release must report up, got {got:?}"
        );
        assert!(
            !sel.get(),
            "a grabbed pane must not also start a local selection"
        );
    });
}

/// The jump-to-bottom HUD only exists while the pane is scrolled up, and clicking it snaps
/// *that* pane back. It is gated on `scroll-offset > 0`, so the first half of this test is
/// that it is genuinely absent at the live edge — a HUD that is merely transparent would still
/// swallow clicks aimed at the terminal underneath it.
#[test]
fn the_jump_to_bottom_hud_appears_only_when_scrolled_and_snaps_its_own_pane() {
    ui(|| {
        let w = window();
        install_bodies(&w, false, 0);
        assert!(
            by_id(&w, "TerminalPane::jump-ta").is_empty(),
            "at the live edge there is nothing to jump back to"
        );

        install_bodies(&w, false, 12);
        let saw = std::rc::Rc::new(std::cell::Cell::new(-1));
        {
            let s = saw.clone();
            w.on_pane_jump_bottom(move |i| s.set(i));
        }

        let hud = by_id(&w, "TerminalPane::jump-ta");
        assert_eq!(hud.len(), 1, "only the scrolled pane shows the HUD");
        click(&w, &hud[0]);
        assert_eq!(saw.get(), 1);
    });
}

/// Keyboard focus is reported by **uid**, not by index — deliberately, because focus outlives
/// the strip: a pane can be reordered, moved to another tab, or torn into a new window between
/// the press that focused it and the moment Rust reads the report. An index captured at press
/// time would then name whatever pane happens to sit in that slot. This drives the *second*
/// pane so a report hard-coded to the first cannot pass.
#[test]
fn pressing_a_pane_body_reports_that_panes_own_uid_as_focused() {
    ui(|| {
        let w = window();
        install_bodies(&w, false, 0);
        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::<(String, bool)>::new()));
        let seen = log.clone();
        w.on_pane_focus_changed(move |uid, on| seen.borrow_mut().push((uid.to_string(), on)));

        let b = bodies(&w)[1].clone();
        let p = at(&b, 0.5, 0.5);
        w.window()
            .dispatch_event(WindowEvent::PointerMoved { position: p });
        w.window().dispatch_event(WindowEvent::PointerPressed {
            position: p,
            button: PointerEventButton::Left,
        });
        w.window().dispatch_event(WindowEvent::PointerReleased {
            position: p,
            button: PointerEventButton::Left,
        });
        settle();

        assert!(
            log.borrow().contains(&("u1".to_string(), true)),
            "the press must focus pane u1 and say so; got {:?}",
            log.borrow()
        );
    });
}
