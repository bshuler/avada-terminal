//! Track V5f: the preferences overlay — the string settings and the keybinding editor.
//!
//! The keybinding rows are the interesting half. Every one of their callbacks carries a
//! binding **id**, and every one of them writes the user's persisted keymap: a rebind that
//! named the neighbouring row would silently reassign the wrong shortcut, and a reset that
//! named the wrong id would silently discard a customisation. So each test installs several
//! rows and asserts on the id that came back, never merely that something fired.

#![allow(unused_imports)]
use super::*;

/// Open the preferences overlay. Kind 2 is what `Command::OpenPreferences` sets.
fn open_prefs(w: &crate::AppWindow) {
    w.set_overlay_kind(2);
    settle();
}

/// Switch the overlay to a named category. The rail rows are real `tab`s, so the test
/// changes panels the way the user does rather than by poking a private property.
fn open_tab(w: &crate::AppWindow, name: &str) {
    click(w, &only(w, name, AccessibleRole::Tab));
    settle();
}

/// One editable keybinding row. `capturing` decides which affordances the row renders:
/// while recording it shows ⊘ unbind and × cancel; otherwise ✎ rebind.
fn binding(id: &str, label: &str, capturing: bool) -> crate::KeybindingItem {
    crate::KeybindingItem {
        id: id.into(),
        label: label.into(),
        parts: std::rc::Rc::new(slint::VecModel::from(vec![
            slint::SharedString::from("Ctrl"),
            slint::SharedString::from("T"),
        ]))
        .into(),
        category: "Tabs".into(),
        group_first: false,
        overridden: true,
        capturing,
        unbound: false,
        static_row: false,
    }
}

fn install_bindings(w: &crate::AppWindow, rows: Vec<crate::KeybindingItem>) {
    w.set_pref_keybindings(std::rc::Rc::new(slint::VecModel::from(rows)).into());
    w.set_pref_keybinds_overridden(true);
    settle();
}

/// String settings share one callback, discriminated by an integer kind, so the *kind* is
/// as much a part of the contract as the value: kind 8 is the custom font path, and a field
/// that sent the wrong number would quietly overwrite an unrelated setting.
#[test]
fn the_custom_font_path_field_reports_kind_eight_and_the_whole_value() {
    ui(|| {
        let w = window();
        w.set_pref_font_custom(true);
        open_prefs(&w);

        let got = std::rc::Rc::new(std::cell::RefCell::new(Vec::<(i32, String)>::new()));
        {
            let got = got.clone();
            w.on_pref_text(move |k, v| got.borrow_mut().push((k, v.to_string())));
        }

        let field = &by_id(&w, "OverlayLayer::fci")[0];
        click(&w, field);
        for ch in ["A", "B"] {
            let text = slint::SharedString::from(ch);
            w.window()
                .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
            w.window().dispatch_event(WindowEvent::KeyReleased { text });
        }

        assert_eq!(
            got.borrow().as_slice(),
            [(8, "A".to_string()), (8, "AB".to_string())],
            "the font path field sends kind 8 and the field's whole text each keystroke"
        );
    });
}

/// ✎ starts recording. Two rows are installed and the *second* is clicked, because the
/// bug this guards against — a row that closes over the loop's shared state rather than its
/// own item — cannot show up with one row.
#[test]
fn the_rebind_pencil_names_its_own_binding() {
    ui(|| {
        let w = window();
        open_prefs(&w);
        open_tab(&w, "Keybindings");
        install_bindings(
            &w,
            vec![
                binding("newTab", "New tab", false),
                binding("closeTab", "Close tab", false),
            ],
        );

        let saw = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        {
            let saw = saw.clone();
            w.on_pref_rebind(move |id| *saw.borrow_mut() = id.to_string());
        }

        let pencils = by_role(
            &w,
            "Rebind — press the new shortcut next",
            AccessibleRole::Button,
        );
        assert_eq!(pencils.len(), 2, "one rebind affordance per editable row");
        click(&w, &pencils[1]);
        assert_eq!(
            *saw.borrow(),
            "closeTab",
            "the second row must rebind the second binding"
        );
    });
}

/// While a row is recording it swaps ✎ for ×, and × must *cancel* rather than bind — it
/// takes no id because there is only ever one capture in flight.
#[test]
fn cancelling_a_capture_leaves_the_shortcut_alone() {
    ui(|| {
        let w = window();
        open_prefs(&w);
        open_tab(&w, "Keybindings");
        install_bindings(&w, vec![binding("newTab", "New tab", true)]);

        let cancelled = std::rc::Rc::new(std::cell::Cell::new(false));
        let rebound = std::rc::Rc::new(std::cell::Cell::new(false));
        {
            let cancelled = cancelled.clone();
            w.on_pref_cancel_rebind(move || cancelled.set(true));
            let rebound = rebound.clone();
            w.on_pref_rebind(move |_| rebound.set(true));
        }

        click(
            &w,
            &only(
                &w,
                "Cancel — keep the current shortcut",
                AccessibleRole::Button,
            ),
        );
        assert!(cancelled.get(), "× must cancel the capture");
        assert!(
            !rebound.get(),
            "…and must not restart it — the same icon slot does both"
        );
    });
}

/// ⊘ clears the chord outright. It only exists while recording, which is the point: the
/// user has to be in the editor for that row before they can strip its shortcut.
#[test]
fn unbinding_clears_that_bindings_chord() {
    ui(|| {
        let w = window();
        open_prefs(&w);
        open_tab(&w, "Keybindings");
        install_bindings(
            &w,
            vec![
                binding("newTab", "New tab", false),
                binding("closeTab", "Close tab", true),
            ],
        );

        let saw = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        {
            let saw = saw.clone();
            w.on_pref_unbind(move |id| *saw.borrow_mut() = id.to_string());
        }

        let unbinds = by_role(
            &w,
            "Unbind — leave this action with no shortcut",
            AccessibleRole::Button,
        );
        assert_eq!(unbinds.len(), 1, "only the recording row offers ⊘");
        click(&w, &unbinds[0]);
        assert_eq!(*saw.borrow(), "closeTab");
    });
}

/// ↺ resets one row; "Reset all" resets every row. They are deliberately different
/// callbacks — one destroys a single customisation, the other destroys all of them — so a
/// per-row control wired to the global one would be a data-loss bug.
#[test]
fn reset_comes_in_a_per_row_and_an_all_rows_flavour() {
    ui(|| {
        let w = window();
        open_prefs(&w);
        open_tab(&w, "Keybindings");
        install_bindings(
            &w,
            vec![
                binding("newTab", "New tab", false),
                binding("closeTab", "Close tab", false),
            ],
        );

        let one = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        let all = std::rc::Rc::new(std::cell::Cell::new(0));
        {
            let one = one.clone();
            w.on_pref_reset_binding(move |id| *one.borrow_mut() = id.to_string());
            let all = all.clone();
            w.on_pref_reset_all_bindings(move || all.set(all.get() + 1));
        }

        let resets = by_role(
            &w,
            "Reset this shortcut to its default",
            AccessibleRole::Button,
        );
        assert_eq!(resets.len(), 2);
        click(&w, &resets[0]);
        assert_eq!(*one.borrow(), "newTab");
        assert_eq!(
            all.get(),
            0,
            "a per-row reset must not wipe every other binding"
        );

        click(&w, &only(&w, "Reset all", AccessibleRole::Button));
        assert_eq!(all.get(), 1);
    });
}

/// The capture itself. The recording row grabs the keyboard and forwards the chord as four
/// separate pieces, so the test presses a real modified chord and asserts the split — and
/// that a bare modifier is swallowed rather than bound, which is the difference between
/// "press Ctrl+K" working and the binding becoming "Ctrl" the instant the user reaches for
/// it.
#[test]
fn a_captured_chord_arrives_split_into_modifiers_and_key() {
    ui(|| {
        let w = window();
        open_prefs(&w);
        open_tab(&w, "Keybindings");
        install_bindings(&w, vec![binding("newTab", "New tab", true)]);

        let got = std::rc::Rc::new(std::cell::RefCell::new(
            Vec::<(bool, bool, bool, String)>::new(),
        ));
        {
            let got = got.clone();
            w.on_pref_capture(move |c, a, s, t| got.borrow_mut().push((c, a, s, t.to_string())));
        }

        let ctrl = slint::SharedString::from(char::from(slint::platform::Key::Control));
        w.window()
            .dispatch_event(WindowEvent::KeyPressed { text: ctrl.clone() });
        let text = slint::SharedString::from("k");
        w.window()
            .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
        w.window().dispatch_event(WindowEvent::KeyReleased { text });
        w.window()
            .dispatch_event(WindowEvent::KeyReleased { text: ctrl });

        assert_eq!(
            got.borrow().as_slice(),
            [(true, false, false, "k".to_string())],
            "Ctrl alone must be swallowed; only Ctrl+K is a chord"
        );
    });
}
