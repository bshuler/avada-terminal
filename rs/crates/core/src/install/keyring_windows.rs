//! Windows Credential Manager [`KeyStore`] (track H7).
//!
//! The intended home for the install-record signing key on Windows is the user's
//! Credential Manager vault: one generic credential per key id, target name
//! `avada/install-key/<key_id>`, the 32 key bytes as the credential blob, persisted
//! `CRED_PERSIST_LOCAL_MACHINE` so it survives a logoff and is readable only by this
//! user account (the vault is per-user and DPAPI-protected).
//!
//! The vault calls (`CredReadW` / `CredWriteW` / `CredFree`) are compiled only on
//! Windows. Everything that does not touch them — the target-name scheme and its
//! validation, the error shape, the trait plumbing — is here and tested on every OS;
//! on a non-Windows build [`CredentialManagerKeyStore::get_or_create`] returns a typed
//! `Unsupported` error so the fallback to [`FileKeyStore`] is a decision the caller
//! makes, not a silent one.
//!
//! A create/create race between two processes is lost the same way the file store
//! loses it: the loser re-reads and adopts the winner's key. A credential whose blob
//! is not exactly `KEY_LEN` bytes is refused as corrupt, never truncated or padded.
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

/// The one error the non-Windows build returns.
#[cfg(not(windows))]
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

/// The vault calls, compiled only where the vault exists.
#[cfg(not(windows))]
mod vault {
    use super::{unsupported, KeyError, SecretKey};

    pub(super) fn read_or_create(target: &str) -> Result<SecretKey, KeyError> {
        Err(unsupported(target))
    }
}

/// The vault calls: one generic credential per target name, the key as its blob.
#[cfg(windows)]
mod vault {
    use super::{error_path, KeyError, SecretKey};
    use crate::install::keyring::{wipe, KEY_LEN};
    use std::io;
    use windows::core::{HRESULT, PCWSTR, PWSTR};
    use windows::Win32::Foundation::ERROR_NOT_FOUND;
    use windows::Win32::Security::Credentials::{
        CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
    };

    /// NUL-terminated UTF-16, the string shape every `*W` call takes.
    pub(super) fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn vault_err(target: &str, e: windows::core::Error) -> KeyError {
        KeyError::Io {
            path: error_path(target),
            source: io::Error::other(e),
        }
    }

    fn corrupt(target: &str, what: &str) -> KeyError {
        KeyError::Io {
            path: error_path(target),
            source: io::Error::new(
                io::ErrorKind::InvalidData,
                format!("credential is not a {KEY_LEN}-byte install key: {what}"),
            ),
        }
    }

    /// A `CREDENTIALW` the vault allocated; `CredFree` on drop, so every early return
    /// below releases it.
    struct Owned(*mut CREDENTIALW);

    impl Drop for Owned {
        #[allow(unsafe_code)]
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: the pointer came from a successful CredReadW and is freed once.
                unsafe { CredFree(self.0.cast::<std::ffi::c_void>()) };
            }
        }
    }

    /// The key stored under `target`, `None` if there is no such credential.
    #[allow(unsafe_code)] // CredReadW and the blob it returns; SAFETY notes inline
    pub(super) fn read(target: &str) -> Result<Option<SecretKey>, KeyError> {
        let name = wide(target);
        let mut raw: *mut CREDENTIALW = std::ptr::null_mut();
        // SAFETY: `name` is NUL-terminated and outlives the call; `raw` is a valid
        // out-pointer the vault fills on success.
        let res = unsafe { CredReadW(PCWSTR(name.as_ptr()), CRED_TYPE_GENERIC, None, &mut raw) };
        if let Err(e) = res {
            if e.code() == HRESULT::from_win32(ERROR_NOT_FOUND.0) {
                return Ok(None);
            }
            return Err(vault_err(target, e));
        }
        let owned = Owned(raw);
        if owned.0.is_null() {
            return Err(corrupt(target, "CredReadW succeeded with no credential"));
        }
        // SAFETY: non-null and vault-allocated; `owned` keeps it alive for this scope.
        let cred = unsafe { &*owned.0 };
        let len = cred.CredentialBlobSize as usize;
        if len != KEY_LEN || cred.CredentialBlob.is_null() {
            return Err(corrupt(target, &format!("blob is {len} bytes")));
        }
        // SAFETY: the vault guarantees `CredentialBlob` points at `CredentialBlobSize`
        // readable bytes, which we just checked equals KEY_LEN.
        let bytes = unsafe { std::slice::from_raw_parts(cred.CredentialBlob, KEY_LEN) }.to_vec();
        Ok(Some(SecretKey::new(bytes)))
    }

    /// Store `key` under `target`, replacing any credential already there.
    #[allow(unsafe_code)] // CredWriteW; SAFETY note inline
    pub(super) fn write(target: &str, key: &SecretKey) -> Result<(), KeyError> {
        let mut name = wide(target);
        let mut blob = key.expose().to_vec();
        let cred = CREDENTIALW {
            Type: CRED_TYPE_GENERIC,
            TargetName: PWSTR(name.as_mut_ptr()),
            CredentialBlobSize: blob.len() as u32,
            CredentialBlob: blob.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            ..Default::default()
        };
        // SAFETY: every pointer in `cred` targets a buffer that outlives the call;
        // CredWriteW copies the blob into the vault and does not retain the pointers.
        let res = unsafe { CredWriteW(&cred, 0) };
        wipe(&mut blob);
        res.map_err(|e| vault_err(target, e))
    }

    pub(super) fn read_or_create(target: &str) -> Result<SecretKey, KeyError> {
        if let Some(key) = read(target)? {
            return Ok(key);
        }
        write(target, &SecretKey::generate())?;
        // Re-read rather than return the generated key: if another process wrote
        // between our read and write, whichever blob the vault holds now is the key
        // everyone must agree on.
        read(target)?.ok_or_else(|| corrupt(target, "vanished between write and read"))
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

    #[cfg(not(windows))]
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
    fn validates_the_id_before_touching_the_vault() {
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

#[cfg(all(test, windows))]
mod vault_tests {
    use super::*;
    use crate::install::keyring::KEY_LEN;
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Security::Credentials::{
        CredDeleteW, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
    };

    /// A throwaway credential that is deleted when the test ends, pass or fail.
    struct Scratch {
        id: String,
        target: String,
    }

    impl Scratch {
        fn new() -> Self {
            let id = format!("test-{}", uuid::Uuid::new_v4());
            let target = target_name(&id).unwrap();
            Scratch { id, target }
        }
    }

    impl Drop for Scratch {
        #[allow(unsafe_code)]
        fn drop(&mut self) {
            let name = vault::wide(&self.target);
            // SAFETY: NUL-terminated name that outlives the call. A missing credential
            // is fine here — the test may have deleted it itself.
            let _ = unsafe { CredDeleteW(PCWSTR(name.as_ptr()), CRED_TYPE_GENERIC, None) };
        }
    }

    #[test]
    fn creates_once_then_reads_the_same_key_back() {
        let scratch = Scratch::new();
        let store = CredentialManagerKeyStore::new();
        assert!(matches!(vault::read(&scratch.target), Ok(None)));
        let first = store.get_or_create(&scratch.id).unwrap();
        let second = store.get_or_create(&scratch.id).unwrap();
        assert_eq!(first.expose().len(), KEY_LEN);
        assert_eq!(first.expose(), second.expose());
        assert!(matches!(vault::read(&scratch.target), Ok(Some(_))));
    }

    #[test]
    #[allow(unsafe_code)] // plants a malformed credential; SAFETY note inline
    fn a_blob_of_the_wrong_length_is_refused_as_corrupt() {
        let scratch = Scratch::new();
        let mut name = vault::wide(&scratch.target);
        let mut blob = vec![7u8; 5];
        let cred = CREDENTIALW {
            Type: CRED_TYPE_GENERIC,
            TargetName: PWSTR(name.as_mut_ptr()),
            CredentialBlobSize: blob.len() as u32,
            CredentialBlob: blob.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            ..Default::default()
        };
        // SAFETY: buffers outlive the call; CredWriteW copies them.
        unsafe { CredWriteW(&cred, 0) }.unwrap();
        match CredentialManagerKeyStore::new().get_or_create(&scratch.id) {
            Err(KeyError::Io { source, .. }) => {
                assert_eq!(source.kind(), io::ErrorKind::InvalidData)
            }
            other => panic!("expected a corrupt-blob error, got {other:?}"),
        }
    }
}
