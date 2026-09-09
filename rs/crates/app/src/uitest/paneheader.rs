//! Track V5g: the pane header and the resize seams.
//!
//! The header is the densest overloaded control in the app: one press arms a drag, a click
//! focuses, a double-click renames, a right-click opens the menu — all from touch areas
//! that overlap. Each one carries the pane index, and each is a silent-corruption bug when
//! it is wrong: focusing the neighbour, renaming the neighbour, dragging the neighbour.
//! These tests install three panes and drive the **middle** one, so a handler that closed
//! over the loop's shared state instead of its own item cannot pass.

#![allow(unused_imports)]
use super::*;
use slint::Model;

/// The label touch area, one per pane, in strip order. It sits over most of the header and
/// deliberately delegates a left-press to the same `pane-grab` as the bar behind it.
fn labels(w: &crate::AppWindow) -> Vec<ElementHandle> {
    by_id(w, "PaneView::lta")
}

/// Press and move without releasing — the gesture a resize seam actually listens for. Its
/// `moved` handler only runs while pressed, so a press/release pair would move nothing.
fn press_then_move(w: &crate::AppWindow, el: &ElementHandle, dx: f32, dy: f32) {
    let p = el.absolute_position();
    let s = el.size();
    let at = |x: f32, y: f32| LogicalPosition::new(p.x + x, p.y + y);
    let (cx, cy) = (s.width / 2.0, s.height / 2.0);
    w.window().dispatch_event(WindowEvent::PointerMoved {
        position: at(cx, cy),
    });
    w.window().dispatch_event(WindowEvent::PointerPressed {
        position: at(cx, cy),
        button: PointerEventButton::Left,
    });
    w.window().dispatch_event(WindowEvent::PointerMoved {
        position: at(cx + dx, cy + dy),
    });
    settle();
}

/// A single click on a pane header does two things at once, and both matter: it focuses the
/// pane (so typing goes there) and it *arms* a drag (which the movement pump later promotes
/// or discards). Wiring only one of them is the classic regression here — a header that
/// focuses but never arms makes drag-to-rearrange dead in exactly the region the title
/// covers, which is most of the header.
#[test]
fn clicking_a_pane_header_focuses_that_pane_and_arms_its_drag() {
    ui(|| {
        let w = window();
        install_panes(&w, &[0, 0, 0]);
        settle();

        let focused = std::rc::Rc::new(std::cell::Cell::new(-1));
        let grabbed = std::rc::Rc::new(std::cell::Cell::new(-1));
        {
            let focused = focused.clone();
            w.on_focus_pane(move |i| focused.set(i));
            let grabbed = grabbed.clone();
            w.on_pane_grab(move |i| grabbed.set(i));
        }

        let ls = labels(&w);
        assert_eq!(ls.len(), 3, "one header label area per pane");
        click(&w, &ls[1]);
        assert_eq!(
            focused.get(),
            1,
            "the middle header must focus the middle pane"
        );
        assert_eq!(
            grabbed.get(),
            1,
            "…and arm the middle pane's drag, not its neighbour's"
        );
    });
}

/// Right-clicking the header opens that pane's menu at the cursor. The coordinates are
/// absolute-position + mouse offset, so a menu built from the touch area's local frame
/// would open in the top-left corner of the screen.
#[test]
fn right_clicking_a_pane_header_opens_its_menu_at_the_cursor() {
    ui(|| {
        let w = window();
        install_panes(&w, &[0, 0, 0]);
        settle();

        let saw = std::rc::Rc::new(std::cell::Cell::new((-1, 0.0f32, 0.0f32)));
        {
            let saw = saw.clone();
            w.on_pane_context(move |i, x, y| saw.set((i, x, y)));
        }

        right_click(&w, &labels(&w)[1]);
        let (i, x, y) = saw.get();
        assert_eq!(i, 1);
        assert!(
            x > 0.0 && y > 0.0,
            "the menu anchor must be a window coordinate, got ({x}, {y})"
        );
    });
}

/// Double-click opens the inline editor; Return commits it. The commit carries the index
/// *and* the typed text, so this drives the real editor rather than writing the title
/// property — a rename that committed its neighbour's index is invisible until a user
/// notices the wrong pane got the name.
#[test]
fn renaming_a_pane_commits_the_typed_title_for_that_pane() {
    ui(|| {
        let w = window();
        install_panes(&w, &[0, 0, 0]);
        settle();

        let began = std::rc::Rc::new(std::cell::Cell::new(-1));
        {
            let began = began.clone();
            w.on_begin_rename_pane(move |i| began.set(i));
        }
        let el = labels(&w)[1].clone();
        let p = el.absolute_position();
        let s = el.size();
        let at = LogicalPosition::new(p.x + s.width / 2.0, p.y + s.height / 2.0);
        w.window()
            .dispatch_event(WindowEvent::PointerMoved { position: at });
        for _ in 0..2 {
            w.window().dispatch_event(WindowEvent::PointerPressed {
                position: at,
                button: PointerEventButton::Left,
            });
            w.window().dispatch_event(WindowEvent::PointerReleased {
                position: at,
                button: PointerEventButton::Left,
            });
        }
        settle();
        assert_eq!(
            began.get(),
            1,
            "a double-click on the middle header opens its editor"
        );

        // The controller owns `editing`; opening the editor is its job, so the test does
        // for it exactly what it would do, then drives the editor the user gets.
        let rows: Vec<crate::PaneItem> = w
            .get_panes()
            .iter()
            .enumerate()
            .map(|(i, mut p)| {
                p.editing = i == 1;
                p
            })
            .collect();
        w.set_panes(std::rc::Rc::new(slint::VecModel::from(rows)).into());
        settle();

        let saw = std::rc::Rc::new(std::cell::RefCell::new((-1, String::new())));
        {
            let saw = saw.clone();
            w.on_rename_pane(move |i, t| *saw.borrow_mut() = (i, t.to_string()));
        }
        for ch in ["l", "o", "g", "s"] {
            let text = slint::SharedString::from(ch);
            w.window()
                .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
            w.window().dispatch_event(WindowEvent::KeyReleased { text });
        }
        let ret = slint::SharedString::from(char::from(slint::platform::Key::Return));
        w.window()
            .dispatch_event(WindowEvent::KeyPressed { text: ret.clone() });
        w.window()
            .dispatch_event(WindowEvent::KeyReleased { text: ret });
        settle();

        // The editor opens pre-selected, so typing replaces rather than appends.
        assert_eq!(*saw.borrow(), (1, "logs".to_string()));
    });
}

/// A resize seam reports a *delta* from the handle's own centre, plus which boundary it is
/// and which axis. All four matter: the wrong index resizes the wrong boundary, and the
/// wrong axis turns a horizontal drag into a vertical one.
#[test]
fn dragging_a_seam_reports_its_boundary_axis_and_delta() {
    ui(|| {
        let w = window();
        install_panes(&w, &[0, 0]);
        w.set_dividers(
            std::rc::Rc::new(slint::VecModel::from(vec![crate::DividerItem {
                x: 400.0,
                y: 60.0,
                w: 8.0,
                h: 260.0,
                vertical: true,
                index: 0,
                main: false,
            }]))
            .into(),
        );
        settle();

        let saw = std::rc::Rc::new(std::cell::Cell::new(None::<(i32, bool, bool, f32, f32)>));
        {
            let saw = saw.clone();
            w.on_divider_drag(move |i, main, vert, dx, dy| saw.set(Some((i, main, vert, dx, dy))));
        }

        let handles = by_id(&w, "DividerHandle::ta");
        assert_eq!(handles.len(), 1, "one handle per divider");
        press_then_move(&w, &handles[0], 30.0, 0.0);

        let (i, main, vert, dx, dy) = saw.get().expect("moving a pressed seam must report a drag");
        assert_eq!((i, main, vert), (0, false, true));
        assert!(
            dx > 0.0,
            "dragging right must be a positive x delta, got {dx}"
        );
        assert!(
            dy.abs() < 1.0,
            "a horizontal drag must not report vertical travel, got {dy}"
        );
    });
}

/// The pane area tells Rust its own size whenever it changes, which is how the layout
/// engine converts fractional splits into pixels. It is a `changed` handler, so the test
/// resizes the real window and asserts the reported size tracks it — a handler wired to the
/// window rather than the pane area would report the chrome as usable space.
#[test]
fn resizing_the_window_reports_the_new_pane_area() {
    ui(|| {
        let w = window();
        install_panes(&w, &[0]);
        settle();

        let saw = std::rc::Rc::new(std::cell::Cell::new((0.0f32, 0.0f32)));
        {
            let saw = saw.clone();
            w.on_area_resized(move |aw, ah| saw.set((aw, ah)));
        }

        w.window().dispatch_event(WindowEvent::Resized {
            size: slint::LogicalSize::new(1000.0, 600.0),
        });
        settle();

        let (aw, ah) = saw.get();
        assert!(
            aw > 0.0 && ah > 0.0,
            "the pane area must report a real size, got ({aw}, {ah})"
        );
        assert!(
            aw <= 1000.0 && ah <= 600.0,
            "…and it cannot exceed the window it lives in"
        );
    });
}
