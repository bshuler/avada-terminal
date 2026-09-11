//! The module's state machine: everything the Files rail entry does, with no I/O in it.
//!
//! The only thing this file can touch is a [`Fs`], which the caller passes in. That is
//! deliberate twice over. It keeps every rule below testable against an in-memory tree,
//! and it keeps the borrow honest: the real `Fs` is the module's single connection to the
//! host, which `main` also needs to answer the request that is on the stack — so it is
//! lent for the length of a call and no longer.

use crate::tree::{self, FileRow, Fs};
use avada_module_sdk::contract::{ErrorCode, RpcError};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The commands this module registers. Kept in step with `avada.toml` by
/// `every_declared_command_is_dispatched`.
pub const COMMANDS: &[(&str, &str)] = &[
    ("up", "Files: Go up a directory"),
    ("refresh", "Files: Refresh"),
    ("reveal", "Files: Reveal a path"),
    ("filter", "Files: Filter"),
    ("set-root", "Files: Set the root directory"),
];

/// Everything the rows are drawn from.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct State {
    /// The directory the tree is rooted at, or `None` before the first activation.
    pub root: Option<PathBuf>,
    /// The workspace root the host reported, which `up` will not climb past on its own
    /// and which a reveal falls back to.
    pub workspace_root: Option<PathBuf>,
    /// Directories the human has opened.
    pub expanded: BTreeSet<PathBuf>,
    /// The filter box's text. Non-empty swaps the tree for ranked find results.
    pub query: String,
    /// The path the host should scroll to, carried to the row as the `selected` mark.
    pub selected: Option<PathBuf>,
    /// The flattened rows, rebuilt only on a real event — never per frame.
    pub rows: Vec<FileRow>,
    /// Whether the entry is on screen. A deactivated module holds no rows: the tree it
    /// was showing is stale the moment the human looks away, and re-reading it is cheap.
    pub active: bool,
}

/// What a request produced, for `main` to carry out against the connection.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Outcome {
    /// The JSON-RPC result.
    pub result: Value,
    /// Something to say on a toast.
    pub toast: Option<String>,
    /// A file the host should open in a pane (`host.panes.spawn { kind: "file" }`).
    pub open: Option<PathBuf>,
}

impl Outcome {
    fn empty() -> Self {
        Outcome {
            result: json!({}),
            ..Default::default()
        }
    }
}

/// The Files module.
#[derive(Debug, Default)]
pub struct App {
    /// The state the rows are drawn from.
    pub state: State,
}

impl App {
    /// A module that has not been activated yet: no root, no rows.
    pub fn new() -> Self {
        App::default()
    }

    /// Record the workspace the host reported. Does not re-root on its own — the human's
    /// own `up` or `set-root` outranks a workspace that has not actually changed.
    pub fn set_workspace(&mut self, root: Option<PathBuf>) {
        let changed = self.state.workspace_root != root;
        self.state.workspace_root = root.clone();
        if changed {
            self.state.root = root;
            self.state.expanded.clear();
            self.state.selected = None;
            self.state.query.clear();
        }
    }

    /// The entry came on screen: root at the workspace and list it.
    pub fn activate(&mut self, fs: &mut impl Fs) {
        self.state.active = true;
        if self.state.root.is_none() {
            self.state.root = self.state.workspace_root.clone();
        }
        self.rebuild(fs);
    }

    /// The entry went away. The rows go with it, so nothing stale is ever re-shown.
    pub fn deactivate(&mut self) {
        self.state.active = false;
        self.state.rows.clear();
    }

    /// Re-read whatever the current root, expansion set and query describe.
    pub fn rebuild(&mut self, fs: &mut impl Fs) {
        let Some(root) = self.state.root.clone() else {
            self.state.rows.clear();
            return;
        };
        self.state.rows = if self.state.query.trim().is_empty() {
            tree::flatten(fs, &root, &self.state.expanded)
        } else {
            tree::find(fs, &root, &self.state.query)
        };
    }

    /// Re-root the tree. Expansion and selection belong to the old root and do not
    /// survive it.
    pub fn set_root(&mut self, fs: &mut impl Fs, dir: PathBuf) {
        self.state.root = Some(dir);
        self.state.expanded.clear();
        self.state.selected = None;
        self.rebuild(fs);
    }

    /// Root one directory higher — the tree's only navigation that leaves the project, and
    /// the way out when the workspace root guessed too narrowly.
    pub fn up(&mut self, fs: &mut impl Fs) -> bool {
        let Some(root) = self.state.root.clone() else {
            return false;
        };
        let Some(parent) = root.parent().map(Path::to_path_buf) else {
            return false;
        };
        if parent == root {
            return false;
        }
        self.set_root(fs, parent);
        // The directory we came from stays open, so going up reads as zooming out rather
        // than as losing your place.
        self.state.expanded.insert(root);
        self.rebuild(fs);
        true
    }

    /// Open or shut a directory row.
    pub fn toggle(&mut self, fs: &mut impl Fs, path: &Path) {
        if !self.state.expanded.remove(path) {
            self.state.expanded.insert(path.to_path_buf());
        }
        self.rebuild(fs);
    }

    /// Set the filter text. Empty restores the tree.
    pub fn set_query(&mut self, fs: &mut impl Fs, q: &str) {
        if self.state.query == q {
            return;
        }
        self.state.query = q.to_string();
        self.rebuild(fs);
    }

    /// Show `path`: re-root if it is outside the current root, expand exactly the
    /// directories that lead to it, and mark it so the host scrolls there.
    ///
    /// `line`/`col` come from a `file:line:col` hit in a pane's output. The module has
    /// nowhere to put them — a tier-1 row list has no cursor — so they are handed straight
    /// back to the host with the file it opens.
    pub fn reveal(
        &mut self,
        fs: &mut impl Fs,
        path: &Path,
        line: Option<u32>,
        col: Option<u32>,
    ) -> Outcome {
        let is_dir = fs.is_dir(path);
        let inside = self
            .state
            .root
            .as_ref()
            .is_some_and(|r| path.starts_with(r));
        if !inside {
            // Re-root rather than refuse: a click in a pane running somewhere else is
            // exactly when the explorer should follow. The workspace root is preferred
            // over the path's own directory, so revealing one file does not shrink the
            // tree to the directory that file happens to live in.
            let base = match &self.state.workspace_root {
                Some(w) if path.starts_with(w) => w.clone(),
                _ if is_dir => path.to_path_buf(),
                _ => path.parent().map(Path::to_path_buf).unwrap_or_default(),
            };
            self.state.root = Some(base);
            self.state.expanded.clear();
        }
        // A reveal is an answer to "where is this", so it must not arrive filtered.
        self.state.query.clear();
        if let Some(root) = self.state.root.clone() {
            for dir in tree::ancestors_within(&root, path) {
                self.state.expanded.insert(dir);
            }
        }
        if is_dir {
            self.state.expanded.insert(path.to_path_buf());
        }
        self.state.selected = Some(path.to_path_buf());
        self.rebuild(fs);
        Outcome {
            result: json!({
                "path": path.display().to_string(),
                "line": line,
                "col": col,
                "revealed": self.selected_is_visible(),
            }),
            ..Default::default()
        }
    }

    /// Whether the selected path is among the rows the host is about to be given. A reveal
    /// into a directory the flatten truncated has nothing to scroll to, and says so rather
    /// than reporting success.
    pub fn selected_is_visible(&self) -> bool {
        let Some(sel) = self.state.selected.as_ref() else {
            return false;
        };
        self.state.root.as_deref() == Some(sel.as_path())
            || self.state.rows.iter().any(|r| &r.path == sel)
    }

    /// A registered command.
    pub fn command(
        &mut self,
        fs: &mut impl Fs,
        id: &str,
        args: &Value,
    ) -> Result<Outcome, RpcError> {
        match id {
            "up" => {
                if self.up(fs) {
                    Ok(Outcome::empty())
                } else {
                    Ok(Outcome {
                        result: json!({}),
                        toast: Some("Already at the top".into()),
                        open: None,
                    })
                }
            }
            "refresh" => {
                self.rebuild(fs);
                Ok(Outcome::empty())
            }
            "filter" => {
                let q = args["query"].as_str().unwrap_or_default().to_string();
                self.set_query(fs, &q);
                Ok(Outcome::empty())
            }
            "set-root" => {
                let path = str_arg(args, "path")?;
                self.set_root(fs, PathBuf::from(path));
                Ok(Outcome::empty())
            }
            "reveal" => {
                let path = str_arg(args, "path")?;
                Ok(self.reveal(
                    fs,
                    Path::new(&path),
                    args["line"].as_u64().map(|n| n as u32),
                    args["col"].as_u64().map(|n| n as u32),
                ))
            }
            other => Err(RpcError::new(
                ErrorCode::InvalidParams,
                format!("files has no command `{other}`"),
            )),
        }
    }

    /// One of the host's events (`host.events.subscribe`).
    ///
    /// An unknown kind is ignored on purpose: a newer host must be able to announce
    /// something this module has never heard of without it erroring out.
    pub fn event(&mut self, fs: &mut impl Fs, kind: &str, payload: &Value) {
        use avada_module_sdk::contract::methods::events;
        match kind {
            events::RAIL_QUERY if payload["entry"] == crate::rows::ENTRY => {
                let q = payload["query"].as_str().unwrap_or_default().to_string();
                self.set_query(fs, &q);
            }
            events::FILES_REVEAL => {
                if let Some(p) = payload["path"].as_str() {
                    self.reveal(
                        fs,
                        Path::new(p),
                        payload["line"].as_u64().map(|n| n as u32),
                        payload["col"].as_u64().map(|n| n as u32),
                    );
                }
            }
            _ => {}
        }
    }

    /// A row was clicked, double-clicked or right-clicked.
    ///
    /// `context` is not this module's business: the host already owns the path menu that
    /// the built-in browser used to open, and a module that answered it would have to
    /// reimplement "Open in…", "Copy path" and the rest to be no better. So it is a
    /// deliberate no-op here — the host acts on `data.path` without asking.
    pub fn row_activate(
        &mut self,
        fs: &mut impl Fs,
        data: &Value,
        gesture: &str,
    ) -> Result<Outcome, RpcError> {
        if gesture == "context" {
            return Ok(Outcome::empty());
        }
        let kind = data["kind"].as_str().unwrap_or_default();
        if kind == "up" {
            self.up(fs);
            return Ok(Outcome::empty());
        }
        let Some(path) = data["path"].as_str().map(PathBuf::from) else {
            // A note row. Inert by design.
            return Ok(Outcome::empty());
        };
        match (kind, gesture) {
            // A double click on a directory zooms into it: the same "open" the row menu
            // means, and the only way down the tree that does not deepen the indent.
            ("dir", "open") => {
                self.set_root(fs, path);
                Ok(Outcome::empty())
            }
            ("dir", _) => {
                self.toggle(fs, &path);
                Ok(Outcome::empty())
            }
            // A file is selected *and* opened. Selecting is what makes the highlight agree
            // with the pane that just appeared.
            (_, _) => {
                self.state.selected = Some(path.clone());
                Ok(Outcome {
                    result: json!({}),
                    toast: None,
                    open: Some(path),
                })
            }
        }
    }
}

fn str_arg(args: &Value, key: &str) -> Result<String, RpcError> {
    args[key]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            RpcError::new(
                ErrorCode::InvalidParams,
                format!("`{key}` is required and must be a non-empty string"),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::fake::FakeFs;
    use avada_module_sdk::contract::methods::events;

    fn started(paths: &[(&str, bool)]) -> (App, FakeFs) {
        let mut fs = FakeFs::tree(paths);
        let mut app = App::new();
        app.set_workspace(Some(PathBuf::from("/w")));
        app.activate(&mut fs);
        (app, fs)
    }

    fn labels(app: &App) -> Vec<String> {
        app.state.rows.iter().map(|r| r.label.clone()).collect()
    }

    #[test]
    fn activation_roots_at_the_workspace_and_lists_it() {
        let (app, _) = started(&[("/w/a.txt", false), ("/w/src/main.rs", false)]);
        assert_eq!(app.state.root, Some(PathBuf::from("/w")));
        assert_eq!(labels(&app), vec!["src", "a.txt"]);
    }

    #[test]
    fn deactivating_drops_the_rows_it_was_showing() {
        let (mut app, _) = started(&[("/w/a.txt", false)]);
        app.deactivate();
        assert!(app.state.rows.is_empty());
        assert!(!app.state.active);
    }

    #[test]
    fn a_click_opens_and_shuts_a_directory() {
        let (mut app, mut fs) = started(&[("/w/src/main.rs", false)]);
        let data = json!({ "kind": "dir", "path": "/w/src" });
        app.row_activate(&mut fs, &data, "toggle").unwrap();
        assert_eq!(labels(&app), vec!["src", "main.rs"]);
        app.row_activate(&mut fs, &data, "toggle").unwrap();
        assert_eq!(labels(&app), vec!["src"]);
    }

    #[test]
    fn a_double_click_on_a_directory_makes_it_the_root() {
        let (mut app, mut fs) = started(&[("/w/src/main.rs", false)]);
        app.row_activate(&mut fs, &json!({ "kind": "dir", "path": "/w/src" }), "open")
            .unwrap();
        assert_eq!(app.state.root, Some(PathBuf::from("/w/src")));
        assert_eq!(labels(&app), vec!["main.rs"]);
    }

    #[test]
    fn clicking_a_file_asks_the_host_for_a_pane_and_selects_the_row() {
        let (mut app, mut fs) = started(&[("/w/a.txt", false)]);
        let out = app
            .row_activate(
                &mut fs,
                &json!({ "kind": "file", "path": "/w/a.txt" }),
                "open",
            )
            .unwrap();
        assert_eq!(out.open, Some(PathBuf::from("/w/a.txt")));
        assert_eq!(app.state.selected, Some(PathBuf::from("/w/a.txt")));
    }

    #[test]
    fn the_context_gesture_is_the_hosts_and_the_module_does_not_answer_it() {
        let (mut app, mut fs) = started(&[("/w/a.txt", false)]);
        let before = app.state.clone();
        let out = app
            .row_activate(
                &mut fs,
                &json!({ "kind": "file", "path": "/w/a.txt" }),
                "context",
            )
            .unwrap();
        // No pane, no selection move, no re-read: the host owns the path menu.
        assert_eq!(out.open, None);
        assert_eq!(app.state, before);
    }

    #[test]
    fn a_note_row_carries_no_path_and_does_nothing() {
        let (mut app, mut fs) = started(&[("/w", true)]);
        let out = app.row_activate(&mut fs, &Value::Null, "open").unwrap();
        assert_eq!(out.open, None);
    }

    #[test]
    fn going_up_keeps_the_directory_you_came_from_open() {
        let (mut app, mut fs) = started(&[("/w/a.txt", false), ("/other.txt", false)]);
        assert!(app.up(&mut fs));
        assert_eq!(app.state.root, Some(PathBuf::from("/")));
        assert!(app.state.expanded.contains(&PathBuf::from("/w")));
        assert!(labels(&app).contains(&"a.txt".to_string()));
    }

    #[test]
    fn going_up_from_the_filesystem_root_says_so_instead_of_looping() {
        let mut fs = FakeFs::tree(&[("/a.txt", false)]);
        let mut app = App::new();
        app.set_workspace(Some(PathBuf::from("/")));
        app.activate(&mut fs);
        let out = app.command(&mut fs, "up", &json!({})).unwrap();
        assert_eq!(out.toast.as_deref(), Some("Already at the top"));
        assert_eq!(app.state.root, Some(PathBuf::from("/")));
    }

    #[test]
    fn the_filter_swaps_the_tree_for_ranked_results_and_empty_restores_it() {
        let (mut app, mut fs) = started(&[("/w/src/state.rs", false), ("/w/b.txt", false)]);
        app.event(
            &mut fs,
            events::RAIL_QUERY,
            &json!({ "entry": "files", "query": "state" }),
        );
        assert_eq!(labels(&app), vec!["state.rs"]);
        app.event(
            &mut fs,
            events::RAIL_QUERY,
            &json!({ "entry": "files", "query": "" }),
        );
        assert_eq!(labels(&app), vec!["src", "b.txt"]);
    }

    #[test]
    fn a_query_for_another_entry_is_not_ours_to_answer() {
        let (mut app, mut fs) = started(&[("/w/src/state.rs", false)]);
        app.event(
            &mut fs,
            events::RAIL_QUERY,
            &json!({ "entry": "git", "query": "state" }),
        );
        assert!(app.state.query.is_empty());
    }

    #[test]
    fn an_unknown_event_kind_is_ignored_rather_than_erroring() {
        let (mut app, mut fs) = started(&[("/w/a.txt", false)]);
        let before = app.state.clone();
        app.event(&mut fs, "something.new", &json!({ "path": "/w/a.txt" }));
        assert_eq!(app.state, before);
    }

    #[test]
    fn a_reveal_expands_exactly_the_directories_that_lead_to_the_file() {
        let (mut app, mut fs) = started(&[
            ("/w/a/b/deep.rs", false),
            ("/w/z/other.rs", false),
            ("/w/top.txt", false),
        ]);
        let out = app.reveal(&mut fs, Path::new("/w/a/b/deep.rs"), Some(12), Some(3));
        assert_eq!(out.result["line"], 12);
        assert_eq!(out.result["col"], 3);
        assert_eq!(out.result["revealed"], true);
        assert_eq!(app.state.selected, Some(PathBuf::from("/w/a/b/deep.rs")));
        // `a` and `a/b` are open; `z` is not.
        assert!(labels(&app).contains(&"deep.rs".to_string()));
        assert!(!labels(&app).contains(&"other.rs".to_string()));
    }

    #[test]
    fn a_reveal_clears_a_filter_that_would_have_hidden_the_answer() {
        let (mut app, mut fs) = started(&[("/w/a.txt", false), ("/w/b.txt", false)]);
        app.set_query(&mut fs, "a.txt");
        app.reveal(&mut fs, Path::new("/w/b.txt"), None, None);
        assert!(app.state.query.is_empty());
        assert!(labels(&app).contains(&"b.txt".to_string()));
    }

    #[test]
    fn a_reveal_outside_the_root_follows_the_path_home() {
        let (mut app, mut fs) = started(&[("/w/a.txt", false), ("/elsewhere/x/y.rs", false)]);
        app.reveal(&mut fs, Path::new("/elsewhere/x/y.rs"), None, None);
        // Not under the workspace, so the file's own directory becomes the root.
        assert_eq!(app.state.root, Some(PathBuf::from("/elsewhere/x")));
        assert!(labels(&app).contains(&"y.rs".to_string()));
    }

    #[test]
    fn a_reveal_back_inside_the_workspace_prefers_the_workspace_root() {
        let (mut app, mut fs) = started(&[("/w/a/b/deep.rs", false)]);
        app.set_root(&mut fs, PathBuf::from("/w/a/b"));
        app.reveal(&mut fs, Path::new("/w/a/b/deep.rs"), None, None);
        // Already inside: nothing re-roots, because a reveal that shrank the tree every
        // time would fight the human's own navigation.
        assert_eq!(app.state.root, Some(PathBuf::from("/w/a/b")));

        app.set_root(&mut fs, PathBuf::from("/w/a/b"));
        app.reveal(&mut fs, Path::new("/w/other.txt"), None, None);
        assert_eq!(app.state.root, Some(PathBuf::from("/w")));
    }

    #[test]
    fn revealing_a_directory_opens_it_rather_than_only_pointing_at_it() {
        let (mut app, mut fs) = started(&[("/w/deep/inner.rs", false)]);
        app.reveal(&mut fs, Path::new("/w/deep"), None, None);
        assert!(app.state.expanded.contains(&PathBuf::from("/w/deep")));
        assert!(labels(&app).contains(&"inner.rs".to_string()));
    }

    #[test]
    fn a_reveal_event_and_the_reveal_command_do_the_same_thing() {
        let (mut a, mut fsa) = started(&[("/w/a/b/deep.rs", false)]);
        let (mut b, mut fsb) = started(&[("/w/a/b/deep.rs", false)]);
        a.event(
            &mut fsa,
            events::FILES_REVEAL,
            &json!({ "path": "/w/a/b/deep.rs", "line": 4 }),
        );
        b.command(
            &mut fsb,
            "reveal",
            &json!({ "path": "/w/a/b/deep.rs", "line": 4 }),
        )
        .unwrap();
        assert_eq!(a.state, b.state);
    }

    #[test]
    fn set_root_without_a_path_is_a_parameter_error_not_a_root_of_nothing() {
        let (mut app, mut fs) = started(&[("/w/a.txt", false)]);
        let e = app.command(&mut fs, "set-root", &json!({})).unwrap_err();
        assert_eq!(e.kind(), ErrorCode::InvalidParams);
        assert_eq!(app.state.root, Some(PathBuf::from("/w")));
    }

    #[test]
    fn every_declared_command_is_dispatched() {
        // The manifest and the dispatch table are two lists that must not drift: a command
        // the host offers in the palette and the module then rejects is worse than one
        // that was never offered.
        let manifest = avada_module_sdk::manifest::Manifest::parse(crate::MANIFEST).unwrap();
        let declared: Vec<(String, String)> = manifest
            .contributions
            .iter()
            .filter(|c| {
                matches!(
                    c.kind,
                    avada_module_sdk::manifest::ContributionKind::Command
                )
            })
            .map(|c| (c.id.clone(), c.label.clone()))
            .collect();
        let dispatched: Vec<(String, String)> = COMMANDS
            .iter()
            .map(|(id, label)| (id.to_string(), label.to_string()))
            .collect();
        assert_eq!(declared, dispatched);

        let (mut app, mut fs) = started(&[("/w/a.txt", false)]);
        for (id, _) in COMMANDS {
            // Every one of them either works or complains about *its own* arguments.
            // None of them may come back "no such command".
            if let Err(e) = app.command(&mut fs, id, &json!({})) {
                assert_eq!(e.kind(), ErrorCode::InvalidParams, "{id}: {e:?}");
                assert!(
                    !e.message.contains("has no command"),
                    "{id} is declared but not dispatched"
                );
            }
        }
    }

    #[test]
    fn a_workspace_switch_re_roots_and_forgets_the_old_tree() {
        let (mut app, mut fs) = started(&[("/w/a.txt", false), ("/w2/b.txt", false)]);
        app.toggle(&mut fs, Path::new("/w"));
        app.set_workspace(Some(PathBuf::from("/w2")));
        app.activate(&mut fs);
        assert_eq!(app.state.root, Some(PathBuf::from("/w2")));
        assert!(app.state.expanded.is_empty());
        assert_eq!(labels(&app), vec!["b.txt"]);
    }

    #[test]
    fn the_same_workspace_arriving_twice_leaves_the_humans_navigation_alone() {
        let (mut app, mut fs) = started(&[("/w/src/main.rs", false)]);
        app.toggle(&mut fs, Path::new("/w/src"));
        app.set_workspace(Some(PathBuf::from("/w")));
        assert!(app.state.expanded.contains(&PathBuf::from("/w/src")));
    }

    #[test]
    fn a_directory_that_cannot_be_read_is_a_row_not_a_crash() {
        let mut fs = FakeFs::tree(&[("/w/locked", true)]);
        fs.errors
            .insert(PathBuf::from("/w/locked"), "Permission denied".into());
        let mut app = App::new();
        app.set_workspace(Some(PathBuf::from("/w")));
        app.activate(&mut fs);
        app.toggle(&mut fs, Path::new("/w/locked"));
        assert!(labels(&app).iter().any(|l| l.starts_with("Cannot read")));
    }
}
