//! Where an install lives on disk (see the module docs in `install`): one directory per
//! module id, one per version under it, owner-only throughout.

use super::{io_at, InstallError};
use crate::persistence::lockfile::modules_root;
use avada_module_sdk::{Manifest, ModuleId, LOCKFILE_NAME};
use semver::Version;
use std::path::{Path, PathBuf};

/// `record.json` inside a version directory.
pub const RECORD_FILE: &str = "record.json";
/// The module's writable state directory inside a version directory.
pub const DATA_DIR: &str = "data";
/// The artifact directory inside a version directory.
pub const BIN_DIR: &str = "bin";
/// The key directory under the modules root.
pub const KEYS_DIR: &str = "keys";

/// The install layout rooted somewhere. [`InstallPaths::host`] is the real one under the
/// app-support data dir; [`InstallPaths::under`] roots it anywhere (tests use a temp dir
/// so nothing under the real app-support dir is touched).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPaths {
    root: PathBuf,
}

impl InstallPaths {
    /// `<data_dir>/modules`.
    pub fn host() -> Self {
        InstallPaths {
            root: modules_root(),
        }
    }

    /// The same layout rooted at `root`.
    pub fn under(root: impl Into<PathBuf>) -> Self {
        InstallPaths { root: root.into() }
    }

    /// The modules root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `modules.lock`.
    pub fn lockfile(&self) -> PathBuf {
        self.root.join(LOCKFILE_NAME)
    }

    /// The key directory.
    pub fn keys_dir(&self) -> PathBuf {
        self.root.join(KEYS_DIR)
    }

    /// `<root>/<owner>__<repo>`: every installed version of one module.
    pub fn module_dir(&self, id: &ModuleId) -> PathBuf {
        self.root.join(id.dir_name())
    }

    /// `<root>/<owner>__<repo>/<version>`.
    pub fn version_dir(&self, id: &ModuleId, version: &Version) -> PathBuf {
        self.module_dir(id).join(version.to_string())
    }

    /// The signed install record of one version.
    pub fn record_path(&self, id: &ModuleId, version: &Version) -> PathBuf {
        self.version_dir(id, version).join(RECORD_FILE)
    }

    /// The directory handed to the module as `AVADA_MODULE_DATA`.
    pub fn data_dir(&self, id: &ModuleId, version: &Version) -> PathBuf {
        self.version_dir(id, version).join(DATA_DIR)
    }

    /// The artifact: `<version>/bin/<name>`, where `name` is the manifest's
    /// `distribution.bin` or the repo name, plus the platform executable suffix.
    pub fn binary_path(&self, id: &ModuleId, version: &Version, manifest: &Manifest) -> PathBuf {
        self.version_dir(id, version)
            .join(BIN_DIR)
            .join(binary_name(id, manifest))
    }

    /// Whether `path` is inside this layout (tests assert it on every written file).
    pub fn contains(&self, path: &Path) -> bool {
        path.starts_with(&self.root)
    }
}

/// The artifact's file name for a module.
pub fn binary_name(id: &ModuleId, manifest: &Manifest) -> String {
    let stem = manifest
        .distribution
        .bin
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| id.repo());
    format!("{stem}{}", std::env::consts::EXE_SUFFIX)
}

/// Create `dir` (and its parents) readable and writable by the owner only. On Unix the
/// mode is set at creation and re-applied afterwards so a pre-existing directory is
/// narrowed too. On Windows this is a plain create; track H7 adds the DACLs.
#[cfg(unix)]
pub fn ensure_private_dir(dir: &Path) -> Result<(), InstallError> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| io_at(dir, e))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| io_at(dir, e))
}

/// Create `dir` (and its parents). Plain create on this platform.
#[cfg(not(unix))]
pub fn ensure_private_dir(dir: &Path) -> Result<(), InstallError> {
    std::fs::create_dir_all(dir).map_err(|e| io_at(dir, e))
}

/// Make the artifact runnable by its owner and nobody else (0700 on Unix).
#[cfg(unix)]
pub fn make_executable(file: &Path) -> Result<(), InstallError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| io_at(file, e))
}

/// No-op on this platform: Windows runs any file with an executable extension, and
/// track H7 adds the DACLs.
#[cfg(not(unix))]
pub fn make_executable(_file: &Path) -> Result<(), InstallError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Manifest {
        Manifest::parse(include_str!(
            "../../../module-sdk/tests/fixtures/avada.toml"
        ))
        .unwrap()
    }

    #[test]
    fn layout_hangs_off_the_root() {
        let p = InstallPaths::under("/tmp/x");
        let id = ModuleId::new("acme/avada-files").unwrap();
        let v = Version::new(1, 2, 0);
        assert_eq!(p.lockfile(), Path::new("/tmp/x/modules.lock"));
        assert_eq!(p.keys_dir(), Path::new("/tmp/x/keys"));
        assert_eq!(p.module_dir(&id), Path::new("/tmp/x/acme__avada-files"));
        assert_eq!(
            p.version_dir(&id, &v),
            Path::new("/tmp/x/acme__avada-files/1.2.0")
        );
        assert_eq!(
            p.record_path(&id, &v),
            Path::new("/tmp/x/acme__avada-files/1.2.0/record.json")
        );
        assert_eq!(
            p.data_dir(&id, &v),
            Path::new("/tmp/x/acme__avada-files/1.2.0/data")
        );
        let bin = p.binary_path(&id, &v, &manifest());
        assert_eq!(
            bin,
            Path::new("/tmp/x/acme__avada-files/1.2.0/bin")
                .join(format!("avada-files{}", std::env::consts::EXE_SUFFIX))
        );
        assert!(p.contains(&bin));
        assert!(!p.contains(Path::new("/tmp/y/modules.lock")));
    }

    #[test]
    fn binary_name_prefers_the_manifest_bin() {
        let id = ModuleId::new("acme/avada-files").unwrap();
        let mut m = manifest();
        m.distribution.bin = Some("files-host".into());
        assert_eq!(
            binary_name(&id, &m),
            format!("files-host{}", std::env::consts::EXE_SUFFIX)
        );
    }

    #[test]
    fn host_layout_is_under_the_data_dir() {
        assert_eq!(InstallPaths::host().root(), modules_root());
    }

    #[cfg(unix)]
    #[test]
    fn private_dir_is_owner_only_even_when_it_existed() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!(
            "avada-dirs-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let dir = root.join("a").join("b");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        ensure_private_dir(&dir).unwrap();
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let fresh = root.join("c");
        ensure_private_dir(&fresh).unwrap();
        assert_eq!(
            std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
