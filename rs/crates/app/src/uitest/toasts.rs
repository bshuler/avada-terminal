//! Track V5b: the fired-reminder alert toasts in the window's top-right corner.
//!
//! A parked pane's reminder firing is the only thing that brings the pane back to the
//! user's attention, and the toast is where that happens: its body restores the pane, its
//! × stands the toast down while leaving the reminder (and the bell badge) alone. Both are
//! icon-quiet controls inside a `for` over a model, so the risk they carry is the usual
//! one — the *wrong* row's uid — which a screenshot cannot show and this can.
#![allow(unused_imports)]

use super::*;
use std::cell::RefCell;
use std::rc::Rc;

/// One projected toast, shaped exactly as `App::pump_reminders` builds it.
fn item(uid: &str, title: &str, due: &str) -> crate::ReminderItem {
    crate::ReminderItem {
        uid: uid.into(),
        title: title.into(),
        tint: slint::Color::from_rgb_u8(0x89, 0xb4, 0xfa),
        due: due.into(),
        overdue: true,
    }
}

/// Publish the toast stack. `settle()` matters here: the stack is a `for` and its rows do
/// not exist as elements until a frame has been drawn.
fn install_toasts(w: &crate::AppWindow, items: Vec<crate::ReminderItem>) {
    w.global::<crate::RemindersAdapter>()
        .set_toasts(Rc::new(slint::VecModel::from(items)).into());
    settle();
}

/// A shared, ordered log of the uids a callback was handed.
type UidLog = Rc<RefCell<Vec<String>>>;

/// Record every uid a callback is handed, in order. Both halves are the *same* log — one to
/// move into the handler, one to read back afterwards.
fn recorder() -> (UidLog, UidLog) {
    let seen = Rc::new(RefCell::new(Vec::new()));
    (seen.clone(), seen)
}

/// The toast has to say *which* reminder fired. Two parked panes firing a minute apart
/// produce two identical-looking cards otherwise, and restoring the wrong one is silent.
#[test]
fn a_toast_names_the_reminder_and_when_it_was_due() {
    ui(|| {
        let w = window();
        install_toasts(&w, vec![item("r1", "ship the release", "2m ago")]);

        assert_eq!(
            by_label(&w, "Reminder: ship the release").len(),
            1,
            "the toast must carry the pane's own title"
        );
        assert_eq!(
            by_label(&w, "due 2m ago — click to restore").len(),
            1,
            "and say when it was due, plus what a click does"
        );
    });
}

/// The × is the only way to silence a toast without restoring the pane, and it must carry
/// its *own* row's uid — dismissing a neighbour's toast leaves this one on screen and
/// hides the one the user was reading.
#[test]
fn the_dismiss_button_carries_its_own_toasts_uid() {
    ui(|| {
        let w = window();
        install_toasts(
            &w,
            vec![
                item("first", "one", "1m ago"),
                item("second", "two", "2m ago"),
                item("third", "three", "3m ago"),
            ],
        );

        let (seen, sink) = recorder();
        w.global::<crate::RemindersAdapter>()
            .on_toast_dismiss(move |uid| sink.borrow_mut().push(uid.to_string()));

        let x = by_label(&w, "Dismiss — leave the pane parked");
        assert_eq!(x.len(), 3, "one × per toast, in stack order");
        click(&w, &x[1]);
        assert_eq!(
            *seen.borrow(),
            vec!["second".to_string()],
            "the middle × must dismiss the middle toast"
        );
    });
}

/// The card body restores the pane. It shares the "click to restore" promise the second
/// line makes, so a body that reached nothing would make the toast a lie.
#[test]
fn the_toast_body_restores_that_reminder() {
    ui(|| {
        let w = window();
        install_toasts(
            &w,
            vec![
                item("first", "one", "1m ago"),
                item("second", "two", "2m ago"),
            ],
        );

        let (seen, sink) = recorder();
        w.global::<crate::RemindersAdapter>()
            .on_restore(move |uid| sink.borrow_mut().push(uid.to_string()));

        let bodies = by_label(&w, "Bring this parked pane back now");
        assert_eq!(bodies.len(), 2, "one clickable card per toast");
        click(&w, &bodies[0]);
        assert_eq!(
            *seen.borrow(),
            vec!["first".to_string()],
            "the first card must restore the first reminder"
        );
    });
}

/// Nothing fired means nothing on screen. The stack sits above the panes, so a card that
/// outlived its model would cover pane content the user is trying to click.
#[test]
fn with_nothing_fired_there_is_no_toast() {
    ui(|| {
        let w = window();
        install_toasts(&w, vec![]);
        assert!(
            by_label(&w, "Dismiss — leave the pane parked").is_empty(),
            "an empty toast model must draw no cards"
        );
    });
}
