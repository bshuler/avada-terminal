//! Track V5j: the New-Goal box's text field.
//!
//! The goal field is a real `TextInput`: it edits natively, and the controller only mirrors
//! what it holds. Two callbacks carry that arrangement — `goal-text-changed` on every edit,
//! and `goal-key` for the handful of keys the box steals back from the field (submit,
//! dismiss, options, history). The split is the interesting part: forward too much and the
//! user cannot type a letter; forward too little and Enter inserts a newline instead of
//! creating the goal.

#![allow(unused_imports)]
use super::*;

/// The goal field itself. It carries no accessible label — it is the box's single focus
/// target — so it is addressed by element id.
fn goal_input(w: &crate::AppWindow) -> ElementHandle {
    let v = by_id(w, "NewGoalDialog::gi");
    assert_eq!(v.len(), 1, "the New-Goal box has exactly one text field");
    v.into_iter().next().unwrap()
}

fn type_char(w: &crate::AppWindow, ch: char) {
    let t = slint::SharedString::from(ch);
    w.window()
        .dispatch_event(WindowEvent::KeyPressed { text: t.clone() });
    w.window()
        .dispatch_event(WindowEvent::KeyReleased { text: t });
}

/// Every keystroke in the field mirrors the whole text up to Rust. It is the *whole* text and
/// not the delta on purpose: the field also gets native editing (paste, select-all-replace,
/// caret moves), so a controller reconstructing the string from deltas would drift the moment
/// the user edits anywhere but the end.
#[test]
fn typing_in_the_goal_field_mirrors_the_whole_text() {
    ui(|| {
        let w = window();
        open_new_goal(&w, 0);
        settle();

        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        let seen = log.clone();
        w.on_goal_text_changed(move |t| seen.borrow_mut().push(t.to_string()));

        click(&w, &goal_input(&w));
        for ch in "hi".chars() {
            type_char(&w, ch);
        }
        settle();

        assert_eq!(
            *log.borrow(),
            vec!["h".to_string(), "hi".to_string()],
            "each edit reports the field's full contents"
        );
    });
}

/// Return is the one key that must *not* reach the text field: the goal box is multi-line,
/// so a Return the box does not steal inserts a newline and the goal is never created. The
/// same path carries Escape (dismiss) and Tab (next category).
#[test]
fn submit_and_dismiss_keys_are_forwarded_out_of_the_field() {
    ui(|| {
        let w = window();
        open_new_goal(&w, 0);
        settle();

        let keys = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        let seen = keys.clone();
        w.on_goal_key(move |m| seen.borrow_mut().push(m.text.to_string()));
        let edits = std::rc::Rc::new(std::cell::Cell::new(0));
        let e = edits.clone();
        w.on_goal_text_changed(move |_| e.set(e.get() + 1));

        click(&w, &goal_input(&w));
        for key in [slint::platform::Key::Return, slint::platform::Key::Escape] {
            type_char(&w, char::from(key));
        }
        settle();

        assert_eq!(
            *keys.borrow(),
            vec![
                char::from(slint::platform::Key::Return).to_string(),
                char::from(slint::platform::Key::Escape).to_string(),
            ],
            "both keys reach `goal_key`"
        );
        assert_eq!(
            edits.get(),
            0,
            "and neither one is also typed into the field"
        );
    });
}
