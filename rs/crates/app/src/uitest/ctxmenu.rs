//! Track V5d: the context menu's side flyouts.
//!
//! The top-level rows were already proven (`ctx_pick`, separators, disabled rows). The
//! flyouts are a second dispatch layer with its own failure modes: a submenu that never
//! opens, a swatch that reports the wrong palette index, a toggle that sends the value it
//! already had instead of the one the user asked for. Each test opens the real submenu by
//! clicking its header row, then clicks the real control inside it.

#![allow(unused_imports)]
use super::*;

/// A row that opens a submenu. `kind` picks which flyout: 2 = Change Color, 3 = Move to
/// Tab, 4 = Layout, 5 = Reminder.
fn sub_row(label: &str, kind: i32) -> crate::MenuEntry {
    crate::MenuEntry {
        label: label.into(),
        kind,
        ..Default::default()
    }
}

/// Open the menu and click the submenu header, which is what puts the flyout on screen.
/// Hover would also do it, but a click is the deterministic route — a hover that landed a
/// pixel outside would leave the flyout closed and every later `find` empty.
fn open_sub(w: &crate::AppWindow, label: &str, kind: i32) {
    install_menu(w, vec![sub_row(label, kind)]);
    settle();
    click(w, &only(w, label, AccessibleRole::Button));
    settle();
}

/// The palette reports an **index**, not a colour, and the Rust side maps it back. Picking
/// the second slot must not tint the pane with the first.
#[test]
fn a_swatch_reports_its_own_palette_index() {
    ui(|| {
        let w = window();
        w.set_ctx_swatches(
            std::rc::Rc::new(slint::VecModel::from(vec![
                slint::Color::from_rgb_u8(0xf3, 0x8b, 0xa8),
                slint::Color::from_rgb_u8(0xa6, 0xe3, 0xa1),
                slint::Color::from_rgb_u8(0x89, 0xb4, 0xfa),
            ]))
            .into(),
        );
        open_sub(&w, "Change Color", 2);

        let saw = std::rc::Rc::new(std::cell::Cell::new(-1));
        {
            let saw = saw.clone();
            w.on_ctx_swatch(move |i| saw.set(i));
        }

        let slots = by_label(&w, "Tint this pane's frame and dot");
        assert_eq!(slots.len(), 3, "three palette colours, three swatches");
        click(&w, &slots[1]);
        assert_eq!(saw.get(), 1, "the swatch must report its own index");
    });
}

/// "No colour" is a separate callback, not swatch index -1 — the Rust side clears the tint
/// rather than looking one up.
#[test]
fn the_no_colour_chip_clears_the_tint() {
    ui(|| {
        let w = window();
        w.set_ctx_swatches(
            std::rc::Rc::new(slint::VecModel::from(vec![slint::Color::from_rgb_u8(
                0xf3, 0x8b, 0xa8,
            )]))
            .into(),
        );
        open_sub(&w, "Change Color", 2);

        let fired = std::rc::Rc::new(std::cell::Cell::new(false));
        {
            let fired = fired.clone();
            w.on_ctx_swatch_none(move || fired.set(true));
        }

        click(
            &w,
            &only(
                &w,
                "No colour — hide this pane's frame and dot",
                AccessibleRole::Button,
            ),
        );
        assert!(fired.get(), "the slashed chip must clear the tint");
    });
}

/// The toggles send the value the click *asks for*, which is the opposite of the one they
/// are showing. A toggle that echoed its current state back would leave the checkbox stuck.
#[test]
fn the_frame_and_dot_toggles_send_the_flipped_value() {
    ui(|| {
        let w = window();
        w.set_ctx_frame(true);
        w.set_ctx_dot(false);
        open_sub(&w, "Change Color", 2);

        let frame = std::rc::Rc::new(std::cell::Cell::new(None));
        let dot = std::rc::Rc::new(std::cell::Cell::new(None));
        {
            let frame = frame.clone();
            w.on_ctx_frame_set(move |v| frame.set(Some(v)));
            let dot = dot.clone();
            w.on_ctx_dot_set(move |v| dot.set(Some(v)));
        }

        let show_frame = only(&w, "Show Frame", AccessibleRole::Checkbox);
        assert_eq!(
            show_frame.accessible_checked(),
            Some(true),
            "the frame row must show the state it was given"
        );
        click(&w, &show_frame);
        assert_eq!(
            frame.get(),
            Some(false),
            "a checked row must ask to uncheck"
        );

        click(&w, &only(&w, "Show Dot", AccessibleRole::Checkbox));
        assert_eq!(dot.get(), Some(true), "an unchecked row must ask to check");
    });
}

/// Move-to-Tab rows carry the destination tab's **id**, not their position in the flyout —
/// the current tab is missing from the list, so the two differ for every row after it.
#[test]
fn move_to_tab_carries_the_destination_tabs_own_id() {
    ui(|| {
        let w = window();
        w.set_ctx_tabs(
            std::rc::Rc::new(slint::VecModel::from(vec![
                crate::CtxTab {
                    label: "one".into(),
                    idx: 0,
                },
                crate::CtxTab {
                    label: "three".into(),
                    idx: 2,
                },
            ]))
            .into(),
        );
        open_sub(&w, "Move to Tab", 3);

        let saw = std::rc::Rc::new(std::cell::Cell::new(-1));
        {
            let saw = saw.clone();
            w.on_ctx_move_tab(move |i| saw.set(i));
        }

        click(&w, &only(&w, "three", AccessibleRole::Button));
        assert_eq!(
            saw.get(),
            2,
            "the row must send the tab's id, not its row number"
        );
    });
}

/// Layout rows likewise carry the preset's id, which matches the Rust `Layout` discriminant
/// order and is not the row's position once "Automatic" is in the list.
#[test]
fn a_layout_row_carries_the_presets_id() {
    ui(|| {
        let w = window();
        w.set_ctx_layouts(
            std::rc::Rc::new(slint::VecModel::from(vec![
                crate::LayoutOption {
                    id: 0,
                    label: "Automatic".into(),
                    active: true,
                    hint: "— grid".into(),
                },
                crate::LayoutOption {
                    id: 4,
                    label: "Grid".into(),
                    active: false,
                    hint: "".into(),
                },
            ]))
            .into(),
        );
        open_sub(&w, "Layout", 4);

        let saw = std::rc::Rc::new(std::cell::Cell::new(-1));
        {
            let saw = saw.clone();
            w.on_ctx_layout(move |i| saw.set(i));
        }

        click(&w, &only(&w, "Grid", AccessibleRole::Button));
        assert_eq!(saw.get(), 4, "the row must send the preset id");
    });
}

/// The Reminder flyout's Custom field re-parses on every keystroke through a *pure* Rust
/// callback, and Enter submits the parsed minutes over the frozen `pick(int)` channel by
/// adding them to a base. Both halves are asserted: a field that parsed but submitted the
/// raw text, or submitted without the base, would park the reminder at the wrong time.
#[test]
fn the_custom_reminder_field_parses_in_rust_and_submits_the_minutes() {
    ui(|| {
        let w = window();

        let asked = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        {
            let asked = asked.clone();
            w.global::<crate::ReminderCustom>()
                .on_parse_minutes(move |t| {
                    asked.borrow_mut().push(t.to_string());
                    // "90m" → 90 minutes; anything else is unparseable.
                    if t == "90m" {
                        90
                    } else {
                        -1
                    }
                });
        }

        open_sub(&w, "Remind Me", 5);

        let picked = std::rc::Rc::new(std::cell::Cell::new(-1));
        {
            let picked = picked.clone();
            w.on_ctx_pick(move |i| picked.set(i));
        }

        // The field focuses itself on `init`, so the keys land in it without a click.
        for ch in ["9", "0", "m"] {
            let text = slint::SharedString::from(ch);
            w.window()
                .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
            w.window().dispatch_event(WindowEvent::KeyReleased { text });
        }
        assert_eq!(
            asked.borrow().as_slice(),
            ["9".to_string(), "90".to_string(), "90m".to_string()],
            "every keystroke must re-ask Rust, so the border can tint as you type"
        );

        let text = slint::SharedString::from(char::from(slint::platform::Key::Return));
        w.window()
            .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
        w.window().dispatch_event(WindowEvent::KeyReleased { text });
        assert_eq!(
            picked.get(),
            1_000_090,
            "Enter must submit CTX_CUSTOM_REMIND_BASE + the parsed minutes"
        );
    });
}

/// An unparseable duration must submit nothing at all — parking a reminder at a time the
/// user never asked for is worse than refusing.
#[test]
fn an_unparseable_custom_duration_submits_nothing() {
    ui(|| {
        let w = window();
        w.global::<crate::ReminderCustom>().on_parse_minutes(|_| -1);
        open_sub(&w, "Remind Me", 5);

        let picked = std::rc::Rc::new(std::cell::Cell::new(-1));
        {
            let picked = picked.clone();
            w.on_ctx_pick(move |i| picked.set(i));
        }

        let text = slint::SharedString::from("x");
        w.window()
            .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
        w.window().dispatch_event(WindowEvent::KeyReleased { text });
        let text = slint::SharedString::from(char::from(slint::platform::Key::Return));
        w.window()
            .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
        w.window().dispatch_event(WindowEvent::KeyReleased { text });
        assert_eq!(picked.get(), -1, "a rejected duration must park nothing");
    });
}
