//! Track V5i: a markdown link inside a view pane.
//!
//! `pane-view-link` is the only pane callback that reports a *string* rather than a row index,
//! and that is the whole point: the row is a paragraph, the link is one span inside it, and
//! two links in one sentence are indistinguishable by row. The emitter is Slint's own
//! `StyledText` link handling, so the thing under test is the wiring — that the href reaches
//! Rust intact, and that clicking a link also focuses the pane it lives in.

#![allow(unused_imports)]
use super::*;

/// A markdown paragraph row (role 16), rendered by the `flow := StyledText` that owns links.
fn md_row(src: &str) -> crate::PaneViewRow {
    crate::PaneViewRow {
        role: 16,
        text: src.into(),
        md: slint::StyledText::from_markdown(src).expect("the fixture is valid markdown"),
        check: -1,
        ..Default::default()
    }
}

/// Clicking a link reports its href and focuses the pane. Both halves matter: a viewer that
/// opened the link without focusing leaves the keyboard in whatever pane the user came from,
/// so the next keystroke lands somewhere they are no longer looking.
#[test]
fn clicking_a_markdown_link_reports_its_href_and_focuses_the_pane() {
    ui(|| {
        let w = window();
        install_view_pane(
            &w,
            4,
            "notes.md",
            vec![md_row("[open](https://example.test/a)")],
            (-1, -1),
            "",
        );
        settle();

        let hrefs = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        let seen = hrefs.clone();
        w.on_pane_view_link(move |_i, l| seen.borrow_mut().push(l.to_string()));
        let focus = std::rc::Rc::new(std::cell::Cell::new(-1));
        let f = focus.clone();
        w.on_focus_pane(move |i| f.set(i));

        let flows = by_id(&w, "ViewRowView::flow");
        assert!(!flows.is_empty(), "the paragraph row draws a StyledText");
        let el = &flows[0];
        let p = el.absolute_position();
        let s = el.size();
        // The link is the whole paragraph, so its first glyph sits at the left edge; aim a few
        // pixels in and vertically centred rather than at the exact corner, which is padding.
        let at = LogicalPosition::new(p.x + 6.0, p.y + s.height / 2.0);
        w.window()
            .dispatch_event(WindowEvent::PointerMoved { position: at });
        w.window().dispatch_event(WindowEvent::PointerPressed {
            position: at,
            button: PointerEventButton::Left,
        });
        w.window().dispatch_event(WindowEvent::PointerReleased {
            position: at,
            button: PointerEventButton::Left,
        });
        settle();

        assert_eq!(
            *hrefs.borrow(),
            vec!["https://example.test/a".to_string()],
            "the href travels to Rust verbatim"
        );
        assert_eq!(focus.get(), 0, "and the pane it lives in takes focus");
    });
}
