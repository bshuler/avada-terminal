//! Track V5e: the in-pane search box (Ctrl+F), which lives inside the terminal widget.
//!
//! Every one of these callbacks carries the **pane index**, because search is per-pane: a
//! next-match that reported the wrong pane would scroll a terminal the user is not looking
//! at. The box is instantiated unconditionally and visibility-toggled, so the tests drive
//! the real controls with `search-open` set, exactly as the pane menu's "Search…" leaves it.

#![allow(unused_imports)]
use super::*;
use slint::Model;

/// Two terminal panes, the second one showing its search box. The *second* is deliberate:
/// a callback that hard-coded pane 0, or reported the focused pane rather than the one the
/// box belongs to, would pass a single-pane test.
fn install_search_panes(w: &crate::AppWindow) {
    let rows: Vec<crate::PaneItem> = (0..2)
        .map(|i| crate::PaneItem {
            title: format!("pane {i}").into(),
            x: 8.0 + i as f32 * 420.0,
            y: 40.0,
            w: 400.0,
            h: 300.0,
            visible: true,
            focused: i == 0,
            kind: 0,
            search_open: i == 1,
            search_count: "2/7".into(),
            font_px: 14.0,
            ..Default::default()
        })
        .collect();
    w.set_panes(std::rc::Rc::new(slint::VecModel::from(rows)).into());
    settle();
}

/// The ↑ ↓ × buttons, in that order — the box's own declaration order. They carry no
/// accessible label (they are single-glyph chrome inside the widget), so they are addressed
/// by element id, which is still a real hit-tested click on the real control.
///
/// Only the *open* box's buttons come back. The search box is instantiated in every pane and
/// visibility-toggled, but Slint prunes hidden subtrees from the accessibility tree — which
/// is the correct behaviour, and it is why these lists are three long rather than six.
fn search_buttons(w: &crate::AppWindow) -> Vec<ElementHandle> {
    by_id(w, "SearchBtn::btn-ta")
}

/// The query box sends the WHOLE query on every keystroke, and it sends the pane it belongs
/// to. This is the search equivalent of the rail's query contract: the controller re-runs
/// the search against a statement of what the box now says.
#[test]
fn typing_in_the_search_box_reports_the_query_and_its_pane() {
    ui(|| {
        let w = window();
        install_search_panes(&w);

        let got = std::rc::Rc::new(std::cell::RefCell::new(Vec::<(i32, String)>::new()));
        {
            let got = got.clone();
            w.on_pane_search_edited(move |i, q| got.borrow_mut().push((i, q.to_string())));
        }

        let boxes = by_id(&w, "TerminalPane::query");
        assert_eq!(
            boxes.len(),
            1,
            "only the pane whose search is open offers a query box"
        );
        click(&w, &boxes[0]);
        for ch in ["e", "r"] {
            let text = slint::SharedString::from(ch);
            w.window()
                .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
            w.window().dispatch_event(WindowEvent::KeyReleased { text });
        }

        assert_eq!(
            got.borrow().as_slice(),
            [(1, "e".to_string()), (1, "er".to_string())],
            "each keystroke sends the whole query, tagged with the pane it came from"
        );
    });
}

/// ↓ and ↑ step the match cursor. They are separate callbacks rather than one with a sign,
/// so both have to be reached — and both have to name pane 1.
#[test]
fn the_next_and_previous_buttons_step_that_panes_matches() {
    ui(|| {
        let w = window();
        install_search_panes(&w);

        let next = std::rc::Rc::new(std::cell::Cell::new(-1));
        let prev = std::rc::Rc::new(std::cell::Cell::new(-1));
        {
            let next = next.clone();
            w.on_pane_search_next(move |i| next.set(i));
            let prev = prev.clone();
            w.on_pane_search_prev(move |i| prev.set(i));
        }

        // ↑ ↓ ×, belonging to the only open box — the second pane's.
        let btns = search_buttons(&w);
        assert_eq!(
            btns.len(),
            3,
            "the open box offers previous, next and close"
        );
        click(&w, &btns[1]);
        assert_eq!(next.get(), 1, "↓ must advance the second pane's search");
        click(&w, &btns[0]);
        assert_eq!(prev.get(), 1, "↑ must step the second pane's search back");
    });
}

/// × closes the box. The controller owns `search-open`, so this must reach Rust — a widget
/// that hid the box locally would leave the controller believing a search is still running,
/// and the match highlights painted over the terminal would never be cleared.
#[test]
fn the_close_button_tells_rust_which_panes_search_ended() {
    ui(|| {
        let w = window();
        install_search_panes(&w);

        let closed = std::rc::Rc::new(std::cell::Cell::new(-1));
        {
            let closed = closed.clone();
            w.on_pane_search_closed(move |i| closed.set(i));
        }

        click(&w, &search_buttons(&w)[2]);
        assert_eq!(closed.get(), 1, "× must close the second pane's search");
        assert!(
            w.get_panes().row_data(1).unwrap().search_open,
            "and it must NOT hide the box on its own — the controller decides, and a box \
             that closed itself would desync from the highlights it left behind"
        );
    });
}
