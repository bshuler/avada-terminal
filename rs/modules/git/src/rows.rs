//! Turning the state into the host's tier-1 [`Row`] list.
//!
//! A tier-1 module draws nothing: it hands the host a flat list of rows and the host
//! decides what this looks like on this platform, in this theme, at this DPI. Everything
//! the module wants to say therefore has to fit into `label`, `detail`, `icon`, `marks`
//! and `data` — five vocabularies the contract fixes.
//!
//! The one thing here that is not cosmetic is `data.git`. It is the fact only this module
//! knows — which repository a row came from, and which revision it was listed FROM — and
//! it is what lets the HOST offer the diff. The module never spawns that process itself.

use crate::app::State;
use crate::git::{self, SECTIONS};
use avada_module_sdk::rail::Row;
use serde_json::{json, Value};

/// The rail entry id, matching `avada.toml`.
pub const ENTRY: &str = "git";

/// Row id of the head row: the branch, or the commit being shown.
pub const HEAD_ROW: &str = "head";

/// The mark the host scrolls to. `host.rows.set` carries no selection field on purpose
/// (see `docs/module-contract.md` §10.1); a mark is how a module says "this one".
pub const MARK_SELECTED: &str = "selected";

/// The `data.git` every row that belongs to a repository carries.
fn origin(root: &str, rev: Option<&str>, short: Option<&str>) -> Value {
    json!({ "root": root, "rev": rev, "short": short })
}

/// The whole row list for the current state.
pub fn rows(state: &State) -> Vec<Row> {
    if let Some(e) = &state.error {
        return vec![note("error", e)];
    }
    if let Some(rev) = &state.missing {
        return vec![note("missing", &format!("No commit {rev}"))];
    }
    match &state.commit {
        Some(c) => commit_rows(state, c),
        None => tree_rows(state),
    }
}

/// The working tree: a head row, then the three sections in reading order.
fn tree_rows(state: &State) -> Vec<Row> {
    let Some(root) = state.status.root.as_deref().filter(|_| state.status.repo) else {
        return vec![note("no-repo", "Not a git repository")];
    };
    let mut out = Vec::with_capacity(state.status.rows.len() + 4);
    out.push(head_row(
        if state.status.summary.is_empty() {
            "HEAD"
        } else {
            &state.status.summary
        },
        String::new(),
        root,
        None,
        None,
    ));

    let mut any = false;
    for (wire, title) in SECTIONS {
        let files: Vec<_> = state
            .status
            .rows
            .iter()
            .filter(|r| r.section == wire && matches(state, &r.path))
            .collect();
        if files.is_empty() {
            continue;
        }
        any = true;
        out.push(section(title, files.len()));
        for r in files {
            out.push(file_row(
                state,
                &git::abs(root, &r.path),
                &r.label,
                &r.code,
                root,
                None,
                None,
            ));
        }
    }
    if !any {
        out.push(note(
            "clean",
            if state.query.trim().is_empty() {
                "Nothing to commit"
            } else {
                "No matching changes"
            },
        ));
    }
    out
}

/// A commit: its subject as the head row, then the files it touched.
fn commit_rows(state: &State, c: &crate::git::Commit) -> Vec<Row> {
    let root = c.root.as_deref().unwrap_or_default();
    let rev = Some(c.hash.as_str());
    let short = Some(c.short.as_str());
    let mut out = Vec::with_capacity(c.files.len() + 2);
    out.push(head_row(
        &format!("{}  {}", c.short, c.subject),
        // The host draws `detail` dimmed after the label, which is exactly the weight
        // authorship and date deserve next to the subject.
        format!("{}  {}", c.author, c.date),
        root,
        rev,
        short,
    ));
    let files: Vec<_> = c.files.iter().filter(|f| matches(state, &f.path)).collect();
    if files.is_empty() {
        out.push(note("empty", "No files in this commit"));
        return out;
    }
    out.push(section("Files", files.len()));
    for f in files {
        out.push(file_row(
            state,
            &git::abs(root, &f.path),
            &f.label,
            &f.code,
            root,
            rev,
            short,
        ));
    }
    out
}

/// The repository row. Right-clicking it is how a human reaches the whole-tree or
/// whole-commit diff: its path IS the root, so the host's menu strips it to nothing and
/// asks for the diff with no path filter — which is what the deleted panel's header
/// button did.
fn head_row(
    label: &str,
    detail: String,
    root: &str,
    rev: Option<&str>,
    short: Option<&str>,
) -> Row {
    Row {
        id: HEAD_ROW.into(),
        label: label.into(),
        detail,
        depth: 0,
        expandable: false,
        expanded: false,
        icon: Some("git".into()),
        marks: Vec::new(),
        data: json!({
            "kind": if rev.is_some() { "commit" } else { "root" },
            "path": root,
            "git": origin(root, rev, short),
        }),
    }
}

/// A section heading. No `path`, which is precisely how the host knows there is no file
/// menu to draw over it.
fn section(title: &str, count: usize) -> Row {
    Row {
        id: format!("section:{title}"),
        label: title.into(),
        detail: count.to_string(),
        depth: 0,
        expandable: false,
        expanded: false,
        icon: None,
        marks: Vec::new(),
        data: json!({ "kind": "section" }),
    }
}

#[allow(clippy::too_many_arguments)]
fn file_row(
    state: &State,
    path: &str,
    label: &str,
    code: &str,
    root: &str,
    rev: Option<&str>,
    short: Option<&str>,
) -> Row {
    let mut marks = Vec::new();
    if state.selected.as_deref() == Some(path) {
        marks.push(MARK_SELECTED.to_string());
    }
    Row {
        // The path is the identity. An index would move under a click the moment a file
        // above it was staged.
        id: path.into(),
        label: label.into(),
        // git's status letter, not the parent directory: in a 260px panel the letter is
        // the thing a human is scanning for, and the directory is already in the path the
        // host shows on hover.
        detail: code.into(),
        depth: 1,
        expandable: false,
        expanded: false,
        icon: None,
        marks,
        data: json!({
            "kind": "file",
            "path": path,
            "code": code,
            "git": origin(root, rev, short),
        }),
    }
}

/// An inert message row: no `data.path`, so `module.row.activate` has nothing to open and
/// the host has no file menu to draw.
fn note(id: &str, label: &str) -> Row {
    Row {
        id: id.into(),
        label: label.into(),
        detail: String::new(),
        depth: 0,
        expandable: false,
        expanded: false,
        icon: None,
        marks: Vec::new(),
        data: Value::Null,
    }
}

/// Whether a repo-relative path survives the filter box. Case-insensitive substring, over
/// the whole path rather than the file name: `src/` is the filter people actually type.
fn matches(state: &State, path: &str) -> bool {
    let q = state.query.trim().to_lowercase();
    q.is_empty() || path.to_lowercase().contains(&q)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{Commit, CommitFile, Status, StatusRow};

    fn changed(path: &str, code: &str, section: &str) -> StatusRow {
        let (detail, label) = match path.rsplit_once('/') {
            Some((d, n)) => (d.to_string(), n.to_string()),
            None => (String::new(), path.to_string()),
        };
        StatusRow {
            path: path.into(),
            label,
            detail,
            code: code.into(),
            section: section.into(),
        }
    }

    fn a_tree() -> State {
        State {
            workspace_root: Some("/proj".into()),
            status: Status {
                repo: true,
                root: Some("/proj".into()),
                branch: "main".into(),
                upstream: Some("origin/main".into()),
                ahead: 2,
                behind: 1,
                summary: "main  ↑2  ↓1".into(),
                rows: vec![
                    changed("src/main.rs", "M", "staged"),
                    changed("README.md", "M", "changed"),
                    changed("notes.txt", "?", "untracked"),
                ],
            },
            active: true,
            ..Default::default()
        }
    }

    fn labels(rows: &[Row]) -> Vec<String> {
        rows.iter().map(|r| r.label.clone()).collect()
    }

    /// The shape the deleted built-in mode drew: the branch on top, then the three
    /// sections in the order a human reads them, each file under its heading.
    #[test]
    fn a_working_tree_is_a_head_row_and_three_sections_in_order() {
        let rows = rows(&a_tree());
        assert_eq!(
            labels(&rows),
            [
                "main  ↑2  ↓1",
                "Staged",
                "main.rs",
                "Changed",
                "README.md",
                "Untracked",
                "notes.txt"
            ]
        );
        assert!(rows.iter().all(|r| r.depth <= 1));
        assert_eq!(rows[2].depth, 1, "files sit under their heading");
        assert_eq!(rows[2].detail, "M", "the status letter is the detail");
    }

    /// Paths reach the host absolute, because the host's own file menu finds the
    /// repo-relative part again by stripping the root off.
    #[test]
    fn a_file_row_carries_the_absolute_path_and_its_origin() {
        let rows = rows(&a_tree());
        let r = &rows[2];
        assert_eq!(r.id, "/proj/src/main.rs");
        assert_eq!(r.data["path"], json!("/proj/src/main.rs"));
        assert_eq!(r.data["kind"], json!("file"));
        assert_eq!(
            r.data["git"],
            json!({ "root": "/proj", "rev": null, "short": null }),
            "a null rev is how a row says `the working tree`"
        );
    }

    /// Section headings must not carry a path. A path is what makes the host draw its file
    /// menu, and "Staged" is not a file.
    #[test]
    fn a_heading_carries_no_path() {
        let rows = rows(&a_tree());
        for r in rows.iter().filter(|r| r.data["kind"] == json!("section")) {
            assert!(r.data.get("path").is_none(), "{} carries a path", r.label);
        }
    }

    /// The head row's path IS the root, which is how right-clicking it reaches the
    /// whole-tree diff the panel's header button used to offer.
    #[test]
    fn the_head_row_is_the_repository_itself() {
        let rows = rows(&a_tree());
        assert_eq!(rows[0].data["path"], json!("/proj"));
        assert_eq!(rows[0].data["kind"], json!("root"));
        assert_eq!(rows[0].data["git"]["root"], json!("/proj"));
    }

    /// An empty section is absent, not an empty heading — and a clean tree says so once
    /// rather than printing three empty headings.
    #[test]
    fn a_clean_tree_says_so_once() {
        let mut state = a_tree();
        state.status.rows.clear();
        let rows = rows(&state);
        assert_eq!(labels(&rows), ["main  ↑2  ↓1", "Nothing to commit"]);
    }

    /// The filter is a view: it hides rows, and a heading whose files all went with them.
    /// It also has its own empty message, because "Nothing to commit" would be a lie.
    #[test]
    fn filtering_hides_rows_and_the_headings_they_emptied() {
        let mut state = a_tree();
        state.query = "READ".into();
        assert_eq!(
            labels(&rows(&state)),
            ["main  ↑2  ↓1", "Changed", "README.md"]
        );

        state.query = "zzz".into();
        assert_eq!(
            labels(&rows(&state)),
            ["main  ↑2  ↓1", "No matching changes"]
        );
    }

    /// A commit's rows say which revision they came from, in every file row. That is the
    /// fact the host turns into "Show Diff in abc1234".
    #[test]
    fn a_commit_labels_every_row_with_its_revision() {
        let mut state = a_tree();
        state.commit = Some(Commit {
            found: true,
            root: Some("/proj".into()),
            hash: "abc1234def".into(),
            short: "abc1234".into(),
            subject: "the subject".into(),
            author: "T".into(),
            date: "2026-09-08".into(),
            files: vec![CommitFile {
                path: "src/a.rs".into(),
                label: "a.rs".into(),
                detail: "src".into(),
                code: "M".into(),
            }],
        });
        let rows = rows(&state);
        assert_eq!(labels(&rows), ["abc1234  the subject", "Files", "a.rs"]);
        assert_eq!(rows[0].data["kind"], json!("commit"));
        assert_eq!(
            rows[2].data["git"],
            json!({ "root": "/proj", "rev": "abc1234def", "short": "abc1234" })
        );
    }

    /// A directory that is not a repository is a sentence, not an empty panel. Silence
    /// there reads as "loading" forever.
    #[test]
    fn a_plain_directory_says_it_is_not_a_repository() {
        let state = State::default();
        let rows = rows(&state);
        assert_eq!(labels(&rows), ["Not a git repository"]);
        assert!(rows[0].data.is_null(), "nothing to activate");
    }

    /// A failed call outranks everything: it is the reason the rows are missing, and
    /// showing a stale tree under it would hide that.
    #[test]
    fn a_failure_replaces_the_listing() {
        let mut state = a_tree();
        state.error = Some("git.read was not permitted".into());
        assert_eq!(labels(&rows(&state)), ["git.read was not permitted"]);
    }

    /// The selected path is marked, which is how the host is told what to scroll to.
    #[test]
    fn the_selected_row_is_marked() {
        let mut state = a_tree();
        state.selected = Some("/proj/README.md".into());
        let marked: Vec<_> = rows(&state)
            .into_iter()
            .filter(|r| r.marks.iter().any(|m| m == MARK_SELECTED))
            .map(|r| r.label)
            .collect();
        assert_eq!(marked, ["README.md"]);
    }
}
