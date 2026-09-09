//! Track V5k: renaming a project, and resuming one of its Claude sessions.
//!
//! Both callbacks carry an index the surrounding row supplies rather than the control that
//! fires them, which is exactly where an off-by-one hides: the rename box lives inside a
//! popup menu that outlives the row it was opened from, and the session row lives two
//! components below the project it belongs to. Each test therefore drives the **second**
//! project so a hard-coded zero cannot pass.

#![allow(unused_imports)]
use super::*;

/// Unfold a project so its worktrees and Claude history are instantiated. The chevron names
/// the project rather than the gesture, which is what makes this addressable at all.
fn expand(w: &crate::AppWindow, name: &str) {
    let tip = format!("Expand {name} — worktrees and Claude history");
    click(w, &only(w, &tip, AccessibleRole::Button));
    settle();
}

fn type_str(w: &crate::AppWindow, s: &str) {
    for ch in s.chars() {
        let t = slint::SharedString::from(ch);
        w.window()
            .dispatch_event(WindowEvent::KeyPressed { text: t.clone() });
        w.window()
            .dispatch_event(WindowEvent::KeyReleased { text: t });
    }
}

/// The rename field opens pre-selected with the project's current name in it, so typing
/// replaces rather than appends — and Enter commits. The index has to come from the row the
/// menu was opened from, not from whichever project is selected or first.
#[test]
fn the_rename_field_commits_the_typed_name_to_the_project_it_opened_from() {
    ui(|| {
        let w = window();
        install_projects(&w, false, 0);
        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::<(i32, String)>::new()));
        let seen = log.clone();
        w.on_rename_project(move |i, t| seen.borrow_mut().push((i, t.to_string())));

        right_click(&w, &only(&w, "claude-standards", AccessibleRole::ListItem));
        settle();
        assert_eq!(
            by_label(&w, "Rename project claude-standards").len(),
            1,
            "the field names the project whose menu this is"
        );

        type_str(&w, "std");
        w.window().dispatch_event(WindowEvent::KeyPressed {
            text: slint::SharedString::from(char::from(slint::platform::Key::Return)),
        });
        w.window().dispatch_event(WindowEvent::KeyReleased {
            text: slint::SharedString::from(char::from(slint::platform::Key::Return)),
        });
        settle();

        assert_eq!(*log.borrow(), vec![(1, "std".to_string())]);
    });
}

/// Resuming a session sends the project index *and* the session id. Neither alone is enough:
/// the id says which conversation to reopen, the index says which repo to open it in, and
/// resuming in the wrong working directory is a silent failure that looks like a working
/// feature until the session's first file edit lands in the wrong tree.
#[test]
fn resuming_a_session_names_both_its_project_and_its_id() {
    ui(|| {
        let w = window();
        install_projects(&w, true, 1);
        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::<(i32, String)>::new()));
        let seen = log.clone();
        w.on_resume_session(move |i, id| seen.borrow_mut().push((i, id.to_string())));

        expand(&w, "avada");
        click(
            &w,
            &only(
                &w,
                "Resume this Claude session in a new pane",
                AccessibleRole::Button,
            ),
        );

        assert_eq!(*log.borrow(), vec![(0, "abc123".to_string())]);
    });
}
