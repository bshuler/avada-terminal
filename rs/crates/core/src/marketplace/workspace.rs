//! Per-workspace enable/disable state.
//!
//! An installed module is enabled per workspace, not globally. The state is the SDK's
//! [`WorkspaceModuleState`] persisted at `<modules root>/workspaces/<key>/modules.json`,
//! where `<key>` is the caller's workspace key (the app's stable workspace id). This
//! is a routine call recorded in `docs/marketplace.md`: the persistence layer already
//! had `read_workspace_modules`/`write_workspace_modules`, so the marketplace only
//! chose where the file lives and what the key looks like.

use crate::persistence::lockfile::{
    read_workspace_modules, write_workspace_modules, LockfileIoError,
};
use avada_module_sdk::install::WorkspaceModuleState;
use avada_module_sdk::ModuleId;
use semver::Version;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Subdirectory of the modules root holding per-workspace state.
pub const WORKSPACES_DIR: &str = "workspaces";
/// File name inside each workspace directory.
pub const STATE_FILE: &str = "modules.json";

/// The per-workspace state files under one root.
#[derive(Debug, Clone)]
pub struct WorkspaceStates {
    dir: PathBuf,
}

/// Whether `key` is a usable workspace key: non-empty, at most 128 chars, only
/// letters, digits, `-`, `_` and `.`, and not a dot-name (so it can never leave the
/// workspaces directory).
pub fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 128
        && key != "."
        && key != ".."
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

impl WorkspaceStates {
    /// State files under `<root>/workspaces`.
    pub fn under(root: &Path) -> Self {
        WorkspaceStates {
            dir: root.join(WORKSPACES_DIR),
        }
    }

    /// The directory holding every workspace's state.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// `<root>/workspaces/<key>/modules.json`.
    pub fn path(&self, key: &str) -> PathBuf {
        self.dir.join(key).join(STATE_FILE)
    }

    /// The state for `key` (default when there is no file).
    pub fn get(&self, key: &str) -> Result<WorkspaceModuleState, LockfileIoError> {
        Ok(read_workspace_modules(&self.path(key))?.get(key))
    }

    /// Enable or disable `id` in workspace `key`.
    pub fn set_enabled(
        &self,
        key: &str,
        id: &ModuleId,
        enabled: bool,
    ) -> Result<WorkspaceModuleState, LockfileIoError> {
        let path = self.path(key);
        let mut file = read_workspace_modules(&path)?;
        let mut state = file.get(key);
        state.enabled.insert(id.clone(), enabled);
        file.set(key, state.clone());
        write_workspace_modules(&path, &file)?;
        Ok(state)
    }

    /// Every workspace that mentions `id` → whether it is enabled there.
    pub fn enabled_in(&self, id: &ModuleId) -> BTreeMap<String, bool> {
        let mut out = BTreeMap::new();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let key = entry.file_name().to_string_lossy().into_owned();
            if !valid_key(&key) {
                continue;
            }
            if let Ok(state) = self.get(&key) {
                if let Some(enabled) = state.enabled.get(id) {
                    out.insert(key, *enabled);
                }
            }
        }
        out
    }

    /// Forget `id` in every workspace (after an uninstall of its last version).
    pub fn remove_module(&self, id: &ModuleId) -> Result<(), LockfileIoError> {
        for key in self.keys() {
            let path = self.path(&key);
            let mut file = read_workspace_modules(&path)?;
            let mut state = file.get(&key);
            let had = state.enabled.remove(id).is_some();
            let pinned = state.pins.remove(id).is_some();
            if !had && !pinned {
                continue;
            }
            file.set(&key, state);
            write_workspace_modules(&path, &file)?;
        }
        Ok(())
    }

    // ---- track G6 resolver

    /// Every workspace that has a state file, in directory order.
    ///
    /// A workspace only exists once something has been enabled or pinned in it, so
    /// this is the set the resolver and uninstall have to consider, not the set of
    /// workspaces the app knows about.
    pub fn keys(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut out: Vec<String> = entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|k| valid_key(k))
            .collect();
        out.sort();
        out
    }

    /// The versions pinned in `key`.
    ///
    /// A pin is the workspace saying "this exact version, whatever the resolver would
    /// otherwise pick": [`crate::install::resolver`] offers a pinned module no other
    /// candidate, so a pin that cannot be satisfied is a conflict rather than a
    /// silent upgrade.
    pub fn pins(&self, key: &str) -> Result<BTreeMap<ModuleId, Version>, LockfileIoError> {
        Ok(self.get(key)?.pins)
    }

    /// Pin `id` to `version` in `key`. The caller checks first that the pin can be
    /// satisfied ([`crate::install::resolver::check_pin`]); this only records it.
    pub fn set_pin(
        &self,
        key: &str,
        id: &ModuleId,
        version: &Version,
    ) -> Result<WorkspaceModuleState, LockfileIoError> {
        let path = self.path(key);
        let mut file = read_workspace_modules(&path)?;
        let mut state = file.get(key);
        state.pins.insert(id.clone(), version.clone());
        file.set(key, state.clone());
        write_workspace_modules(&path, &file)?;
        Ok(state)
    }

    /// Forget the pin on `id` in `key`; the resolver is free to move it again.
    pub fn clear_pin(
        &self,
        key: &str,
        id: &ModuleId,
    ) -> Result<WorkspaceModuleState, LockfileIoError> {
        let path = self.path(key);
        let mut file = read_workspace_modules(&path)?;
        let mut state = file.get(key);
        state.pins.remove(id);
        file.set(key, state.clone());
        write_workspace_modules(&path, &file)?;
        Ok(state)
    }

    // ---- end track G6 resolver
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("avada-mp-ws-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn keys_are_checked() {
        for ok in ["ws1", "my-work_space.v2", "a"] {
            assert!(valid_key(ok), "{ok}");
        }
        for bad in ["", ".", "..", "a/b", "a\\b", "with space", "ünïcode"] {
            assert!(!valid_key(bad), "{bad}");
        }
        assert!(!valid_key(&"x".repeat(129)));
    }

    #[test]
    fn enable_disable_persists_across_reopen_and_removal_forgets() {
        let root = scratch();
        let id = ModuleId::new("acme/avada-files").unwrap();
        let other = ModuleId::new("acme/avada-git").unwrap();
        {
            let ws = WorkspaceStates::under(&root);
            assert!(ws.get("ws1").unwrap().enabled.is_empty());
            assert!(ws.enabled_in(&id).is_empty());
            ws.set_enabled("ws1", &id, true).unwrap();
            ws.set_enabled("ws2", &id, false).unwrap();
            ws.set_enabled("ws2", &other, true).unwrap();
            assert!(ws.path("ws1").is_file());
        }
        let ws = WorkspaceStates::under(&root);
        let m = ws.enabled_in(&id);
        assert_eq!(m.get("ws1"), Some(&true));
        assert_eq!(m.get("ws2"), Some(&false));
        assert_eq!(m.len(), 2);
        ws.set_enabled("ws1", &id, false).unwrap();
        assert_eq!(ws.enabled_in(&id).get("ws1"), Some(&false));
        ws.remove_module(&id).unwrap();
        assert!(ws.enabled_in(&id).is_empty());
        assert_eq!(
            ws.enabled_in(&other).get("ws2"),
            Some(&true),
            "others untouched"
        );
        // The file is the SDK's format, keyed by the workspace key.
        let text = std::fs::read_to_string(ws.path("ws2")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["workspaces"]["ws2"]["enabled"]["acme/avada-git"], true);
        let _ = std::fs::remove_dir_all(&root);
    }
}
