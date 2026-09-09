//! `record.json`: signing, verifying, reading and writing the install record. The
//! HMAC itself is `avada_module_sdk::rights::SignedInstallRecord`; this module supplies
//! the key through a [`KeyStore`] and the file through the owner-only atomic writer.

use super::keyring::{KeyStore, SecretKey};
use super::{io_at, InstallError};
use crate::persistence::paths;
use avada_module_sdk::rights::{InstallRecord, SignedInstallRecord};
use std::path::Path;

/// Sign `record` under `key_id` with the key the store holds for it.
pub fn sign_record(
    record: InstallRecord,
    keys: &dyn KeyStore,
    key_id: &str,
) -> Result<SignedInstallRecord, InstallError> {
    let key: SecretKey = keys.get_or_create(key_id)?;
    Ok(SignedInstallRecord::sign(record, key.expose(), key_id)?)
}

/// Verify `signed` with the key its own `key_id` names. A record whose `key_id` is not
/// `expected_key_id` is refused before any key is fetched, so a tampered `key_id` never
/// makes the store mint a stray key.
pub fn verify_record<'a>(
    signed: &'a SignedInstallRecord,
    keys: &dyn KeyStore,
    expected_key_id: &str,
) -> Result<&'a InstallRecord, InstallError> {
    if signed.key_id != expected_key_id {
        return Err(InstallError::Rights(
            avada_module_sdk::rights::RightsError::BadSignature,
        ));
    }
    let key: SecretKey = keys.get_or_create(&signed.key_id)?;
    Ok(signed.verify(key.expose())?)
}

/// Parse a `record.json`. No verification: pair with [`verify_record`].
pub fn read_record(path: &Path) -> Result<SignedInstallRecord, InstallError> {
    let text = std::fs::read_to_string(path).map_err(|e| io_at(path, e))?;
    serde_json::from_str(&text).map_err(|e| InstallError::Record {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })
}

/// Write a `record.json` atomically and owner-only.
pub fn write_record(path: &Path, signed: &SignedInstallRecord) -> Result<(), InstallError> {
    let mut text = serde_json::to_string_pretty(signed).map_err(|e| InstallError::Record {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;
    text.push('\n');
    paths::write_atomic_private(path, text.as_bytes()).map_err(|e| io_at(path, e))
}

#[cfg(test)]
pub(crate) mod fixtures {
    use avada_module_sdk::manifest::DistributionKind;
    use avada_module_sdk::rights::{InstallKind, InstallRecord};
    use avada_module_sdk::{Capability, Manifest};
    use semver::Version;
    use std::collections::BTreeSet;

    /// The SDK's reference manifest: `acme/avada-files` 1.2.0.
    pub fn manifest() -> Manifest {
        Manifest::parse(include_str!(
            "../../../module-sdk/tests/fixtures/avada.toml"
        ))
        .unwrap()
    }

    /// The reference manifest at another version.
    pub fn manifest_at(version: Version) -> Manifest {
        let mut m = manifest();
        m.module.version = version;
        m
    }

    /// An install record for `manifest` accepting `accepted`, with an empty artifact
    /// hash for the store to fill in.
    pub fn record(manifest: Manifest, accepted: &[Capability]) -> InstallRecord {
        let version = manifest.module.version.clone();
        InstallRecord {
            module_id: manifest.id().clone(),
            repo: format!("https://github.com/{}", manifest.id().as_str()),
            tag: manifest.tag(),
            commit: "0123456789abcdef0123456789abcdef01234567".into(),
            version,
            artifact_sha256: String::new(),
            skills_sha256: String::new(),
            source: DistributionKind::Source,
            accepted: accepted.iter().copied().collect::<BTreeSet<_>>(),
            manifest,
            installed_at: 1_700_000_000,
            kind: InstallKind::Manual,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{manifest, record};
    use super::*;
    use crate::install::keyring::{MemoryKeyStore, DEFAULT_KEY_ID};
    use avada_module_sdk::rights::RightsError;
    use avada_module_sdk::Capability;
    use std::path::PathBuf;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "avada-record-{tag}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn sign_then_verify_round_trips_through_disk() {
        let dir = scratch("roundtrip");
        let keys = MemoryKeyStore::new();
        let rec = record(manifest(), &[Capability::FsRead]);
        let signed = sign_record(rec.clone(), &keys, DEFAULT_KEY_ID).unwrap();
        assert_eq!(signed.key_id, DEFAULT_KEY_ID);
        let path = dir.join("record.json");
        write_record(&path, &signed).unwrap();
        let back = read_record(&path).unwrap();
        assert_eq!(back, signed);
        assert_eq!(verify_record(&back, &keys, DEFAULT_KEY_ID).unwrap(), &rec);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_flipped_byte_fails_verification() {
        let dir = scratch("flip");
        let keys = MemoryKeyStore::new();
        let signed = sign_record(record(manifest(), &[]), &keys, DEFAULT_KEY_ID).unwrap();
        let path = dir.join("record.json");
        write_record(&path, &signed).unwrap();
        let mut text = std::fs::read_to_string(&path).unwrap();
        // Flip the timestamp: still valid JSON, different canonical bytes.
        text = text.replace("1700000000", "1700000001");
        std::fs::write(&path, text).unwrap();
        let back = read_record(&path).unwrap();
        assert_eq!(
            verify_record(&back, &keys, DEFAULT_KEY_ID)
                .unwrap_err()
                .to_string(),
            InstallError::Rights(RightsError::BadSignature).to_string()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn another_key_or_key_id_is_refused() {
        let keys = MemoryKeyStore::new();
        let signed = sign_record(record(manifest(), &[]), &keys, DEFAULT_KEY_ID).unwrap();
        let other = MemoryKeyStore::new();
        assert!(matches!(
            verify_record(&signed, &other, DEFAULT_KEY_ID).unwrap_err(),
            InstallError::Rights(RightsError::BadSignature)
        ));
        let mut renamed = signed.clone();
        renamed.key_id = "install-record-v9".into();
        assert!(matches!(
            verify_record(&renamed, &keys, DEFAULT_KEY_ID).unwrap_err(),
            InstallError::Rights(RightsError::BadSignature)
        ));
        assert_eq!(
            format!("{keys:?}"),
            "MemoryKeyStore(1 keys)",
            "no stray key minted"
        );
    }

    #[test]
    fn accepting_an_unrequested_capability_is_refused_at_signing() {
        let keys = MemoryKeyStore::new();
        let err = sign_record(
            record(manifest(), &[Capability::ProcessSpawn]),
            &keys,
            DEFAULT_KEY_ID,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            InstallError::Rights(RightsError::AcceptedNotRequested(Capability::ProcessSpawn))
        ));
    }

    #[test]
    fn garbage_on_disk_is_a_record_error_not_a_panic() {
        let dir = scratch("garbage");
        let path = dir.join("record.json");
        std::fs::write(&path, "{not json").unwrap();
        assert!(matches!(
            read_record(&path).unwrap_err(),
            InstallError::Record { .. }
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
