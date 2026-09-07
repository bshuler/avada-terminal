//! The lockfile (`modules.lock`) and per-workspace module state.
//!
//! The lockfile is the machine's record of what is installed and which provider
//! was chosen for each shape. It is plain JSON, written atomically by the host, and
//! is *not* trusted for rights (that is the signed install record); it exists so a
//! second machine can reproduce the same set by `avada module sync`.

use crate::manifest::{DistributionKind, ModuleId};
use crate::rights::InstallKind;
use semver::Version;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Lockfile schema version.
pub const LOCKFILE_VERSION: u32 = 1;

/// One installed module.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedModule {
    /// `owner/repo`.
    pub id: ModuleId,
    /// Installed version.
    pub version: Version,
    /// Git tag.
    pub tag: String,
    /// Commit the tag resolved to.
    pub commit: String,
    /// Artifact digest, hex.
    pub sha256: String,
    /// Source or binary.
    pub source: DistributionKind,
    /// Unix seconds.
    pub installed_at: u64,
    /// Manual or dependency.
    pub kind: InstallKind,
}

/// The lockfile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lockfile {
    /// [`LOCKFILE_VERSION`].
    pub version: u32,
    /// Sorted by id.
    #[serde(default)]
    pub modules: Vec<LockedModule>,
    /// Shape → the module chosen to provide it when more than one could.
    #[serde(default)]
    pub providers: BTreeMap<String, ModuleId>,
    /// Module → enabled by default in new workspaces.
    #[serde(default)]
    pub defaults: BTreeMap<ModuleId, bool>,
}

impl Default for Lockfile {
    fn default() -> Self {
        Lockfile {
            version: LOCKFILE_VERSION,
            modules: Vec::new(),
            providers: BTreeMap::new(),
            defaults: BTreeMap::new(),
        }
    }
}

/// Lockfile problems.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockfileError {
    /// Not JSON, or wrong shape.
    Parse(String),
    /// A newer host wrote it.
    UnsupportedVersion(u32),
    /// Two entries share an id.
    Duplicate(ModuleId),
    /// A provider names a module that is not installed.
    UnknownProvider(String, ModuleId),
}

impl std::fmt::Display for LockfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockfileError::Parse(e) => write!(f, "modules.lock does not parse: {e}"),
            LockfileError::UnsupportedVersion(v) => {
                write!(
                    f,
                    "modules.lock version {v} is newer than this host understands"
                )
            }
            LockfileError::Duplicate(id) => write!(f, "modules.lock lists `{id}` twice"),
            LockfileError::UnknownProvider(shape, id) => {
                write!(
                    f,
                    "modules.lock names `{id}` as provider of `{shape}` but it is not installed"
                )
            }
        }
    }
}
impl std::error::Error for LockfileError {}

impl Lockfile {
    /// Parse and validate.
    pub fn parse(text: &str) -> Result<Lockfile, LockfileError> {
        let lf: Lockfile =
            serde_json::from_str(text).map_err(|e| LockfileError::Parse(e.to_string()))?;
        lf.validate()?;
        Ok(lf)
    }

    /// Pretty JSON with a trailing newline.
    pub fn to_json(&self) -> String {
        let mut s = serde_json::to_string_pretty(self).expect("lockfile serializes");
        s.push('\n');
        s
    }

    /// Structural rules.
    pub fn validate(&self) -> Result<(), LockfileError> {
        if self.version > LOCKFILE_VERSION {
            return Err(LockfileError::UnsupportedVersion(self.version));
        }
        let mut seen = std::collections::BTreeSet::new();
        for m in &self.modules {
            if !seen.insert(&m.id) {
                return Err(LockfileError::Duplicate(m.id.clone()));
            }
        }
        for (shape, id) in &self.providers {
            if !seen.contains(id) {
                return Err(LockfileError::UnknownProvider(shape.clone(), id.clone()));
            }
        }
        Ok(())
    }

    /// Look up one module.
    pub fn get(&self, id: &ModuleId) -> Option<&LockedModule> {
        self.modules.iter().find(|m| &m.id == id)
    }

    /// Insert or replace, keeping the list sorted by id.
    pub fn upsert(&mut self, m: LockedModule) {
        self.modules.retain(|x| x.id != m.id);
        self.modules.push(m);
        self.modules.sort_by(|a, b| a.id.cmp(&b.id));
    }

    /// Remove a module and any provider choice or default that named it.
    pub fn remove(&mut self, id: &ModuleId) -> Option<LockedModule> {
        let pos = self.modules.iter().position(|m| &m.id == id)?;
        let removed = self.modules.remove(pos);
        self.providers.retain(|_, v| v != id);
        self.defaults.remove(id);
        Some(removed)
    }

    /// Dependency-kind modules that no manual module (transitively) depends on.
    /// `deps` gives each installed module's direct dependencies.
    pub fn orphans(&self, deps: &BTreeMap<ModuleId, Vec<ModuleId>>) -> Vec<ModuleId> {
        let mut keep = std::collections::BTreeSet::new();
        let mut stack: Vec<&ModuleId> = self
            .modules
            .iter()
            .filter(|m| m.kind == InstallKind::Manual)
            .map(|m| &m.id)
            .collect();
        while let Some(id) = stack.pop() {
            if keep.insert(id.clone()) {
                if let Some(ds) = deps.get(id) {
                    stack.extend(ds.iter());
                }
            }
        }
        self.modules
            .iter()
            .filter(|m| m.kind == InstallKind::Dependency && !keep.contains(&m.id))
            .map(|m| m.id.clone())
            .collect()
    }
}

/// Per-workspace module state, stored in the workspace file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WorkspaceModuleState {
    /// Module → enabled here. Absent means the lockfile default.
    #[serde(default)]
    pub enabled: BTreeMap<ModuleId, bool>,
    /// Module → pinned version for this workspace.
    #[serde(default)]
    pub pins: BTreeMap<ModuleId, Version>,
}

impl WorkspaceModuleState {
    /// Whether a module runs in this workspace.
    pub fn is_enabled(&self, id: &ModuleId, lock: &Lockfile) -> bool {
        self.enabled
            .get(id)
            .copied()
            .or_else(|| lock.defaults.get(id).copied())
            .unwrap_or(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> ModuleId {
        ModuleId::new(s).unwrap()
    }
    fn locked(s: &str, kind: InstallKind) -> LockedModule {
        LockedModule {
            id: id(s),
            version: Version::new(1, 0, 0),
            tag: "v1.0.0".into(),
            commit: "c".repeat(40),
            sha256: "0".repeat(64),
            source: DistributionKind::Source,
            installed_at: 1,
            kind,
        }
    }

    #[test]
    fn round_trip_and_sorted_upsert() {
        let mut lf = Lockfile::default();
        lf.upsert(locked("zed/z", InstallKind::Manual));
        lf.upsert(locked("acme/a", InstallKind::Manual));
        lf.upsert(locked("zed/z", InstallKind::Dependency));
        assert_eq!(lf.modules.len(), 2);
        assert_eq!(lf.modules[0].id.as_str(), "acme/a");
        assert_eq!(lf.modules[1].kind, InstallKind::Dependency);
        lf.providers.insert("avada.files.tree".into(), id("acme/a"));
        lf.defaults.insert(id("zed/z"), false);
        let back = Lockfile::parse(&lf.to_json()).unwrap();
        assert_eq!(back, lf);
    }

    #[test]
    fn validation_catches_duplicates_versions_and_dangling_providers() {
        let mut lf = Lockfile::default();
        lf.modules.push(locked("a/b", InstallKind::Manual));
        lf.modules.push(locked("a/b", InstallKind::Manual));
        assert_eq!(lf.validate(), Err(LockfileError::Duplicate(id("a/b"))));
        let mut lf = Lockfile::default();
        lf.providers.insert("x.y".into(), id("a/b"));
        assert!(matches!(
            lf.validate(),
            Err(LockfileError::UnknownProvider(..))
        ));
        let text = r#"{"version": 99}"#;
        assert_eq!(
            Lockfile::parse(text),
            Err(LockfileError::UnsupportedVersion(99))
        );
        assert!(matches!(
            Lockfile::parse("nope"),
            Err(LockfileError::Parse(_))
        ));
    }

    #[test]
    fn remove_clears_provider_and_default() {
        let mut lf = Lockfile::default();
        lf.upsert(locked("a/b", InstallKind::Manual));
        lf.providers.insert("x.y".into(), id("a/b"));
        lf.defaults.insert(id("a/b"), false);
        assert!(lf.remove(&id("a/b")).is_some());
        assert!(lf.providers.is_empty() && lf.defaults.is_empty());
        assert!(lf.remove(&id("a/b")).is_none());
    }

    #[test]
    fn orphans_are_dependencies_nothing_manual_reaches() {
        let mut lf = Lockfile::default();
        lf.upsert(locked("m/top", InstallKind::Manual));
        lf.upsert(locked("d/used", InstallKind::Dependency));
        lf.upsert(locked("d/deep", InstallKind::Dependency));
        lf.upsert(locked("d/orphan", InstallKind::Dependency));
        let deps = BTreeMap::from([
            (id("m/top"), vec![id("d/used")]),
            (id("d/used"), vec![id("d/deep")]),
        ]);
        assert_eq!(lf.orphans(&deps), vec![id("d/orphan")]);
    }

    #[test]
    fn workspace_state_layers_over_lock_defaults() {
        let mut lf = Lockfile::default();
        lf.defaults.insert(id("a/b"), false);
        let mut ws = WorkspaceModuleState::default();
        assert!(!ws.is_enabled(&id("a/b"), &lf));
        assert!(ws.is_enabled(&id("c/d"), &lf), "absent everywhere means on");
        ws.enabled.insert(id("a/b"), true);
        assert!(ws.is_enabled(&id("a/b"), &lf));
        let back: WorkspaceModuleState =
            serde_json::from_str(&serde_json::to_string(&ws).unwrap()).unwrap();
        assert_eq!(back, ws);
    }
}
