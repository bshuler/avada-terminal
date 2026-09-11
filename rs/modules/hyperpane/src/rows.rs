//! The rail rows: what the Hyperpane entry looks like in the left panel.
//!
//! Two rows, always the same two. This entry is not a browser — it is a switch and a
//! signpost — so the row list is a pure function of [`crate::app::State`] and needs no
//! I/O at all. The filter from `rail.query` narrows it, because the filter box is shared
//! across the whole rail and an entry that ignored it would look broken next to one that
//! does not.

use avada_module_sdk::rail::Row;

use crate::app::{Action, State};

/// The rail entry id this module owns. Matches `[[contributions]] kind = "rail"`.
pub const ENTRY: &str = "hyperpane";

/// Row ids, so the tests and the activate arm agree on the spelling.
pub const SESSION: &str = "session";
/// See [`SESSION`].
pub const DIRECTORY: &str = "directory";

/// The rows for `state`, filtered by `state.filter`.
pub fn rows(state: &State) -> Vec<Row> {
    let running = state.pane.is_some();
    let all = vec![
        Row {
            id: SESSION.into(),
            label: "Hyperpane".into(),
            detail: if running {
                "running".into()
            } else {
                "not started".into()
            },
            // A mark, not a colour: the host decides what "modified" looks like, and a
            // module that picked its own would be the only thing on the rail that did.
            marks: if running {
                vec!["modified".into()]
            } else {
                Vec::new()
            },
            data: serde_json::json!({ "action": "open" }),
            depth: 0,
            expandable: false,
            expanded: false,
            icon: None,
        },
        Row {
            id: DIRECTORY.into(),
            label: dir_label(state),
            detail: state.dir.display().to_string(),
            data: serde_json::json!({ "action": "reveal" }),
            depth: 0,
            expandable: false,
            expanded: false,
            icon: None,
            marks: Vec::new(),
        },
    ];
    if state.filter.trim().is_empty() {
        return all;
    }
    let needle = state.filter.to_lowercase();
    all.into_iter()
        .filter(|r| {
            r.label.to_lowercase().contains(&needle) || r.detail.to_lowercase().contains(&needle)
        })
        .collect()
}

/// The last path component, or the whole path when there isn't one (`/`).
fn dir_label(state: &State) -> String {
    state
        .dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| state.dir.display().to_string())
}

/// What clicking a row means. `None` for a row this module does not own — a host that
/// echoed back something else must not make the module guess.
pub fn action_for(data: &serde_json::Value, gesture: &str) -> Option<Action> {
    match (data["action"].as_str()?, gesture) {
        // Alt-click on the session row is the restart gesture: same row, harder verb.
        ("open", "alt") => Some(Action::Restart),
        ("open", _) => Some(Action::Open),
        ("reveal", _) => Some(Action::Reveal),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn state() -> State {
        State {
            dir: PathBuf::from("/data/bshuler__avada-hyperpane"),
            pane: None,
            filter: String::new(),
        }
    }

    #[test]
    fn the_session_row_says_whether_the_pane_is_open() {
        let mut s = state();
        let cold = rows(&s);
        assert_eq!(cold.len(), 2);
        assert_eq!(cold[0].detail, "not started");
        assert!(cold[0].marks.is_empty());
        assert_eq!(cold[1].label, "bshuler__avada-hyperpane");

        s.pane = Some("p-1".into());
        let hot = rows(&s);
        assert_eq!(hot[0].detail, "running");
        assert_eq!(hot[0].marks, ["modified"]);
    }

    #[test]
    fn the_filter_narrows_on_label_and_on_path() {
        let mut s = state();
        s.filter = "HYPER".into();
        assert_eq!(
            rows(&s).iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["session", "directory"],
            "the directory row matches on its path, which contains the module id"
        );
        s.filter = "nothing here".into();
        assert!(rows(&s).is_empty());
    }

    #[test]
    fn a_row_gesture_becomes_an_action_and_an_unknown_one_becomes_nothing() {
        let open = serde_json::json!({ "action": "open" });
        assert!(matches!(action_for(&open, "open"), Some(Action::Open)));
        assert!(matches!(action_for(&open, "alt"), Some(Action::Restart)));
        assert!(matches!(
            action_for(&serde_json::json!({ "action": "reveal" }), "open"),
            Some(Action::Reveal)
        ));
        assert!(action_for(&serde_json::json!({ "action": "eject" }), "open").is_none());
        assert!(action_for(&serde_json::Value::Null, "open").is_none());
    }
}
