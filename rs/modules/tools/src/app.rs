//! The module's state machine: everything the tool entries do, with no I/O in it.
//!
//! The only thing this file can touch is an [`Api`], which the caller passes in — the same
//! shape `avada-files` gives its `Fs`. Here it buys something slightly different: the host
//! calls this module makes are HTTP, over a socket that can be slow, refused, or absent
//! entirely, and every one of those three has to produce a *row* rather than a crash. A
//! trait boundary is how each of them gets a test.

use crate::api::{Api, PaneSpec, Session, Tool};
use avada_module_sdk::contract::{ErrorCode, RpcError};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

/// The commands this module registers. Kept in step with `avada.toml` by
/// `every_declared_command_is_dispatched`.
pub const COMMANDS: &[(&str, &str)] = &[
    ("refresh", "Tools: Re-read the conversation history"),
    ("filter", "Tools: Filter conversations"),
    ("resume", "Tools: Resume a conversation"),
];

/// The tools this module can ever offer an entry for, with the names its manifest gives
/// them. Kept in step with `avada.toml` by `the_contributed_tools_match_the_manifest`.
///
/// The host's catalogue is the truth and normally supplies all of this, plus the brand
/// colour and whether the binary is installed at all. This list is only the fallback for a
/// host that answered nothing: without it the rail would end up with no entries and no
/// explanation, which reads as a crash rather than as a host that is not talking.
pub const CONTRIBUTED: &[(&str, &str)] = &[
    ("claude", "Claude Code"),
    ("cursor-agent", "Cursor"),
    ("copilot", "Copilot"),
];

/// Where a rail entry's own order starts.
///
/// Files claims 10 because it is where the built-in browser was. The tools follow it in
/// the human's own starred order, which is the order they had as tabs.
pub const ORDER_BASE: i32 = 20;

/// Everything the rows are drawn from.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct State {
    /// Every tool the host knows, whether or not it has an entry.
    pub tools: Vec<Tool>,
    /// The rail entry ids, in the order they are registered. Each is a tool id.
    pub entries: Vec<String>,
    /// Conversations per tool id, newest-first as the host sent them.
    pub sessions: BTreeMap<String, Vec<Session>>,
    /// Why a tool's list is empty, when the reason is a refusal rather than an absence.
    /// Keyed by tool id, plus the empty string for a catalogue that could not be read.
    pub errors: BTreeMap<String, String>,
    /// Filter text per entry.
    pub queries: BTreeMap<String, String>,
    /// `(entry, project)` pairs the human has folded shut.
    pub collapsed: BTreeSet<(String, String)>,
    /// The row the host should scroll to, per entry.
    pub selected: BTreeMap<String, String>,
    /// Whether an entry is on screen.
    pub active: bool,
    /// The clock the relative times were rendered against, in epoch milliseconds.
    pub now: u64,
}

impl State {
    /// One tool from the catalogue.
    pub fn tool(&self, id: &str) -> Option<&Tool> {
        self.tools.iter().find(|t| t.id == id)
    }

    /// The filter text for an entry.
    pub fn query(&self, entry: &str) -> &str {
        self.queries.get(entry).map(String::as_str).unwrap_or("")
    }
}

/// What a request produced, for `main` to carry out against the connection.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Outcome {
    /// The JSON-RPC result.
    pub result: Value,
    /// Something to say on a toast.
    pub toast: Option<String>,
    /// A pane the host should open (`POST /command` with `newPane`).
    pub spawn: Option<PaneSpec>,
}

impl Outcome {
    fn empty() -> Self {
        Outcome {
            result: json!({}),
            ..Default::default()
        }
    }

    fn said(text: impl Into<String>) -> Self {
        Outcome {
            result: json!({}),
            toast: Some(text.into()),
            spawn: None,
        }
    }
}

/// The Tools module.
#[derive(Debug, Default)]
pub struct App {
    /// The state the rows are drawn from.
    pub state: State,
}

impl App {
    /// A module that has read nothing yet.
    pub fn new() -> Self {
        App::default()
    }

    /// The entries came on screen: re-read everything.
    pub fn activate(&mut self, api: &mut impl Api) {
        self.state.active = true;
        self.refresh(api);
    }

    /// The entries went away. The conversations go with them — a transcript list is stale
    /// the moment the human looks away, since the tool they just used keeps writing to it.
    pub fn deactivate(&mut self) {
        self.state.active = false;
        self.state.sessions.clear();
    }

    /// Re-read the catalogue, decide which entries exist, and read each one's history.
    pub fn refresh(&mut self, api: &mut impl Api) {
        self.state.now = api.now_ms();
        self.state.errors.clear();
        match api.tools() {
            Ok(tools) => self.state.tools = tools,
            Err(e) => {
                // Keep whatever catalogue we had: a refusal now does not make the tools
                // this machine has stop existing, and blanking the rail would be a worse
                // lie than showing yesterday's list with a reason on it.
                self.state.errors.insert(String::new(), e);
                if self.state.tools.is_empty() {
                    // Nothing was ever read. Stand the manifest's own three up so that
                    // each entry exists and can say why it is empty.
                    self.state.tools = CONTRIBUTED
                        .iter()
                        .map(|(id, name)| Tool {
                            id: (*id).to_string(),
                            name: (*name).to_string(),
                            has_history: true,
                            ..Tool::default()
                        })
                        .collect();
                }
            }
        }
        // A host that cannot say which tools are starred is not an error worth showing.
        // `Ok(None)` is a headless host with no preferences at all; a hard failure is
        // treated the same way, because the fallback below is a correct answer either way.
        let favourites = api.favourites().unwrap_or(None);
        self.state.entries = choose_entries(&self.state.tools, favourites.as_deref());

        let wanted: BTreeSet<&String> = self.state.entries.iter().collect();
        self.state.sessions.retain(|k, _| wanted.contains(k));
        for id in self.state.entries.clone() {
            match api.sessions(&id) {
                Ok(rows) => {
                    self.state.sessions.insert(id, rows);
                }
                Err(e) => {
                    self.state.sessions.remove(&id);
                    self.state.errors.insert(id, e);
                }
            }
        }
    }

    /// Set an entry's filter text. Empty restores the whole list.
    pub fn set_query(&mut self, entry: &str, q: &str) {
        if q.trim().is_empty() {
            self.state.queries.remove(entry);
        } else {
            self.state.queries.insert(entry.to_string(), q.to_string());
        }
    }

    /// Fold or unfold one project group.
    pub fn toggle(&mut self, entry: &str, project: &str) {
        let key = (entry.to_string(), project.to_string());
        if !self.state.collapsed.remove(&key) {
            self.state.collapsed.insert(key);
        }
    }

    /// Find one conversation.
    pub fn session(&self, tool: &str, id: &str) -> Option<&Session> {
        self.state.sessions.get(tool)?.iter().find(|s| s.id == id)
    }

    /// Turn a conversation into the pane that resumes it, or into the reason it cannot be.
    ///
    /// The host decided that reason when it built the row — the binary is missing, the
    /// project directory is gone, the id is not one the tool would accept. The module does
    /// not second-guess any of it: it has no way to check, and a resume it waved through
    /// would fail in a pane instead of in a toast.
    pub fn resume(&mut self, tool: &str, id: &str) -> Result<Outcome, RpcError> {
        let Some(session) = self.session(tool, id) else {
            return Err(RpcError::new(
                ErrorCode::InvalidParams,
                format!("no conversation `{id}` under `{tool}`"),
            ));
        };
        if let Some(why) = session.blocked.clone() {
            return Ok(Outcome::said(why));
        }
        let Some(resume) = session.resume.clone() else {
            return Ok(Outcome::said("This conversation cannot be resumed"));
        };
        let label = crate::rows::session_label(session);
        let subtitle = session.project.clone();
        let color = self
            .state
            .tool(tool)
            .map(|t| t.brand.clone())
            .unwrap_or_default();
        self.state
            .selected
            .insert(tool.to_string(), crate::rows::session_row_id(&session.id));
        Ok(Outcome {
            result: json!({ "tool": tool, "session": id }),
            toast: None,
            spawn: Some(PaneSpec {
                command: resume.command,
                args: resume.args,
                cwd: resume.cwd,
                label,
                subtitle,
                color,
            }),
        })
    }

    /// A registered command.
    pub fn command(
        &mut self,
        api: &mut impl Api,
        id: &str,
        args: &Value,
    ) -> Result<Outcome, RpcError> {
        match id {
            "refresh" => {
                self.refresh(api);
                Ok(Outcome::empty())
            }
            "filter" => {
                // No entry means every entry: the command palette has no idea which rail
                // entry the human is looking at, and filtering all of them is what makes
                // one keystroke useful from there.
                let q = args["query"].as_str().unwrap_or_default().to_string();
                match args["entry"].as_str() {
                    Some(entry) => self.set_query(entry, &q),
                    None => {
                        for entry in self.state.entries.clone() {
                            self.set_query(&entry, &q);
                        }
                    }
                }
                Ok(Outcome::empty())
            }
            "resume" => {
                let tool = str_arg(args, "tool")?;
                let session = str_arg(args, "session")?;
                self.resume(&tool, &session)
            }
            other => Err(RpcError::new(
                ErrorCode::InvalidParams,
                format!("tools has no command `{other}`"),
            )),
        }
    }

    /// One of the host's events.
    ///
    /// An unknown kind is ignored on purpose: a newer host must be able to announce
    /// something this module has never heard of without it erroring out.
    pub fn event(&mut self, _api: &mut impl Api, kind: &str, payload: &Value) {
        use avada_module_sdk::contract::methods::events;
        if kind == events::RAIL_QUERY {
            if let Some(entry) = payload["entry"].as_str() {
                if self.state.entries.iter().any(|e| e == entry) {
                    let q = payload["query"].as_str().unwrap_or_default().to_string();
                    self.set_query(entry, &q);
                }
            }
        }
    }

    /// A row was clicked, double-clicked or right-clicked.
    ///
    /// `context` is a no-op: the host owns the row menu, and a module that answered it
    /// would only be reimplementing "Copy path" worse.
    pub fn row_activate(
        &mut self,
        _api: &mut impl Api,
        data: &Value,
        gesture: &str,
    ) -> Result<Outcome, RpcError> {
        if gesture == "context" {
            return Ok(Outcome::empty());
        }
        let Some(tool) = data["tool"].as_str() else {
            // A note row. Inert by design.
            return Ok(Outcome::empty());
        };
        match data["kind"].as_str().unwrap_or_default() {
            "project" => {
                let project = data["project"].as_str().unwrap_or_default().to_string();
                self.toggle(tool, &project);
                Ok(Outcome::empty())
            }
            "session" => {
                let id = data["session"].as_str().unwrap_or_default().to_string();
                // A toggle on a conversation only selects it. Resuming is a pane, and a
                // pane is not something a stray arrow key should be able to open.
                if gesture != "open" {
                    self.state
                        .selected
                        .insert(tool.to_string(), crate::rows::session_row_id(&id));
                    return Ok(Outcome::empty());
                }
                self.resume(tool, &id)
            }
            _ => Ok(Outcome::empty()),
        }
    }
}

/// Which tools get a rail entry.
///
/// A tool with no transcript reader can never have one: the entry would be permanently
/// empty and the human would have no way to tell that from "no conversations yet". Beyond
/// that the human's stars decide, in their own order — those stars are the same
/// `toolFavorites` that used to choose the panel's tool tabs, so the rail comes up looking
/// like the tabs did. Starring nothing shows everything readable rather than nothing at
/// all, because a module whose whole rail is empty on first run looks broken.
pub fn choose_entries(tools: &[Tool], favourites: Option<&[String]>) -> Vec<String> {
    let readable: Vec<&str> = tools
        .iter()
        .filter(|t| t.has_history)
        .map(|t| t.id.as_str())
        .collect();
    let mut seen = BTreeSet::new();
    let starred: Vec<String> = favourites
        .unwrap_or_default()
        .iter()
        .filter(|id| readable.contains(&id.as_str()))
        .filter(|id| seen.insert((*id).clone()))
        .cloned()
        .collect();
    if starred.is_empty() {
        return readable.iter().map(|s| (*s).to_string()).collect();
    }
    starred
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
pub mod fake {
    //! An [`Api`] backed by literals, so every rule above has a test.

    use super::*;
    use crate::api::Resume;

    /// A scripted host.
    #[derive(Debug)]
    pub struct FakeApi {
        /// What `GET /tools` answers.
        pub tools: Result<Vec<Tool>, String>,
        /// What `GET /tools/{id}/sessions` answers, per tool.
        pub sessions: BTreeMap<String, Result<Vec<Session>, String>>,
        /// What `GET /settings` answers.
        pub favourites: Result<Option<Vec<String>>, String>,
        /// Every pane asked for, in order.
        pub spawned: Vec<PaneSpec>,
        /// What `spawn` answers.
        pub spawn_result: Result<String, String>,
        /// The clock.
        pub now: u64,
    }

    impl Default for FakeApi {
        fn default() -> Self {
            FakeApi {
                tools: Ok(Vec::new()),
                sessions: BTreeMap::new(),
                favourites: Ok(None),
                spawned: Vec::new(),
                spawn_result: Ok(String::new()),
                now: 0,
            }
        }
    }

    impl FakeApi {
        /// A host with the three readable tools installed and no stars.
        pub fn ready() -> Self {
            FakeApi {
                tools: Ok(vec![
                    tool("claude", "Claude Code", true, true),
                    tool("cursor-agent", "Cursor", true, true),
                    tool("copilot", "Copilot", true, false),
                    tool("aider", "Aider", false, true),
                ]),
                favourites: Ok(Some(Vec::new())),
                spawn_result: Ok("pane-1".into()),
                now: 1_000_000_000_000,
                ..Default::default()
            }
        }

        /// Script one tool's conversations.
        pub fn with(mut self, tool: &str, sessions: Result<Vec<Session>, String>) -> Self {
            self.sessions.insert(tool.into(), sessions);
            self
        }
    }

    impl Api for FakeApi {
        fn tools(&mut self) -> Result<Vec<Tool>, String> {
            self.tools.clone()
        }
        fn sessions(&mut self, tool: &str) -> Result<Vec<Session>, String> {
            self.sessions
                .get(tool)
                .cloned()
                .unwrap_or_else(|| Ok(vec![]))
        }
        fn favourites(&mut self) -> Result<Option<Vec<String>>, String> {
            self.favourites.clone()
        }
        fn spawn(&mut self, spec: &PaneSpec) -> Result<String, String> {
            self.spawned.push(spec.clone());
            self.spawn_result.clone()
        }
        fn now_ms(&mut self) -> u64 {
            self.now
        }
    }

    /// One catalogue entry.
    pub fn tool(id: &str, name: &str, has_history: bool, installed: bool) -> Tool {
        Tool {
            id: id.into(),
            name: name.into(),
            brand: "#d97757".into(),
            has_history,
            path: installed.then(|| format!("/usr/local/bin/{id}")),
            source: installed.then(|| "path".to_string()),
        }
    }

    /// A resumable conversation.
    pub fn session(id: &str, project: &str, summary: &str) -> Session {
        Session {
            id: id.into(),
            project: project.into(),
            project_exact: true,
            branch: Some("main".into()),
            started_at: Some(1_000_000_000_000),
            summary: summary.into(),
            first_user: "hello there".into(),
            message_count: 7,
            resume: Some(Resume {
                command: "/usr/local/bin/claude".into(),
                args: vec!["--resume".into(), id.into()],
                cwd: project.into(),
            }),
            blocked: None,
            desktop: None,
        }
    }

    /// A conversation that cannot be resumed.
    pub fn blocked(id: &str, project: &str, why: &str) -> Session {
        Session {
            resume: None,
            blocked: Some(why.into()),
            ..session(id, project, "")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::*;
    use super::*;

    fn app_with(api: &mut FakeApi) -> App {
        let mut app = App::new();
        app.activate(api);
        app
    }

    #[test]
    fn the_contributed_tools_match_the_manifest() {
        // The fallback list and the manifest's rail contributions are two statements of
        // the same fact. A tool in one and not the other is either an entry the host
        // refuses to draw or a silent host leaving a hole where an entry should be.
        let manifest = crate::MANIFEST;
        let declared: BTreeSet<String> = manifest
            .lines()
            .zip(manifest.lines().skip(1))
            .filter(|(k, _)| k.trim() == "kind = \"rail\"")
            .filter_map(|(_, id)| id.trim().strip_prefix("id = \"")?.strip_suffix('"'))
            .map(str::to_string)
            .collect();
        let ours: BTreeSet<String> = CONTRIBUTED.iter().map(|(id, _)| id.to_string()).collect();
        assert_eq!(declared, ours);
        for (id, label) in CONTRIBUTED {
            assert!(
                manifest.contains(&format!("label = \"{label}\"")),
                "{id}: the manifest does not call it {label:?}"
            );
        }
    }

    #[test]
    fn a_host_that_never_answered_still_gets_its_entries_and_a_reason() {
        let mut api = FakeApi {
            tools: Err("this host offered no control server".into()),
            favourites: Err("this host offered no control server".into()),
            ..Default::default()
        };
        let mut app = App::new();
        app.activate(&mut api);
        assert_eq!(app.state.entries, ["claude", "cursor-agent", "copilot"]);
        assert_eq!(
            app.state.errors[""], "this host offered no control server",
            "the catalogue's reason is kept, for the rows to show"
        );
    }

    #[test]
    fn every_declared_command_is_dispatched() {
        // The manifest and the dispatch table are two lists that must not drift: a command
        // in `avada.toml` the host can invoke but this module answers `InvalidParams` to
        // is a palette entry that does nothing.
        let manifest = crate::MANIFEST;
        let declared: BTreeSet<String> = manifest
            .lines()
            .zip(manifest.lines().skip(1))
            .filter(|(k, _)| k.trim() == "kind = \"command\"")
            .filter_map(|(_, id)| id.trim().strip_prefix("id = \"")?.strip_suffix('"'))
            .map(str::to_string)
            .collect();
        assert_eq!(declared.len(), 3, "found: {declared:?}");
        let dispatched: BTreeSet<String> = COMMANDS.iter().map(|(id, _)| id.to_string()).collect();
        assert_eq!(declared, dispatched);

        let mut api = FakeApi::ready();
        let mut app = app_with(&mut api);
        for (id, _) in COMMANDS {
            let out = app.command(&mut api, id, &json!({ "tool": "claude", "session": "s1" }));
            // `resume` is allowed to complain that the session is gone; nothing is allowed
            // to complain that the command does not exist.
            if let Err(e) = out {
                assert!(!e.message.contains("no command"), "{id}: {}", e.message);
            }
        }
        assert!(app
            .command(&mut api, "teleport", &json!({}))
            .unwrap_err()
            .message
            .contains("no command `teleport`"));
    }

    #[test]
    fn the_declared_labels_match_the_manifests() {
        for (id, label) in COMMANDS {
            assert!(
                crate::MANIFEST.contains(&format!("label = \"{label}\"")),
                "`{id}` is labelled `{label}` here and something else in avada.toml"
            );
        }
    }

    #[test]
    fn a_tool_without_a_transcript_reader_never_gets_an_entry() {
        let mut api = FakeApi::ready();
        let app = app_with(&mut api);
        assert_eq!(app.state.entries, ["claude", "cursor-agent", "copilot"]);
        assert!(
            !app.state.entries.iter().any(|e| e == "aider"),
            "aider has no history reader, so its entry could only ever be empty"
        );
    }

    #[test]
    fn stars_choose_the_entries_and_their_order() {
        let mut api = FakeApi::ready();
        api.favourites = Ok(Some(vec![
            "copilot".into(),
            "claude".into(),
            // A star on a tool with no reader, and a duplicate: neither is an entry.
            "aider".into(),
            "copilot".into(),
        ]));
        let app = app_with(&mut api);
        assert_eq!(app.state.entries, ["copilot", "claude"]);
    }

    #[test]
    fn a_host_that_cannot_say_which_tools_are_starred_shows_them_all() {
        // 503 from `/settings` (no GUI attached) and an outright failure both land here.
        for favourites in [Ok(None), Err("boom".to_string())] {
            let mut api = FakeApi::ready();
            api.favourites = favourites;
            let app = app_with(&mut api);
            assert_eq!(app.state.entries, ["claude", "cursor-agent", "copilot"]);
            assert!(
                !app.state.errors.contains_key(""),
                "a missing preference is not an error worth showing"
            );
        }
    }

    #[test]
    fn a_refused_catalogue_keeps_the_last_one_and_says_why() {
        let mut api = FakeApi::ready();
        let mut app = app_with(&mut api);
        assert_eq!(app.state.tools.len(), 4);

        api.tools = Err("/tools: HTTP 403 — capability".into());
        app.refresh(&mut api);
        assert_eq!(app.state.tools.len(), 4, "the tools did not stop existing");
        assert!(app.state.errors[""].contains("403"));
    }

    #[test]
    fn a_refused_history_empties_that_entry_alone() {
        let mut api = FakeApi::ready()
            .with("claude", Ok(vec![session("s1", "/w/p", "Fix the parser")]))
            .with("cursor-agent", Err("HTTP 403 — capability".into()));
        let app = app_with(&mut api);
        assert_eq!(app.state.sessions["claude"].len(), 1);
        assert!(!app.state.sessions.contains_key("cursor-agent"));
        assert!(app.state.errors["cursor-agent"].contains("403"));
        assert!(!app.state.errors.contains_key("claude"));
    }

    #[test]
    fn looking_away_drops_the_conversations() {
        let mut api =
            FakeApi::ready().with("claude", Ok(vec![session("s1", "/w/p", "Fix the parser")]));
        let mut app = app_with(&mut api);
        assert!(!app.state.sessions.is_empty());
        app.deactivate();
        assert!(!app.state.active);
        assert!(
            app.state.sessions.is_empty(),
            "the tool keeps writing to that transcript while nobody is looking"
        );
        // The entries survive: the rail still has to show them.
        assert_eq!(app.state.entries.len(), 3);
    }

    #[test]
    fn resuming_a_conversation_asks_for_the_pane_the_host_described() {
        let mut api =
            FakeApi::ready().with("claude", Ok(vec![session("s1", "/w/p", "Fix the parser")]));
        let mut app = app_with(&mut api);
        let out = app
            .row_activate(
                &mut api,
                &json!({ "kind": "session", "tool": "claude", "session": "s1" }),
                "open",
            )
            .unwrap();
        let spec = out.spawn.expect("a resume is a pane");
        assert_eq!(spec.command, "/usr/local/bin/claude");
        assert_eq!(spec.args, ["--resume", "s1"]);
        assert_eq!(spec.cwd, "/w/p");
        assert_eq!(spec.label, "Fix the parser");
        assert_eq!(spec.subtitle, "/w/p");
        assert_eq!(spec.color, "#d97757", "the pane wears the tool's colour");
        assert_eq!(
            app.state.selected["claude"],
            crate::rows::session_row_id("s1")
        );
    }

    #[test]
    fn a_blocked_conversation_says_the_hosts_reason_and_opens_nothing() {
        let mut api = FakeApi::ready().with(
            "claude",
            Ok(vec![blocked("s1", "/w/gone", "project folder is missing")]),
        );
        let mut app = app_with(&mut api);
        let out = app
            .row_activate(
                &mut api,
                &json!({ "kind": "session", "tool": "claude", "session": "s1" }),
                "open",
            )
            .unwrap();
        assert!(out.spawn.is_none());
        assert_eq!(out.toast.unwrap(), "project folder is missing");
    }

    #[test]
    fn a_toggle_selects_a_conversation_but_never_opens_one() {
        let mut api =
            FakeApi::ready().with("claude", Ok(vec![session("s1", "/w/p", "Fix the parser")]));
        let mut app = app_with(&mut api);
        let out = app
            .row_activate(
                &mut api,
                &json!({ "kind": "session", "tool": "claude", "session": "s1" }),
                "toggle",
            )
            .unwrap();
        assert!(out.spawn.is_none(), "an arrow key must not spawn a process");
        assert!(app.state.selected.contains_key("claude"));
    }

    #[test]
    fn a_project_row_folds_and_unfolds() {
        let mut api =
            FakeApi::ready().with("claude", Ok(vec![session("s1", "/w/p", "Fix the parser")]));
        let mut app = app_with(&mut api);
        let data = json!({ "kind": "project", "tool": "claude", "project": "/w/p" });
        app.row_activate(&mut api, &data, "toggle").unwrap();
        assert!(app
            .state
            .collapsed
            .contains(&("claude".into(), "/w/p".into())));
        app.row_activate(&mut api, &data, "toggle").unwrap();
        assert!(app.state.collapsed.is_empty());
    }

    #[test]
    fn a_context_click_and_a_note_row_do_nothing_at_all() {
        let mut api = FakeApi::ready();
        let mut app = app_with(&mut api);
        for (data, gesture) in [
            (json!({ "kind": "session", "tool": "claude" }), "context"),
            (Value::Null, "open"),
            (json!({ "kind": "wat", "tool": "claude" }), "open"),
        ] {
            let out = app.row_activate(&mut api, &data, gesture).unwrap();
            assert_eq!(out, Outcome::empty(), "{data} / {gesture}");
        }
    }

    #[test]
    fn resuming_something_that_is_not_there_is_an_error_not_a_pane() {
        let mut api = FakeApi::ready();
        let mut app = app_with(&mut api);
        let e = app
            .command(
                &mut api,
                "resume",
                &json!({ "tool": "claude", "session": "nope" }),
            )
            .unwrap_err();
        assert!(
            e.message.contains("no conversation `nope`"),
            "{}",
            e.message
        );
        // And a command missing its arguments names the one it wanted.
        let e = app.command(&mut api, "resume", &json!({})).unwrap_err();
        assert!(e.message.contains("`tool`"), "{}", e.message);
    }

    #[test]
    fn a_rail_query_filters_the_entry_it_names_and_nothing_else() {
        let mut api = FakeApi::ready();
        let mut app = app_with(&mut api);
        app.event(
            &mut api,
            "rail.query",
            &json!({ "entry": "claude", "query": "parser" }),
        );
        assert_eq!(app.state.query("claude"), "parser");
        assert_eq!(app.state.query("copilot"), "");

        // An entry that is not ours, and a kind we have never heard of, are both ignored.
        app.event(
            &mut api,
            "rail.query",
            &json!({ "entry": "files", "query": "x" }),
        );
        app.event(&mut api, "some.future.event", &json!({ "entry": "claude" }));
        assert_eq!(app.state.query("claude"), "parser");
        assert_eq!(app.state.queries.len(), 1);

        // Clearing it removes the entry rather than storing a blank.
        app.event(
            &mut api,
            "rail.query",
            &json!({ "entry": "claude", "query": "  " }),
        );
        assert!(app.state.queries.is_empty());
    }

    #[test]
    fn the_filter_command_with_no_entry_filters_every_entry() {
        let mut api = FakeApi::ready();
        let mut app = app_with(&mut api);
        app.command(&mut api, "filter", &json!({ "query": "parser" }))
            .unwrap();
        assert_eq!(app.state.queries.len(), 3);
        app.command(
            &mut api,
            "filter",
            &json!({ "entry": "claude", "query": "" }),
        )
        .unwrap();
        assert_eq!(app.state.queries.len(), 2);
    }

    #[test]
    fn a_tool_losing_its_star_loses_its_conversations_too() {
        let mut api = FakeApi::ready()
            .with("claude", Ok(vec![session("s1", "/w/p", "a")]))
            .with("copilot", Ok(vec![session("s2", "/w/p", "b")]));
        let mut app = app_with(&mut api);
        assert!(app.state.sessions.contains_key("copilot"));

        api.favourites = Ok(Some(vec!["claude".into()]));
        app.refresh(&mut api);
        assert_eq!(app.state.entries, ["claude"]);
        assert!(
            !app.state.sessions.contains_key("copilot"),
            "rows for an entry that no longer exists would never be replaced"
        );
    }
}
