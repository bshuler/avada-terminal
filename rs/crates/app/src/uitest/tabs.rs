//! Track V5c: the tab strip's pointer and keyboard routes.
//!
//! Every callback here carries an **index** or a **geometry**, and every one of them is a
//! silent-corruption bug when it is wrong: a reorder that arms the wrong tab, a rename that
//! retitles its neighbour, a drag hit-test built from stale coordinates. A property write
//! would prove none of that, so each test drives the real control and asserts on the
//! payload, not merely that something fired.

#![allow(unused_imports)]
use super::*;

/// Press the left button on an element and then move the pointer, without releasing — the
/// gesture that starts a window drag. `click()` cannot express it: the drag area only acts
/// on `moved` **while pressed**, so a press-and-release would move nothing.
fn press_then_move(w: &crate::AppWindow, el: &ElementHandle, dx: f32, dy: f32) {
    let pos = el.absolute_position();
    let size = el.size();
    assert!(
        size.width > 0.0 && size.height > 0.0,
        "the drag strip laid out to {}x{} — nothing can grab it",
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
    win.dispatch_event(WindowEvent::PointerMoved {
        position: LogicalPosition::new(at.x + dx, at.y + dy),
    });
    win.dispatch_event(WindowEvent::PointerReleased {
        position: LogicalPosition::new(at.x + dx, at.y + dy),
        button: PointerEventButton::Left,
    });
}

/// Type a string into whatever currently holds focus, one press/release pair per character.
fn typed(w: &crate::AppWindow, s: &str) {
    for ch in s.chars() {
        let text = slint::SharedString::from(ch);
        w.window()
            .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
        w.window().dispatch_event(WindowEvent::KeyReleased { text });
    }
}

/// Press a named key (Return, Escape, …) on the focused element.
fn press_key(w: &crate::AppWindow, key: slint::platform::Key) {
    let text = slint::SharedString::from(char::from(key));
    w.window()
        .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    w.window().dispatch_event(WindowEvent::KeyReleased { text });
}

/// The chip's own touch area, which owns press / right-press / double-click. There is one
/// per tab and they are returned in strip order.
fn chips(w: &crate::AppWindow) -> Vec<ElementHandle> {
    ElementHandle::find_by_element_id(w, "TabChip::ta").collect()
}

/// Arming a reorder on the wrong tab drags the wrong terminal to a new position, and the
/// user only finds out after the drop. The second tab is deliberately not the active one:
/// a grab that reported the *focused* index instead of the *pressed* one would still pass
/// a single-tab test.
#[test]
fn pressing_a_tab_arms_a_reorder_of_that_tab() {
    ui(|| {
        let w = window();
        install_tabs(&w, &["one", "two", "three"], 0, None);
        settle();

        let saw = std::rc::Rc::new(std::cell::Cell::new(-1));
        {
            let saw = saw.clone();
            w.on_tab_grab(move |i| saw.set(i));
        }

        let ta = chips(&w);
        assert_eq!(ta.len(), 3, "three tabs, three chips");
        click(&w, &ta[1]);
        assert_eq!(saw.get(), 1, "the grab must carry the pressed tab's index");
    });
}

/// The tab context menu acts on the tab under the cursor. It must also report *where* it
/// was invoked, because the app anchors the menu at that point — a menu that always opened
/// at the origin would still satisfy an index-only assertion.
#[test]
fn right_clicking_a_tab_opens_its_menu_at_the_cursor() {
    ui(|| {
        let w = window();
        install_tabs(&w, &["one", "two"], 0, None);
        settle();

        let saw = std::rc::Rc::new(std::cell::RefCell::new(None::<(i32, f32, f32)>));
        {
            let saw = saw.clone();
            w.on_tab_context(move |i, x, y| *saw.borrow_mut() = Some((i, x, y)));
        }

        right_click(&w, &chips(&w)[1]);
        let got = saw.borrow().expect("a right-click must open the tab menu");
        assert_eq!(got.0, 1, "the menu must act on the tab that was clicked");
        assert!(
            got.1 > 0.0 && got.2 > 0.0,
            "the anchor must be a real window point, got ({}, {})",
            got.1,
            got.2
        );
    });
}

/// Drag-and-drop hit-testing runs in Rust, off geometry the strip reports. The report is
/// driven by a 250 ms timer, so `settle()`'s 50 ms is not enough — and a test that only
/// settled would have "proved" a feature that never fires.
#[test]
fn the_strip_reports_each_tabs_position_and_width() {
    ui(|| {
        let w = window();
        install_tabs(&w, &["one", "two"], 0, None);
        settle();

        let saw = std::rc::Rc::new(std::cell::RefCell::new(Vec::<(i32, f32, f32)>::new()));
        {
            let saw = saw.clone();
            w.on_tab_geom(move |i, x, width| saw.borrow_mut().push((i, x, width)));
        }

        i_slint_backend_testing::mock_elapsed_time(300);

        let got = saw.borrow().clone();
        for idx in [0, 1] {
            let r = got
                .iter()
                .find(|r| r.0 == idx)
                .unwrap_or_else(|| panic!("tab {idx} never reported its geometry"));
            assert!(r.2 > 0.0, "tab {idx} reported a width of {}", r.2);
        }
        let first = got.iter().find(|r| r.0 == 0).unwrap();
        let second = got.iter().find(|r| r.0 == 1).unwrap();
        assert!(
            second.1 > first.1,
            "tab 1 sits right of tab 0, but they reported x {} and {}",
            first.1,
            second.1
        );
    });
}

/// The rename editor replaces the chip's label with a focused, pre-selected text box, and
/// Return commits. The committed text is asserted against what was *typed*, so an editor
/// that failed to select-all — leaving the old title in front of the new one — fails here
/// rather than shipping.
#[test]
fn renaming_a_tab_commits_the_typed_title_for_that_tab() {
    ui(|| {
        let w = window();
        install_tabs(&w, &["one", "two"], 0, None);
        w.set_editing_tab(1);
        settle();

        let saw = std::rc::Rc::new(std::cell::RefCell::new(None::<(i32, String)>));
        {
            let saw = saw.clone();
            w.on_rename_tab(move |i, t| *saw.borrow_mut() = Some((i, t.to_string())));
        }

        typed(&w, "build");
        press_key(&w, slint::platform::Key::Return);

        assert_eq!(
            saw.borrow().clone(),
            Some((1, "build".to_string())),
            "Return must commit exactly what was typed, for the tab being edited"
        );
    });
}

/// The empty stretch of the strip is the window's drag handle — on a frameless window it is
/// the only way to move the window at all.
#[test]
fn dragging_the_empty_strip_moves_the_window() {
    ui(|| {
        let w = window();
        install_tabs(&w, &["one"], 0, None);
        settle();

        let fired = std::rc::Rc::new(std::cell::Cell::new(false));
        {
            let fired = fired.clone();
            w.on_start_drag(move || fired.set(true));
        }

        press_then_move(&w, &by_id(&w, "TopBar::drag")[0], 40.0, 0.0);
        assert!(
            fired.get(),
            "a press-and-move on the strip must start a window drag"
        );
    });
}

/// The hidden-pane taskbar has its own right-click menu, and its own indices. A pane menu
/// that reported the wrong index would close or recolour a pane the user cannot even see.
#[test]
fn right_clicking_a_taskbar_button_opens_that_panes_menu() {
    ui(|| {
        let w = window();
        install_panes(&w, &[0, 0, 0]);
        w.set_taskbar_visible(true);
        settle();

        let saw = std::rc::Rc::new(std::cell::RefCell::new(None::<(i32, f32, f32)>));
        {
            let saw = saw.clone();
            w.on_taskbar_context(move |i, x, y| *saw.borrow_mut() = Some((i, x, y)));
        }

        let items = by_id(&w, "TaskItem::ta");
        assert_eq!(items.len(), 3, "three panes, three taskbar buttons");
        right_click(&w, &items[2]);
        let got = saw.borrow().expect("a right-click must open the pane menu");
        assert_eq!(got.0, 2, "the menu must act on the button that was clicked");
        assert!(
            got.1 > 0.0 && got.2 > 0.0,
            "the anchor must be a real window point, got ({}, {})",
            got.1,
            got.2
        );
    });
}
