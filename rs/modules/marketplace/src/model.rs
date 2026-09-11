//! What the marketplace routes answer, as this module reads it.
//!
//! Every field is tolerant (`default`) so a newer host that adds keys, or an older
//! one that lacks some, still renders. The shapes follow `docs/marketplace.md`.

use std::collections::BTreeMap;

use serde::Deserialize;

/// `GET /marketplace/toolchain`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Toolchain {
    /// A free build can run.
    #[serde(default)]
    pub ready: bool,
    /// Tools missing on `PATH` (`rustup`, `cargo`, `git`).
    #[serde(default)]
    pub missing: Vec<String>,
    /// Per-OS install guide when something is missing.
    #[serde(default)]
    pub guide: Option<String>,
    /// A GitHub token is stored.
    #[serde(default)]
    pub signed_in: bool,
}

/// One entry of `GET /marketplace/installed`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Installed {
    /// `owner/repo`; `None` when the directory name is unreadable.
    #[serde(default)]
    pub module: Option<String>,
    /// The version; `None` when unreadable.
    #[serde(default)]
    pub version: Option<String>,
    /// The lockfile pins this version.
    #[serde(default)]
    pub active: bool,
    /// `manual` or `dependency`.
    #[serde(default)]
    pub kind: Option<String>,
    /// The tag installed.
    #[serde(default)]
    pub tag: Option<String>,
    /// Capabilities the user accepted.
    #[serde(default)]
    pub accepted: Vec<String>,
    /// Workspace key → enabled there.
    #[serde(default)]
    pub enabled: BTreeMap<String, bool>,
    /// Why the record cannot be trusted, when it cannot.
    #[serde(default)]
    pub broken: Option<String>,
}

impl Installed {
    /// Enabled in `workspace`, `false` when unknown.
    pub fn enabled_in(&self, workspace: Option<&str>) -> bool {
        workspace
            .and_then(|w| self.enabled.get(w))
            .copied()
            .unwrap_or(false)
    }
}

/// One entry of `GET /marketplace/search`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct RepoSummary {
    /// `owner/repo`.
    #[serde(default)]
    pub full_name: String,
    /// Repository description.
    #[serde(default)]
    pub description: Option<String>,
    /// Web URL.
    #[serde(default)]
    pub html_url: String,
    /// Stargazers.
    #[serde(default)]
    pub stars: u64,
    /// Last push, ISO-8601.
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// `GET /marketplace/jobs/{id}` and the `job` of a 202 install answer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Job {
    /// Job id.
    #[serde(default)]
    pub id: String,
    /// `owner/repo`.
    #[serde(default)]
    pub module: String,
    /// Tag requested or chosen.
    #[serde(default)]
    pub tag: Option<String>,
    /// `fetch | verify | build | install | done | failed`.
    #[serde(default)]
    pub phase: String,
    /// 0..=100 when known.
    #[serde(default)]
    pub progress: Option<u8>,
    /// Last lines of build output.
    #[serde(default)]
    pub log_tail: Vec<String>,
    /// Why it failed.
    #[serde(default)]
    pub error: Option<String>,
    /// Version installed when done.
    #[serde(default)]
    pub version: Option<String>,
}

impl Job {
    /// `done` or `failed`.
    pub fn finished(&self) -> bool {
        matches!(self.phase.as_str(), "done" | "failed")
    }
}

/// `POST /marketplace/signin` and `GET /marketplace/signin/{id}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct SignIn {
    /// Sign-in id to poll.
    #[serde(default)]
    pub id: String,
    /// The code the user types.
    #[serde(default)]
    pub user_code: String,
    /// Where they type it.
    #[serde(default)]
    pub verification_uri: String,
    /// Unix seconds when the code expires.
    #[serde(default)]
    pub expires_at: u64,
    /// Minimum seconds between polls.
    #[serde(default)]
    pub interval: u64,
    /// `pending | done | expired | denied`.
    #[serde(default)]
    pub status: String,
}

impl SignIn {
    /// Still waiting for the user.
    pub fn pending(&self) -> bool {
        self.status == "pending"
    }
}

/// One tag of `GET /marketplace/modules/{owner}/{repo}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct TagInfo {
    /// `v1.2.3`.
    #[serde(default)]
    pub name: String,
    /// The commit the tag points at.
    #[serde(default)]
    pub commit: String,
}

/// `GET /marketplace/modules/{owner}/{repo}`: what the version picker draws.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ModuleView {
    /// `owner/repo`.
    #[serde(default)]
    pub module: String,
    /// The repository, when GitHub knows it.
    #[serde(default)]
    pub repo: Option<RepoSummary>,
    /// Why the manifest could not be read, when it could not.
    #[serde(default)]
    pub manifest_error: Option<String>,
    /// Tags GitHub lists, newest first.
    #[serde(default)]
    pub tags: Vec<TagInfo>,
    /// The newest installable tag.
    #[serde(default)]
    pub newest_tag: Option<String>,
    /// Versions installed on this machine.
    #[serde(default)]
    pub installed: Vec<String>,
    /// The version the lockfile pins.
    #[serde(default)]
    pub active: Option<String>,
    /// Workspace key → enabled there.
    #[serde(default)]
    pub enabled: BTreeMap<String, bool>,
}

/// One permission profile the module offers, from `GET .../rights`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Profile {
    /// Display name, and the value `POST .../profile` takes.
    #[serde(default)]
    pub name: String,
    /// One line.
    #[serde(default)]
    pub description: String,
    /// Capability → value, as the profile sets it.
    #[serde(default)]
    pub values: BTreeMap<String, String>,
}

/// One capability row of `GET .../rights`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct RightsRow {
    /// Wire name, e.g. `fs.read`.
    #[serde(default)]
    pub cap: String,
    /// What the capability lets the module do.
    #[serde(default)]
    pub description: String,
    /// The user accepted it at install; a capability they did not accept can still
    /// be listed, and stays denied however the values read.
    #[serde(default)]
    pub accepted: bool,
    /// The user's own override, when they set one.
    #[serde(default)]
    pub user: Option<String>,
    /// This workspace's override, when the query named a workspace.
    #[serde(default)]
    pub workspace: Option<String>,
    /// What the gate would answer now.
    #[serde(default)]
    pub effective: String,
}

/// `GET /marketplace/modules/{owner}/{repo}/rights`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct RightsView {
    /// `owner/repo`.
    #[serde(default)]
    pub module: String,
    /// The installed version the rights belong to.
    #[serde(default)]
    pub version: String,
    /// The workspace whose overrides are shown, when one was asked for.
    #[serde(default)]
    pub workspace: Option<String>,
    /// The profile the user selected, when they selected one.
    #[serde(default)]
    pub profile: Option<String>,
    /// Every profile the module declares.
    #[serde(default)]
    pub profiles: Vec<Profile>,
    /// One row per declared capability.
    #[serde(default)]
    pub rows: Vec<RightsRow>,
}

/// The rows are a list on the wire because their order is the host's to choose;
/// only the tests ever need to reach into one by name.
#[cfg(test)]
impl RightsView {
    pub fn row(&self, cap: &str) -> Option<&RightsRow> {
        self.rows.iter().find(|r| r.cap == cap)
    }
}

/// Everything the rows are derived from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct State {
    /// `marketplace.manage` was granted; without it nothing can be fetched.
    pub manage_granted: bool,
    /// The host gave a control URL and token.
    pub control_available: bool,
    /// The active workspace id, the key for enable/disable.
    pub workspace: Option<String>,
    /// Last toolchain report; `None` before the first refresh.
    pub toolchain: Option<Toolchain>,
    /// Installed versions, as last fetched.
    pub installed: Vec<Installed>,
    /// The last search query.
    pub query: Option<String>,
    /// Its results.
    pub results: Vec<RepoSummary>,
    /// Install jobs this session started or polled, newest first.
    pub jobs: Vec<Job>,
    /// A sign-in in progress or just finished.
    pub signin: Option<SignIn>,
    /// The last error, shown as a row until the next successful call.
    pub notice: Option<String>,
    /// A module pane is open on the `marketplace` surface. The host refuses rows for a
    /// surface it never spawned, so nothing is pushed there until this is true.
    pub pane_open: bool,
    /// The module the pane is showing in depth; `None` is the pane's index.
    pub focus: Option<String>,
    /// `marketplace.show` for [`State::focus`].
    pub view: Option<ModuleView>,
    /// `marketplace.rights` for [`State::focus`], scoped to the active workspace.
    pub rights: Option<RightsView>,
    /// This workspace's pins, module → version.
    pub pins: BTreeMap<String, String>,
}

impl State {
    /// Record or replace a job by id, keeping newest first.
    pub fn upsert_job(&mut self, job: Job) {
        self.jobs.retain(|j| j.id != job.id);
        self.jobs.insert(0, job);
        self.jobs.truncate(20);
    }

    /// The pin this workspace holds on `module`, when it holds one.
    pub fn pin(&self, module: &str) -> Option<&str> {
        self.pins.get(module).map(String::as_str)
    }

    /// Drop everything that described the focused module; called whenever the focus
    /// moves so a stale version list can never be drawn under a new heading.
    pub fn clear_focus(&mut self) {
        self.focus = None;
        self.view = None;
        self.rights = None;
    }

    /// The active installed version of `module`, else the newest listed.
    pub fn installed_version(&self, module: &str) -> Option<String> {
        let mine: Vec<&Installed> = self
            .installed
            .iter()
            .filter(|i| i.module.as_deref() == Some(module))
            .collect();
        mine.iter()
            .find(|i| i.active)
            .or_else(|| mine.first())
            .and_then(|i| i.version.clone())
    }
}
