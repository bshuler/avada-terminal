//! The module's state machine: everything the Git rail entry does, with no I/O in it.
//!
//! The only thing this file can touch is a [`Host`], which the caller passes in — the same
//! shape `avada-files` uses for the filesystem, and for the same two reasons. It keeps
//! every rule below testable against a scripted host, and it keeps the borrow honest: the
//! real `Host` is the module's single connection, which `main` also needs to answer the
//! request that is on the stack, so it is lent for the length of a call and no longer.

use crate::git::{Commit, Host, Status};
use avada_module_sdk::contract::{ErrorCode, RpcError};
use serde_json::{json, Value};

/// The commands this module registers. Kept in step with `avada.toml` by
/// `every_declared_command_is_dispatched`.
pub const COMMANDS: &[(&str, &str)] = &[
    ("refresh", "Git: Refresh"),
    ("show-commit", "Git: Show a commit"),
    ("working-tree", "Git: Back to the working tree"),
    ("filter", "Git: Filter"),
];

/// Everything the rows are drawn from.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct State {
    /// The workspace root the host reported; every question is asked about this path.
    pub workspace_root: Option<String>,
    /// The working tree as of the last refresh.
    pub status: Status,
    /// The commit being shown instead of the working tree, if any. `Some` is the commit
    /// view; `None` is the working tree, and there is no third state.
    pub commit: Option<Commit>,
    /// A revision that did not resolve, kept so the panel can say so instead of silently
    /// showing the working tree again.
    pub missing: Option<String>,
    /// The last conversational failure — a refused capability, a broken call — as a row.
    pub error: Option<String>,
    /// The filter box's text.
    pub query: String,
    /// The path the host should scroll to, carried to its row as the `selected` mark.
    pub selected: Option<String>,
    /// Whether the entry is on screen.
    pub active: bool,
}

impl State {
    /// The repository the rows currently describe: the commit's own root when a commit is
    /// shown, because a link can send us into a repository that is not the workspace.
    pub fn root(&self) -> Option<&str> {
        match &self.commit {
            Some(c) => c.root.as_deref(),
            None => self.status.root.as_deref(),
        }
    }

    /// The revision the rows were listed from — `None` for the working tree. This is the
    /// single fact the host cannot work out for itself, and the reason every file row
    /// carries a `data.git`.
    pub fn rev(&self) -> Option<&str> {
        self.commit.as_ref().map(|c| c.hash.as_str())
    }
}

/// What a request produced, for `main` to carry out against the connection.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// The JSON-RPC result.
    pub result: Value,
    /// Something to say on a toast.
    pub toast: Option<String>,
    /// A file the host should open in a pane.
    pub open: Option<String>,
}

impl Outcome {
    fn empty() -> Self {
        Outcome {
            result: json!({}),
            ..Default::default()
        }
    }
}

/// The Git module.
#[derive(Debug, Default)]
pub struct App {
    /// The state the rows are drawn from.
    pub state: State,
}

impl App {
    /// A module that has not been activated yet: no repository, no rows.
    pub fn new() -> Self {
        App::default()
    }

    /// Record the workspace the host reported. A genuinely new workspace drops the commit
    /// view with it: a hash listed from the old project means nothing in the new one.
    pub fn set_workspace(&mut self, root: Option<String>) {
        if self.state.workspace_root == root {
            return;
        }
        self.state.workspace_root = root;
        self.state.commit = None;
        self.state.missing = None;
        self.state.selected = None;
        self.state.query.clear();
        self.state.status = Status::default();
    }

    /// The entry came on screen. A working tree is stale the moment you look away, so
    /// coming back always re-reads it.
    pub fn activate(&mut self, host: &mut impl Host) {
        self.state.active = true;
        self.refresh(host);
    }

    /// The entry went away. The rows go with it, so nothing stale is ever re-shown.
    pub fn deactivate(&mut self) {
        self.state.active = false;
        self.state.status = Status::default();
    }

    /// Re-read whatever the panel is currently showing — the working tree, or the commit
    /// it was sent to. Refreshing a commit is not pointless: a commit that was rewritten
    /// out from under us should stop claiming to exist.
    pub fn refresh(&mut self, host: &mut impl Host) {
        self.state.error = None;
        let path = self.state.workspace_root.clone();
        if let Some(rev) = self.state.rev().map(str::to_string) {
            self.load_commit(host, &rev);
            return;
        }
        match host.status(path.as_deref()) {
            Ok(s) => self.state.status = s,
            Err(e) => {
                self.state.status = Status::default();
                self.state.error = Some(e);
            }
        }
    }

    /// Show a revision instead of the working tree.
    fn load_commit(&mut self, host: &mut impl Host, rev: &str) {
        // The commit's own root when we already have one, so a second look at a commit we
        // were linked into keeps asking the repository it actually lives in.
        let scope = self
            .state
            .root()
            .map(str::to_string)
            .or_else(|| self.state.workspace_root.clone());
        match host.commit(rev, scope.as_deref()) {
            Ok(c) if c.found => {
                self.state.commit = Some(c);
                self.state.missing = None;
                self.state.error = None;
            }
            Ok(_) => {
                self.state.commit = None;
                self.state.missing = Some(rev.to_string());
            }
            Err(e) => {
                self.state.error = Some(e);
            }
        }
    }

    /// Back to the working tree. Always re-reads: the tree has moved on while a commit
    /// was on screen, and showing the listing from before the detour would be a lie.
    pub fn working_tree(&mut self, host: &mut impl Host) {
        self.state.commit = None;
        self.state.missing = None;
        self.state.selected = None;
        self.refresh(host);
    }

    /// Set the filter text. Purely a view over rows already fetched — filtering must never
    /// be a reason to ask git anything.
    pub fn set_query(&mut self, query: &str) {
        self.state.query = query.to_string();
    }

    /// `module.command.invoke`.
    pub fn command(
        &mut self,
        host: &mut impl Host,
        id: &str,
        args: &Value,
    ) -> Result<Outcome, RpcError> {
        match id {
            "refresh" => {
                self.refresh(host);
                Ok(Outcome::empty())
            }
            "working-tree" => {
                self.working_tree(host);
                Ok(Outcome::empty())
            }
            "filter" => {
                // A missing `query` clears the filter; a missing `rev` below does not
                // default, because guessing a revision would show the wrong commit.
                self.set_query(args["query"].as_str().unwrap_or_default());
                Ok(Outcome::empty())
            }
            "show-commit" => {
                let rev = str_arg(args, "rev")?;
                self.load_commit(host, &rev);
                if let Some(bad) = self.state.missing.as_deref() {
                    return Ok(Outcome {
                        result: json!({ "found": false }),
                        toast: Some(format!("No commit {bad}")),
                        open: None,
                    });
                }
                Ok(Outcome {
                    result: json!({ "found": self.state.commit.is_some() }),
                    ..Default::default()
                })
            }
            other => Err(RpcError::new(
                ErrorCode::InvalidParams,
                format!("git has no command {other}"),
            )),
        }
    }

    /// `module.event`. Unknown kinds are ignored on purpose: a host is allowed to announce
    /// something a module built against an older SDK has never heard of.
    pub fn event(&mut self, host: &mut impl Host, kind: &str, payload: &Value) {
        match kind {
            avada_module_sdk::contract::methods::events::RAIL_QUERY => {
                self.set_query(payload["query"].as_str().unwrap_or_default());
            }
            crate::git::GIT_COMMIT_EVENT => {
                let Some(rev) = payload["rev"].as_str().filter(|r| !r.is_empty()) else {
                    return;
                };
                // The host says which repository the hash came from, and it need not be
                // the workspace: a link in a pane can point at a sibling checkout.
                if let Some(root) = payload["root"].as_str().filter(|r| !r.is_empty()) {
                    self.state.commit = Some(Commit {
                        root: Some(root.to_string()),
                        ..Default::default()
                    });
                }
                self.load_commit(host, rev);
            }
            _ => {}
        }
    }

    /// `module.row.activate`.
    pub fn row_activate(
        &mut self,
        _host: &mut impl Host,
        data: &Value,
        gesture: &str,
    ) -> Result<Outcome, RpcError> {
        // A right-click is a deliberate no-op. The host draws the path menu over our rows
        // (§10.6) — including the diff, which spawns a process and is therefore its verb,
        // not ours. Answering it here would put two menus on one click.
        if gesture == "context" {
            return Ok(Outcome::empty());
        }
        let Some(path) = data["path"].as_str().filter(|p| !p.is_empty()) else {
            // A section heading. Nothing to open, and nothing wrong either.
            return Ok(Outcome::empty());
        };
        if data["kind"] == json!("file") {
            self.state.selected = Some(path.to_string());
            return Ok(Outcome {
                result: json!({}),
                toast: None,
                open: Some(path.to_string()),
            });
        }
        Ok(Outcome::empty())
    }
}

/// A required string argument, refused rather than guessed when it is missing or blank.
fn str_arg(args: &Value, key: &str) -> Result<String, RpcError> {
    args[key]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            RpcError::new(
                ErrorCode::InvalidParams,
                format!("git: {key} is required and must not be blank"),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{Commit, CommitFile, StatusRow};

    /// A host that answers from a script and remembers what it was asked. Nothing here
    /// runs git; the point of the trait is that nothing in the module can.
    #[derive(Default)]
    struct Fake {
        status: Status,
        commits: Vec<Commit>,
        asked: Vec<String>,
        fail: Option<String>,
    }

    impl Host for Fake {
        fn status(&mut self, path: Option<&str>) -> Result<Status, String> {
            self.asked
                .push(format!("status {}", path.unwrap_or("<none>")));
            match &self.fail {
                Some(e) => Err(e.clone()),
                None => Ok(self.status.clone()),
            }
        }
        fn commit(&mut self, rev: &str, path: Option<&str>) -> Result<Commit, String> {
            self.asked
                .push(format!("commit {rev} in {}", path.unwrap_or("<none>")));
            if let Some(e) = &self.fail {
                return Err(e.clone());
            }
            Ok(self
                .commits
                .iter()
                .find(|c| c.hash == rev)
                .cloned()
                .unwrap_or_default())
        }
    }

    fn a_status() -> Status {
        Status {
            repo: true,
            root: Some("/proj".into()),
            branch: "main".into(),
            upstream: Some("origin/main".into()),
            ahead: 2,
            behind: 0,
            summary: "main  ↑2".into(),
            rows: vec![
                StatusRow {
                    path: "src/main.rs".into(),
                    label: "main.rs".into(),
                    detail: "src".into(),
                    code: "M".into(),
                    section: "staged".into(),
                },
                StatusRow {
                    path: "README.md".into(),
                    label: "README.md".into(),
                    detail: String::new(),
                    code: "M".into(),
                    section: "changed".into(),
                },
            ],
        }
    }

    fn a_commit() -> Commit {
        Commit {
            found: true,
            root: Some("/other".into()),
            hash: "abc1234def".into(),
            short: "abc1234".into(),
            subject: "the subject".into(),
            author: "T".into(),
            date: "2026-09-08".into(),
            files: vec![CommitFile {
                path: "a.rs".into(),
                label: "a.rs".into(),
                detail: String::new(),
                code: "M".into(),
            }],
        }
    }

    fn app_on(host: &mut Fake) -> App {
        let mut app = App::new();
        app.set_workspace(Some("/proj".into()));
        app.activate(host);
        app
    }

    /// Every command in `avada.toml` must be dispatched, and every dispatched command must
    /// be declared — the two lists drift apart silently otherwise, and a command that is
    /// declared but unhandled is a palette entry that does nothing.
    #[test]
    fn every_declared_command_is_dispatched() {
        let manifest = crate::MANIFEST;
        for (id, label) in COMMANDS {
            assert!(
                manifest.contains(&format!("id = \"{id}\"")),
                "{id} is dispatched but not declared in avada.toml"
            );
            assert!(
                manifest.contains(&format!("label = \"{label}\"")),
                "{label} is not the label avada.toml gives {id}"
            );
        }
        let declared = manifest.matches("kind = \"command\"").count();
        assert_eq!(
            declared,
            COMMANDS.len(),
            "avada.toml declares {declared} commands, the app dispatches {}",
            COMMANDS.len()
        );
    }

    /// Activating asks the host about the workspace — and asks it EVERY time, because a
    /// working tree changes while the human is looking at something else.
    #[test]
    fn activating_re_reads_the_working_tree() {
        let mut host = Fake {
            status: a_status(),
            ..Default::default()
        };
        let mut app = app_on(&mut host);
        assert_eq!(app.state.status.rows.len(), 2);
        app.deactivate();
        assert!(app.state.status.rows.is_empty(), "nothing stale is kept");
        app.activate(&mut host);
        assert_eq!(host.asked, ["status /proj", "status /proj"]);
    }

    /// A refused capability or a dead socket is a state the panel can show, not a panic
    /// and not an empty tree pretending the project is clean.
    #[test]
    fn a_failed_call_becomes_something_the_panel_can_say() {
        let mut host = Fake {
            status: a_status(),
            fail: Some("git.read was not permitted".into()),
            ..Default::default()
        };
        let app = app_on(&mut host);
        assert_eq!(
            app.state.error.as_deref(),
            Some("git.read was not permitted")
        );
        assert!(app.state.status.rows.is_empty());
    }

    /// The commit link, end to end: the event carries the repository the hash came from,
    /// the module asks about THAT repository rather than the workspace, and the rows then
    /// describe the commit.
    #[test]
    fn a_commit_event_lists_the_commit_in_its_own_repository() {
        let mut host = Fake {
            status: a_status(),
            commits: vec![a_commit()],
            ..Default::default()
        };
        let mut app = app_on(&mut host);
        app.event(
            &mut host,
            crate::git::GIT_COMMIT_EVENT,
            &json!({ "root": "/other", "rev": "abc1234def", "short": "abc1234" }),
        );
        assert_eq!(app.state.root(), Some("/other"));
        assert_eq!(app.state.rev(), Some("abc1234def"));
        assert_eq!(
            host.asked.last().unwrap(),
            "commit abc1234def in /other",
            "the link's own repository, not the workspace"
        );
    }

    /// A hash that does not resolve must SAY so. Falling back to the working tree would
    /// leave a human staring at a listing that has nothing to do with what they clicked.
    #[test]
    fn a_revision_that_does_not_resolve_is_reported_not_hidden() {
        let mut host = Fake {
            status: a_status(),
            ..Default::default()
        };
        let mut app = app_on(&mut host);
        let out = app
            .command(&mut host, "show-commit", &json!({ "rev": "deadbee" }))
            .expect("a resolvable question, even with an unresolvable answer");
        assert_eq!(out.result, json!({ "found": false }));
        assert_eq!(out.toast.as_deref(), Some("No commit deadbee"));
        assert_eq!(app.state.missing.as_deref(), Some("deadbee"));
        assert!(app.state.commit.is_none());
    }

    /// A blank revision is refused rather than guessed: `HEAD` would be a plausible
    /// default and the wrong commit.
    #[test]
    fn show_commit_refuses_a_blank_revision() {
        let mut host = Fake::default();
        let mut app = App::new();
        let e = app
            .command(&mut host, "show-commit", &json!({ "rev": "  " }))
            .expect_err("blank is not a revision");
        assert_eq!(e.kind(), ErrorCode::InvalidParams);
        assert!(host.asked.is_empty(), "nothing was asked of git");
    }

    /// Going back re-reads. The tree moved on while the commit was on screen.
    #[test]
    fn going_back_to_the_working_tree_re_reads_it() {
        let mut host = Fake {
            status: a_status(),
            commits: vec![a_commit()],
            ..Default::default()
        };
        let mut app = app_on(&mut host);
        app.command(&mut host, "show-commit", &json!({ "rev": "abc1234def" }))
            .unwrap();
        assert!(app.state.commit.is_some());
        app.command(&mut host, "working-tree", &json!({})).unwrap();
        assert!(app.state.commit.is_none());
        assert_eq!(app.state.rev(), None);
        assert_eq!(host.asked.last().unwrap(), "status /proj");
    }

    /// Refreshing while a commit is shown refreshes THE COMMIT. Silently dropping back to
    /// the working tree would make refresh a navigation.
    #[test]
    fn refreshing_a_commit_view_stays_on_the_commit() {
        let mut host = Fake {
            status: a_status(),
            commits: vec![a_commit()],
            ..Default::default()
        };
        let mut app = app_on(&mut host);
        app.command(&mut host, "show-commit", &json!({ "rev": "abc1234def" }))
            .unwrap();
        app.command(&mut host, "refresh", &json!({})).unwrap();
        assert_eq!(app.state.rev(), Some("abc1234def"));
        assert_eq!(host.asked.last().unwrap(), "commit abc1234def in /other");
    }

    /// Filtering is a view over rows already fetched. If it asked git anything, typing in
    /// the filter box would run a process per keystroke.
    #[test]
    fn filtering_never_asks_git_anything() {
        let mut host = Fake {
            status: a_status(),
            ..Default::default()
        };
        let mut app = app_on(&mut host);
        let before = host.asked.len();
        app.command(&mut host, "filter", &json!({ "query": "main" }))
            .unwrap();
        app.event(
            &mut host,
            avada_module_sdk::contract::methods::events::RAIL_QUERY,
            &json!({ "query": "read" }),
        );
        assert_eq!(app.state.query, "read");
        assert_eq!(host.asked.len(), before, "not one extra call");
    }

    /// Opening a file row is the click the deleted built-in mode answered, and it is still
    /// the module's: the host is asked to spawn a pane for the ABSOLUTE path.
    #[test]
    fn clicking_a_file_row_asks_for_that_file() {
        let mut host = Fake {
            status: a_status(),
            ..Default::default()
        };
        let mut app = app_on(&mut host);
        let data = json!({ "kind": "file", "path": "/proj/src/main.rs" });
        let out = app.row_activate(&mut host, &data, "open").unwrap();
        assert_eq!(out.open.as_deref(), Some("/proj/src/main.rs"));
        assert_eq!(app.state.selected.as_deref(), Some("/proj/src/main.rs"));
    }

    /// A right-click and a section heading both do nothing, for different reasons: the
    /// host owns the menu, and a heading is not a file.
    #[test]
    fn a_right_click_and_a_heading_are_both_deliberate_no_ops() {
        let mut host = Fake::default();
        let mut app = App::new();
        let file = json!({ "kind": "file", "path": "/proj/a.rs" });
        assert_eq!(
            app.row_activate(&mut host, &file, "context").unwrap().open,
            None,
            "the host draws the context menu, including the diff"
        );
        let heading = json!({ "kind": "section" });
        assert_eq!(
            app.row_activate(&mut host, &heading, "open").unwrap().open,
            None
        );
    }

    /// A new workspace is a new project: the commit a link sent us to in the old one is
    /// not something to keep showing.
    #[test]
    fn a_new_workspace_drops_the_commit_view() {
        let mut host = Fake {
            status: a_status(),
            commits: vec![a_commit()],
            ..Default::default()
        };
        let mut app = app_on(&mut host);
        app.command(&mut host, "show-commit", &json!({ "rev": "abc1234def" }))
            .unwrap();
        app.set_workspace(Some("/elsewhere".into()));
        assert!(app.state.commit.is_none());
        app.set_workspace(Some("/elsewhere".into()));
        assert_eq!(app.state.workspace_root.as_deref(), Some("/elsewhere"));
    }

    /// An unknown event kind is ignored rather than refused: the contract lets a host
    /// announce kinds this module's SDK never named.
    #[test]
    fn an_unknown_event_kind_is_ignored() {
        let mut host = Fake {
            status: a_status(),
            ..Default::default()
        };
        let mut app = app_on(&mut host);
        let before = app.state.clone();
        app.event(
            &mut host,
            "some.future.kind",
            &json!({ "rev": "abc1234def" }),
        );
        assert_eq!(app.state, before);
    }
}
