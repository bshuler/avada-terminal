//! Where rights live on disk and how they get there.
//!
//! Layout under the store root (default: `<data_dir>/modules/rights`, i.e. on macOS
//! `~/Library/Application Support/avada/modules/rights`):
//!
//! ```text
//! modules/rights/<owner>__<repo>.json          user-level ModuleRights (profile + overrides)
//! modules/rights/workspaces/<key>/rights.json  that workspace's override column
//! ```
//!
//! The workspace `<key>` is [`workspace_dir_name`] of the opaque key the app passes
//! (the workspace file's stem or path): the key sanitised to `[A-Za-z0-9._-]` plus a
//! short SHA-256 suffix, so two workspaces whose names differ only in characters the
//! filesystem would fold never share a file. The SDK's `WorkspaceModuleState` cannot be
//! changed, so the override column is a sibling `rights.json` rather than a field on it.
//!
//! Every write is temp-file + rename via `persistence::paths::write_atomic_private`:
//! the temp file is created `0o600` on Unix before the first byte lands; on Windows it
//! is a plain create (H7 adds the DACL). Rights are not secrets, but they are the user's
//! security decisions and nothing else on the machine should be able to widen them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{Capability, ModuleId, ModuleRights, RightValue};
use crate::persistence::paths;

/// One workspace's override column: module → capability → value. Absent means "no say".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkspaceRights {
    modules: BTreeMap<ModuleId, BTreeMap<Capability, RightValue>>,
}

impl WorkspaceRights {
    /// The override for one row, `None` when unset.
    pub fn get(&self, module: &ModuleId, cap: Capability) -> Option<RightValue> {
        self.modules.get(module).and_then(|m| m.get(&cap)).copied()
    }

    /// Set (`Some`) or clear (`None`) one row; an emptied module entry is dropped so
    /// the file stays minimal.
    pub fn set(&mut self, module: &ModuleId, cap: Capability, value: Option<RightValue>) {
        match value {
            Some(v) => {
                self.modules
                    .entry(module.clone())
                    .or_default()
                    .insert(cap, v);
            }
            None => {
                if let Some(m) = self.modules.get_mut(module) {
                    m.remove(&cap);
                    if m.is_empty() {
                        self.modules.remove(module);
                    }
                }
            }
        }
    }

    /// True when no module has any override here.
    pub fn is_empty(&self) -> bool {
        self.modules.is_empty()
    }
}

/// The on-disk side of the rights service. Pure path arithmetic plus JSON; it never
/// decides anything.
#[derive(Debug, Clone)]
pub struct RightsStore {
    root: PathBuf,
}

/// Directory name for a workspace key: sanitised stem + 8 hex chars of its SHA-256.
pub fn workspace_dir_name(key: &str) -> String {
    use sha2::Digest as _;
    let stem: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(48)
        .collect();
    let stem = stem.trim_matches('.');
    let stem = if stem.is_empty() { "workspace" } else { stem };
    let digest = sha2::Sha256::digest(key.as_bytes());
    let mut hex = String::with_capacity(8);
    for b in &digest[..4] {
        hex.push_str(&format!("{b:02x}"));
    }
    format!("{stem}-{hex}")
}

impl RightsStore {
    /// The real location: `<data_dir>/modules/rights`.
    pub fn default_root() -> PathBuf {
        paths::data_dir().join("modules").join("rights")
    }

    /// A store rooted at `root`. Nothing is created until the first save.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        RightsStore { root: root.into() }
    }

    /// The root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<root>/<owner>__<repo>.json`.
    pub fn user_path(&self, module: &ModuleId) -> PathBuf {
        self.root.join(format!("{}.json", module.dir_name()))
    }

    /// `<root>/workspaces/<key>/rights.json`.
    pub fn workspace_path(&self, workspace: &str) -> PathBuf {
        self.root
            .join("workspaces")
            .join(workspace_dir_name(workspace))
            .join("rights.json")
    }

    /// Read a module's user-level rights; a missing file is the default (`Ask`
    /// everywhere, no profile). A present but malformed file is an error so the caller
    /// can log it — it must not be silently replaced by defaults *on disk*.
    pub fn load_user(&self, module: &ModuleId) -> std::io::Result<ModuleRights> {
        read_json(&self.user_path(module))
    }

    /// Write a module's user-level rights atomically.
    pub fn save_user(&self, module: &ModuleId, rights: &ModuleRights) -> std::io::Result<()> {
        write_json(&self.user_path(module), rights)
    }

    /// Read one workspace's override column; a missing file is empty.
    pub fn load_workspace(&self, workspace: &str) -> std::io::Result<WorkspaceRights> {
        read_json(&self.workspace_path(workspace))
    }

    /// Write one workspace's override column atomically.
    pub fn save_workspace(&self, workspace: &str, rights: &WorkspaceRights) -> std::io::Result<()> {
        write_json(&self.workspace_path(workspace), rights)
    }
}

fn read_json<T: Default + for<'de> Deserialize<'de>>(path: &Path) -> std::io::Result<T> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(std::io::Error::other),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e),
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let bytes = serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?;
    paths::write_atomic_private(path, &bytes)
}
