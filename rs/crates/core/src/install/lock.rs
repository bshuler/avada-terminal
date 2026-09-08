//! The lockfile's view of an install: one `LockedModule` per module id naming the active
//! version. Pure functions over `avada_module_sdk::install::Lockfile`; the file itself is
//! read and written by `persistence::lockfile`.

use avada_module_sdk::install::{LockedModule, Lockfile};
use avada_module_sdk::rights::InstallRecord;
use avada_module_sdk::ModuleId;
use semver::Version;

/// The lockfile entry that pins `record`'s version as the active one.
pub fn locked_from_record(record: &InstallRecord) -> LockedModule {
    LockedModule {
        id: record.module_id.clone(),
        version: record.version.clone(),
        tag: record.tag.clone(),
        commit: record.commit.clone(),
        sha256: record.artifact_sha256.clone(),
        source: record.source,
        installed_at: record.installed_at,
        kind: record.kind,
    }
}

/// Make `record`'s version the active one, replacing whatever was pinned.
pub fn activate(lock: &mut Lockfile, record: &InstallRecord) {
    lock.upsert(locked_from_record(record));
}

/// The active version of a module, if it has a lockfile entry.
pub fn active_version(lock: &Lockfile, id: &ModuleId) -> Option<Version> {
    lock.get(id).map(|m| m.version.clone())
}

/// Whether `version` is the one the lockfile pins for `id`.
pub fn is_active(lock: &Lockfile, id: &ModuleId, version: &Version) -> bool {
    lock.get(id).is_some_and(|m| &m.version == version)
}

/// Drop the lockfile entry for `id` if it pins exactly `version`. Returns whether it
/// did; another version's entry is left alone.
pub fn deactivate_version(lock: &mut Lockfile, id: &ModuleId, version: &Version) -> bool {
    if is_active(lock, id, version) {
        lock.remove(id);
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::record::fixtures::{manifest, manifest_at, record};

    #[test]
    fn locked_entry_mirrors_the_record() {
        let mut rec = record(manifest(), &[]);
        rec.artifact_sha256 = "ab".repeat(32);
        let m = locked_from_record(&rec);
        assert_eq!(m.id, rec.module_id);
        assert_eq!(m.version, Version::new(1, 2, 0));
        assert_eq!(m.tag, "v1.2.0");
        assert_eq!(m.sha256, rec.artifact_sha256);
        assert_eq!(m.installed_at, rec.installed_at);
    }

    #[test]
    fn activate_replaces_and_deactivate_only_matches_its_version() {
        let mut lock = Lockfile::default();
        let v1 = record(manifest(), &[]);
        let v2 = record(manifest_at(Version::new(1, 3, 0)), &[]);
        let id = v1.module_id.clone();
        activate(&mut lock, &v1);
        assert_eq!(active_version(&lock, &id), Some(Version::new(1, 2, 0)));
        activate(&mut lock, &v2);
        assert_eq!(lock.modules.len(), 1, "one entry per id");
        assert!(is_active(&lock, &id, &Version::new(1, 3, 0)));
        assert!(!deactivate_version(&mut lock, &id, &Version::new(1, 2, 0)));
        assert!(lock.get(&id).is_some(), "the other version's pin survives");
        assert!(deactivate_version(&mut lock, &id, &Version::new(1, 3, 0)));
        assert_eq!(active_version(&lock, &id), None);
        lock.validate().unwrap();
    }
}
