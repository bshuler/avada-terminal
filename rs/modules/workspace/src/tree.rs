//! Turning a workspace root on disk into the node tree the rail draws.
//!
//! All I/O goes through the [`Fs`] trait rather than `std::fs`, for the same reason the
//! files module does it: the host owns the filesystem, this process reaches it over the
//! wire, and a trait is what lets every rule below be tested against a fake tree in
//! microseconds instead of against a temp directory the test then has to clean up.

use crate::model::{
    self, PaneAddr, SetMember, WorkspaceFile, WorkspaceSet, LEGACY_PROJECT_DIR, PROJECT_DIR,
    PROJECT_FILE,
};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The three sections of the rail entry, in draw order.
pub const SECTIONS: [Section; 3] = [Section::Project, Section::Library, Section::Sets];

/// The most files this module will look at while sweeping the workspace root for
/// workspaces and sets. A workspace root is a source checkout, and a source checkout can
/// hold a hundred thousand files; a rail entry that walks all of them makes the panel
/// stall on the one machine that matters. Past the cap the sweep stops and says so.
pub const MAX_SCAN: usize = 4_000;
/// The most directory entries to read from any one directory.
pub const MAX_ENTRIES: usize = 2_000;
/// How deep the sweep descends below the workspace root.
pub const MAX_DEPTH: usize = 4;
/// The largest row list this module will hand the host in one paint.
pub const MAX_ROWS: usize = 5_000;

/// Directories never worth sweeping for workspace files: they are large, machine-owned,
/// and a workspace file inside one is a build artefact rather than something a human saved.
pub const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "build",
    "dist",
    ".venv",
    "venv",
    "__pycache__",
    ".next",
    ".cache",
    "DerivedData",
    "Pods",
    "vendor",
];

/// File extensions that may hold a workspace or a set.
pub const SUFFIXES: &[&str] = &[".avada.json", ".workspace.json", ".set.json", ".json"];

/// Which of the three lists a node belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Section {
    /// This repository's own saved layout, `.avada/project.json`.
    Project,
    /// Every other workspace file found under the root.
    Library,
    /// Every `avada-set` file found under the root.
    Sets,
}

impl Section {
    /// The stable row id of the section header.
    pub fn id(self) -> &'static str {
        match self {
            Section::Project => "section:project",
            Section::Library => "section:library",
            Section::Sets => "section:sets",
        }
    }

    /// The header label.
    pub fn label(self) -> &'static str {
        match self {
            Section::Project => "PROJECT",
            Section::Library => "LIBRARY",
            Section::Sets => "SETS",
        }
    }

    /// What the header says when the section found nothing, phrased as what the human
    /// would have to do rather than as a bare "empty".
    pub fn empty_note(self) -> &'static str {
        match self {
            Section::Project => "no .avada/project.json in this workspace yet",
            Section::Library => "no saved workspace files under the workspace root",
            Section::Sets => "no workspace sets under the workspace root",
        }
    }
}

/// One directory entry, as the host reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The final path component.
    pub name: String,
    /// `"dir"` or `"file"`; anything else is neither.
    pub kind: String,
}

impl Entry {
    /// Whether this entry can be descended into. Only the literal `"dir"` counts — an
    /// unrecognised kind from a newer host must not be guessed at.
    pub fn is_dir(&self) -> bool {
        self.kind == "dir"
    }
}

/// The host filesystem, as much of it as this module needs.
pub trait Fs {
    /// Children of `dir`, or a human-readable reason there are none.
    fn list(&mut self, dir: &Path) -> Result<Vec<Entry>, String>;
    /// The whole text of `path`.
    fn read(&mut self, path: &Path) -> Result<String, String>;
    /// Replace `path` with `text`.
    fn write(&mut self, path: &Path, text: &str) -> Result<(), String>;
}

/// What a row *is*, which is what decides what activating it does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeKind {
    /// A section header.
    Section(Section),
    /// A workspace file. Opening it opens every pane it describes.
    Workspace,
    /// One OS window inside a workspace file.
    Window,
    /// One tab inside a window. Opening it opens that tab's panes.
    Group,
    /// One pane. Opening it spawns that single pane.
    Pane,
    /// A workspace set.
    Set,
    /// One member of a set.
    Member,
    /// A line that reports something rather than naming something openable.
    Note,
}

impl NodeKind {
    /// The `kind` mark the host uses to pick an icon.
    pub fn mark(&self) -> &'static str {
        match self {
            NodeKind::Section(_) => "section",
            NodeKind::Workspace => "workspace",
            NodeKind::Window => "window",
            NodeKind::Group => "group",
            NodeKind::Pane => "pane",
            NodeKind::Set => "set",
            NodeKind::Member => "member",
            NodeKind::Note => "note",
        }
    }
}

/// One line of the tree, already flattened and already carrying everything a row needs.
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    /// Stable identity, used both as the row id and as the expansion key.
    pub id: String,
    /// Indent level, 0 for a section header.
    pub depth: u8,
    /// What this node is.
    pub kind: NodeKind,
    /// Primary text.
    pub label: String,
    /// Secondary text.
    pub detail: Option<String>,
    /// Whether it has children at all.
    pub expandable: bool,
    /// The file this node came from, absent for a section header.
    pub file: Option<PathBuf>,
    /// The pane this node names, for the nodes that name one.
    pub addr: Option<PaneAddr>,
}

impl Node {
    /// Whether activating this row can do anything. A note names nothing to open, so the
    /// host is told it carries no payload and the row cannot be activated at all — the
    /// alternative is a row that looks live and answers nothing.
    pub fn activatable(&self) -> bool {
        !matches!(self.kind, NodeKind::Note)
    }
}

/// Everything the sweep learned about the workspace root, before expansion is applied.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Scan {
    /// `.avada/project.json`, when it is there and parses.
    pub project: Option<(PathBuf, WorkspaceFile)>,
    /// Every other workspace file, sorted by path.
    pub library: Vec<(PathBuf, WorkspaceFile)>,
    /// Every set file, sorted by path.
    pub sets: Vec<(PathBuf, WorkspaceSet)>,
    /// Files that looked like a workspace and did not parse: path and reason. A parse
    /// failure is shown, never swallowed — a file a human hand-edited into invalidity is
    /// exactly the file they need told about.
    pub broken: Vec<(PathBuf, String)>,
    /// Whether the sweep stopped at [`MAX_SCAN`] rather than finishing.
    pub truncated: bool,
}

/// Sweep `root` for the project file, workspaces and sets.
pub fn scan(fs: &mut dyn Fs, root: &Path) -> Scan {
    let mut out = Scan::default();
    let mut seen = 0usize;
    let mut queue: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];

    let project_path = project_file(fs, root);

    while let Some((dir, depth)) = queue.pop() {
        let Ok(entries) = fs.list(&dir) else { continue };
        // A directory wider than the per-directory cap is truncation just as much as a
        // sweep that ran out of budget, and the human is owed the same warning: silently
        // dropping the 2001st file is how a workspace goes missing without explanation.
        if entries.len() > MAX_ENTRIES {
            out.truncated = true;
        }
        for entry in entries.into_iter().take(MAX_ENTRIES) {
            if seen >= MAX_SCAN {
                out.truncated = true;
                queue.clear();
                break;
            }
            seen += 1;
            let path = dir.join(&entry.name);
            if entry.is_dir() {
                // `.avada` is swept even though it starts with a dot: it is the one hidden
                // directory whose whole purpose is to hold what this module shows.
                let hidden = entry.name.starts_with('.')
                    && entry.name != PROJECT_DIR
                    && entry.name != LEGACY_PROJECT_DIR;
                if hidden || SKIP_DIRS.contains(&entry.name.as_str()) || depth + 1 > MAX_DEPTH {
                    continue;
                }
                queue.push((path, depth + 1));
                continue;
            }
            if !SUFFIXES.iter().any(|s| entry.name.ends_with(s)) {
                continue;
            }
            let Ok(text) = fs.read(&path) else { continue };
            classify(&path, &text, project_path.as_deref(), &mut out);
        }
    }

    out.library.sort_by(|a, b| a.0.cmp(&b.0));
    out.sets.sort_by(|a, b| a.0.cmp(&b.0));
    out.broken.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Decide which list one file's text belongs in.
///
/// Set-ness is decided first and by the `format` field, not by the filename: a file named
/// `x.set.json` that holds a workspace is a workspace, and the human who has to reconcile
/// the two would rather see the truth than the filename's claim.
fn classify(path: &Path, text: &str, project: Option<&Path>, out: &mut Scan) {
    let looks_like_set =
        text.contains(model::SET_FORMAT) || text.contains(model::LEGACY_SET_FORMAT);
    if looks_like_set {
        match model::parse_set(text) {
            Ok(set) => out.sets.push((path.to_path_buf(), set)),
            Err(e) => out.broken.push((path.to_path_buf(), e)),
        }
        return;
    }
    // A file that carries our `format` discriminator, and the project file itself, are
    // taken at their word. Everything else is a bare object judged on its contents,
    // because every field of a workspace is optional and `package.json` parses cleanly as
    // an empty one. Listing it would put every repository's config into the library.
    let ours = model::claims_envelope(text) || Some(path) == project;
    match model::parse_workspace(text) {
        Ok(ws) if !ours && model::pane_count(&ws) == 0 => {}
        Ok(ws) => {
            if Some(path) == project {
                out.project = Some((path.to_path_buf(), ws));
            } else {
                out.library.push((path.to_path_buf(), ws));
            }
        }
        Err(e) => {
            // Only complain about files whose name claims to be ours. Every repository is
            // full of `.json`, and a rail entry that lists all of them as broken is noise.
            if path.to_string_lossy().ends_with(".avada.json") || Some(path) == project {
                out.broken.push((path.to_path_buf(), e));
            }
        }
    }
}

/// Where the repo-local project file is, preferring `.avada/` and falling back to the
/// pre-rename `.hyperpanes/` only when it is the one that exists — the host's own
/// precedence, restated so a module and its host never disagree about which file is live.
pub fn project_file(fs: &mut dyn Fs, root: &Path) -> Option<PathBuf> {
    let new = root.join(PROJECT_DIR).join(PROJECT_FILE);
    if fs.read(&new).is_ok() {
        return Some(new);
    }
    let old = root.join(LEGACY_PROJECT_DIR).join(PROJECT_FILE);
    if fs.read(&old).is_ok() {
        return Some(old);
    }
    Some(new)
}

/// Flatten the scan into rows, honouring `expanded` and `query`.
///
/// A query filters *leaves* and keeps their ancestors, which is the only behaviour that
/// makes a tree filter useful: matching a pane and hiding the tab it lives in tells the
/// human what matched but not where it is.
pub fn flatten(scan: &Scan, expanded: &BTreeSet<String>, query: &str) -> Vec<Node> {
    let q = query.trim().to_lowercase();
    let mut out = Vec::new();
    for section in SECTIONS {
        let mut body = section_nodes(scan, section, expanded);
        if !q.is_empty() {
            body = filter(body, &q);
        }
        let count = body.iter().filter(|n| n.depth == 1).count();
        out.push(Node {
            id: section.id().to_string(),
            depth: 0,
            kind: NodeKind::Section(section),
            label: section.label().to_string(),
            detail: (count > 0).then(|| count.to_string()),
            expandable: count > 0,
            file: None,
            addr: None,
        });
        if !expanded.contains(section.id()) {
            continue;
        }
        if body.is_empty() {
            out.push(note(
                &format!("{}:empty", section.id()),
                1,
                if q.is_empty() {
                    section.empty_note().to_string()
                } else {
                    format!("nothing here matches “{query}”")
                },
            ));
        }
        out.append(&mut body);
        if out.len() >= MAX_ROWS {
            out.truncate(MAX_ROWS - 1);
            out.push(note(
                "truncated",
                1,
                format!("stopped at {MAX_ROWS} rows — narrow the filter to see the rest"),
            ));
            return out;
        }
    }
    if scan.truncated {
        out.push(note(
            "scan-truncated",
            0,
            format!("stopped after {MAX_SCAN} files; some workspaces may be missing"),
        ));
    }
    out
}

/// Keep every node that matches, plus each match's ancestors, plus the children of a
/// matching container.
fn filter(nodes: Vec<Node>, q: &str) -> Vec<Node> {
    let hit = |n: &Node| {
        n.label.to_lowercase().contains(q)
            || n.detail
                .as_deref()
                .is_some_and(|d| d.to_lowercase().contains(q))
    };
    let mut keep = vec![false; nodes.len()];
    for (i, node) in nodes.iter().enumerate() {
        if !hit(node) {
            continue;
        }
        keep[i] = true;
        // Ancestors: walk back to the first node shallower than the last one kept.
        let mut want = node.depth;
        for j in (0..i).rev() {
            if want == 0 {
                break;
            }
            if nodes[j].depth < want {
                keep[j] = true;
                want = nodes[j].depth;
            }
        }
        // Descendants of a matching container come along whole.
        for (j, item) in nodes.iter().enumerate().skip(i + 1) {
            if item.depth <= node.depth {
                break;
            }
            keep[j] = true;
        }
    }
    nodes
        .into_iter()
        .zip(keep)
        .filter_map(|(n, k)| k.then_some(n))
        .collect()
}

/// The nodes of one section, below its header.
fn section_nodes(scan: &Scan, section: Section, expanded: &BTreeSet<String>) -> Vec<Node> {
    let mut out = Vec::new();
    match section {
        Section::Project => {
            if let Some((path, ws)) = &scan.project {
                workspace_nodes(&mut out, path, ws, 1, expanded);
            }
            for (path, why) in &scan.broken {
                if scan.project.as_ref().is_none_or(|(p, _)| p != path) {
                    continue;
                }
                out.push(note(&format!("broken:{}", path.display()), 1, why.clone()));
            }
        }
        Section::Library => {
            for (path, ws) in &scan.library {
                workspace_nodes(&mut out, path, ws, 1, expanded);
            }
            for (path, why) in &scan.broken {
                if scan.project.as_ref().is_some_and(|(p, _)| p == path) {
                    continue;
                }
                out.push(note(
                    &format!("broken:{}", path.display()),
                    1,
                    format!("{}: {why}", name_of(path)),
                ));
            }
        }
        Section::Sets => {
            for (path, set) in &scan.sets {
                set_nodes(&mut out, path, set, expanded);
            }
        }
    }
    out
}

/// One workspace file and, when expanded, its windows/tabs/panes.
fn workspace_nodes(
    out: &mut Vec<Node>,
    path: &Path,
    ws: &WorkspaceFile,
    depth: u8,
    expanded: &BTreeSet<String>,
) {
    let root_id = format!("ws:{}", path.display());
    let windows = model::windows_of(ws);
    let panes = model::pane_count(ws);
    // A single-window file is drawn without its window row. The window level is real in
    // the format but invisible to a human who only ever had one window, and an extra
    // always-expanded row for it is a level of indent that buys nothing.
    let skip_window_level = windows.len() <= 1;
    out.push(Node {
        id: root_id.clone(),
        depth,
        kind: NodeKind::Workspace,
        label: ws.name.clone().unwrap_or_else(|| name_of(path).to_string()),
        detail: Some(plural(panes, "pane")),
        expandable: panes > 0,
        file: Some(path.to_path_buf()),
        addr: None,
    });
    if !expanded.contains(&root_id) {
        return;
    }
    let addressed = addresses(ws);
    for (wi, window) in windows.iter().enumerate() {
        let window_id = format!("{root_id}/w{wi}");
        let mut level = depth + 1;
        if !skip_window_level {
            out.push(Node {
                id: window_id.clone(),
                depth: level,
                kind: NodeKind::Window,
                label: window
                    .title
                    .clone()
                    .unwrap_or_else(|| format!("Window {}", wi + 1)),
                detail: Some(plural(window.groups.len(), "tab")),
                expandable: !window.groups.is_empty(),
                file: Some(path.to_path_buf()),
                addr: None,
            });
            if !expanded.contains(&window_id) {
                continue;
            }
            level += 1;
        }
        for (gi, group) in window.groups.iter().enumerate() {
            let group_id = format!("{window_id}/g{gi}");
            out.push(Node {
                id: group_id.clone(),
                depth: level,
                kind: NodeKind::Group,
                label: group.title(),
                detail: Some(plural(group.panes.len(), "pane")),
                expandable: !group.panes.is_empty(),
                file: Some(path.to_path_buf()),
                addr: addressed.get(&(wi, gi, 0)).copied(),
            });
            if !expanded.contains(&group_id) {
                continue;
            }
            for (pi, pane) in group.panes.iter().enumerate() {
                out.push(Node {
                    id: format!("{group_id}/p{pi}"),
                    depth: level + 1,
                    kind: NodeKind::Pane,
                    label: pane.title(),
                    detail: pane.note.clone().or_else(|| pane.cwd.clone()),
                    expandable: false,
                    file: Some(path.to_path_buf()),
                    addr: addressed.get(&(wi, gi, pi)).copied(),
                });
            }
        }
    }
}

/// One set and, when expanded, its members.
fn set_nodes(out: &mut Vec<Node>, path: &Path, set: &WorkspaceSet, expanded: &BTreeSet<String>) {
    let id = format!("set:{}", path.display());
    out.push(Node {
        id: id.clone(),
        depth: 1,
        kind: NodeKind::Set,
        label: set.name.clone(),
        detail: Some(plural(set.members.len(), "workspace")),
        expandable: !set.members.is_empty(),
        file: Some(path.to_path_buf()),
        addr: None,
    });
    if !expanded.contains(&id) {
        return;
    }
    for (i, member) in set.members.iter().enumerate() {
        out.push(Node {
            id: format!("{id}/m{i}"),
            depth: 2,
            kind: NodeKind::Member,
            label: member_label(member),
            detail: Some(member.path.clone()),
            expandable: false,
            // A member names *its own* file, not the set's: activating it must open the
            // workspace it points at.
            file: Some(resolve_member(path, &member.path)),
            addr: None,
        });
    }
}

/// A member's display name: its own, else the file stem it points at.
fn member_label(member: &SetMember) -> String {
    member
        .name
        .clone()
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| name_of(Path::new(&member.path)).to_string())
}

/// A member path, resolved against the set file's own directory when relative.
pub fn resolve_member(set_file: &Path, member: &str) -> PathBuf {
    let p = Path::new(member);
    if p.is_absolute() {
        return p.to_path_buf();
    }
    set_file
        .parent()
        .map(|d| d.join(p))
        .unwrap_or_else(|| p.to_path_buf())
}

/// Map every normalised (window, group, pane) triple back to the address it has in the
/// file as written, so an edit can find the pane again without reshaping the file.
fn addresses(ws: &WorkspaceFile) -> std::collections::BTreeMap<(usize, usize, usize), PaneAddr> {
    let mut map = std::collections::BTreeMap::new();
    if let Some(windows) = &ws.windows {
        // `windows_of` drops groupless windows, so the normalised index and the file index
        // diverge exactly there; walking both counters is what keeps them aligned.
        let mut norm = 0usize;
        for (wi, window) in windows.iter().enumerate() {
            if window.groups.is_empty() {
                continue;
            }
            for (gi, group) in window.groups.iter().enumerate() {
                for pi in 0..group.panes.len().max(1) {
                    map.insert(
                        (norm, gi, pi),
                        PaneAddr {
                            window: Some(wi),
                            group: Some(gi),
                            pane: pi,
                        },
                    );
                }
            }
            norm += 1;
        }
        if !map.is_empty() {
            return map;
        }
    }
    if let Some(groups) = &ws.groups {
        for (gi, group) in groups.iter().enumerate() {
            for pi in 0..group.panes.len().max(1) {
                map.insert(
                    (0, gi, pi),
                    PaneAddr {
                        window: None,
                        group: Some(gi),
                        pane: pi,
                    },
                );
            }
        }
        if !map.is_empty() {
            return map;
        }
    }
    if let Some(panes) = &ws.panes {
        for pi in 0..panes.len().max(1) {
            map.insert(
                (0, 0, pi),
                PaneAddr {
                    window: None,
                    group: None,
                    pane: pi,
                },
            );
        }
    }
    map
}

/// A row that reports rather than names.
fn note(id: &str, depth: u8, text: String) -> Node {
    Node {
        id: id.to_string(),
        depth,
        kind: NodeKind::Note,
        label: text,
        detail: None,
        expandable: false,
        file: None,
        addr: None,
    }
}

/// `1 pane` / `2 panes`, because "1 panes" is the kind of detail that makes a panel feel
/// unfinished.
fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

/// The display name of a file: its stem with our known suffixes taken off.
pub fn name_of(path: &Path) -> &str {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    for suffix in [".avada.json", ".workspace.json", ".set.json", ".json"] {
        if let Some(stem) = name.strip_suffix(suffix) {
            if !stem.is_empty() {
                return stem;
            }
        }
    }
    name
}

/// An in-memory [`Fs`] so the rules above can be tested without a host or a temp dir.
#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::collections::BTreeMap;

    /// A fake tree: directories with their entries, files with their text.
    #[derive(Default)]
    pub struct FakeFs {
        /// Directory listings.
        pub dirs: BTreeMap<PathBuf, Vec<Entry>>,
        /// File contents.
        pub files: BTreeMap<PathBuf, String>,
        /// Every path written, in order, so a test can assert what was written.
        pub writes: Vec<(PathBuf, String)>,
    }

    impl FakeFs {
        /// Add a file, creating every directory above it.
        pub fn file(&mut self, path: &str, text: &str) -> &mut Self {
            let p = PathBuf::from(path);
            self.files.insert(p.clone(), text.to_string());
            let mut child = p;
            while let Some(parent) = child.parent().map(Path::to_path_buf) {
                let name = child
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
                    .to_string();
                let kind = if self.files.contains_key(&child) {
                    "file"
                } else {
                    "dir"
                };
                let entries = self.dirs.entry(parent.clone()).or_default();
                if !entries.iter().any(|e| e.name == name) {
                    entries.push(Entry {
                        name,
                        kind: kind.to_string(),
                    });
                }
                if parent.as_os_str().is_empty() || parent == Path::new("/") {
                    break;
                }
                child = parent;
            }
            self
        }
    }

    impl Fs for FakeFs {
        fn list(&mut self, dir: &Path) -> Result<Vec<Entry>, String> {
            self.dirs
                .get(dir)
                .cloned()
                .ok_or_else(|| format!("`{}`: no such directory", dir.display()))
        }
        fn read(&mut self, path: &Path) -> Result<String, String> {
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| format!("`{}`: no such file", path.display()))
        }
        fn write(&mut self, path: &Path, text: &str) -> Result<(), String> {
            self.files.insert(path.to_path_buf(), text.to_string());
            self.writes.push((path.to_path_buf(), text.to_string()));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeFs;
    use super::*;

    const PROJECT: &str = r#"{"format":"avada","version":1,"workspace":{"name":"repo",
        "windows":[{"title":"main","groups":[
          {"title":"build","panes":[{"command":"cargo watch"},{"command":"vim"}]},
          {"title":"logs","panes":[{"command":"tail -f log"}]}]}]}}"#;
    const SAVED: &str = r#"{"format":"avada","version":1,"workspace":{"name":"triage",
        "panes":[{"command":"htop"}]}}"#;
    const SET: &str = r#"{"format":"avada-set","version":1,"set":{"name":"Morning",
        "members":[{"path":"saved.avada.json"},{"path":"other.json","name":"Other"}]}}"#;

    fn tree() -> FakeFs {
        let mut fs = FakeFs::default();
        fs.file("/ws/.avada/project.json", PROJECT)
            .file("/ws/saved.avada.json", SAVED)
            .file("/ws/morning.set.json", SET)
            .file("/ws/package.json", r#"{"name":"x","dependencies":{}}"#)
            .file("/ws/node_modules/dep/what.avada.json", SAVED)
            .file("/ws/.git/config.json", SAVED);
        fs
    }

    fn all_expanded(scan: &Scan) -> BTreeSet<String> {
        // Expanding everything is how a test sees the whole tree at once; the flattener is
        // then exercised on its deepest path rather than only on collapsed headers.
        let mut set: BTreeSet<String> = SECTIONS.iter().map(|s| s.id().to_string()).collect();
        for _ in 0..5 {
            for node in flatten(scan, &set, "") {
                if node.expandable {
                    set.insert(node.id);
                }
            }
        }
        set
    }

    #[test]
    fn the_sweep_sorts_files_into_the_three_sections() {
        let mut fs = tree();
        let scan = scan(&mut fs, Path::new("/ws"));
        assert_eq!(
            scan.project.as_ref().unwrap().0,
            PathBuf::from("/ws/.avada/project.json")
        );
        assert_eq!(scan.library.len(), 1, "{:?}", scan.library);
        assert_eq!(scan.library[0].0, PathBuf::from("/ws/saved.avada.json"));
        assert_eq!(scan.sets.len(), 1);
        assert_eq!(scan.sets[0].1.name, "Morning");
        assert!(scan.broken.is_empty());
        assert!(!scan.truncated);
    }

    #[test]
    fn machine_owned_directories_are_never_swept() {
        let mut fs = tree();
        let scan = scan(&mut fs, Path::new("/ws"));
        // The workspace files under node_modules/ and .git/ are build and tool artefacts;
        // neither may appear in a human's library.
        for (path, _) in &scan.library {
            let p = path.to_string_lossy();
            assert!(!p.contains("node_modules") && !p.contains(".git"), "{p}");
        }
    }

    #[test]
    fn a_json_file_that_is_not_a_workspace_is_neither_shown_nor_reported_broken() {
        let mut fs = tree();
        let scan = scan(&mut fs, Path::new("/ws"));
        // package.json parses as a nameless, paneless workspace. Listing it would put
        // every repository's config into the library; calling it broken would be a lie.
        assert!(scan
            .library
            .iter()
            .all(|(p, _)| !p.ends_with("package.json")));
        assert!(scan.broken.is_empty());
    }

    #[test]
    fn a_file_that_claims_to_be_ours_and_is_not_parseable_is_reported() {
        let mut fs = FakeFs::default();
        fs.file("/ws/bad.avada.json", "{ not json ")
            .file("/ws/other.json", "{ not json ");
        let scan = scan(&mut fs, Path::new("/ws"));
        assert_eq!(scan.broken.len(), 1);
        assert!(scan.broken[0].0.ends_with("bad.avada.json"));
        // And it reaches the human as a row rather than dying in a log.
        let rows = flatten(&scan, &all_expanded(&scan), "");
        assert!(rows.iter().any(|n| n.label.contains("bad")), "{rows:#?}");
    }

    #[test]
    fn the_project_tree_flattens_to_tabs_and_panes() {
        let mut fs = tree();
        let scan = scan(&mut fs, Path::new("/ws"));
        let rows = flatten(&scan, &all_expanded(&scan), "");
        let labels: Vec<&str> = rows.iter().map(|n| n.label.as_str()).collect();
        assert!(labels.contains(&"PROJECT"));
        assert!(labels.contains(&"repo"));
        assert!(labels.contains(&"build"));
        assert!(labels.contains(&"cargo watch"));
        // One window means no window row: the level exists in the format, not on screen.
        assert!(!rows.iter().any(|n| n.kind == NodeKind::Window));
        let pane = rows.iter().find(|n| n.label == "vim").unwrap();
        assert_eq!(
            pane.addr,
            Some(PaneAddr {
                window: Some(0),
                group: Some(0),
                pane: 1
            })
        );
    }

    #[test]
    fn a_collapsed_section_shows_only_its_header() {
        let mut fs = tree();
        let scan = scan(&mut fs, Path::new("/ws"));
        let rows = flatten(&scan, &BTreeSet::new(), "");
        assert_eq!(rows.len(), SECTIONS.len());
        assert!(rows.iter().all(|n| n.depth == 0));
        // The header still counts what is inside, so a collapsed panel is not a blind one.
        assert_eq!(rows[0].detail.as_deref(), Some("1"));
        assert!(rows[0].expandable);
    }

    #[test]
    fn an_empty_section_says_what_would_fill_it() {
        let scan = Scan::default();
        let expanded: BTreeSet<String> = SECTIONS.iter().map(|s| s.id().to_string()).collect();
        let rows = flatten(&scan, &expanded, "");
        let note = rows
            .iter()
            .find(|n| n.id == "section:project:empty")
            .unwrap();
        assert!(note.label.contains("project.json"));
        assert!(!note.activatable());
    }

    #[test]
    fn a_filter_keeps_the_match_and_the_path_down_to_it() {
        let mut fs = tree();
        let scan = scan(&mut fs, Path::new("/ws"));
        let rows = flatten(&scan, &all_expanded(&scan), "cargo");
        let labels: Vec<&str> = rows.iter().map(|n| n.label.as_str()).collect();
        assert!(labels.contains(&"cargo watch"));
        // Its tab and its workspace come with it, or the row says what matched without
        // saying where it is.
        assert!(labels.contains(&"build"));
        assert!(labels.contains(&"repo"));
        assert!(!labels.contains(&"logs"));
        assert!(!labels.contains(&"triage"));
    }

    #[test]
    fn a_filter_that_matches_a_container_keeps_everything_under_it() {
        let mut fs = tree();
        let scan = scan(&mut fs, Path::new("/ws"));
        let rows = flatten(&scan, &all_expanded(&scan), "logs");
        let labels: Vec<&str> = rows.iter().map(|n| n.label.as_str()).collect();
        assert!(labels.contains(&"logs"));
        assert!(labels.contains(&"tail -f log"));
    }

    #[test]
    fn a_filter_that_matches_nothing_says_so_per_section() {
        let mut fs = tree();
        let scan = scan(&mut fs, Path::new("/ws"));
        let rows = flatten(&scan, &all_expanded(&scan), "zzz-no-such-thing");
        assert!(rows
            .iter()
            .all(|n| n.depth == 0 || n.kind == NodeKind::Note));
        assert!(rows.iter().any(|n| n.label.contains("matches")));
    }

    #[test]
    fn a_set_member_points_at_its_own_file_not_the_sets() {
        let mut fs = tree();
        let scan = scan(&mut fs, Path::new("/ws"));
        let rows = flatten(&scan, &all_expanded(&scan), "");
        let member = rows.iter().find(|n| n.label == "Other").unwrap();
        assert_eq!(member.file.as_deref(), Some(Path::new("/ws/other.json")));
        // A member with no name of its own is labelled by the file it points at, with our
        // own suffix dropped — the same name the workspace itself is listed under.
        let first = rows.iter().find(|n| n.label == "saved").unwrap();
        assert_eq!(
            first.file.as_deref(),
            Some(Path::new("/ws/saved.avada.json"))
        );
    }

    #[test]
    fn the_legacy_project_directory_is_used_only_when_it_is_the_one_that_exists() {
        let mut fs = FakeFs::default();
        fs.file("/ws/.hyperpanes/project.json", PROJECT);
        assert_eq!(
            project_file(&mut fs, Path::new("/ws")),
            Some(PathBuf::from("/ws/.hyperpanes/project.json"))
        );
        fs.file("/ws/.avada/project.json", PROJECT);
        assert_eq!(
            project_file(&mut fs, Path::new("/ws")),
            Some(PathBuf::from("/ws/.avada/project.json"))
        );
    }

    #[test]
    fn a_groupless_window_does_not_shift_every_address_after_it() {
        // `windows_of` drops it, so the normalised index and the file index diverge here
        // and nowhere else — the one place an off-by-one would silently edit a stranger.
        let ws = model::parse_workspace(
            r#"{"windows":[{"title":"empty","groups":[]},
                 {"title":"real","groups":[{"panes":[{"command":"a"}]}]}]}"#,
        )
        .unwrap();
        let map = addresses(&ws);
        assert_eq!(
            map.get(&(0, 0, 0)),
            Some(&PaneAddr {
                window: Some(1),
                group: Some(0),
                pane: 0
            })
        );
    }

    #[test]
    fn the_sweep_stops_at_the_cap_and_says_it_did() {
        let mut fs = FakeFs::default();
        for i in 0..(MAX_SCAN + 50) {
            fs.file(&format!("/ws/f{i}.txt"), "x");
        }
        let scan = scan(&mut fs, Path::new("/ws"));
        assert!(scan.truncated);
        let expanded: BTreeSet<String> = SECTIONS.iter().map(|s| s.id().to_string()).collect();
        assert!(flatten(&scan, &expanded, "")
            .iter()
            .any(|n| n.id == "scan-truncated"));
    }

    #[test]
    fn a_file_named_like_a_set_but_holding_a_workspace_is_a_workspace() {
        let mut fs = FakeFs::default();
        fs.file("/ws/liar.set.json", SAVED);
        let scan = scan(&mut fs, Path::new("/ws"));
        assert!(scan.sets.is_empty());
        assert_eq!(scan.library.len(), 1);
    }

    #[test]
    fn a_display_name_drops_the_suffix_we_put_on() {
        assert_eq!(name_of(Path::new("/a/b/triage.avada.json")), "triage");
        assert_eq!(name_of(Path::new("/a/b/x.set.json")), "x");
        assert_eq!(name_of(Path::new("/a/b/plain.json")), "plain");
        assert_eq!(name_of(Path::new("/a/b/README")), "README");
    }
}
