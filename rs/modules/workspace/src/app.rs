//! The module's whole behaviour, with the wire held at arm's length.
//!
//! [`App`] owns state and answers questions; it never touches a socket. Every method takes
//! the filesystem as a `&mut dyn Fs` argument and returns an [`Outcome`] describing what
//! the caller should say to the host. That is what makes the interesting rules — which
//! rows appear, what a click opens, what a note writes — testable without a running host,
//! and it is why `tests/e2e.rs` can be about the protocol rather than about the logic.

use crate::model::{self, PaneAddr, PaneSpec, WorkspaceFile};
use crate::tree::{self, Fs, Node, NodeKind, Scan};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Every command this module contributes, id and title, in manifest order.
///
/// This table and `avada.toml` are checked against each other by
/// `every_declared_command_is_dispatched`: a command in the manifest with no arm here is a
/// palette entry that shrugs, which is worse than a missing feature because the human has
/// been told it exists.
pub const COMMANDS: &[(&str, &str)] = &[
    ("refresh", "Workspace: Refresh"),
    ("filter", "Workspace: Filter"),
    ("reveal", "Workspace: Reveal a path"),
    ("open-group", "Workspace: Open every pane of a tab"),
    ("note", "Workspace: Note what a pane is for"),
];

/// One pane the host is being asked to open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spawn {
    /// The host's pane kind (`terminal`, `file`, …).
    pub kind: String,
    /// The directory the pane starts in, when the saved pane named one.
    pub path: Option<String>,
}

/// What the caller should do with the host after a call into [`App`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outcome {
    /// The JSON-RPC result to answer the host's request with.
    pub result: Value,
    /// A line to show the human, and its severity.
    pub toast: Option<(String, &'static str)>,
    /// Panes to ask the host to open, in order.
    pub spawn: Vec<Spawn>,
    /// Whether the row list changed and must be repainted.
    pub repaint: bool,
}

impl Outcome {
    /// An outcome that says nothing and asks only for a repaint, for the callers that
    /// changed state through `App` directly rather than through a method that reports.
    pub fn repainted() -> Self {
        Outcome::ok()
    }

    /// The plain "it worked, repaint" outcome.
    fn ok() -> Self {
        Outcome {
            result: json!({}),
            repaint: true,
            ..Default::default()
        }
    }

    /// An outcome that only tells the human something went wrong. The request still
    /// *succeeds*: a command whose argument the human mis-typed is not a protocol error,
    /// and answering with an RPC error would put a red line in the host's log for a typo.
    fn problem(text: impl Into<String>) -> Self {
        Outcome {
            result: json!({}),
            toast: Some((text.into(), "error")),
            ..Default::default()
        }
    }

    /// An outcome that reports something worth saying but not alarming.
    fn said(text: impl Into<String>) -> Self {
        Outcome {
            result: json!({}),
            toast: Some((text.into(), "info")),
            repaint: true,
            ..Default::default()
        }
    }
}

/// Everything the module remembers between messages.
#[derive(Debug, Clone, Default)]
pub struct State {
    /// The workspace root the host reported at handshake. Without one there is nothing to
    /// scan, and the rail says so rather than sweeping the process's own cwd.
    pub root: Option<PathBuf>,
    /// The last sweep.
    pub scan: Scan,
    /// Node ids the human has opened.
    pub expanded: BTreeSet<String>,
    /// The filter text.
    pub query: String,
    /// The row the human last activated.
    pub selected: Option<String>,
    /// The flattened tree as last painted.
    pub nodes: Vec<Node>,
    /// Whether the rail entry is the one on screen.
    pub active: bool,
}

/// The module.
#[derive(Debug, Clone, Default)]
pub struct App {
    /// Public so tests can look, not so callers can poke.
    pub state: State,
}

impl App {
    /// A module with no workspace and nothing scanned.
    pub fn new() -> Self {
        let mut app = App::default();
        // The three headers start open. A rail entry whose first paint is three collapsed
        // words has told the human nothing about whether it works.
        for section in tree::SECTIONS {
            app.state.expanded.insert(section.id().to_string());
        }
        app
    }

    /// Point the module at the workspace root the host reported, and sweep it.
    pub fn set_workspace(&mut self, fs: &mut dyn Fs, root: Option<PathBuf>) {
        self.state.root = root;
        self.rebuild(fs);
    }

    /// The rail entry came on screen.
    ///
    /// Re-sweeping on the closed→open edge rather than on a timer is deliberate: these
    /// files are written by the host and by the human's editor, and the moment the human
    /// looks is the only moment the answer has to be current.
    pub fn activate(&mut self, fs: &mut dyn Fs) {
        self.state.active = true;
        self.rebuild(fs);
    }

    /// The rail entry went off screen.
    pub fn deactivate(&mut self) {
        self.state.active = false;
    }

    /// Re-sweep the root and re-flatten.
    pub fn rebuild(&mut self, fs: &mut dyn Fs) {
        self.state.scan = match self.state.root.clone() {
            Some(root) => tree::scan(fs, &root),
            None => Scan::default(),
        };
        self.reflow();
    }

    /// Re-flatten without touching the disk.
    fn reflow(&mut self) {
        self.state.nodes = tree::flatten(&self.state.scan, &self.state.expanded, &self.state.query);
    }

    /// The rows to paint.
    pub fn nodes(&self) -> &[Node] {
        &self.state.nodes
    }

    /// Set the filter text.
    pub fn set_query(&mut self, query: &str) {
        self.state.query = query.to_string();
        self.reflow();
    }

    /// Open or close a node.
    pub fn toggle(&mut self, id: &str) {
        if !self.state.expanded.remove(id) {
            self.state.expanded.insert(id.to_string());
        }
        self.reflow();
    }

    /// Open everything down to `path` and select the row that names it.
    ///
    /// A reveal of a path this module has no row for is a no-op with a complaint, not a
    /// silent nothing: the caller asked a question and is owed an answer.
    pub fn reveal(&mut self, fs: &mut dyn Fs, path: &Path) -> Outcome {
        self.rebuild(fs);
        let found = self
            .state
            .nodes
            .iter()
            .find(|n| n.file.as_deref() == Some(path))
            .map(|n| n.id.clone());
        let Some(id) = found else {
            return Outcome::problem(format!(
                "{} is not a workspace file under this workspace",
                tree::name_of(path)
            ));
        };
        // Ids nest by `/`, so a node's ancestors are its own id's prefixes.
        for (i, _) in id.match_indices('/') {
            self.state.expanded.insert(id[..i].to_string());
        }
        for section in tree::SECTIONS {
            self.state.expanded.insert(section.id().to_string());
        }
        self.state.selected = Some(id);
        self.reflow();
        Outcome::ok()
    }

    /// A row was clicked, toggled or right-clicked.
    pub fn row_activate(
        &mut self,
        fs: &mut dyn Fs,
        row: &str,
        data: &Value,
        gesture: avada_module_sdk::rail::Gesture,
    ) -> Outcome {
        use avada_module_sdk::rail::Gesture;
        let _ = data;
        let Some(node) = self.node(row) else {
            // The row list the host is holding is older than ours. Repainting is the whole
            // fix: guessing what the stale row meant is how a click opens the wrong pane.
            return Outcome::ok();
        };
        match gesture {
            Gesture::Toggle => {
                self.toggle(row);
                Outcome::ok()
            }
            // Right-click has no menu at tier 1 — there is no vocabulary for one — so it
            // does the most useful thing available: say where the row came from.
            Gesture::Context => match &node.file {
                Some(file) => Outcome::said(file.display().to_string()),
                None => Outcome::said(node.label.clone()),
            },
            Gesture::Open => self.open(fs, &node),
        }
    }

    /// Open what a row names.
    fn open(&mut self, fs: &mut dyn Fs, node: &Node) -> Outcome {
        self.state.selected = Some(node.id.clone());
        let mut out = match node.kind {
            // A header or a window row names no panes of its own; clicking it can only
            // sensibly mean "show me what is inside".
            NodeKind::Section(_) | NodeKind::Window => {
                self.toggle(&node.id);
                return Outcome::ok();
            }
            NodeKind::Note => Outcome::ok(),
            NodeKind::Workspace | NodeKind::Group | NodeKind::Set | NodeKind::Member => {
                self.open_file(fs, node)
            }
            NodeKind::Pane => self.open_pane(node),
        };
        out.repaint = true;
        self.reflow();
        out
    }

    /// Open every pane of a workspace, of one tab, of a set, or of one member of a set.
    fn open_file(&mut self, fs: &mut dyn Fs, node: &Node) -> Outcome {
        let Some(path) = node.file.clone() else {
            return Outcome::ok();
        };
        let panes = match node.kind {
            NodeKind::Set => self.set_panes(fs, &path),
            NodeKind::Group => match node.addr {
                Some(addr) => self.group_panes(fs, &path, addr),
                None => Ok(Vec::new()),
            },
            _ => self.panes_of(fs, &path),
        };
        match panes {
            Ok(panes) => self.spawn_all(&path, &panes),
            Err(e) => Outcome::problem(e),
        }
    }

    /// Open the single pane a row names.
    fn open_pane(&mut self, node: &Node) -> Outcome {
        let (Some(path), Some(addr)) = (node.file.clone(), node.addr) else {
            return Outcome::ok();
        };
        let Some(mut file) = self.file_of(&path).cloned() else {
            return Outcome::problem(format!("{} is no longer here", path.display()));
        };
        let Some(pane) = model::pane_at(&mut file, addr).cloned() else {
            return Outcome::problem("that pane is no longer in the file");
        };
        self.spawn_all(&path, std::slice::from_ref(&pane))
    }

    /// Turn panes into spawn requests, resolving each saved cwd against the directory of
    /// the file that saved it.
    ///
    /// `host.panes.spawn` takes a `kind` and a `path` and nothing else — there is no way
    /// for a module to hand the host a command line. So a saved pane's *command* cannot be
    /// restored from here today; what can be restored is a terminal in the right place.
    /// Saying that out loud in the toast is better than opening five shells in `$HOME` and
    /// leaving the human to work out why.
    fn spawn_all(&self, file: &Path, panes: &[PaneSpec]) -> Outcome {
        if panes.is_empty() {
            return Outcome::said("nothing to open: that describes no panes");
        }
        let base = file.parent().unwrap_or(Path::new("."));
        let spawn: Vec<Spawn> = panes
            .iter()
            .map(|p| Spawn {
                kind: match p.kind() {
                    "" => "terminal".to_string(),
                    other => other.to_string(),
                },
                path: p.cwd.as_deref().map(|cwd| resolve_cwd(base, cwd)),
            })
            .collect();
        let lost = panes.iter().filter(|p| p.command.is_some()).count();
        let mut out = Outcome::ok();
        out.spawn = spawn;
        if lost > 0 {
            out.toast = Some((
                format!(
                    "opened {} in place; {lost} saved command{} could not be restored \
                     (the host has no way to give a module's pane a command line)",
                    plural(panes.len(), "pane"),
                    if lost == 1 { "" } else { "s" },
                ),
                "info",
            ));
        }
        out
    }

    /// Every pane of every member of a set, in the order the set names them.
    fn set_panes(&self, fs: &mut dyn Fs, path: &Path) -> Result<Vec<PaneSpec>, String> {
        let Some((_, set)) = self.state.scan.sets.iter().find(|(p, _)| p == path) else {
            return Err(format!("{} is no longer here", path.display()));
        };
        let members: Vec<PathBuf> = set
            .members
            .iter()
            .map(|m| tree::resolve_member(path, &m.path))
            .collect();
        let mut all = Vec::new();
        for member in members {
            all.append(&mut self.panes_of(fs, &member)?);
        }
        Ok(all)
    }

    /// Every pane of a workspace file, read fresh from disk rather than from the sweep:
    /// the sweep's copy is a cache, and what gets opened must be what is on disk now.
    fn panes_of(&self, fs: &mut dyn Fs, path: &Path) -> Result<Vec<PaneSpec>, String> {
        let file = model::parse_workspace(&fs.read(path)?)?;
        Ok(model::windows_of(&file)
            .into_iter()
            .flat_map(|w| w.groups)
            .flat_map(|g| g.panes)
            .collect())
    }

    /// Every pane of the one tab `addr` names.
    fn group_panes(
        &self,
        fs: &mut dyn Fs,
        path: &Path,
        addr: PaneAddr,
    ) -> Result<Vec<PaneSpec>, String> {
        let file = model::parse_workspace(&fs.read(path)?)?;
        let groups = match addr.window {
            Some(w) => file
                .windows
                .as_ref()
                .and_then(|ws| ws.get(w))
                .map(|w| &w.groups),
            None => file.groups.as_ref(),
        };
        match (groups, addr.group) {
            (Some(groups), Some(g)) => groups
                .get(g)
                .map(|g| g.panes.clone())
                .ok_or_else(|| "that tab is no longer in the file".to_string()),
            // A `panes`-shorthand file has exactly one implicit tab, and it is all of it.
            _ => Ok(file.panes.clone().unwrap_or_default()),
        }
    }

    /// The parsed file behind a path, from the last sweep.
    fn file_of(&self, path: &Path) -> Option<&WorkspaceFile> {
        if let Some((p, ws)) = &self.state.scan.project {
            if p == path {
                return Some(ws);
            }
        }
        self.state
            .scan
            .library
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, ws)| ws)
    }

    /// A command from the palette.
    pub fn command(&mut self, fs: &mut dyn Fs, id: &str, args: &Value) -> Outcome {
        match id {
            "refresh" => {
                self.rebuild(fs);
                let workspaces =
                    self.state.scan.library.len() + usize::from(self.state.scan.project.is_some());
                Outcome::said(format!(
                    "{}, {}",
                    plural(workspaces, "workspace"),
                    plural(self.state.scan.sets.len(), "set"),
                ))
            }
            "filter" => {
                self.set_query(str_arg(args, "query"));
                Outcome::ok()
            }
            "reveal" => {
                let path = str_arg(args, "path");
                if path.is_empty() {
                    return Outcome::problem("reveal needs a `path`");
                }
                self.reveal(fs, Path::new(path))
            }
            "open-group" => self.open_group(fs),
            "note" => self.note(fs, str_arg(args, "text")),
            other => Outcome::problem(format!("unknown command `{other}`")),
        }
    }

    /// Open the tab the selection is in, whether the selected row is the tab or a pane of
    /// it — the human who selected a pane and asked for "the whole tab" meant its parent.
    fn open_group(&mut self, fs: &mut dyn Fs) -> Outcome {
        let Some(node) = self.selected_node() else {
            return Outcome::problem("select a tab first");
        };
        match node.kind {
            NodeKind::Group | NodeKind::Workspace | NodeKind::Set => self.open_file(fs, &node),
            NodeKind::Pane => {
                let parent = node.id.rsplit_once('/').map(|(p, _)| p.to_string());
                match parent.and_then(|p| self.node(&p)) {
                    Some(group) => self.open_file(fs, &group),
                    None => Outcome::problem("that pane's tab is no longer here"),
                }
            }
            _ => Outcome::problem("select a tab first"),
        }
    }

    /// Write a line about what the selected pane is for, back into the file it came from.
    ///
    /// This is the module's only write, and it is a read-modify-write rather than a
    /// re-serialisation of the swept copy: the file may have changed since the sweep, and
    /// this module does not own it. What goes back is the same envelope, two-space pretty,
    /// so a `git diff` of the result is one line.
    fn note(&mut self, fs: &mut dyn Fs, text: &str) -> Outcome {
        let Some(node) = self.selected_node() else {
            return Outcome::problem("select a pane first");
        };
        let (NodeKind::Pane, Some(path), Some(addr)) = (&node.kind, node.file.clone(), node.addr)
        else {
            return Outcome::problem("select a pane first");
        };
        let raw = match fs.read(&path) {
            Ok(raw) => raw,
            Err(e) => return Outcome::problem(e),
        };
        let mut file = match model::parse_workspace(&raw) {
            Ok(file) => file,
            Err(e) => return Outcome::problem(e),
        };
        let Some(pane) = model::pane_at(&mut file, addr) else {
            return Outcome::problem("that pane is no longer in the file");
        };
        // An empty note clears the field rather than storing `""`: the round-trip contract
        // is that an absent option does not serialise at all, and leaving `"note": ""` in
        // a file the human asked to have the note taken *off* is not what they asked for.
        pane.note = Some(text.trim().to_string()).filter(|t| !t.is_empty());
        let text = match model::to_envelope_text(&file) {
            Ok(text) => text,
            Err(e) => return Outcome::problem(e),
        };
        if let Err(e) = fs.write(&path, &text) {
            return Outcome::problem(e);
        }
        self.rebuild(fs);
        Outcome::said(format!("noted in {}", tree::name_of(&path)))
    }

    /// A host event.
    pub fn event(&mut self, fs: &mut dyn Fs, kind: &str, payload: &Value) -> Outcome {
        use avada_module_sdk::contract::methods::events;
        match kind {
            events::RAIL_QUERY => {
                // The filter box belongs to the host, one per rail entry, so an event for
                // somebody else's entry must be ignored, not applied to ours.
                if payload["entry"].as_str() != Some(crate::rows::ENTRY) {
                    return Outcome::default();
                }
                self.set_query(payload["query"].as_str().unwrap_or_default());
                Outcome::ok()
            }
            events::FILES_REVEAL => {
                let path = payload["path"].as_str().unwrap_or_default();
                if path.is_empty() {
                    return Outcome::default();
                }
                let out = self.reveal(fs, Path::new(path));
                // A reveal this module cannot answer is somebody else's file. Silence is
                // right here, where a *command* the human ran has to report its failure.
                if out.toast.is_some() {
                    return Outcome::default();
                }
                out
            }
            _ => Outcome::default(),
        }
    }

    /// The node behind an id, from the last paint.
    fn node(&self, id: &str) -> Option<Node> {
        self.state.nodes.iter().find(|n| n.id == id).cloned()
    }

    /// The selected node, if it survived the last repaint.
    fn selected_node(&self) -> Option<Node> {
        self.state.selected.as_deref().and_then(|id| self.node(id))
    }
}

/// Resolve a saved cwd against the directory of the file that saved it.
fn resolve_cwd(base: &Path, cwd: &str) -> String {
    let p = Path::new(cwd);
    if p.is_absolute() {
        cwd.to_string()
    } else {
        base.join(p).to_string_lossy().into_owned()
    }
}

/// `1 pane` / `2 panes`.
fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

/// One string argument out of a command's params, empty when absent. Commands are typed by
/// a human into a palette; a missing argument is an ordinary event, not a protocol breach.
pub fn str_arg<'a>(args: &'a Value, key: &str) -> &'a str {
    args.get(key).and_then(Value::as_str).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::fake::FakeFs;
    use avada_module_sdk::rail::Gesture;

    const PROJECT: &str = r#"{"format":"avada","version":1,"workspace":{"name":"repo",
        "windows":[{"title":"main","groups":[
          {"title":"build","panes":[{"command":"cargo watch","cwd":"rs"},{"command":"vim"}]},
          {"title":"logs","panes":[{"command":"tail -f log"}]}]}]}}"#;
    const SAVED: &str = r#"{"format":"avada","version":1,"workspace":{"name":"triage",
        "panes":[{"command":"htop"}]}}"#;
    const SET: &str = r#"{"format":"avada-set","version":1,"set":{"name":"Morning",
        "members":[{"path":"saved.avada.json"}]}}"#;

    fn app() -> (App, FakeFs) {
        let mut fs = FakeFs::default();
        fs.file("/ws/.avada/project.json", PROJECT)
            .file("/ws/saved.avada.json", SAVED)
            .file("/ws/morning.set.json", SET);
        let mut app = App::new();
        app.set_workspace(&mut fs, Some(PathBuf::from("/ws")));
        (app, fs)
    }

    fn id_of(app: &App, label: &str) -> String {
        app.nodes()
            .iter()
            .find(|n| n.label == label)
            .unwrap_or_else(|| panic!("no row {label:?} in {:?}", labels(app)))
            .id
            .clone()
    }

    fn open(app: &mut App, fs: &mut FakeFs, label: &str) -> Outcome {
        let id = id_of(app, label);
        app.row_activate(fs, &id, &Value::Null, Gesture::Open)
    }

    fn expand(app: &mut App, fs: &mut FakeFs, label: &str) {
        let id = id_of(app, label);
        app.row_activate(fs, &id, &Value::Null, Gesture::Toggle);
    }

    fn labels(app: &App) -> Vec<String> {
        app.nodes().iter().map(|n| n.label.clone()).collect()
    }

    #[test]
    fn every_declared_command_is_dispatched() {
        // The manifest is the human-visible contract and this table is the code's; a drift
        // between them is a palette entry that shrugs.
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

        let (mut app, mut fs) = app();
        for (id, _) in COMMANDS {
            let out = app.command(&mut fs, id, &json!({}));
            let said = out.toast.map(|(t, _)| t).unwrap_or_default();
            assert!(!said.contains("unknown command"), "`{id}` is not handled");
        }
    }

    #[test]
    fn the_first_paint_shows_all_three_sections_with_their_contents() {
        let (app, _) = app();
        let l = labels(&app);
        for want in ["PROJECT", "LIBRARY", "SETS", "repo", "triage", "Morning"] {
            assert!(l.contains(&want.to_string()), "{want} missing from {l:?}");
        }
    }

    #[test]
    fn with_no_workspace_root_every_section_says_it_is_empty_rather_than_sweeping_the_cwd() {
        let mut fs = FakeFs::default();
        fs.file("/elsewhere/x.avada.json", SAVED);
        let mut app = App::new();
        app.set_workspace(&mut fs, None);
        assert!(app
            .nodes()
            .iter()
            .all(|n| n.depth == 0 || n.kind == NodeKind::Note));
        assert!(!labels(&app).contains(&"triage".to_string()));
    }

    #[test]
    fn a_toggle_expands_and_an_open_opens() {
        let (mut app, mut fs) = app();
        expand(&mut app, &mut fs, "repo");
        assert!(labels(&app).contains(&"build".to_string()));
        let out = open(&mut app, &mut fs, "repo");
        assert_eq!(out.spawn.len(), 3, "every pane the file describes");
    }

    #[test]
    fn opening_a_tab_opens_only_that_tabs_panes() {
        let (mut app, mut fs) = app();
        expand(&mut app, &mut fs, "repo");
        let out = open(&mut app, &mut fs, "logs");
        assert_eq!(out.spawn.len(), 1);
        let out = open(&mut app, &mut fs, "build");
        assert_eq!(out.spawn.len(), 2);
    }

    #[test]
    fn a_relative_cwd_resolves_against_the_file_that_saved_it() {
        let (mut app, mut fs) = app();
        expand(&mut app, &mut fs, "repo");
        expand(&mut app, &mut fs, "build");
        let out = open(&mut app, &mut fs, "cargo watch");
        assert_eq!(out.spawn.len(), 1);
        assert_eq!(out.spawn[0].path.as_deref(), Some("/ws/.avada/rs"));
        assert_eq!(out.spawn[0].kind, "terminal");
    }

    #[test]
    fn a_saved_command_that_cannot_be_restored_is_said_out_loud() {
        let (mut app, mut fs) = app();
        let out = open(&mut app, &mut fs, "triage");
        assert_eq!(out.spawn.len(), 1);
        let (text, level) = out.toast.expect("the human is told what was lost");
        assert!(text.contains("could not be restored"), "{text}");
        assert_eq!(level, "info");
    }

    #[test]
    fn opening_a_set_opens_every_member_it_names() {
        let (mut app, mut fs) = app();
        let out = open(&mut app, &mut fs, "Morning");
        assert_eq!(out.spawn.len(), 1, "the one member's one pane");
    }

    #[test]
    fn a_set_that_points_at_a_missing_file_says_which_one() {
        let mut fs = FakeFs::default();
        fs.file(
            "/ws/broken.set.json",
            r#"{"format":"avada-set","version":1,"set":{"name":"S",
               "members":[{"path":"gone.json"}]}}"#,
        );
        let mut app = App::new();
        app.set_workspace(&mut fs, Some(PathBuf::from("/ws")));
        let out = open(&mut app, &mut fs, "S");
        let (text, level) = out.toast.expect("a dangling member is reported");
        assert!(text.contains("gone.json"), "{text}");
        assert_eq!(level, "error");
        assert!(out.spawn.is_empty(), "nothing half-opens");
    }

    #[test]
    fn a_stale_row_id_repaints_instead_of_guessing() {
        let (mut app, mut fs) = app();
        let out = app.row_activate(&mut fs, "ws:/ws/vanished.json", &Value::Null, Gesture::Open);
        assert!(out.repaint);
        assert!(out.spawn.is_empty());
        assert!(out.toast.is_none());
    }

    #[test]
    fn a_note_is_written_back_as_one_line_of_diff() {
        let (mut app, mut fs) = app();
        expand(&mut app, &mut fs, "repo");
        expand(&mut app, &mut fs, "build");
        open(&mut app, &mut fs, "vim");
        let out = app.command(&mut fs, "note", &json!({ "text": " editing the parser " }));
        assert!(out.toast.unwrap().0.contains("noted"));

        let (path, text) = fs.writes.last().cloned().expect("the file was written");
        assert_eq!(path, PathBuf::from("/ws/.avada/project.json"));
        assert!(
            text.starts_with("{\n  \"format\": \"avada\",\n  \"version\": 1,"),
            "the envelope survived: {text}"
        );
        let back = model::parse_workspace(&text).unwrap();
        let panes = &back.windows.as_ref().unwrap()[0].groups[0].panes;
        assert_eq!(panes[1].note.as_deref(), Some("editing the parser"));
        assert_eq!(panes[0].command.as_deref(), Some("cargo watch"));
        assert_eq!(panes[0].note, None, "no neighbour was touched");
    }

    #[test]
    fn an_empty_note_removes_the_field_rather_than_storing_a_blank() {
        let (mut app, mut fs) = app();
        expand(&mut app, &mut fs, "repo");
        expand(&mut app, &mut fs, "build");
        open(&mut app, &mut fs, "vim");
        app.command(&mut fs, "note", &json!({ "text": "x" }));
        app.command(&mut fs, "note", &json!({ "text": "  " }));
        let (_, text) = fs.writes.last().cloned().unwrap();
        assert!(!text.contains("\"note\""), "{text}");
    }

    #[test]
    fn note_without_a_pane_selected_says_so_rather_than_writing_somewhere() {
        let (mut app, mut fs) = app();
        let out = app.command(&mut fs, "note", &json!({ "text": "x" }));
        assert_eq!(out.toast.unwrap().1, "error");
        assert!(fs.writes.is_empty(), "nothing was written");
        // A *workspace* is now selected, not a pane. Still no write.
        open(&mut app, &mut fs, "triage");
        let out = app.command(&mut fs, "note", &json!({ "text": "x" }));
        assert_eq!(out.toast.unwrap().1, "error");
        assert!(fs.writes.is_empty());
    }

    #[test]
    fn the_filter_command_and_the_rail_query_event_do_the_same_thing() {
        use avada_module_sdk::contract::methods::events;
        let (mut app, mut fs) = app();
        app.command(&mut fs, "filter", &json!({ "query": "triage" }));
        let by_command = labels(&app);
        app.set_query("");
        app.event(
            &mut fs,
            events::RAIL_QUERY,
            &json!({ "entry": crate::rows::ENTRY, "query": "triage" }),
        );
        assert_eq!(labels(&app), by_command);
        assert!(by_command.contains(&"triage".to_string()));
        assert!(!by_command.contains(&"repo".to_string()));
    }

    #[test]
    fn a_rail_query_for_somebody_elses_entry_is_ignored() {
        use avada_module_sdk::contract::methods::events;
        let (mut app, mut fs) = app();
        let before = labels(&app);
        app.event(
            &mut fs,
            events::RAIL_QUERY,
            &json!({ "entry": "files", "query": "triage" }),
        );
        assert_eq!(labels(&app), before);
        assert_eq!(app.state.query, "");
    }

    #[test]
    fn a_reveal_opens_the_path_down_to_the_file_and_selects_it() {
        use avada_module_sdk::contract::methods::events;
        let (mut app, mut fs) = app();
        let out = app.event(
            &mut fs,
            events::FILES_REVEAL,
            &json!({ "path": "/ws/saved.avada.json" }),
        );
        assert!(out.repaint);
        assert_eq!(
            app.state.selected.as_deref(),
            Some("ws:/ws/saved.avada.json")
        );
    }

    #[test]
    fn a_reveal_of_somebody_elses_file_is_silent_but_the_command_is_not() {
        use avada_module_sdk::contract::methods::events;
        let (mut app, mut fs) = app();
        let quiet = app.event(
            &mut fs,
            events::FILES_REVEAL,
            &json!({ "path": "/ws/x.rs" }),
        );
        assert!(
            quiet.toast.is_none(),
            "an event about another module's file"
        );
        let loud = app.command(&mut fs, "reveal", &json!({ "path": "/ws/x.rs" }));
        assert_eq!(loud.toast.unwrap().1, "error", "a command the human ran");
    }

    #[test]
    fn open_group_from_a_selected_pane_opens_the_whole_tab() {
        let (mut app, mut fs) = app();
        expand(&mut app, &mut fs, "repo");
        expand(&mut app, &mut fs, "build");
        let one = open(&mut app, &mut fs, "vim");
        assert_eq!(one.spawn.len(), 1);
        let all = app.command(&mut fs, "open-group", &json!({}));
        assert_eq!(all.spawn.len(), 2, "both panes of `build`");
    }

    #[test]
    fn open_group_with_nothing_selected_complains_instead_of_opening_everything() {
        let (mut app, mut fs) = app();
        let out = app.command(&mut fs, "open-group", &json!({}));
        assert_eq!(out.toast.unwrap().1, "error");
        assert!(out.spawn.is_empty());
    }

    #[test]
    fn activation_re_sweeps_so_a_file_written_while_hidden_is_there_when_looked_at() {
        let (mut app, mut fs) = app();
        assert!(!labels(&app).contains(&"late".to_string()));
        fs.file(
            "/ws/late.avada.json",
            r#"{"format":"avada","version":1,"workspace":{"name":"late",
               "panes":[{"command":"a"}]}}"#,
        );
        app.deactivate();
        app.activate(&mut fs);
        assert!(app.state.active);
        assert!(labels(&app).contains(&"late".to_string()));
    }

    #[test]
    fn a_context_click_reports_where_the_row_came_from() {
        let (mut app, mut fs) = app();
        let id = id_of(&app, "triage");
        let out = app.row_activate(&mut fs, &id, &Value::Null, Gesture::Context);
        assert_eq!(out.toast.unwrap().0, "/ws/saved.avada.json");
        assert!(out.spawn.is_empty());
    }

    #[test]
    fn refresh_counts_what_it_found() {
        let (mut app, mut fs) = app();
        let out = app.command(&mut fs, "refresh", &json!({}));
        assert_eq!(out.toast.unwrap().0, "2 workspaces, 1 set");
    }
}
