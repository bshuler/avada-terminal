//! The capability vocabulary and the four-valued right.
//!
//! Capabilities are a **closed** set. A module asks for them by name in its manifest;
//! the user accepts them at install; the host checks the accepted set server-side on
//! every connection, per route and per RPC. There is no wildcard and no "all".
//!
//! Names are namespaced `area.verb` so the rights page can group them and so a new
//! capability can never be confused with an old one. Adding a variant is a contract
//! change (Wave 0 rule: orchestrator only).

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// One thing a module may be allowed to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Capability {
    /// Read files under the workspace's project roots.
    #[serde(rename = "fs.read")]
    FsRead,
    /// Write files under the workspace's project roots.
    #[serde(rename = "fs.write")]
    FsWrite,
    /// Read files anywhere the user can.
    #[serde(rename = "fs.read_any")]
    FsReadAny,
    /// Write files anywhere the user can.
    #[serde(rename = "fs.write_any")]
    FsWriteAny,
    /// Open new panes (terminal or module surfaces) through the host.
    #[serde(rename = "panes.spawn")]
    PanesSpawn,
    /// Send input to panes in scope.
    #[serde(rename = "panes.input")]
    PanesInput,
    /// Read pane output / screen text in scope.
    #[serde(rename = "panes.output")]
    PanesOutput,
    /// Fork a subprocess directly instead of going through `panes.spawn`. Shown at
    /// install with its own line; it is the one capability that escapes the host.
    #[serde(rename = "process.spawn")]
    ProcessSpawn,
    /// Query the git service (status, log, diff) for project roots.
    #[serde(rename = "git.read")]
    GitRead,
    /// Mutate repositories through the git service (checkout, commit, worktrees).
    #[serde(rename = "git.write")]
    GitWrite,
    /// Outbound HTTP through the host's client.
    #[serde(rename = "net.fetch")]
    NetFetch,
    /// Read user settings and the module's own typed prefs.
    #[serde(rename = "settings.read")]
    SettingsRead,
    /// Write user settings (the module's own prefs never need this).
    #[serde(rename = "settings.write")]
    SettingsWrite,
    /// Read the workspace model: tabs, panes, layout, project roots.
    #[serde(rename = "workspace.read")]
    WorkspaceRead,
    /// Change the workspace model: open/close/move panes, rename tabs.
    #[serde(rename = "workspace.write")]
    WorkspaceWrite,
    /// Register a left-panel rail entry and its contents.
    #[serde(rename = "ui.rail")]
    UiRail,
    /// Own pane surfaces (tier 2, 3 or 5 contributions).
    #[serde(rename = "ui.pane")]
    UiPane,
    /// Register string-id commands in the palette and menus.
    #[serde(rename = "ui.commands")]
    UiCommands,
    /// Contribute typed preference rows to the prefs window.
    #[serde(rename = "ui.prefs")]
    UiPrefs,
    /// Show toasts.
    #[serde(rename = "ui.toast")]
    UiToast,
    /// Read and write the system clipboard.
    #[serde(rename = "clipboard")]
    Clipboard,
    /// Store and read secrets under the module's own keychain namespace.
    #[serde(rename = "keychain")]
    Keychain,
    /// Ship skills the host materializes into project and user scope.
    #[serde(rename = "skills.materialize")]
    SkillsMaterialize,
    /// Add routes to the control plane (and therefore verbs to the CLI).
    #[serde(rename = "control.route")]
    ControlRoute,
    /// Receive the host event stream.
    #[serde(rename = "events.subscribe")]
    EventsSubscribe,
    /// Search, install, enable, disable and remove modules through the host's marketplace
    /// routes. Installing runs a build on this machine, so this is an escape hatch.
    #[serde(rename = "marketplace.manage")]
    MarketplaceManage,
}

impl Capability {
    /// Every capability, in display order. Rights pages and install prompts iterate
    /// this so a new capability can never be silently absent from the UI.
    pub const ALL: &'static [Capability] = &[
        Capability::FsRead,
        Capability::FsWrite,
        Capability::FsReadAny,
        Capability::FsWriteAny,
        Capability::PanesSpawn,
        Capability::PanesInput,
        Capability::PanesOutput,
        Capability::ProcessSpawn,
        Capability::GitRead,
        Capability::GitWrite,
        Capability::NetFetch,
        Capability::SettingsRead,
        Capability::SettingsWrite,
        Capability::WorkspaceRead,
        Capability::WorkspaceWrite,
        Capability::UiRail,
        Capability::UiPane,
        Capability::UiCommands,
        Capability::UiPrefs,
        Capability::UiToast,
        Capability::Clipboard,
        Capability::Keychain,
        Capability::SkillsMaterialize,
        Capability::ControlRoute,
        Capability::EventsSubscribe,
        Capability::MarketplaceManage,
    ];

    /// The wire / manifest spelling, e.g. `fs.read`.
    pub fn name(self) -> &'static str {
        match self {
            Capability::FsRead => "fs.read",
            Capability::FsWrite => "fs.write",
            Capability::FsReadAny => "fs.read_any",
            Capability::FsWriteAny => "fs.write_any",
            Capability::PanesSpawn => "panes.spawn",
            Capability::PanesInput => "panes.input",
            Capability::PanesOutput => "panes.output",
            Capability::ProcessSpawn => "process.spawn",
            Capability::GitRead => "git.read",
            Capability::GitWrite => "git.write",
            Capability::NetFetch => "net.fetch",
            Capability::SettingsRead => "settings.read",
            Capability::SettingsWrite => "settings.write",
            Capability::WorkspaceRead => "workspace.read",
            Capability::WorkspaceWrite => "workspace.write",
            Capability::UiRail => "ui.rail",
            Capability::UiPane => "ui.pane",
            Capability::UiCommands => "ui.commands",
            Capability::UiPrefs => "ui.prefs",
            Capability::UiToast => "ui.toast",
            Capability::Clipboard => "clipboard",
            Capability::Keychain => "keychain",
            Capability::SkillsMaterialize => "skills.materialize",
            Capability::ControlRoute => "control.route",
            Capability::EventsSubscribe => "events.subscribe",
            Capability::MarketplaceManage => "marketplace.manage",
        }
    }

    /// The namespace before the dot (`fs`, `panes`, …); the rights page groups by it.
    pub fn namespace(self) -> &'static str {
        self.name().split('.').next().unwrap_or(self.name())
    }

    /// One line for the install prompt and the rights row.
    pub fn describe(self) -> &'static str {
        match self {
            Capability::FsRead => "Read files in your project folders",
            Capability::FsWrite => "Change files in your project folders",
            Capability::FsReadAny => "Read any file you can read",
            Capability::FsWriteAny => "Change any file you can change",
            Capability::PanesSpawn => "Open new panes and run commands in them",
            Capability::PanesInput => "Type into panes",
            Capability::PanesOutput => "Read what panes show",
            Capability::ProcessSpawn => "Start programs directly, outside any pane",
            Capability::GitRead => "Read git status, history and diffs",
            Capability::GitWrite => "Commit, check out and change git repositories",
            Capability::NetFetch => "Make network requests",
            Capability::SettingsRead => "Read your settings",
            Capability::SettingsWrite => "Change your settings",
            Capability::WorkspaceRead => "See your tabs, panes and layout",
            Capability::WorkspaceWrite => "Open, close and move panes and tabs",
            Capability::UiRail => "Add an entry to the left panel",
            Capability::UiPane => "Show its own panes",
            Capability::UiCommands => "Add commands to the palette and menus",
            Capability::UiPrefs => "Add a preferences page",
            Capability::UiToast => "Show notifications",
            Capability::Clipboard => "Read and write the clipboard",
            Capability::Keychain => "Store its own secrets in your keychain",
            Capability::SkillsMaterialize => "Install AI skills into your projects",
            Capability::ControlRoute => "Add commands to the control API and CLI",
            Capability::EventsSubscribe => "Watch for events in the app",
            Capability::MarketplaceManage => "Install, enable and remove other modules",
        }
    }

    /// Capabilities that reach outside the host's mediation and therefore get their own
    /// line and an explicit warning at install time.
    pub fn is_escape_hatch(self) -> bool {
        matches!(
            self,
            Capability::ProcessSpawn
                | Capability::FsWriteAny
                | Capability::FsReadAny
                | Capability::MarketplaceManage
        )
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Error for a capability name this build does not know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownCapability(pub String);

impl fmt::Display for UnknownCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown capability `{}`", self.0)
    }
}
impl std::error::Error for UnknownCapability {}

impl FromStr for Capability {
    type Err = UnknownCapability;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Capability::ALL
            .iter()
            .copied()
            .find(|c| c.name() == s.trim())
            .ok_or_else(|| UnknownCapability(s.to_string()))
    }
}

/// What the user chose for one capability at one scope.
///
/// * `never` and `always` are final.
/// * `workspace` at user level means "decide per workspace" — the workspace override
///   column then answers, and an unset workspace override means *ask*.
/// * `ask` produces a toast (never a modal) the first time in a session, then remembers
///   the answer for that session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RightValue {
    /// Denied, no prompt.
    Never,
    /// Granted, no prompt.
    Always,
    /// Defer to the workspace override.
    Workspace,
    /// Prompt with a toast.
    #[default]
    Ask,
}

impl RightValue {
    /// Every value, in the order the 4-way control shows them.
    pub const ALL: &'static [RightValue] = &[
        RightValue::Never,
        RightValue::Always,
        RightValue::Workspace,
        RightValue::Ask,
    ];
}

impl fmt::Display for RightValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            RightValue::Never => "never",
            RightValue::Always => "always",
            RightValue::Workspace => "workspace",
            RightValue::Ask => "ask",
        })
    }
}

/// The outcome of resolving a capability for one module in one workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    /// Proceed.
    Allow,
    /// Refuse.
    Deny,
    /// Show the toast and wait for the answer.
    Ask,
}

/// Resolve the user-level value and the workspace override into a decision.
///
/// `accepted` is whether the capability is in the install record at all. A capability
/// the user never accepted is denied regardless of any right value — the record is the
/// only source of rights.
///
/// The rule, in one place:
///
/// | user | workspace override | decision |
/// |---|---|---|
/// | not accepted | — | deny |
/// | never | any | deny |
/// | always | any | allow |
/// | workspace | never / always / ask | that |
/// | workspace | unset | ask |
/// | ask | never / always | that (a workspace may settle a user-level ask) |
/// | ask | unset / ask / workspace | ask |
pub fn resolve(accepted: bool, user: RightValue, workspace: Option<RightValue>) -> Decision {
    if !accepted {
        return Decision::Deny;
    }
    match user {
        RightValue::Never => Decision::Deny,
        RightValue::Always => Decision::Allow,
        RightValue::Workspace | RightValue::Ask => match workspace {
            Some(RightValue::Never) => Decision::Deny,
            Some(RightValue::Always) => Decision::Allow,
            Some(RightValue::Ask) | Some(RightValue::Workspace) | None => Decision::Ask,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_capability_round_trips_by_name_and_serde() {
        for c in Capability::ALL {
            assert_eq!(c.name().parse::<Capability>().unwrap(), *c);
            let json = serde_json::to_string(c).unwrap();
            assert_eq!(json, format!("\"{}\"", c.name()));
            assert_eq!(serde_json::from_str::<Capability>(&json).unwrap(), *c);
            assert!(!c.describe().is_empty());
            assert!(c.name().contains('.') || c.namespace() == c.name());
        }
    }

    #[test]
    fn names_are_unique_and_namespaced() {
        let mut seen = std::collections::BTreeSet::new();
        for c in Capability::ALL {
            assert!(seen.insert(c.name()), "duplicate name {}", c.name());
            assert!(
                c.name()
                    .chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch == '.' || ch == '_'),
                "{} is not lowercase.dotted",
                c.name()
            );
        }
    }

    #[test]
    fn unknown_capability_is_an_error_not_a_default() {
        assert!("fs.everything".parse::<Capability>().is_err());
        assert!(serde_json::from_str::<Capability>("\"*\"").is_err());
    }

    #[test]
    fn resolution_table() {
        use Decision::{Allow, Deny};
        use RightValue::{Always, Ask, Never, Workspace};
        const ASK: Decision = Decision::Ask;
        // never accepted: always deny, whatever the values say
        assert_eq!(resolve(false, Always, Some(Always)), Deny);
        // user-level finals win
        assert_eq!(resolve(true, Never, Some(Always)), Deny);
        assert_eq!(resolve(true, Always, Some(Never)), Allow);
        // workspace defers
        assert_eq!(resolve(true, Workspace, Some(Always)), Allow);
        assert_eq!(resolve(true, Workspace, Some(Never)), Deny);
        assert_eq!(resolve(true, Workspace, Some(Ask)), ASK);
        assert_eq!(resolve(true, Workspace, None), ASK);
        // ask may be settled by the workspace
        assert_eq!(resolve(true, Ask, Some(Always)), Allow);
        assert_eq!(resolve(true, Ask, Some(Never)), Deny);
        assert_eq!(resolve(true, Ask, None), ASK);
        assert_eq!(resolve(true, Ask, Some(Workspace)), ASK);
    }

    #[test]
    fn right_value_serializes_lowercase_and_defaults_to_ask() {
        assert_eq!(
            serde_json::to_string(&RightValue::Never).unwrap(),
            "\"never\""
        );
        assert_eq!(RightValue::default(), RightValue::Ask);
        for v in RightValue::ALL {
            let s = serde_json::to_string(v).unwrap();
            assert_eq!(serde_json::from_str::<RightValue>(&s).unwrap(), *v);
        }
    }
}
