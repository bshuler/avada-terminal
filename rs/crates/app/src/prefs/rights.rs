//! The per-module rights page in Preferences (docs/modules-fanout-plan.md, track H2): one
//! row per capability, `never|always|workspace|ask`, profiles, the held-update diff.
//!
//! The Slint side (`ui/prefs_rights.slint`, global `RightsAdapter` in `ui/types.slint`)
//! is a pure projection. This module is the bridge in both directions:
//!
//! * [`fill`] projects a [`RightsService`] into the adapter (module list, the selected
//!   module's rows, profiles, held-update diff, the ask toast);
//! * [`wire`] turns each adapter callback into a [`RightsCommand`] and hands it to one
//!   closure the controller supplies (Seam #1: mutate, then resync);
//! * [`apply`] performs a command on the service and says what happened, so the
//!   controller's call site is one line per direction.
//!
//! Everything that is a *decision* — the value-index mapping, the badge, the diff text —
//! is a pure function here so it can be tested without a window.

use std::collections::BTreeSet;

use avada_core::rights::{
    AskAnswer, Capability, Decision, ModuleId, PendingAsk, RightValue, RightsRow, RightsService,
};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

// ---------------------------------------------------------------------------------
// value ↔ picker index
// ---------------------------------------------------------------------------------

/// The picker order the page draws: `never | always | workspace | ask`. The index a
/// callback carries is an index into this; the page never sees the enum names.
pub const PICKER: [RightValue; 4] = [
    RightValue::Never,
    RightValue::Always,
    RightValue::Workspace,
    RightValue::Ask,
];

/// The picker index of a user-level value.
pub fn value_index(value: RightValue) -> i32 {
    PICKER
        .iter()
        .position(|v| *v == value)
        .map(|i| i as i32)
        .unwrap_or(3)
}

/// The value at a picker index; `None` for anything the page cannot send.
pub fn value_from_index(index: i32) -> Option<RightValue> {
    usize::try_from(index)
        .ok()
        .and_then(|i| PICKER.get(i).copied())
}

/// The picker index of a workspace override: `-1` when unset.
pub fn workspace_index(value: Option<RightValue>) -> i32 {
    value.map(value_index).unwrap_or(-1)
}

/// The workspace override a picker index means: `Some(None)` clears it (`-1`),
/// `Some(Some(v))` sets it, `None` is an index the page cannot send. A workspace column
/// cannot hold `workspace` (the SDK's `resolve` treats it as unset), so that index is
/// refused rather than written.
pub fn workspace_from_index(index: i32) -> Option<Option<RightValue>> {
    if index < 0 {
        return Some(None);
    }
    match value_from_index(index)? {
        RightValue::Workspace => None,
        v => Some(Some(v)),
    }
}

// ---------------------------------------------------------------------------------
// the badge
// ---------------------------------------------------------------------------------

/// The one word the module list shows per module: the *widest* effective right across
/// its rows, since that is what the user needs to notice. Ordered so `max` is "worst".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Badge {
    /// No declared capabilities.
    None,
    /// Every row resolves to deny.
    Denied,
    /// Nothing allowed outright; at least one row asks.
    Asks,
    /// At least one ordinary capability is allowed without asking.
    Allowed,
    /// An escape-hatch capability (`process.spawn`, `fs.*.any`) is allowed without asking.
    EscapeHatch,
}

impl Badge {
    /// The word drawn in the module list.
    pub fn label(self) -> &'static str {
        match self {
            Badge::None => "no rights",
            Badge::Denied => "denied",
            Badge::Asks => "asks",
            Badge::Allowed => "allowed",
            Badge::EscapeHatch => "escape hatch",
        }
    }

    /// The order as an int for the page's colour choice.
    pub fn level(self) -> i32 {
        match self {
            Badge::None => 0,
            Badge::Denied => 1,
            Badge::Asks => 2,
            Badge::Allowed => 3,
            Badge::EscapeHatch => 4,
        }
    }
}

/// The badge for a module's rows.
pub fn badge(rows: &[RightsRow]) -> Badge {
    rows.iter()
        .map(|r| match r.effective {
            Decision::Allow if r.cap.is_escape_hatch() => Badge::EscapeHatch,
            Decision::Allow => Badge::Allowed,
            Decision::Ask => Badge::Asks,
            Decision::Deny => Badge::Denied,
        })
        .max()
        .unwrap_or(Badge::None)
}

/// The decision word the page shows (`resolve`'s serde spelling).
pub fn decision_label(decision: Decision) -> &'static str {
    match decision {
        Decision::Allow => "allow",
        Decision::Deny => "deny",
        Decision::Ask => "ask",
    }
}

/// A set of capabilities as the comma-separated list the held-update block shows.
pub fn cap_list(caps: &BTreeSet<Capability>) -> String {
    caps.iter().map(|c| c.name()).collect::<Vec<_>>().join(", ")
}

/// The sentence the ask toast shows under "<module> wants <cap>".
pub fn ask_text(ask: &PendingAsk) -> String {
    match &ask.workspace {
        Some(ws) => format!("{} (in workspace {ws})", ask.cap.describe()),
        None => ask.cap.describe().to_string(),
    }
}

// ---------------------------------------------------------------------------------
// commands: page → Rust
// ---------------------------------------------------------------------------------

/// What a click on the page means. One variant per adapter callback; the strings the
/// page sent have already been parsed (an unparseable module id or capability name is
/// dropped by [`wire`] with a warning rather than surfacing as a command).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RightsCommand {
    /// Show this module (index into the list [`fill`] projected).
    SelectModule(usize),
    /// Set a module's user-level value for one capability.
    SetUser {
        module: ModuleId,
        cap: Capability,
        value: RightValue,
    },
    /// Set (`Some`) or clear (`None`) the current workspace's override for one row.
    SetWorkspace {
        module: ModuleId,
        cap: Capability,
        value: Option<RightValue>,
    },
    /// Select a named profile (`None` = no profile).
    SelectProfile {
        module: ModuleId,
        profile: Option<String>,
    },
    /// Accept the held update's capability diff.
    HeldAccept(ModuleId),
    /// Reject it; the installed version keeps running.
    HeldReject(ModuleId),
    /// Allow this one ask; ask again next time.
    AskAllowOnce(u64),
    /// Allow and write `always` at user level.
    AskAllowAlways(u64),
    /// Deny and write `never` at user level.
    AskDeny(u64),
}

fn parse_module(id: &str) -> Option<ModuleId> {
    match ModuleId::new(id) {
        Ok(m) => Some(m),
        Err(e) => {
            tracing::warn!(id, error = %e, "rights page sent an invalid module id");
            None
        }
    }
}

fn parse_cap(name: &str) -> Option<Capability> {
    match name.parse::<Capability>() {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::warn!(name, error = %e, "rights page sent an unknown capability");
            None
        }
    }
}

/// Decode a `set-user` callback.
pub fn decode_set_user(module: &str, cap: &str, index: i32) -> Option<RightsCommand> {
    Some(RightsCommand::SetUser {
        module: parse_module(module)?,
        cap: parse_cap(cap)?,
        value: value_from_index(index)?,
    })
}

/// Decode a `set-workspace` callback.
pub fn decode_set_workspace(module: &str, cap: &str, index: i32) -> Option<RightsCommand> {
    Some(RightsCommand::SetWorkspace {
        module: parse_module(module)?,
        cap: parse_cap(cap)?,
        value: workspace_from_index(index)?,
    })
}

/// Decode a `select-profile` callback (`""` = no profile).
pub fn decode_select_profile(module: &str, profile: &str) -> Option<RightsCommand> {
    Some(RightsCommand::SelectProfile {
        module: parse_module(module)?,
        profile: (!profile.is_empty()).then(|| profile.to_string()),
    })
}

/// Bind every `RightsAdapter` callback to `on`. Call once at window construction; the
/// controller's closure applies the command to its state and resyncs.
pub fn wire(app: &crate::AppWindow, on: impl Fn(RightsCommand) + 'static) {
    let on = std::rc::Rc::new(on);
    let g = app.global::<crate::RightsAdapter>();
    {
        let on = on.clone();
        g.on_select_module(move |i| {
            if let Ok(i) = usize::try_from(i) {
                on(RightsCommand::SelectModule(i));
            }
        });
    }
    {
        let on = on.clone();
        g.on_set_user(move |m, c, v| {
            if let Some(cmd) = decode_set_user(&m, &c, v) {
                on(cmd);
            }
        });
    }
    {
        let on = on.clone();
        g.on_set_workspace(move |m, c, v| {
            if let Some(cmd) = decode_set_workspace(&m, &c, v) {
                on(cmd);
            }
        });
    }
    {
        let on = on.clone();
        g.on_select_profile(move |m, p| {
            if let Some(cmd) = decode_select_profile(&m, &p) {
                on(cmd);
            }
        });
    }
    {
        let on = on.clone();
        g.on_held_accept(move |m| {
            if let Some(m) = parse_module(&m) {
                on(RightsCommand::HeldAccept(m));
            }
        });
    }
    {
        let on = on.clone();
        g.on_held_reject(move |m| {
            if let Some(m) = parse_module(&m) {
                on(RightsCommand::HeldReject(m));
            }
        });
    }
    {
        let on = on.clone();
        g.on_ask_allow_once(move |id| on(RightsCommand::AskAllowOnce(id as u64)));
    }
    {
        let on = on.clone();
        g.on_ask_allow_always(move |id| on(RightsCommand::AskAllowAlways(id as u64)));
    }
    g.on_ask_deny(move |id| on(RightsCommand::AskDeny(id as u64)));
}

// ---------------------------------------------------------------------------------
// apply: a command against the service
// ---------------------------------------------------------------------------------

/// What [`apply`] did, for the controller to act on beyond a resync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// Nothing to do (already answered, unknown module, …).
    Nothing,
    /// A rights file was written; a resync shows the new column.
    Written,
    /// The page selection moved to this module index.
    Selected(usize),
    /// A held update was accepted: this is the accepted set the install store must
    /// re-sign into the module's record before the update proceeds.
    Accepted {
        module: ModuleId,
        accepted: BTreeSet<Capability>,
    },
    /// A held update was rejected.
    Rejected(ModuleId),
    /// An ask was answered; the module host delivers `decision` to the waiting request.
    Answered { ask: PendingAsk, decision: Decision },
}

/// Perform `cmd` on `service`. `workspace` is the key of the open workspace (the one the
/// page's workspace column shows); a `SetWorkspace` with no open workspace is a no-op.
pub fn apply(
    service: &mut RightsService,
    cmd: &RightsCommand,
    workspace: Option<&str>,
) -> std::io::Result<Applied> {
    Ok(match cmd {
        RightsCommand::SelectModule(i) => Applied::Selected(*i),
        RightsCommand::SetUser { module, cap, value } => {
            service.set_user(module, *cap, *value)?;
            Applied::Written
        }
        RightsCommand::SetWorkspace { module, cap, value } => match workspace {
            Some(ws) => {
                service.set_workspace(module, ws, *cap, *value)?;
                Applied::Written
            }
            None => Applied::Nothing,
        },
        RightsCommand::SelectProfile { module, profile } => {
            service.set_profile(module, profile.as_deref())?;
            Applied::Written
        }
        RightsCommand::HeldAccept(module) => match service.accept_held(module) {
            Some(accepted) => Applied::Accepted {
                module: module.clone(),
                accepted,
            },
            None => Applied::Nothing,
        },
        RightsCommand::HeldReject(module) => match service.reject_held(module) {
            Some(_) => Applied::Rejected(module.clone()),
            None => Applied::Nothing,
        },
        RightsCommand::AskAllowOnce(id) => answered(service, *id, AskAnswer::AllowOnce)?,
        RightsCommand::AskAllowAlways(id) => answered(service, *id, AskAnswer::Always)?,
        RightsCommand::AskDeny(id) => answered(service, *id, AskAnswer::Never)?,
    })
}

fn answered(service: &mut RightsService, id: u64, answer: AskAnswer) -> std::io::Result<Applied> {
    Ok(match service.answer(id, answer)? {
        Some((ask, decision)) => Applied::Answered { ask, decision },
        None => Applied::Nothing,
    })
}

// ---------------------------------------------------------------------------------
// fill: the service → the adapter
// ---------------------------------------------------------------------------------

fn model<T: Clone + 'static>(rows: Vec<T>) -> ModelRc<T> {
    ModelRc::from(std::rc::Rc::new(VecModel::from(rows)))
}

/// One module's list entry.
pub fn module_row(
    service: &RightsService,
    record: &avada_core::rights::InstallRecord,
    workspace: Option<&str>,
) -> crate::RightsModuleRow {
    let b = badge(&service.rows(&record.module_id, workspace));
    crate::RightsModuleRow {
        id: SharedString::from(record.module_id.as_str()),
        name: SharedString::from(record.manifest.module.name.as_str()),
        version: SharedString::from(record.version.to_string()),
        badge: SharedString::from(b.label()),
        level: b.level(),
        held: service.held.get(&record.module_id).is_some(),
    }
}

/// One capability row of the selected module.
pub fn cap_row(row: &RightsRow) -> crate::RightsCapRow {
    crate::RightsCapRow {
        cap: SharedString::from(row.cap.name()),
        description: SharedString::from(row.description),
        accepted: row.accepted,
        escape_hatch: row.cap.is_escape_hatch(),
        user: value_index(row.user),
        workspace: workspace_index(row.workspace),
        effective: SharedString::from(decision_label(row.effective)),
    }
}

/// Project the service into `RightsAdapter`. `selected` is the module the page shows
/// (`None` → the first one, so the page never opens blank when something is
/// installed); `workspace` is the open workspace's key and the label the page prints.
pub fn fill(
    app: &crate::AppWindow,
    service: &RightsService,
    selected: Option<&ModuleId>,
    workspace: Option<&str>,
) {
    let g = app.global::<crate::RightsAdapter>();
    let modules = service.modules();
    let sel_idx = selected
        .and_then(|s| modules.iter().position(|r| &r.module_id == s))
        .or(if modules.is_empty() { None } else { Some(0) });
    let sel = sel_idx.map(|i| modules[i].module_id.clone());

    g.set_modules(model(
        modules
            .iter()
            .map(|r| module_row(service, r, workspace))
            .collect(),
    ));
    g.set_selected(sel_idx.map(|i| i as i32).unwrap_or(-1));
    g.set_selected_id(SharedString::from(
        sel.as_ref().map(|m| m.as_str()).unwrap_or(""),
    ));
    g.set_workspace(SharedString::from(workspace.unwrap_or("")));

    match &sel {
        Some(m) => {
            g.set_rows(model(
                service.rows(m, workspace).iter().map(cap_row).collect(),
            ));
            let profiles = service.profiles(m);
            let current = service.user_rights(m).profile;
            g.set_profiles(model(
                profiles
                    .iter()
                    .map(|p| SharedString::from(p.name.as_str()))
                    .collect(),
            ));
            g.set_profile(
                current
                    .as_deref()
                    .and_then(|name| profiles.iter().position(|p| p.name == name))
                    .map(|i| i as i32)
                    .unwrap_or(-1),
            );
            match service.held.get(m) {
                Some(h) => {
                    g.set_held_present(true);
                    g.set_held_from(SharedString::from(h.from.to_string()));
                    g.set_held_to(SharedString::from(h.to.to_string()));
                    g.set_held_added(SharedString::from(cap_list(&h.added)));
                    g.set_held_removed(SharedString::from(cap_list(&h.removed)));
                }
                None => {
                    g.set_held_present(false);
                    g.set_held_from(SharedString::default());
                    g.set_held_to(SharedString::default());
                    g.set_held_added(SharedString::default());
                    g.set_held_removed(SharedString::default());
                }
            }
        }
        None => {
            g.set_rows(model(Vec::new()));
            g.set_profiles(model(Vec::new()));
            g.set_profile(-1);
            g.set_held_present(false);
        }
    }

    match service.asks.front() {
        Some(ask) => {
            g.set_ask_present(true);
            g.set_ask_id(ask.id as i32);
            g.set_ask_module(SharedString::from(ask.module.as_str()));
            g.set_ask_cap(SharedString::from(ask.cap.name()));
            g.set_ask_text(SharedString::from(ask_text(ask)));
        }
        None => {
            g.set_ask_present(false);
            g.set_ask_id(0);
            g.set_ask_module(SharedString::default());
            g.set_ask_cap(SharedString::default());
            g.set_ask_text(SharedString::default());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(cap: Capability, effective: Decision) -> RightsRow {
        RightsRow {
            cap,
            description: cap.describe(),
            accepted: true,
            user: RightValue::Ask,
            workspace: None,
            effective,
        }
    }

    #[test]
    fn picker_indices_round_trip() {
        for (i, v) in PICKER.iter().enumerate() {
            assert_eq!(value_index(*v), i as i32);
            assert_eq!(value_from_index(i as i32), Some(*v));
        }
        assert_eq!(value_from_index(4), None);
        assert_eq!(value_from_index(-1), None);
        assert_eq!(workspace_index(None), -1);
        assert_eq!(workspace_index(Some(RightValue::Always)), 1);
        assert_eq!(workspace_from_index(-1), Some(None));
        assert_eq!(workspace_from_index(0), Some(Some(RightValue::Never)));
        assert_eq!(
            workspace_from_index(2),
            None,
            "a workspace column cannot say `workspace`"
        );
        assert_eq!(workspace_from_index(3), Some(Some(RightValue::Ask)));
    }

    #[test]
    fn the_badge_is_the_widest_row() {
        assert_eq!(badge(&[]), Badge::None);
        assert_eq!(
            badge(&[row(Capability::FsRead, Decision::Deny)]),
            Badge::Denied
        );
        assert_eq!(
            badge(&[
                row(Capability::FsRead, Decision::Deny),
                row(Capability::NetFetch, Decision::Ask)
            ]),
            Badge::Asks
        );
        assert_eq!(
            badge(&[
                row(Capability::FsRead, Decision::Allow),
                row(Capability::NetFetch, Decision::Ask)
            ]),
            Badge::Allowed
        );
        assert_eq!(
            badge(&[
                row(Capability::FsRead, Decision::Allow),
                row(Capability::ProcessSpawn, Decision::Allow)
            ]),
            Badge::EscapeHatch
        );
        assert_eq!(
            badge(&[row(Capability::ProcessSpawn, Decision::Ask)]),
            Badge::Asks,
            "an escape hatch that still asks is not the widest thing"
        );
        assert!(Badge::EscapeHatch > Badge::Allowed && Badge::Allowed > Badge::Asks);
    }

    #[test]
    fn callbacks_decode_or_are_dropped() {
        assert_eq!(
            decode_set_user("acme/avada-files", "fs.read", 1),
            Some(RightsCommand::SetUser {
                module: ModuleId::new("acme/avada-files").unwrap(),
                cap: Capability::FsRead,
                value: RightValue::Always,
            })
        );
        assert_eq!(decode_set_user("not a module id", "fs.read", 1), None);
        assert_eq!(
            decode_set_user("acme/avada-files", "fs.everything", 1),
            None
        );
        assert_eq!(decode_set_user("acme/avada-files", "fs.read", 9), None);
        assert_eq!(
            decode_set_workspace("acme/avada-files", "fs.read", -1),
            Some(RightsCommand::SetWorkspace {
                module: ModuleId::new("acme/avada-files").unwrap(),
                cap: Capability::FsRead,
                value: None,
            })
        );
        assert_eq!(
            decode_select_profile("acme/avada-files", ""),
            Some(RightsCommand::SelectProfile {
                module: ModuleId::new("acme/avada-files").unwrap(),
                profile: None,
            })
        );
        assert_eq!(
            decode_select_profile("acme/avada-files", "High security")
                .map(|c| match c {
                    RightsCommand::SelectProfile { profile, .. } => profile,
                    _ => None,
                })
                .flatten(),
            Some("High security".to_string())
        );
    }

    #[test]
    fn labels_are_the_serde_words() {
        assert_eq!(decision_label(Decision::Allow), "allow");
        assert_eq!(decision_label(Decision::Deny), "deny");
        assert_eq!(decision_label(Decision::Ask), "ask");
        let caps: BTreeSet<Capability> = [Capability::NetFetch, Capability::FsRead]
            .into_iter()
            .collect();
        assert_eq!(cap_list(&caps), "fs.read, net.fetch");
        assert_eq!(cap_list(&BTreeSet::new()), "");
    }
}
