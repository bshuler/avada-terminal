//! Rights service (docs/modules-fanout-plan.md, track H2): resolves what a module may
//! do from its signed install record, the user's `never|always|workspace|ask` values
//! and the workspace override column. NOT `permissions/`, which probes the OS.
//!
//! The rule set lives in the frozen SDK ([`resolve`]); this module owns the *state*
//! around it and the two flows the plan asks for:
//!
//! * [`RightsService`] — the in-memory truth the host consults on every capability call:
//!   verified install records (registered by the install store after signature
//!   verification), the user-level [`ModuleRights`] per module (profile + per-row
//!   overrides, persisted as JSON by [`store::RightsStore`]) and the per-workspace
//!   override column. [`RightsService::decide`] is the whole gate; the orchestrator
//!   wraps it in the `CapabilityGate` trait from `module/gate.rs`.
//! * [`asks::AskQueue`] — a `Decision::Ask` is a toast, not a modal. The host parks
//!   the request as a [`PendingAsk`] and the answer (`allow once | always | workspace |
//!   never`) either just resolves that one request or writes the persisted value.
//! * [`held::HeldStore`] — an update that widens the capability set is parked as a
//!   [`HeldUpdate`] until the user accepts the diff; `accept` yields the new accepted
//!   set the install store re-signs into the next record.
//!
//! Rights come only from the install record: a capability outside `accepted` is denied
//! whatever the user or workspace column says (`resolve(false, ..)`), so nothing here
//! can widen what the user agreed to at install time.

pub mod asks;
pub mod held;
pub mod store;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub use avada_module_sdk::caps::{resolve, Capability, Decision, RightValue};
pub use avada_module_sdk::manifest::{DistributionKind, Manifest, ModuleId};
pub use avada_module_sdk::rights::{
    HeldUpdate, InstallKind, InstallRecord, ModuleRights, PermissionProfile, RightsError,
    SignedInstallRecord,
};

pub use asks::{AskAnswer, AskQueue, PendingAsk};
pub use held::HeldStore;
pub use store::{RightsStore, WorkspaceRights};

/// One line of the per-module rights page: a declared capability and every column that
/// feeds its decision, so the page never re-derives the rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RightsRow {
    /// The capability this row is about.
    pub cap: Capability,
    /// [`Capability::describe`] — the one-line plain-language meaning.
    pub description: &'static str,
    /// Whether it is in the install record's accepted set. `false` rows are shown
    /// greyed: the user declined them at install and no column can re-grant them.
    pub accepted: bool,
    /// The user-level value (profile or override), which the picker edits.
    pub user: RightValue,
    /// The workspace override column, `None` when the workspace has no say.
    pub workspace: Option<RightValue>,
    /// [`resolve`] over the three columns — what the module would get right now.
    pub effective: Decision,
}

/// The rights truth for every installed module. See the module docs for the shape.
#[derive(Debug)]
pub struct RightsService {
    store: RightsStore,
    installs: BTreeMap<ModuleId, InstallRecord>,
    user: BTreeMap<ModuleId, ModuleRights>,
    /// Workspace key → overrides, loaded lazily per workspace on first touch.
    workspaces: BTreeMap<String, WorkspaceRights>,
    /// The ask toast queue. Public because the host drains it directly.
    pub asks: AskQueue,
    /// Held updates awaiting the user's accept/reject.
    pub held: HeldStore,
}

impl RightsService {
    /// A service rooted at [`RightsStore::default_root`] (the real app-support dir).
    /// Tests use [`RightsService::with_root`] on a temp dir.
    pub fn new() -> Self {
        Self::with_root(RightsStore::default_root())
    }

    /// A service whose files live under `root` (see [`RightsStore`] for the layout).
    /// Nothing is read until a module is registered.
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        RightsService {
            store: RightsStore::new(root),
            installs: BTreeMap::new(),
            user: BTreeMap::new(),
            workspaces: BTreeMap::new(),
            asks: AskQueue::default(),
            held: HeldStore::default(),
        }
    }

    /// Where this service persists (for the report line in the prefs page).
    pub fn root(&self) -> &Path {
        self.store.root()
    }

    /// Register a *verified* install record — the caller (install store, H5) has already
    /// checked the signature; this type never sees the key. Loads the module's saved
    /// user-level rights from disk (a missing or unreadable file means "defaults",
    /// i.e. every accepted capability at [`RightValue::Ask`], which is the safe side).
    pub fn register(&mut self, record: InstallRecord) {
        let id = record.module_id.clone();
        let rights = self.store.load_user(&id).unwrap_or_else(|e| {
            tracing::warn!(module = %id, error = %e, "rights: user file unreadable, using defaults");
            ModuleRights::default()
        });
        self.user.insert(id.clone(), rights);
        self.installs.insert(id, record);
    }

    /// Forget a module (uninstall). Its files stay on disk so a reinstall keeps the
    /// user's choices; the install store deletes them on an explicit purge.
    pub fn unregister(&mut self, module: &ModuleId) {
        self.installs.remove(module);
        self.user.remove(module);
        self.held.reject(module);
        self.asks.drop_module(module);
    }

    /// Every registered module, sorted by id.
    pub fn modules(&self) -> Vec<&InstallRecord> {
        self.installs.values().collect()
    }

    /// The verified record for a module, if registered.
    pub fn record(&self, module: &ModuleId) -> Option<&InstallRecord> {
        self.installs.get(module)
    }

    /// The user-level rights (profile + overrides) for a module; defaults when unknown.
    pub fn user_rights(&self, module: &ModuleId) -> ModuleRights {
        self.user.get(module).cloned().unwrap_or_default()
    }

    /// The named profiles the module's manifest ships.
    pub fn profiles(&self, module: &ModuleId) -> &[PermissionProfile] {
        self.installs
            .get(module)
            .map(|r| r.manifest.profiles.as_slice())
            .unwrap_or(&[])
    }

    /// The user-level value for one capability: override → selected profile → `Ask`.
    pub fn user_value(&self, module: &ModuleId, cap: Capability) -> RightValue {
        match (self.user.get(module), self.installs.get(module)) {
            (Some(rights), Some(rec)) => rights.value(cap, &rec.manifest.profiles),
            (Some(rights), None) => rights.value(cap, &[]),
            _ => RightValue::default(),
        }
    }

    /// The workspace override for one capability, `None` when unset.
    pub fn workspace_value(
        &self,
        module: &ModuleId,
        cap: Capability,
        workspace: Option<&str>,
    ) -> Option<RightValue> {
        let key = workspace?;
        self.workspaces.get(key).and_then(|w| w.get(module, cap))
    }

    /// The gate. Not registered or not accepted → `Deny`; otherwise [`resolve`] over the
    /// user value and the workspace override.
    pub fn decide(&self, module: &ModuleId, cap: Capability, workspace: Option<&str>) -> Decision {
        let Some(rec) = self.installs.get(module) else {
            return Decision::Deny;
        };
        let accepted = rec.accepted.contains(&cap);
        let user = self.user_value(module, cap);
        let ws = self.workspace_value(module, cap, workspace);
        resolve(accepted, user, ws)
    }

    /// Set a user-level override for one row and persist the module's rights file.
    pub fn set_user(
        &mut self,
        module: &ModuleId,
        cap: Capability,
        value: RightValue,
    ) -> std::io::Result<()> {
        let rights = self.user.entry(module.clone()).or_default();
        rights.overrides.insert(cap, value);
        self.store.save_user(module, rights)
    }

    /// Clear a user-level override so the row falls back to the profile (or `Ask`).
    pub fn clear_user(&mut self, module: &ModuleId, cap: Capability) -> std::io::Result<()> {
        let rights = self.user.entry(module.clone()).or_default();
        rights.overrides.remove(&cap);
        self.store.save_user(module, rights)
    }

    /// Select (or with `None`, drop) the named profile. Selecting a profile only
    /// pre-selects rows — every override the user made stays, so a profile is never a
    /// hidden grant on top of an explicit choice. `Err(InvalidInput)` for a name the
    /// manifest does not ship.
    pub fn set_profile(&mut self, module: &ModuleId, profile: Option<&str>) -> std::io::Result<()> {
        if let Some(name) = profile {
            if !self.profiles(module).iter().any(|p| p.name == name) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("module {module} ships no profile named {name:?}"),
                ));
            }
        }
        let rights = self.user.entry(module.clone()).or_default();
        rights.profile = profile.map(str::to_owned);
        self.store.save_user(module, rights)
    }

    /// Make sure a workspace's override file is loaded (a missing file is empty).
    pub fn load_workspace(&mut self, workspace: &str) {
        if !self.workspaces.contains_key(workspace) {
            let w = self.store.load_workspace(workspace).unwrap_or_else(|e| {
                tracing::warn!(workspace, error = %e, "rights: workspace file unreadable, using empty");
                WorkspaceRights::default()
            });
            self.workspaces.insert(workspace.to_owned(), w);
        }
    }

    /// Set (`Some`) or clear (`None`) the workspace override for one row and persist
    /// that workspace's `rights.json`.
    pub fn set_workspace(
        &mut self,
        module: &ModuleId,
        workspace: &str,
        cap: Capability,
        value: Option<RightValue>,
    ) -> std::io::Result<()> {
        self.load_workspace(workspace);
        let w = self.workspaces.entry(workspace.to_owned()).or_default();
        w.set(module, cap, value);
        self.store.save_workspace(workspace, w)
    }

    /// One row per capability the manifest declares, in manifest order.
    pub fn rows(&self, module: &ModuleId, workspace: Option<&str>) -> Vec<RightsRow> {
        let Some(rec) = self.installs.get(module) else {
            return Vec::new();
        };
        rec.manifest
            .capabilities
            .iter()
            .map(|&cap| {
                let accepted = rec.accepted.contains(&cap);
                let user = self.user_value(module, cap);
                let ws = self.workspace_value(module, cap, workspace);
                RightsRow {
                    cap,
                    description: cap.describe(),
                    accepted,
                    user,
                    workspace: ws,
                    effective: resolve(accepted, user, ws),
                }
            })
            .collect()
    }

    // ---- ask flow -------------------------------------------------------------------

    /// Park a `Decision::Ask` as a toast. Returns the ask id the answer refers to.
    pub fn ask(&mut self, module: &ModuleId, cap: Capability, workspace: Option<&str>) -> u64 {
        self.asks
            .push(module.clone(), cap, workspace.map(str::to_owned))
    }

    /// Answer a pending ask. `AllowOnce` resolves only this request; `Always` and
    /// `Never` write the user-level value; `Workspace` writes `Always` into the ask's
    /// workspace column (and, so the row actually defers there, moves a user value of
    /// `Ask` to `Workspace`). Returns the ask and the decision the host should relay to
    /// the module — `None` for an unknown id (already answered or dropped).
    pub fn answer(
        &mut self,
        id: u64,
        answer: AskAnswer,
    ) -> std::io::Result<Option<(PendingAsk, Decision)>> {
        let Some(ask) = self.asks.take(id) else {
            return Ok(None);
        };
        let decision = match answer {
            AskAnswer::AllowOnce => Decision::Allow,
            AskAnswer::Always => {
                self.set_user(&ask.module, ask.cap, RightValue::Always)?;
                Decision::Allow
            }
            AskAnswer::Never => {
                self.set_user(&ask.module, ask.cap, RightValue::Never)?;
                Decision::Deny
            }
            AskAnswer::Workspace => match ask.workspace.as_deref() {
                Some(ws) => {
                    if self.user_value(&ask.module, ask.cap) == RightValue::Ask {
                        self.set_user(&ask.module, ask.cap, RightValue::Workspace)?;
                    }
                    self.set_workspace(&ask.module, ws, ask.cap, Some(RightValue::Always))?;
                    Decision::Allow
                }
                // No workspace to write into: behave as allow-once rather than
                // silently widening the user column.
                None => Decision::Allow,
            },
        };
        Ok(Some((ask, decision)))
    }

    // ---- held updates ---------------------------------------------------------------

    /// Compare a candidate manifest with the installed record. An update that adds a
    /// capability is parked in [`Self::held`] and `Some(&held)` returned; `None` means
    /// the update widens nothing and may be applied at once.
    pub fn check_update(&mut self, module: &ModuleId, candidate: &Manifest) -> Option<&HeldUpdate> {
        let rec = self.installs.get(module)?;
        let held = HeldUpdate::check(rec, candidate)?;
        self.held.hold(held);
        self.held.get(module)
    }

    /// Accept a held update: returns the accepted set for the *next* install record —
    /// what the user had accepted, minus what the update dropped, plus what it added —
    /// for the install store to re-sign. `None` when nothing is held for the module.
    pub fn accept_held(&mut self, module: &ModuleId) -> Option<BTreeSet<Capability>> {
        let installed = self.installs.get(module).map(|r| &r.accepted);
        self.held.accept(module, installed)
    }

    /// Reject a held update (it stays uninstalled; the module keeps running as is).
    pub fn reject_held(&mut self, module: &ModuleId) -> Option<HeldUpdate> {
        self.held.reject(module)
    }
}

impl Default for RightsService {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
