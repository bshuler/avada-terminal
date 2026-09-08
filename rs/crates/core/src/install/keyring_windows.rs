//! Windows Credential Manager [`KeyStore`] (track H7).
//!
//! The intended home for the install-record signing key on Windows is the user's
//! Credential Manager vault: one generic credential per key id, target name
//! `avada/install-key/<key_id>`, the 32 key bytes as the credential blob, persisted
//! `CRED_PERSIST_LOCAL_MACHINE` so it survives a logoff and is readable only by this
//! user account (the vault is per-user and DPAPI-protected).
//!
//! **Status: stub.** `CredReadW` / `CredWriteW` / `CredFree` live in the
//! `Win32_Security_Credentials` feature of the `windows` crate, which is not in the
//! frozen feature list for this wave. Everything that does not touch that feature —
//! the target-name scheme and its validation, the error shape, the trait plumbing —
//! is here and tested on every OS; [`CredentialManagerKeyStore::get_or_create`] returns
//! a typed `Unsupported` error until the feature lands, at which point only the body of
//! the private `vault` module changes. Callers keep using [`FileKeyStore`] as the
//! fallback in the meantime, exactly as on Unix.
//!
//! Key bytes are never logged, printed, or placed in an error, here or anywhere.
//!
//! [`FileKeyStore`]: super::keyring::FileKeyStore

#![cfg_attr(not(windows), allow(dead_code))]

use super::keyring::{KeyError, KeyStore, SecretKey};
use std::io;
use std::path::PathBuf;

/// Prefix of every Credential Manager target name this store writes.
pub const TARGET_PREFIX: &str = "avada/install-key/";

/// Credential Manager caps target names at this many characters
/// (`CRED_MAX_GENERIC_TARGET_NAME_LENGTH`).
pub const MAX_TARGET_LEN: usize = 32767;

/// The Credential Manager target name for `key_id`, or `BadKeyId` if the id is not
/// something we would accept as a name: same rules as the file store (non-empty, not
/// dot-led, ASCII alphanumerics plus `-`, `_`, `.`), plus the vault's length cap.
pub fn target_name(key_id: &str) -> Result<String, KeyError> {
    let well_formed = !key_id.is_empty()
        && !key_id.starts_with('.')
        && key_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !well_formed {
        return Err(KeyError::BadKeyId(key_id.to_string()));
    }
    let target = format!("{TARGET_PREFIX}{key_id}");
    if target.len() > MAX_TARGET_LEN {
        return Err(KeyError::BadKeyId(key_id.to_string()));
    }
    Ok(target)
}

/// The pseudo-path an error for `target` names — there is no file, so the `path` slot
/// of [`KeyError::Io`] carries a `cred://` URI instead.
pub fn error_path(target: &str) -> PathBuf {
    PathBuf::from(format!("cred://{target}"))
}

/// The one error the stub returns.
fn unsupported(target: &str) -> KeyError {
    KeyError::Io {
        path: error_path(target),
        source: io::Error::new(
            io::ErrorKind::Unsupported,
            "Credential Manager key store needs the `Win32_Security_Credentials` \
             feature of the `windows` crate; use FileKeyStore until it lands",
        ),
    }
}

/// [`KeyStore`] over the Windows Credential Manager. See the module docs for the status.
#[derive(Debug, Default, Clone, Copy)]
pub struct CredentialManagerKeyStore;

impl CredentialManagerKeyStore {
    /// Construct the store. Nothing is opened until [`KeyStore::get_or_create`].
    pub fn new() -> Self {
        CredentialManagerKeyStore
    }
}

impl KeyStore for CredentialManagerKeyStore {
    fn get_or_create(&self, key_id: &str) -> Result<SecretKey, KeyError> {
        let target = target_name(key_id)?;
        vault::read_or_create(&target)
    }
}

/// The vault calls. Today every path is the stub; the real body goes here.
mod vault {
    use super::{unsupported, KeyError, SecretKey};

    /// STUB — needs `Win32_Security_Credentials` (`CredReadW`, `CredWriteW`,
    /// `CredFree`, `CREDENTIALW`, `CRED_TYPE_GENERIC`, `CRED_PERSIST_LOCAL_MACHINE`).
    /// Planned body: `CredReadW(target, CRED_TYPE_GENERIC)` → 32-byte blob → key;
    /// `ERROR_NOT_FOUND` → `SecretKey::generate()`, `CredWriteW` with the bytes as the
    /// blob, re-read to lose a create/create race the same way the file store loses it.
    pub(super) fn read_or_create(target: &str) -> Result<SecretKey, KeyError> {
        Err(unsupported(target))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::keyring::DEFAULT_KEY_ID;

    #[test]
    fn target_name_is_prefixed_and_keeps_the_id() {
        assert_eq!(
            target_name(DEFAULT_KEY_ID).unwrap(),
            "avada/install-key/install-record-v1"
        );
        assert_eq!(target_name("a.b-c_9").unwrap(), "avada/install-key/a.b-c_9");
    }

    #[test]
    fn target_name_rejects_what_the_file_store_rejects() {
        for bad in ["", ".hidden", "..", "a/b", "a\\b", "a b", "ünïcode", "a:b"] {
            assert!(
                matches!(target_name(bad), Err(KeyError::BadKeyId(id)) if id == bad),
                "{bad:?} must be refused"
            );
        }
        let long = "x".repeat(MAX_TARGET_LEN);
        assert!(matches!(target_name(&long), Err(KeyError::BadKeyId(_))));
    }

    #[test]
    fn stub_refuses_with_a_typed_unsupported_error_and_no_key_material() {
        let store = CredentialManagerKeyStore::new();
        match store.get_or_create(DEFAULT_KEY_ID) {
            Err(KeyError::Io { path, source }) => {
                assert_eq!(
                    path,
                    PathBuf::from("cred://avada/install-key/install-record-v1")
                );
                assert_eq!(source.kind(), io::ErrorKind::Unsupported);
                assert!(source.to_string().contains("Win32_Security_Credentials"));
            }
            other => panic!("expected the Unsupported stub error, got {other:?}"),
        }
    }

    #[test]
    fn stub_validates_the_id_before_touching_the_vault() {
        let store = CredentialManagerKeyStore::new();
        assert!(matches!(
            store.get_or_create("../escape"),
            Err(KeyError::BadKeyId(_))
        ));
    }

    #[test]
    fn store_is_object_safe_and_shareable() {
        fn takes(_: &dyn KeyStore) {}
        fn send_sync<T: Send + Sync>(_: &T) {}
        let store = CredentialManagerKeyStore::new();
        takes(&store);
        send_sync(&store);
    }
}
