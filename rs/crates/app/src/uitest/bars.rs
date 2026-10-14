//! The top welcome bar and bottom status tray: pinned by default, drag-resizable, with the
//! auto-hide slide kept as the opt-in; and the tab strip not shifting when the subagent
//! toggle goes from live to inert.
//!
//! e2e: bar_heights_changed
#![allow(unused_imports)]

use super::*;
use std::cell::Cell;
use std::rc::Rc;

fn drag(w: &crate::AppWindow, from: LogicalPosition, to_y: f32) {
    let win = w.window();
    win.dispatch_event(WindowEvent::PointerMoved { position: from });
    win.dispatch_event(WindowEvent::PointerPressed { position: from, button: PointerEventButton::Left });
    settle();
    win.dispatch_event(WindowEvent::PointerMoved { position: LogicalPosition::new(from.x, to_y) });
    settle();
    win.dispatch_event(WindowEvent::PointerReleased {
        position: LogicalPosition::new(from.x, to_y),
        button: PointerEventButton::Left,
    });
    settle();
}

fn grip_centre(w: &crate::AppWindow, label: &str) -> (LogicalPosition, f32) {
    let g = by_label(w, label);
    assert_eq!(g.len(), 1, "{label}: one grip");
    let p = g[0].absolute_position();
    let s = g[0].size();
    (LogicalPosition::new(p.x + s.width / 2.0, p.y + s.height / 2.0), s.height)
}

/// Out of the box both bars are in view without any hover, and the panes sit between them.
#[test]
fn the_bars_are_pinned_by_default() {
    ui(|| {
        let w = window();
        w.set_pref_bars_autohide(false);
        for _ in 0..8 {
            settle();
        }
        let (top, _) = grip_centre(&w, "Resize the top bar");
        let (bot, _) = grip_centre(&w, "Resize the bottom bar");
        assert!(top.y > 0.0 && top.y < 300.0, "top bar not in view: {}", top.y);
        assert!(bot.y < 800.0 && bot.y > 600.0, "bottom bar not in view: {}", bot.y);
    });
}

/// Dragging the inner edge resizes live, clamps to the limits, and reports once on release.
#[test]
fn dragging_a_bar_edge_resizes_it_and_reports_on_release() {
    ui(|| {
        let w = window();
        w.set_pref_bars_autohide(false);
        let seen = Rc::new(Cell::new((0.0f32, 0.0f32, 0u32)));
        {
            let seen = seen.clone();
            w.on_bar_heights_changed(move |t, b| {
                let n = seen.get().2;
                seen.set((t, b, n + 1));
            });
        }
        for _ in 0..8 {
            settle();
        }
        let before = w.get_top_bar_h();
        let (at, _) = grip_centre(&w, "Resize the top bar");
        drag(&w, at, at.y + 40.0);
        let grown = w.get_top_bar_h();
        assert!(grown > before + 30.0, "top bar did not grow: {before} -> {grown}");
        assert_eq!(seen.get().2, 1, "one persist per drag");
        assert!((seen.get().0 - grown).abs() < 0.5);

        let (at, _) = grip_centre(&w, "Resize the top bar");
        drag(&w, at, at.y - 1000.0);
        assert_eq!(w.get_top_bar_h(), 48.0, "clamped to the minimum");
        let (at, _) = grip_centre(&w, "Resize the top bar");
        drag(&w, at, at.y + 1000.0);
        assert_eq!(w.get_top_bar_h(), 280.0, "clamped to the maximum");

        let b0 = w.get_bottom_bar_h();
        let (at, _) = grip_centre(&w, "Resize the bottom bar");
        drag(&w, at, at.y - 30.0);
        assert!(w.get_bottom_bar_h() > b0 + 20.0, "bottom bar grows when dragged up");
    });
}

/// Auto-hide stays available: with it on, a bar is parked off the edge until the pointer comes.
#[test]
fn autohide_still_parks_and_slides_the_bars() {
    ui(|| {
        let w = window();
        w.set_pref_bars_autohide(false);
        for _ in 0..8 {
            settle();
        }
        let (pinned, _) = grip_centre(&w, "Resize the top bar");
        w.set_pref_bars_autohide(true);
        for _ in 0..8 {
            settle();
        }
        let (top, _) = grip_centre(&w, "Resize the top bar");
        assert!(top.y < pinned.y - 60.0, "top bar should be parked: {} vs pinned {}", top.y, pinned.y);
        let size = w.window().size().to_logical(w.window().scale_factor());
        // The drawer hangs under the tab strip, so its edge is where the parked grip sits.
        w.window().dispatch_event(WindowEvent::PointerMoved {
            position: LogicalPosition::new(size.width / 2.0, top.y),
        });
        for _ in 0..12 {
            settle();
        }
        let (top, _) = grip_centre(&w, "Resize the top bar");
        assert!(top.y > pinned.y - 5.0, "top bar should have slid into view: {} vs {}", top.y, pinned.y);
    });
}

/// The lurch: tabs must sit in exactly the same place whether or not the active tab has
/// subagents, because the toggle is always in the row.
#[test]
fn tabs_do_not_move_when_the_subagent_toggle_goes_inert() {
    ui(|| {
        let w = window();
        install_tabs(&w, &["DEX Test", "Open Questions"], 0, None);
        w.set_has_subagents(true);
        for _ in 0..8 {
            settle();
        }
        let xs = |w: &crate::AppWindow| -> Vec<f32> {
            by_id(w, "TabChip::ta").iter().map(|c| c.absolute_position().x).collect()
        };
        let with = xs(&w);
        assert_eq!(with.len(), 2);
        w.set_has_subagents(false);
        for _ in 0..8 {
            settle();
        }
        assert_eq!(xs(&w), with, "tab positions shifted with the subagent toggle");
    });
}
