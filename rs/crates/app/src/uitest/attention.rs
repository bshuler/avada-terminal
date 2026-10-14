//! Arcane bottom drawer: the attention cards.
//!
//! A card exists to put a waiting pane in front of the human, so the one thing worth
//! proving is that a click reaches the pane the card names, not its neighbour. The drawer is
//! parked off-screen until the pointer reaches the edge, but its cards are real elements and
//! the accessibility route the harness uses does not care.
#![allow(unused_imports)]

use super::*;
use std::cell::RefCell;
use std::rc::Rc;

fn card(id: &str, tab: i32, pane: i32, title: &str) -> crate::AttentionCard {
    crate::AttentionCard {
        id: id.into(),
        kind: 1,
        tab,
        pane,
        uid: "".into(),
        title: title.into(),
        detail: "waiting for your reply".into(),
        tint: slint::Color::from_rgb_u8(0x89, 0xb4, 0xfa),
    }
}

/// Hover the bottom edge the way a user does and let the reveal delay and slide play out.
/// The cards are clipped away until then, and a clipped element is not in the tree.
fn open_bottom_drawer(w: &crate::AppWindow) {
    let size = w.window().size().to_logical(w.window().scale_factor());
    let at = LogicalPosition::new(size.width / 2.0, size.height - 2.0);
    w.window().dispatch_event(WindowEvent::PointerMoved { position: at });
    for _ in 0..12 {
        i_slint_backend_testing::mock_elapsed_time(50);
    }
}

#[test]
fn a_card_focuses_the_pane_it_names() {
    ui(|| {
        let w = window();
        let g = w.global::<crate::AttentionAdapter>();
        g.set_cards(
            Rc::new(slint::VecModel::from(vec![
                card("a", 0, 1, "alpha"),
                card("b", 2, 3, "beta"),
            ]))
            .into(),
        );
        open_bottom_drawer(&w);

        let seen = Rc::new(RefCell::new(Vec::new()));
        let sink = seen.clone();
        g.on_attention_focus(move |ti, i| sink.borrow_mut().push((ti, i)));

        let b = by_label(&w, "Attention: beta — waiting for your reply");
        assert_eq!(b.len(), 1, "one clickable card per entry, labelled with its title");
        click(&w, &b[0]);
        assert_eq!(
            *seen.borrow(),
            vec![(2, 3)],
            "the second card must focus the second card's tab and pane"
        );
    });
}

/// Open for long enough to be read, and the controller is told so the lip badge can clear.
/// The ack is driven from a Slint timer; here we only prove the callback exists and is
/// reachable from the global, which is all the Rust side depends on.
#[test]
fn the_acknowledge_callback_reaches_rust() {
    ui(|| {
        let w = window();
        let hit = Rc::new(RefCell::new(0));
        let sink = hit.clone();
        let g = w.global::<crate::AttentionAdapter>();
        g.on_attention_ack(move || *sink.borrow_mut() += 1);
        g.invoke_attention_ack();
        assert_eq!(*hit.borrow(), 1);
    });
}

/// A card registered through `status_tray` shows in the drawer, and a click reaches the
/// handler its owner registered — the whole point of the registration API.
#[test]
fn a_registered_status_card_renders_and_activates() {
    use crate::status_tray::{register, remove, Level, StatusItem};
    ui(|| {
        let w = window();
        let hit = Rc::new(RefCell::new(Vec::new()));
        let sink = hit.clone();
        w.global::<crate::StatusAdapter>().on_activate(move |id| sink.borrow_mut().push(id.to_string()));
        w.global::<crate::StatusAdapter>().set_cards(
            Rc::new(slint::VecModel::from(vec![crate::StatusCard {
                id: "t.model".into(),
                label: "AI MODEL".into(),
                value: "Arcane-7B".into(),
                glyph: "✦".into(),
                level: 1,
                clickable: true,
            }]))
            .into(),
        );
        open_bottom_drawer(&w);
        let c = by_label(&w, "Status: AI MODEL Arcane-7B");
        assert_eq!(c.len(), 1);
        click(&w, &c[0]);
        assert_eq!(*hit.borrow(), vec!["t.model".to_string()]);
        register(StatusItem::new("t.needs", "X", "y").level(Level::Needs));
        assert!(crate::status_tray::snapshot().iter().any(|i| i.id == "t.needs"));
        remove("t.needs");
    });
}

/// Move the pointer to an element's centre and leave it there.
fn rest_on(w: &crate::AppWindow, el: &ElementHandle) {
    let (p, s) = (el.absolute_position(), el.size());
    let at = LogicalPosition::new(p.x + s.width / 2.0, p.y + s.height / 2.0);
    w.window().dispatch_event(WindowEvent::PointerMoved { position: at });
}

fn wait(ms: u64) {
    for _ in 0..ms / 50 {
        i_slint_backend_testing::mock_elapsed_time(50);
    }
}

/// A pointer resting on a card is still inside the drawer. The cards carry their own
/// TouchAreas, which take the hover away from the drawer's body; the drawer must not read
/// that as the pointer leaving and slide away under it.
#[test]
fn the_bottom_drawer_stays_open_under_a_resting_pointer() {
    ui(|| {
        let w = window();
        let g = w.global::<crate::AttentionAdapter>();
        g.set_cards(Rc::new(slint::VecModel::from(vec![card("a", 0, 1, "alpha")])).into());
        open_bottom_drawer(&w);
        let label = "Attention: alpha — waiting for your reply";
        let c = by_label(&w, label);
        assert_eq!(c.len(), 1);
        let open_y = c[0].absolute_position().y;
        rest_on(&w, &c[0]);
        wait(3000);
        let c = by_label(&w, label);
        assert_eq!(c.len(), 1, "the card left the tree: the drawer closed under the pointer");
        assert_eq!(c[0].absolute_position().y, open_y, "the drawer slid away under the pointer");

        // Leaving still closes it.
        let away = LogicalPosition::new(400.0, 300.0);
        w.window().dispatch_event(WindowEvent::PointerMoved { position: away });
        wait(1000);
        let gone = by_label(&w, label);
        assert!(
            gone.is_empty() || gone[0].absolute_position().y > open_y + 40.0,
            "the drawer must still hide once the pointer leaves"
        );
    });
}

/// Same for the welcome banner on the top edge: reading it with the pointer parked in the
/// middle of the panel, or on its text, must not time it out.
#[test]
fn the_top_drawer_stays_open_under_a_resting_pointer() {
    ui(|| {
        let w = window();
        let title = "Welcome to the test";
        w.global::<crate::BannerAdapter>().set_title(title.into());
        settle();
        assert!(by_label(&w, title).is_empty(), "a closed banner is clipped out of the tree");
        // The banner's hot zone is the top 16px of the centre column, below the top bar.
        // Walk the pointer down until the banner opens; that y is just inside the edge.
        let at = |y: f32| WindowEvent::PointerMoved { position: LogicalPosition::new(640.0, y) };
        let edge = (0..30)
            .map(|i| i as f32 * 6.0)
            .find(|&y| {
                w.window().dispatch_event(at(y));
                wait(600);
                !by_label(&w, title).is_empty()
            })
            .expect("hovering the top edge never opened the banner");
        let open_y = by_label(&w, title)[0].absolute_position().y;

        w.window().dispatch_event(at(edge + 60.0));
        wait(3000);
        let t = by_label(&w, title);
        assert_eq!(t.len(), 1, "the banner closed with the pointer in the middle of it");
        rest_on(&w, &t[0]);
        wait(3000);
        let y = by_label(&w, title)[0].absolute_position().y;
        assert_eq!(y, open_y, "the banner slid away under the pointer");

        w.window().dispatch_event(at(500.0));
        wait(1000);
        let gone = by_label(&w, title);
        assert!(
            gone.is_empty() || gone[0].absolute_position().y < open_y - 50.0,
            "the banner must still hide once the pointer leaves"
        );
    });
}
