//! `modules.lock` on disk (docs/modules-fanout-plan.md, track H5): where the lockfile and
//! the per-workspace module state live, atomic write, and the read path that tolerates a
//! missing file. The wire format itself is `avada_module_sdk::install::Lockfile`.
//! Owned by the H5 track in Wave 1.
//!
//! Two files:
//!
//! * `<data_dir>/modules/modules.lock` — the machine-wide lockfile. One entry per module
//!   id naming the **active** version; other versions may sit beside it on disk
//!   (`install::dirs`) and a workspace may pin one of them.
//! * `<workspace dir>/modules.json` — [`WorkspaceModulesFile`], a map from a workspace
//!   key to its [`WorkspaceModuleState`] (enabled overrides and pins). It sits beside the
//!   workspace files rather than inside them so the workspace format itself is untouched.
//!   The key is [`workspace_key`]: the workspace file's stem, because a saved workspace
//!   has no uid of its own today. When the model grows one, pass it to
//!   [`WorkspaceModulesFile::get`] / [`WorkspaceModulesFile::set`] directly.
//!
//! Both reads treat a missing file as empty and refuse a file whose `version` is newer
//! than this build understands, so an older build never silently drops what a newer one
//! wrote. Both writes are atomic (temp + rename) and owner-only.

use crate::persistence::paths;
use avada_module_sdk::install::{Lockfile, LockfileError, WorkspaceModuleState};
use avada_module_sdk::LOCKFILE_NAME;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

/// The directory under the app-support data dir that holds every installed module, the
/// lockfile and the signing keys.
pub const MODULES_DIR: &str = "modules";

/// The per-workspace module state file, written beside the workspace files.
pub const WORKSPACE_MODULES_FILE: &str = "modules.json";

/// Schema version of [`WorkspaceModulesFile`].
pub const WORKSPACE_MODULES_VERSION: u32 = 1;

/// `<data_dir>/modules`: the root every install-store path hangs off.
#[tracing::instrument(level = "debug", ret)]
pub fn modules_root() -> PathBuf {
    paths::data_dir().join(MODULES_DIR)
}

/// `<data_dir>/modules/modules.lock`.
#[tracing::instrument(level = "debug", ret)]
pub fn lockfile_path() -> PathBuf {
    modules_root().join(LOCKFILE_NAME)
}

/// Why a lockfile or workspace-state read or write failed.
#[derive(Debug)]
pub enum LockfileIoError {
    /// The filesystem said no.
    Io {
        /// The file involved.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The file is present but not something this build can use: malformed, a newer
    /// schema version, or structurally invalid.
    Format {
        /// The file involved.
        path: PathBuf,
        /// What the SDK's validator objected to.
        source: LockfileError,
    },
}

impl fmt::Display for LockfileIoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LockfileIoError::Io { path, source } => write!(f, "{}: {source}", path.display()),
            LockfileIoError::Format { path, source } => {
                write!(f, "{}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for LockfileIoError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LockfileIoError::Io { source, .. } => Some(source),
            LockfileIoError::Format { source, .. } => Some(source),
        }
    }
}

fn io_err(path: &Path, source: std::io::Error) -> LockfileIoError {
    LockfileIoError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Read `modules.lock`. A missing file is an empty lockfile; a file written by a newer
/// build (`version > LOCKFILE_VERSION`) is refused rather than misread.
#[tracing::instrument(level = "debug", skip_all, fields(path = %path.display()))]
pub fn read_lockfile(path: &Path) -> Result<Lockfile, LockfileIoError> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Lockfile::default()),
        Err(e) => return Err(io_err(path, e)),
    };
    Lockfile::parse(&text).map_err(|source| LockfileIoError::Format {
        path: path.to_path_buf(),
        source,
    })
}

/// Write `modules.lock` atomically (temp file + rename, owner-only). The lockfile is
/// validated first so a structurally broken one never reaches disk.
#[tracing::instrument(level = "debug", skip_all, fields(path = %path.display()))]
pub fn write_lockfile(path: &Path, lockfile: &Lockfile) -> Result<(), LockfileIoError> {
    lockfile
        .validate()
        .map_err(|source| LockfileIoError::Format {
            path: path.to_path_buf(),
            source,
        })?;
    paths::write_atomic_private(path, lockfile.to_json().as_bytes()).map_err(|e| io_err(path, e))
}

/// The on-disk shape of `modules.json`: every workspace's module state, keyed by
/// [`workspace_key`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceModulesFile {
    /// [`WORKSPACE_MODULES_VERSION`].
    pub version: u32,
    /// Workspace key → its state. A workspace with no entry uses the lockfile defaults.
    #[serde(default)]
    pub workspaces: BTreeMap<String, WorkspaceModuleState>,
}

impl Default for WorkspaceModulesFile {
    fn default() -> Self {
        WorkspaceModulesFile {
            version: WORKSPACE_MODULES_VERSION,
            workspaces: BTreeMap::new(),
        }
    }
}

impl WorkspaceModulesFile {
    /// The state for one workspace, or the default when it has none.
    pub fn get(&self, key: &str) -> WorkspaceModuleState {
        self.workspaces.get(key).cloned().unwrap_or_default()
    }

    /// Replace one workspace's state. An all-default state removes the entry so the file
    /// never accumulates rows that say nothing.
    pub fn set(&mut self, key: &str, state: WorkspaceModuleState) {
        if state == WorkspaceModuleState::default() {
            self.workspaces.remove(key);
        } else {
            self.workspaces.insert(key.to_string(), state);
        }
    }

    /// Pretty JSON with a trailing newline.
    pub fn to_json(&self) -> String {
        let mut s = serde_json::to_string_pretty(self).expect("workspace modules file serializes");
        s.push('\n');
        s
    }

    /// Parse and refuse a newer schema.
    pub fn parse(text: &str) -> Result<Self, LockfileError> {
        let file: WorkspaceModulesFile =
            serde_json::from_str(text).map_err(|e| LockfileError::Parse(e.to_string()))?;
        if file.version > WORKSPACE_MODULES_VERSION {
            return Err(LockfileError::UnsupportedVersion(file.version));
        }
        Ok(file)
    }
}

/// The `modules.json` beside a workspace file: same directory, fixed name.
pub fn workspace_modules_path(workspace_file: &Path) -> PathBuf {
    workspace_file
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(WORKSPACE_MODULES_FILE)
}

/// The key a workspace file is stored under in `modules.json`: its file stem
/// (`dev.avada` → `dev`). Saved workspaces carry no uid, so the stem is the stable
/// identity the library already uses to list them.
pub fn workspace_key(workspace_file: &Path) -> String {
    workspace_file
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Read a `modules.json`; missing means empty, a newer version is refused.
#[tracing::instrument(level = "debug", skip_all, fields(path = %path.display()))]
pub fn read_workspace_modules(path: &Path) -> Result<WorkspaceModulesFile, LockfileIoError> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(WorkspaceModulesFile::default())
        }
        Err(e) => return Err(io_err(path, e)),
    };
    WorkspaceModulesFile::parse(&text).map_err(|source| LockfileIoError::Format {
        path: path.to_path_buf(),
        source,
    })
}

/// Write a `modules.json` atomically, owner-only.
#[tracing::instrument(level = "debug", skip_all, fields(path = %path.display()))]
pub fn write_workspace_modules(
    path: &Path,
    file: &WorkspaceModulesFile,
) -> Result<(), LockfileIoError> {
    paths::write_atomic_private(path, file.to_json().as_bytes()).map_err(|e| io_err(path, e))
}

/// The module state of one workspace file (its sibling `modules.json`, its own key).
pub fn read_workspace_state(
    workspace_file: &Path,
) -> Result<WorkspaceModuleState, LockfileIoError> {
    let file = read_workspace_modules(&workspace_modules_path(workspace_file))?;
    Ok(file.get(&workspace_key(workspace_file)))
}

/// Store the module state of one workspace file: read-modify-write of the sibling
/// `modules.json`, so other workspaces' rows survive.
pub fn write_workspace_state(
    workspace_file: &Path,
    state: &WorkspaceModuleState,
) -> Result<(), LockfileIoError> {
    let path = workspace_modules_path(workspace_file);
    let mut file = read_workspace_modules(&path)?;
    file.set(&workspace_key(workspace_file), state.clone());
    write_workspace_modules(&path, &file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use avada_module_sdk::install::{LockedModule, LOCKFILE_VERSION};
    use avada_module_sdk::manifest::DistributionKind;
    use avada_module_sdk::rights::InstallKind;
    use avada_module_sdk::ModuleId;
    use semver::Version;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "avada-lockfile-{tag}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn id(s: &str) -> ModuleId {
        ModuleId::new(s).unwrap()
    }

    fn locked(s: &str) -> LockedModule {
        LockedModule {
            id: id(s),
            version: Version::new(1, 0, 0),
            tag: "v1.0.0".into(),
            commit: "c".repeat(40),
            sha256: "0".repeat(64),
            source: DistributionKind::Source,
            installed_at: 1,
            kind: InstallKind::Manual,
        }
    }

    #[test]
    fn missing_lockfile_reads_as_empty() {
        let dir = scratch("missing");
        let lf = read_lockfile(&dir.join(LOCKFILE_NAME)).unwrap();
        assert_eq!(lf, Lockfile::default());
        assert_eq!(lf.version, LOCKFILE_VERSION);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lockfile_round_trips_and_is_written_atomically() {
        let dir = scratch("roundtrip");
        let path = dir.join(LOCKFILE_NAME);
        let mut lf = Lockfile::default();
        lf.upsert(locked("acme/a"));
        lf.defaults.insert(id("acme/a"), false);
        write_lockfile(&path, &lf).unwrap();
        assert_eq!(read_lockfile(&path).unwrap(), lf);
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n != LOCKFILE_NAME)
            .collect();
        assert!(
            leftovers.is_empty(),
            "no temp files left behind: {leftovers:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_newer_lockfile_is_refused_not_misread() {
        let dir = scratch("newer");
        let path = dir.join(LOCKFILE_NAME);
        std::fs::write(&path, format!("{{\"version\": {}}}", LOCKFILE_VERSION + 1)).unwrap();
        match read_lockfile(&path).unwrap_err() {
            LockfileIoError::Format { source, .. } => {
                assert_eq!(
                    source,
                    LockfileError::UnsupportedVersion(LOCKFILE_VERSION + 1)
                )
            }
            other => panic!("expected a format error, got {other}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_invalid_lockfile_never_reaches_disk() {
        let dir = scratch("invalid");
        let path = dir.join(LOCKFILE_NAME);
        let mut lf = Lockfile::default();
        lf.providers.insert("x.y".into(), id("a/b"));
        assert!(matches!(
            write_lockfile(&path, &lf).unwrap_err(),
            LockfileIoError::Format { .. }
        ));
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn workspace_state_round_trips_through_a_sibling_file() {
        let dir = scratch("ws");
        let dev = dir.join("dev.avada");
        let ops = dir.join("ops.avada");
        assert_eq!(workspace_key(&dev), "dev");
        assert_eq!(
            workspace_modules_path(&dev),
            dir.join(WORKSPACE_MODULES_FILE)
        );
        assert_eq!(
            read_workspace_state(&dev).unwrap(),
            WorkspaceModuleState::default()
        );

        let mut state = WorkspaceModuleState::default();
        state.enabled.insert(id("acme/a"), false);
        state.pins.insert(id("acme/b"), Version::new(0, 9, 0));
        write_workspace_state(&dev, &state).unwrap();
        let mut other = WorkspaceModuleState::default();
        other.enabled.insert(id("acme/c"), true);
        write_workspace_state(&ops, &other).unwrap();

        assert_eq!(
            read_workspace_state(&dev).unwrap(),
            state,
            "dev survives ops's write"
        );
        assert_eq!(read_workspace_state(&ops).unwrap(), other);
        let file = read_workspace_modules(&workspace_modules_path(&dev)).unwrap();
        assert_eq!(file.workspaces.len(), 2);

        write_workspace_state(&dev, &WorkspaceModuleState::default()).unwrap();
        let file = read_workspace_modules(&workspace_modules_path(&dev)).unwrap();
        assert!(
            !file.workspaces.contains_key("dev"),
            "a default state removes its row"
        );
        assert_eq!(file.workspaces.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_newer_workspace_modules_file_is_refused() {
        let dir = scratch("ws-newer");
        let path = dir.join(WORKSPACE_MODULES_FILE);
        std::fs::write(
            &path,
            format!("{{\"version\": {}}}", WORKSPACE_MODULES_VERSION + 1),
        )
        .unwrap();
        assert!(matches!(
            read_workspace_modules(&path).unwrap_err(),
            LockfileIoError::Format {
                source: LockfileError::UnsupportedVersion(_),
                ..
            }
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn host_paths_hang_off_the_data_dir() {
        assert_eq!(modules_root(), paths::data_dir().join(MODULES_DIR));
        assert_eq!(lockfile_path(), modules_root().join(LOCKFILE_NAME));
    }
}
