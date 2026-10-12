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
