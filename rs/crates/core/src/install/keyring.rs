//! The key that signs install records.
//!
//! [`KeyStore`] is the seam: the host asks for a key by id and gets back a [`SecretKey`]
//! that zeroes itself on drop, prints nothing under `Debug`, and has no `Display`.
//! Two stores ship today:
//!
//! * [`FileKeyStore`] — the OS-keychain **fallback**. The intended home for the key is the
//!   platform keychain through the `keyring` crate, which cannot be added while the
//!   registry is unreachable (frozen-file request filed with track H5's report). Until
//!   then the key is 32 random bytes in an owner-only file under
//!   `<data_dir>/modules/keys/<key_id>`, created exclusively (`O_EXCL`) so two racing
//!   processes never clobber each other's key. The Windows path is a plain create;
//!   track H7 adds the DACLs.
//! * [`MemoryKeyStore`] — for tests: keys live in a map and never touch disk.
//!
//! Key bytes are never logged, printed, or placed in an error, here or anywhere. The
//! file store writes the key with plain `std::fs` calls rather than
//! `paths::write_atomic_private`, whose `tracing::instrument(ret)` would record its
//! `contents` argument in a debug span.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The key id every install record is signed under today.
pub const DEFAULT_KEY_ID: &str = "install-record-v1";

/// Size of a generated key, in bytes (256 bits for HMAC-SHA-256).
pub const KEY_LEN: usize = 32;

/// Raw HMAC key bytes. Zeroed on drop; `Debug` prints only the length.
pub struct SecretKey(Vec<u8>);

impl SecretKey {
    /// Wrap key bytes.
    pub fn new(bytes: Vec<u8>) -> Self {
        SecretKey(bytes)
    }

    /// Generate [`KEY_LEN`] random bytes from the OS RNG.
    pub fn generate() -> Self {
        use rand::Rng;
        let mut bytes = vec![0u8; KEY_LEN];
        rand::rng().fill_bytes(&mut bytes);
        SecretKey(bytes)
    }

    /// The bytes, for `SignedInstallRecord::sign` / `verify`.
    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

/// Overwrite `bytes` with zeros in a way the optimizer cannot elide, even though the
/// buffer is about to be freed.
fn wipe(bytes: &mut [u8]) {
    for b in bytes.iter_mut() {
        // SAFETY: `b` is a valid, aligned, exclusively borrowed `u8`.
        unsafe { std::ptr::write_volatile(b, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

impl Drop for SecretKey {
    fn drop(&mut self) {
        wipe(&mut self.0);
    }
}

impl fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SecretKey([redacted; {} bytes])", self.0.len())
    }
}

/// Why a key could not be produced. Never carries key bytes.
#[derive(Debug)]
pub enum KeyError {
    /// The store's storage failed.
    Io {
        /// The file involved.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The stored key is present but not a usable key (wrong length, empty).
    Corrupt {
        /// The file involved.
        path: PathBuf,
    },
    /// The key id is not something this store will accept as a file name.
    BadKeyId(String),
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyError::Io { path, source } => write!(f, "{}: {source}", path.display()),
            KeyError::Corrupt { path } => write!(f, "{}: stored key is unusable", path.display()),
            KeyError::BadKeyId(id) => write!(f, "invalid key id {id:?}"),
        }
    }
}

impl std::error::Error for KeyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            KeyError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Where the install-record signing key lives.
pub trait KeyStore: Send + Sync {
    /// The key named `key_id`, generating and storing a fresh one if none exists.
    fn get_or_create(&self, key_id: &str) -> Result<SecretKey, KeyError>;
}

/// Keys held in memory only. For tests, and for any host that wants a per-run key.
#[derive(Default)]
pub struct MemoryKeyStore {
    keys: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl MemoryKeyStore {
    /// An empty store; keys are generated on first request.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the key for `key_id`, so a test can prove a record signed under one key
    /// fails under another.
    pub fn set(&self, key_id: &str, key: SecretKey) {
        self.keys
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key_id.to_string(), key.expose().to_vec());
    }
}

impl Drop for MemoryKeyStore {
    fn drop(&mut self) {
        if let Ok(keys) = self.keys.get_mut() {
            for bytes in keys.values_mut() {
                wipe(bytes);
            }
        }
    }
}

impl fmt::Debug for MemoryKeyStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = self.keys.lock().map(|m| m.len()).unwrap_or(0);
        write!(f, "MemoryKeyStore({n} keys)")
    }
}

impl KeyStore for MemoryKeyStore {
    fn get_or_create(&self, key_id: &str) -> Result<SecretKey, KeyError> {
        let mut keys = self.keys.lock().unwrap_or_else(|e| e.into_inner());
        let bytes = keys
            .entry(key_id.to_string())
            .or_insert_with(|| SecretKey::generate().expose().to_vec());
        Ok(SecretKey::new(bytes.clone()))
    }
}

/// The keychain fallback: one owner-only file per key id under a directory.
#[derive(Debug, Clone)]
pub struct FileKeyStore {
    dir: PathBuf,
}

impl FileKeyStore {
    /// A store whose keys live directly in `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        FileKeyStore { dir: dir.into() }
    }

    /// The directory the keys live in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path_for(&self, key_id: &str) -> Result<PathBuf, KeyError> {
        let ok = !key_id.is_empty()
            && !key_id.starts_with('.')
            && key_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        if !ok {
            return Err(KeyError::BadKeyId(key_id.to_string()));
        }
        Ok(self.dir.join(key_id))
    }

    fn read(path: &Path) -> Result<Option<SecretKey>, KeyError> {
        let mut file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_err(path, e)),
        };
        let mut bytes = Vec::with_capacity(KEY_LEN);
        file.read_to_end(&mut bytes).map_err(|e| io_err(path, e))?;
        let key = SecretKey::new(bytes);
        if key.expose().len() != KEY_LEN {
            return Err(KeyError::Corrupt {
                path: path.to_path_buf(),
            });
        }
        Ok(Some(key))
    }

    /// Create the key file exclusively. `Ok(false)` means somebody else got there first.
    fn create(path: &Path, key: &SecretKey) -> Result<bool, KeyError> {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = match opts.open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
            Err(e) => return Err(io_err(path, e)),
        };
        file.write_all(key.expose())
            .and_then(|_| file.sync_all())
            .map_err(|e| io_err(path, e))?;
        Ok(true)
    }
}

fn io_err(path: &Path, source: std::io::Error) -> KeyError {
    KeyError::Io {
        path: path.to_path_buf(),
        source,
    }
}

impl KeyStore for FileKeyStore {
    fn get_or_create(&self, key_id: &str) -> Result<SecretKey, KeyError> {
        let path = self.path_for(key_id)?;
        if let Some(key) = Self::read(&path)? {
            return Ok(key);
        }
        super::dirs::ensure_private_dir(&self.dir).map_err(|e| match e {
            super::InstallError::Io { path, source } => KeyError::Io { path, source },
            other => KeyError::Io {
                path: self.dir.clone(),
                source: std::io::Error::other(other.to_string()),
            },
        })?;
        let fresh = SecretKey::generate();
        if Self::create(&path, &fresh)? {
            return Ok(fresh);
        }
        // Lost the race: the other writer's key is the key.
        Self::read(&path)?.ok_or(KeyError::Corrupt { path })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "avada-keys-{tag}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ))
    }

    #[test]
    fn secret_key_is_redacted_and_wiped() {
        let key = SecretKey::generate();
        assert_eq!(key.expose().len(), KEY_LEN);
        assert_eq!(format!("{key:?}"), "SecretKey([redacted; 32 bytes])");
        let mut bytes = vec![7u8; 4];
        wipe(&mut bytes);
        assert_eq!(bytes, vec![0u8; 4]);
    }

    #[test]
    fn memory_store_is_stable_per_id_and_distinct_across_ids() {
        let store = MemoryKeyStore::new();
        let a1 = store.get_or_create("a").unwrap();
        let a2 = store.get_or_create("a").unwrap();
        let b = store.get_or_create("b").unwrap();
        assert_eq!(a1.expose(), a2.expose());
        assert_ne!(a1.expose(), b.expose());
        assert_eq!(format!("{store:?}"), "MemoryKeyStore(2 keys)");
        store.set("a", SecretKey::generate());
        assert_ne!(store.get_or_create("a").unwrap().expose(), a1.expose());
    }

    #[test]
    fn file_store_creates_once_and_reads_back() {
        let dir = scratch("file");
        let store = FileKeyStore::new(&dir);
        let k1 = store.get_or_create(DEFAULT_KEY_ID).unwrap();
        let k2 = store.get_or_create(DEFAULT_KEY_ID).unwrap();
        assert_eq!(k1.expose(), k2.expose());
        assert_eq!(k1.expose().len(), KEY_LEN);
        let path = dir.join(DEFAULT_KEY_ID);
        assert!(path.starts_with(&dir));
        assert!(path.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_store_refuses_a_corrupt_key_and_bad_ids() {
        let dir = scratch("corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("short"), b"abc").unwrap();
        let store = FileKeyStore::new(&dir);
        assert!(matches!(
            store.get_or_create("short").unwrap_err(),
            KeyError::Corrupt { .. }
        ));
        assert!(matches!(
            store.get_or_create("../escape").unwrap_err(),
            KeyError::BadKeyId(_)
        ));
        assert!(matches!(
            store.get_or_create("").unwrap_err(),
            KeyError::BadKeyId(_)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn errors_and_debug_never_mention_key_bytes() {
        let dir = scratch("errors");
        let store = FileKeyStore::new(&dir);
        let key = store.get_or_create("k").unwrap();
        let hex: String = key.expose().iter().map(|b| format!("{b:02x}")).collect();
        let e = KeyError::Corrupt {
            path: dir.join("k"),
        };
        assert!(!e.to_string().contains(&hex));
        assert!(!format!("{key:?}").contains(&hex));
        assert!(!format!("{store:?}").contains(&hex));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
