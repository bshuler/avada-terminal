//! [`InstallStore`]: the one door to installed modules. Every read verifies the
//! record's MAC; every install hashes the artifact, signs the record and pins the
//! lockfile; nothing under the root is trusted because it is there.

use super::artifact::{hash_file, verify_artifact};
use super::dirs::{ensure_private_dir, make_executable, InstallPaths, KEYS_DIR, RECORD_FILE};
use super::keyring::{KeyStore, DEFAULT_KEY_ID};
use super::lock;
use super::record::{read_record, sign_record, verify_record, write_record};
use super::{io_at, InstallError};
use crate::persistence::lockfile::{read_lockfile, write_lockfile};
use avada_module_sdk::install::Lockfile;
use avada_module_sdk::rights::{InstallRecord, SignedInstallRecord};
use avada_module_sdk::{Capability, ModuleId};
use semver::Version;
use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// One installed version, verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    /// `owner/repo`.
    pub id: ModuleId,
    /// The version in this directory.
    pub version: Version,
    /// The record, MAC verified at read time.
    pub record: SignedInstallRecord,
    /// The artifact to spawn. Hash it first: [`InstallStore::verify_binary`].
    pub binary: PathBuf,
    /// The directory handed to the module as `AVADA_MODULE_DATA`.
    pub data_dir: PathBuf,
    /// The version directory itself.
    pub version_dir: PathBuf,
    /// Whether the lockfile pins this version.
    pub active: bool,
}

impl Installed {
    /// The verified record's payload.
    pub fn rights(&self) -> &InstallRecord {
        &self.record.record
    }
}

/// What a scan of the store found in one version directory.
#[derive(Debug)]
pub enum RecordStatus {
    /// A verified install (boxed: it carries the whole record).
    Ok(Box<Installed>),
    /// A version directory that cannot be trusted. Reported, never silently skipped,
    /// so a tampered record shows up in the UI instead of vanishing.
    Broken {
        /// The module, when the directory name parses.
        id: Option<ModuleId>,
        /// The version, when the directory name parses.
        version: Option<Version>,
        /// The version directory.
        dir: PathBuf,
        /// Why it is broken.
        reason: String,
    },
}

/// The store: a layout plus the key that signs its records.
pub struct InstallStore {
    paths: InstallPaths,
    keys: Arc<dyn KeyStore>,
    key_id: String,
}

impl fmt::Debug for InstallStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InstallStore")
            .field("root", &self.paths.root())
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

impl InstallStore {
    /// Open the store at `paths`, creating the root owner-only. Records are signed
    /// under [`DEFAULT_KEY_ID`].
    pub fn open(paths: InstallPaths, keys: Arc<dyn KeyStore>) -> Result<Self, InstallError> {
        ensure_private_dir(paths.root())?;
        Ok(InstallStore {
            paths,
            keys,
            key_id: DEFAULT_KEY_ID.to_string(),
        })
    }

    /// The layout.
    pub fn paths(&self) -> &InstallPaths {
        &self.paths
    }

    /// The current lockfile (missing → empty).
    pub fn lockfile(&self) -> Result<Lockfile, InstallError> {
        Ok(read_lockfile(&self.paths.lockfile())?)
    }

    fn write_lock(&self, lock: &Lockfile) -> Result<(), InstallError> {
        Ok(write_lockfile(&self.paths.lockfile(), lock)?)
    }

    /// Install `artifact` as the version `record` describes: copy it into the version
    /// directory, hash it (the record's `artifact_sha256` is filled in, or checked when
    /// already set), sign, write `record.json`, create `data/`, and pin the lockfile
    /// to this version unless a newer one is already active. An existing directory for
    /// the same version is replaced.
    #[tracing::instrument(level = "info", skip_all, fields(id = %record.module_id.as_str(), version = %record.version))]
    pub fn install(
        &self,
        mut record: InstallRecord,
        artifact: &Path,
    ) -> Result<Installed, InstallError> {
        if record.manifest.id() != &record.module_id {
            return Err(InstallError::Inconsistent(format!(
                "record is for {} but its manifest says {}",
                record.module_id.as_str(),
                record.manifest.id().as_str()
            )));
        }
        if record.manifest.module.version != record.version {
            return Err(InstallError::Inconsistent(format!(
                "record is version {} but its manifest says {}",
                record.version, record.manifest.module.version
            )));
        }
        let id = record.module_id.clone();
        let version = record.version.clone();
        let version_dir = self.paths.version_dir(&id, &version);
        if version_dir.exists() {
            std::fs::remove_dir_all(&version_dir).map_err(|e| io_at(&version_dir, e))?;
        }
        ensure_private_dir(self.paths.module_dir(&id).as_path())?;
        ensure_private_dir(&version_dir)?;
        let data_dir = self.paths.data_dir(&id, &version);
        ensure_private_dir(&data_dir)?;
        let binary = self.paths.binary_path(&id, &version, &record.manifest);
        let bin_dir = binary.parent().unwrap_or(&version_dir).to_path_buf();
        ensure_private_dir(&bin_dir)?;
        std::fs::copy(artifact, &binary).map_err(|e| io_at(artifact, e))?;
        make_executable(&binary)?;

        let actual = hash_file(&binary)?;
        if record.artifact_sha256.is_empty() {
            record.artifact_sha256 = actual;
        } else if !record.artifact_sha256.eq_ignore_ascii_case(&actual) {
            let expected = record.artifact_sha256.clone();
            let _ = std::fs::remove_dir_all(&version_dir);
            return Err(InstallError::HashMismatch {
                path: artifact.to_path_buf(),
                expected,
                actual,
            });
        }

        let signed = sign_record(record, self.keys.as_ref(), &self.key_id)?;
        write_record(&self.paths.record_path(&id, &version), &signed)?;

        let mut lockfile = self.lockfile()?;
        let active = match lock::active_version(&lockfile, &id) {
            Some(current) if current > version => false,
            _ => {
                lock::activate(&mut lockfile, &signed.record);
                self.write_lock(&lockfile)?;
                true
            }
        };
        tracing::info!(active, "module installed");
        Ok(Installed {
            id,
            version,
            record: signed,
            binary,
            data_dir,
            version_dir,
            active,
        })
    }

    /// Every version directory under the root, verified or reported broken. Sorted by
    /// id then version. Does not hash binaries; that is per-spawn work
    /// ([`InstallStore::verify_binary`]).
    pub fn records(&self) -> Result<Vec<RecordStatus>, InstallError> {
        let lockfile = self.lockfile()?;
        let mut out = Vec::new();
        let root = self.paths.root();
        for module_entry in read_dir_sorted(root)? {
            let name = module_entry.file_name().to_string_lossy().into_owned();
            if name == KEYS_DIR || !module_entry.path().is_dir() {
                continue;
            }
            let module_dir = module_entry.path();
            let id = module_id_from_dir(&name);
            for version_entry in read_dir_sorted(&module_dir)? {
                let dir = version_entry.path();
                if !dir.is_dir() {
                    continue;
                }
                let version = Version::parse(&version_entry.file_name().to_string_lossy()).ok();
                out.push(match (&id, &version) {
                    (Some(id), Some(version)) => match self.load(id, version, &lockfile) {
                        Ok(installed) => RecordStatus::Ok(Box::new(installed)),
                        Err(e) => RecordStatus::Broken {
                            id: Some(id.clone()),
                            version: Some(version.clone()),
                            dir,
                            reason: e.to_string(),
                        },
                    },
                    _ => RecordStatus::Broken {
                        id: id.clone(),
                        version,
                        dir,
                        reason: "directory name is not <owner>__<repo>/<version>".into(),
                    },
                });
            }
        }
        Ok(out)
    }

    /// The active version of `id`, verified. `Ok(None)` when the lockfile has no entry;
    /// an error when it does but the install is missing or broken.
    pub fn record(&self, id: &ModuleId) -> Result<Option<Installed>, InstallError> {
        let lockfile = self.lockfile()?;
        match lock::active_version(&lockfile, id) {
            None => Ok(None),
            Some(version) => self.load(id, &version, &lockfile).map(Some),
        }
    }

    /// One specific version, verified.
    pub fn record_at(&self, id: &ModuleId, version: &Version) -> Result<Installed, InstallError> {
        let lockfile = self.lockfile()?;
        self.load(id, version, &lockfile)
    }

    /// The versions of `id` present on disk (verified or not), ascending.
    pub fn installed_versions(&self, id: &ModuleId) -> Result<Vec<Version>, InstallError> {
        let module_dir = self.paths.module_dir(id);
        if !module_dir.exists() {
            return Ok(Vec::new());
        }
        let mut versions: Vec<Version> = read_dir_sorted(&module_dir)?
            .into_iter()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| Version::parse(&e.file_name().to_string_lossy()).ok())
            .collect();
        versions.sort();
        Ok(versions)
    }

    /// Pin `version` of `id` as the active one. The version must be installed and
    /// verify.
    #[tracing::instrument(level = "info", skip(self), fields(id = %id.as_str(), version = %version))]
    pub fn activate(&self, id: &ModuleId, version: &Version) -> Result<Installed, InstallError> {
        let mut lockfile = self.lockfile()?;
        let mut installed = self.load(id, version, &lockfile)?;
        lock::activate(&mut lockfile, &installed.record.record);
        self.write_lock(&lockfile)?;
        installed.active = true;
        Ok(installed)
    }

    /// Remove one version's directory (record, binary and data). If it was the active
    /// version the lockfile entry goes too, along with the module's provider and default
    /// rows; other versions stay installed but unpinned until [`InstallStore::activate`].
    #[tracing::instrument(level = "info", skip(self), fields(id = %id.as_str(), version = %version))]
    pub fn uninstall(&self, id: &ModuleId, version: &Version) -> Result<(), InstallError> {
        let version_dir = self.paths.version_dir(id, version);
        if !version_dir.exists() {
            return Err(InstallError::NotInstalled {
                id: id.clone(),
                version: Some(version.clone()),
            });
        }
        std::fs::remove_dir_all(&version_dir).map_err(|e| io_at(&version_dir, e))?;
        let mut lockfile = self.lockfile()?;
        if lock::deactivate_version(&mut lockfile, id, version) {
            self.write_lock(&lockfile)?;
        }
        let module_dir = self.paths.module_dir(id);
        if read_dir_sorted(&module_dir)?.is_empty() {
            std::fs::remove_dir(&module_dir).map_err(|e| io_at(&module_dir, e))?;
        }
        Ok(())
    }

    /// Re-sign one version's record with a new accepted set: the held-update accept
    /// path (track H2 hands over the set the user agreed to). The set must be a subset
    /// of the manifest's request; nothing else in the record changes.
    #[tracing::instrument(level = "info", skip(self, accepted), fields(id = %id.as_str(), version = %version))]
    pub fn re_sign(
        &self,
        id: &ModuleId,
        version: &Version,
        accepted: BTreeSet<Capability>,
    ) -> Result<Installed, InstallError> {
        let lockfile = self.lockfile()?;
        let mut installed = self.load(id, version, &lockfile)?;
        let mut record = installed.record.record.clone();
        record.accepted = accepted;
        let signed = sign_record(record, self.keys.as_ref(), &self.key_id)?;
        write_record(&self.paths.record_path(id, version), &signed)?;
        installed.record = signed;
        Ok(installed)
    }

    /// The module's writable state directory for one version.
    pub fn data_dir(&self, id: &ModuleId, version: &Version) -> PathBuf {
        self.paths.data_dir(id, version)
    }

    /// The artifact of one version, verified. Hash it before spawning:
    /// [`InstallStore::verify_binary`].
    pub fn binary_path(&self, id: &ModuleId, version: &Version) -> Result<PathBuf, InstallError> {
        Ok(self.record_at(id, version)?.binary)
    }

    /// The per-spawn check: verify the record, then re-hash the binary against it.
    /// Returns the verified install so the caller spawns exactly what was checked.
    #[tracing::instrument(level = "debug", skip(self), fields(id = %id.as_str(), version = %version))]
    pub fn verify_binary(
        &self,
        id: &ModuleId,
        version: &Version,
    ) -> Result<Installed, InstallError> {
        let installed = self.record_at(id, version)?;
        verify_artifact(installed.rights(), &installed.binary)?;
        Ok(installed)
    }

    /// Read, verify and cross-check one version directory.
    fn load(
        &self,
        id: &ModuleId,
        version: &Version,
        lockfile: &Lockfile,
    ) -> Result<Installed, InstallError> {
        let version_dir = self.paths.version_dir(id, version);
        let record_path = self.paths.record_path(id, version);
        if !record_path.exists() {
            return Err(if version_dir.exists() {
                InstallError::Broken {
                    dir: version_dir,
                    reason: format!("no {RECORD_FILE}"),
                }
            } else {
                InstallError::NotInstalled {
                    id: id.clone(),
                    version: Some(version.clone()),
                }
            });
        }
        let signed = read_record(&record_path)?;
        let record = verify_record(&signed, self.keys.as_ref(), &self.key_id)?;
        if &record.module_id != id || &record.version != version {
            return Err(InstallError::Broken {
                dir: version_dir,
                reason: format!(
                    "record is for {} {} but sits in the directory of {} {}",
                    record.module_id.as_str(),
                    record.version,
                    id.as_str(),
                    version
                ),
            });
        }
        if record.manifest.id() != id || record.manifest.module.version != *version {
            return Err(InstallError::Broken {
                dir: version_dir,
                reason: "record's manifest names a different module or version".into(),
            });
        }
        let binary = self.paths.binary_path(id, version, &record.manifest);
        if !binary.is_file() {
            return Err(InstallError::Broken {
                dir: version_dir,
                reason: format!("artifact missing: {}", binary.display()),
            });
        }
        let active = lock::is_active(lockfile, id, version);
        Ok(Installed {
            id: id.clone(),
            version: version.clone(),
            record: signed,
            binary,
            data_dir: self.paths.data_dir(id, version),
            version_dir,
            active,
        })
    }
}

/// `owner__repo` → `owner/repo`.
fn module_id_from_dir(name: &str) -> Option<ModuleId> {
    let (owner, repo) = name.split_once("__")?;
    ModuleId::new(&format!("{owner}/{repo}")).ok()
}

fn read_dir_sorted(dir: &Path) -> Result<Vec<std::fs::DirEntry>, InstallError> {
    let mut entries: Vec<std::fs::DirEntry> = std::fs::read_dir(dir)
        .map_err(|e| io_at(dir, e))?
        .collect::<Result<_, _>>()
        .map_err(|e| io_at(dir, e))?;
    entries.sort_by_key(|e| e.file_name());
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::keyring::MemoryKeyStore;
    use crate::install::record::fixtures::{manifest, manifest_at, record};
    use crate::persistence::lockfile::{
        read_workspace_state, workspace_modules_path, write_workspace_state,
    };
    use avada_module_sdk::install::WorkspaceModuleState;
    use avada_module_sdk::rights::RightsError;
    use avada_module_sdk::LOCKFILE_NAME;

    /// A scratch store under the OS temp dir. Every path the store writes is asserted
    /// to start with `root`, so the real app-support dir is never touched.
    struct Scratch {
        root: PathBuf,
        store: InstallStore,
        keys: Arc<MemoryKeyStore>,
    }

    impl Scratch {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "avada-store-{tag}-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            let keys = Arc::new(MemoryKeyStore::new());
            let store = InstallStore::open(
                InstallPaths::under(&root),
                keys.clone() as Arc<dyn KeyStore>,
            )
            .unwrap();
            Scratch { root, store, keys }
        }

        /// A fake artifact with `body` as its bytes, written beside the store root.
        fn artifact(&self, body: &[u8]) -> PathBuf {
            let staging = self.root.join("staging");
            std::fs::create_dir_all(&staging).unwrap();
            let p = staging.join(format!("artifact-{}", uuid::Uuid::new_v4()));
            std::fs::write(&p, body).unwrap();
            p
        }

        fn install(&self, version: Version, accepted: &[Capability], body: &[u8]) -> Installed {
            let rec = record(manifest_at(version), accepted);
            let installed = self.store.install(rec, &self.artifact(body)).unwrap();
            self.assert_inside(&installed.binary);
            self.assert_inside(&installed.data_dir);
            self.assert_inside(&installed.version_dir);
            installed
        }

        fn assert_inside(&self, path: &Path) {
            assert!(
                path.starts_with(&self.root),
                "{} escaped the scratch root {}",
                path.display(),
                self.root.display()
            );
        }

        /// Every file under the root, each asserted inside it.
        fn all_files(&self) -> Vec<PathBuf> {
            fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
                for e in std::fs::read_dir(dir).unwrap() {
                    let p = e.unwrap().path();
                    if p.is_dir() {
                        walk(&p, out);
                    } else {
                        out.push(p);
                    }
                }
            }
            let mut out = Vec::new();
            walk(&self.root, &mut out);
            for p in &out {
                self.assert_inside(p);
            }
            out
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn id() -> ModuleId {
        ModuleId::new("acme/avada-files").unwrap()
    }

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn install_round_trip_verifies_and_pins() {
        let s = Scratch::new("roundtrip");
        let installed = s.install(v("1.2.0"), &[Capability::FsRead], b"binary one");
        assert!(installed.active);
        assert_eq!(installed.version, v("1.2.0"));
        assert_eq!(
            installed.rights().artifact_sha256,
            hash_file(&installed.binary).unwrap()
        );
        assert_eq!(installed.rights().accepted.len(), 1);
        assert!(installed.data_dir.is_dir());
        assert_eq!(installed.data_dir, s.store.data_dir(&id(), &v("1.2.0")));
        assert_eq!(
            s.store.binary_path(&id(), &v("1.2.0")).unwrap(),
            installed.binary
        );

        let again = s.store.record(&id()).unwrap().expect("active record");
        assert_eq!(again, installed);
        assert_eq!(
            s.store.verify_binary(&id(), &v("1.2.0")).unwrap(),
            installed
        );

        let lock = s.store.lockfile().unwrap();
        let m = lock.get(&id()).unwrap();
        assert_eq!(m.version, v("1.2.0"));
        assert_eq!(m.sha256, installed.rights().artifact_sha256);
        assert_eq!(m.tag, "v1.2.0");

        let files = s.all_files();
        assert!(files.iter().any(|p| p.ends_with(LOCKFILE_NAME)));
        assert!(files.iter().any(|p| p.ends_with(RECORD_FILE)));
        match s.store.records().unwrap().as_slice() {
            [RecordStatus::Ok(only)] => assert_eq!(only.as_ref(), &installed),
            other => panic!("expected one ok record, got {other:?}"),
        }
    }

    #[test]
    fn a_flipped_byte_in_the_record_is_reported_broken_not_skipped() {
        let s = Scratch::new("tamper");
        s.install(v("1.2.0"), &[Capability::FsRead], b"binary");
        let path = s.store.paths().record_path(&id(), &v("1.2.0"));
        let text = std::fs::read_to_string(&path).unwrap();
        // Grant a capability the user never accepted: the MAC no longer matches.
        let tampered = text.replace("\"fs.read\"", "\"fs.write\"");
        assert_ne!(tampered, text);
        std::fs::write(&path, tampered).unwrap();

        match s.store.record(&id()).unwrap_err() {
            InstallError::Rights(RightsError::BadSignature) => {}
            other => panic!("expected bad signature, got {other}"),
        }
        assert!(s.store.verify_binary(&id(), &v("1.2.0")).is_err());
        match s.store.records().unwrap().as_slice() {
            [RecordStatus::Broken {
                id: Some(bid),
                version: Some(bv),
                reason,
                ..
            }] => {
                assert_eq!(bid, &id());
                assert_eq!(bv, &v("1.2.0"));
                assert!(reason.contains("signature"), "{reason}");
            }
            other => panic!("expected one broken record, got {other:?}"),
        }
    }

    #[test]
    fn a_swapped_binary_fails_the_spawn_check() {
        let s = Scratch::new("swap");
        let installed = s.install(v("1.2.0"), &[], b"binary");
        std::fs::write(&installed.binary, b"something else").unwrap();
        match s.store.verify_binary(&id(), &v("1.2.0")).unwrap_err() {
            InstallError::HashMismatch {
                path,
                expected,
                actual,
            } => {
                assert_eq!(path, installed.binary);
                assert_eq!(expected, installed.rights().artifact_sha256);
                assert_ne!(expected, actual);
            }
            other => panic!("expected hash mismatch, got {other}"),
        }
        // The record still verifies: the record is fine, the bytes are not.
        assert!(s.store.record(&id()).unwrap().is_some());
    }

    #[test]
    fn a_record_whose_hash_disagrees_with_the_artifact_is_refused() {
        let s = Scratch::new("prehash");
        let mut rec = record(manifest(), &[]);
        rec.artifact_sha256 = "0".repeat(64);
        let err = s.store.install(rec, &s.artifact(b"binary")).unwrap_err();
        assert!(matches!(err, InstallError::HashMismatch { .. }), "{err}");
        assert!(!s.store.paths().version_dir(&id(), &v("1.2.0")).exists());
        assert!(s.store.record(&id()).unwrap().is_none());
    }

    #[test]
    fn an_inconsistent_record_is_refused() {
        let s = Scratch::new("inconsistent");
        let mut rec = record(manifest(), &[]);
        rec.version = v("9.9.9");
        assert!(matches!(
            s.store.install(rec, &s.artifact(b"x")).unwrap_err(),
            InstallError::Inconsistent(_)
        ));
        let mut rec = record(manifest(), &[]);
        rec.module_id = ModuleId::new("acme/other").unwrap();
        assert!(matches!(
            s.store.install(rec, &s.artifact(b"x")).unwrap_err(),
            InstallError::Inconsistent(_)
        ));
    }

    #[test]
    fn side_by_side_versions_with_the_active_pin_switching() {
        let s = Scratch::new("sidebyside");
        let one = s.install(v("1.2.0"), &[], b"one");
        let two = s.install(v("1.3.0"), &[], b"two");
        assert!(one.active);
        assert!(two.active, "a newer install becomes active");
        assert_eq!(
            s.store.installed_versions(&id()).unwrap(),
            vec![v("1.2.0"), v("1.3.0")]
        );
        assert_eq!(s.store.record(&id()).unwrap().unwrap().version, v("1.3.0"));
        assert!(!s.store.record_at(&id(), &v("1.2.0")).unwrap().active);

        // Installing an older version side by side leaves the newer pin alone.
        let old = s.install(v("1.1.0"), &[], b"old");
        assert!(!old.active);
        assert_eq!(s.store.record(&id()).unwrap().unwrap().version, v("1.3.0"));

        let back = s.store.activate(&id(), &v("1.2.0")).unwrap();
        assert!(back.active);
        assert_eq!(s.store.record(&id()).unwrap().unwrap().version, v("1.2.0"));
        assert!(!s.store.record_at(&id(), &v("1.3.0")).unwrap().active);
        assert_eq!(s.store.lockfile().unwrap().modules.len(), 1);
        assert!(matches!(
            s.store.activate(&id(), &v("4.0.0")).unwrap_err(),
            InstallError::NotInstalled { .. }
        ));

        let statuses = s.store.records().unwrap();
        assert_eq!(statuses.len(), 3);
        assert!(statuses.iter().all(|r| matches!(r, RecordStatus::Ok(_))));
    }

    #[test]
    fn uninstalling_the_active_version_drops_the_lock_entry() {
        let s = Scratch::new("uninstall");
        s.install(v("1.2.0"), &[], b"one");
        s.install(v("1.3.0"), &[], b"two");
        let mut lock = s.store.lockfile().unwrap();
        lock.defaults.insert(id(), true);
        write_lockfile(&s.store.paths().lockfile(), &lock).unwrap();

        s.store.uninstall(&id(), &v("1.3.0")).unwrap();
        let lock = s.store.lockfile().unwrap();
        assert!(
            lock.get(&id()).is_none(),
            "active version gone → entry gone"
        );
        assert!(lock.defaults.is_empty(), "its default row goes with it");
        assert!(s.store.record(&id()).unwrap().is_none());
        assert_eq!(s.store.installed_versions(&id()).unwrap(), vec![v("1.2.0")]);
        assert!(
            s.store.record_at(&id(), &v("1.2.0")).is_ok(),
            "other version still there"
        );

        s.store.uninstall(&id(), &v("1.2.0")).unwrap();
        assert!(
            !s.store.paths().module_dir(&id()).exists(),
            "empty module dir pruned"
        );
        assert!(matches!(
            s.store.uninstall(&id(), &v("1.2.0")).unwrap_err(),
            InstallError::NotInstalled { .. }
        ));
        assert!(s.store.records().unwrap().is_empty());
    }

    #[test]
    fn uninstalling_an_inactive_version_keeps_the_pin() {
        let s = Scratch::new("uninstall-inactive");
        s.install(v("1.2.0"), &[], b"one");
        s.install(v("1.3.0"), &[], b"two");
        s.store.uninstall(&id(), &v("1.2.0")).unwrap();
        assert_eq!(s.store.record(&id()).unwrap().unwrap().version, v("1.3.0"));
    }

    #[test]
    fn re_sign_changes_only_the_accepted_set() {
        let s = Scratch::new("resign");
        let before = s.install(v("1.2.0"), &[Capability::FsRead], b"bin");
        let mut accepted = BTreeSet::new();
        accepted.insert(Capability::FsRead);
        accepted.insert(Capability::WorkspaceRead);
        let after = s
            .store
            .re_sign(&id(), &v("1.2.0"), accepted.clone())
            .unwrap();
        assert_eq!(after.rights().accepted, accepted);
        assert_ne!(after.record.mac, before.record.mac);
        assert_eq!(
            after.rights().artifact_sha256,
            before.rights().artifact_sha256
        );
        assert_eq!(after.rights().manifest, before.rights().manifest);
        assert_eq!(s.store.record(&id()).unwrap().unwrap(), after, "persisted");

        let mut too_much = BTreeSet::new();
        too_much.insert(Capability::ProcessSpawn);
        assert!(matches!(
            s.store.re_sign(&id(), &v("1.2.0"), too_much).unwrap_err(),
            InstallError::Rights(RightsError::AcceptedNotRequested(Capability::ProcessSpawn))
        ));
        assert_eq!(
            s.store.record(&id()).unwrap().unwrap(),
            after,
            "refusal left it alone"
        );
    }

    #[test]
    fn a_record_signed_under_another_key_is_broken() {
        let s = Scratch::new("otherkey");
        s.install(v("1.2.0"), &[], b"bin");
        s.keys
            .set(DEFAULT_KEY_ID, crate::install::SecretKey::generate());
        assert!(matches!(
            s.store.record(&id()).unwrap_err(),
            InstallError::Rights(RightsError::BadSignature)
        ));
    }

    #[test]
    fn stray_directories_are_reported_not_trusted() {
        let s = Scratch::new("stray");
        s.install(v("1.2.0"), &[], b"bin");
        std::fs::create_dir_all(s.root.join("not-a-module").join("1.0.0")).unwrap();
        std::fs::create_dir_all(s.store.paths().module_dir(&id()).join("latest")).unwrap();
        std::fs::create_dir_all(s.store.paths().version_dir(&id(), &v("2.0.0"))).unwrap();
        // A record copied into the wrong version directory.
        std::fs::copy(
            s.store.paths().record_path(&id(), &v("1.2.0")),
            s.store.paths().record_path(&id(), &v("2.0.0")),
        )
        .unwrap();
        // The keys dir and the staging dir (a file-only sibling) are not modules.
        std::fs::create_dir_all(s.store.paths().keys_dir()).unwrap();

        let statuses = s.store.records().unwrap();
        let ok = statuses
            .iter()
            .filter(|r| matches!(r, RecordStatus::Ok(_)))
            .count();
        let broken: Vec<&str> = statuses
            .iter()
            .filter_map(|r| match r {
                RecordStatus::Broken { reason, .. } => Some(reason.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ok, 1);
        assert_eq!(broken.len(), 3, "{broken:?}");
        assert!(broken.iter().any(|r| r.contains("directory name")));
        assert!(broken.iter().any(|r| r.contains("sits in the directory")));
    }

    #[test]
    fn reinstalling_the_same_version_replaces_the_directory() {
        let s = Scratch::new("reinstall");
        let first = s.install(v("1.2.0"), &[], b"one");
        std::fs::write(first.data_dir.join("state"), b"old").unwrap();
        let second = s.install(v("1.2.0"), &[Capability::FsRead], b"two");
        assert_ne!(
            first.rights().artifact_sha256,
            second.rights().artifact_sha256
        );
        assert!(
            !second.data_dir.join("state").exists(),
            "replaced, not merged"
        );
        assert_eq!(s.store.record(&id()).unwrap().unwrap(), second);
        assert_eq!(
            s.store.lockfile().unwrap().get(&id()).unwrap().sha256,
            second.rights().artifact_sha256
        );
    }

    #[test]
    fn workspace_pins_and_enabled_round_trip_beside_the_workspace() {
        let s = Scratch::new("workspace");
        s.install(v("1.2.0"), &[], b"one");
        s.install(v("1.3.0"), &[], b"two");
        let workspaces = s.root.join("workspaces");
        std::fs::create_dir_all(&workspaces).unwrap();
        let ws = workspaces.join("dev.avada");
        std::fs::write(&ws, "{}").unwrap();

        let mut state = WorkspaceModuleState::default();
        state.pins.insert(id(), v("1.2.0"));
        state.enabled.insert(id(), false);
        write_workspace_state(&ws, &state).unwrap();
        s.assert_inside(&workspace_modules_path(&ws));
        assert_eq!(
            std::fs::read_to_string(&ws).unwrap(),
            "{}",
            "workspace file untouched"
        );

        let back = read_workspace_state(&ws).unwrap();
        assert_eq!(back, state);
        let pinned = back.pins.get(&id()).unwrap();
        assert!(!s.store.record_at(&id(), pinned).unwrap().active);
        assert!(!back.is_enabled(&id(), &s.store.lockfile().unwrap()));
        s.all_files();
    }

    #[cfg(unix)]
    #[test]
    fn everything_written_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let s = Scratch::new("perms");
        let installed = s.install(v("1.2.0"), &[], b"bin");
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&s.root), 0o700);
        assert_eq!(mode(&s.store.paths().module_dir(&id())), 0o700);
        assert_eq!(mode(&installed.version_dir), 0o700);
        assert_eq!(mode(&installed.data_dir), 0o700);
        assert_eq!(mode(installed.binary.parent().unwrap()), 0o700);
        assert_eq!(mode(&installed.binary), 0o700);
        assert_eq!(
            mode(&s.store.paths().record_path(&id(), &v("1.2.0"))),
            0o600
        );
        assert_eq!(mode(&s.store.paths().lockfile()), 0o600);
    }

    #[test]
    fn open_creates_the_root_and_nothing_else() {
        let s = Scratch::new("open");
        assert!(s.root.is_dir());
        assert!(s.all_files().is_empty());
        assert!(s.store.record(&id()).unwrap().is_none());
        assert!(s.store.records().unwrap().is_empty());
        assert_eq!(
            s.store.installed_versions(&id()).unwrap(),
            Vec::<Version>::new()
        );
        assert!(matches!(
            s.store.record_at(&id(), &v("1.0.0")).unwrap_err(),
            InstallError::NotInstalled { .. }
        ));
        assert!(format!("{:?}", s.store).contains("InstallStore"));
    }
}
