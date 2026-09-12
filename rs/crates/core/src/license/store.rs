//! Where licenses live: one directory per product under `<modules root>/licenses/`, plus
//! the issuers' cached key sets.
//!
//! ```text
//! <modules root>/licenses/                 0700
//!   keys/<issuer-slug>.json                the issuer's JWKS   (0600)
//!   <owner>__<repo>/meta.json              { "product", "issuer" } — the presence marker
//!   <owner>__<repo>/checkin.json           CheckinRecord
//!   <owner>__<repo>/license.jwt            the token — only as a fallback (see below)
//! ```
//!
//! The bearer **token** is a credential, so its home is the OS keychain (the [`TokenVault`]
//! seam), not a file: `meta.json` is what marks a product as licensed. The `license.jwt`
//! file is written only where no keychain backend is reachable, and a legacy one left by an
//! older build is migrated into the keychain and deleted on first read.
//!
//! Every file is written owner-only through a private temp file and a rename; neither the
//! token file nor the keychain call goes through anything that logs its arguments.

use super::{Jwks, LicenseToken};
use crate::install::InstallPaths;
use avada_module_sdk::license::CheckinRecord;
use avada_module_sdk::ModuleId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Directory under the modules root.
pub const LICENSES_DIR: &str = "licenses";
/// Key cache directory under [`LICENSES_DIR`].
pub const KEYS_DIR: &str = "keys";
/// The token file.
pub const TOKEN_FILE: &str = "license.jwt";
/// The product/issuer file.
pub const META_FILE: &str = "meta.json";
/// The check-in record.
pub const CHECKIN_FILE: &str = "checkin.json";

/// One installed license. `Debug` shows the product and issuer; the token is redacted by
/// its own `Debug`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredLicense {
    /// The product (`owner/repo`).
    pub product: String,
    /// The issuer URL the token was verified against.
    pub issuer: String,
    /// The token.
    pub token: LicenseToken,
    /// The check-in record.
    pub checkin: CheckinRecord,
}

/// Store problems. Paths and products, never contents.
#[derive(Debug)]
pub enum StoreError {
    /// The filesystem said no.
    Io {
        /// The path involved.
        path: PathBuf,
        /// The OS error.
        source: std::io::Error,
    },
    /// Not an `owner/repo` product id.
    BadProduct(String),
    /// A file is present but unreadable.
    Corrupt {
        /// The file.
        path: PathBuf,
        /// Why.
        reason: String,
    },
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Io { path, source } => write!(f, "{}: {source}", path.display()),
            StoreError::BadProduct(p) => write!(f, "{p:?} is not an owner/repo product id"),
            StoreError::Corrupt { path, reason } => {
                write!(f, "{}: unreadable: {reason}", path.display())
            }
        }
    }
}

impl std::error::Error for StoreError {}

fn io_at(path: &Path, source: std::io::Error) -> StoreError {
    StoreError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Where licenses and cached keys are kept.
pub trait LicenseStore: Send + Sync {
    /// The license for a product, if one is installed.
    fn load(&self, product: &str) -> Result<Option<StoredLicense>, StoreError>;
    /// Store (replace) a license.
    fn save(&self, license: &StoredLicense) -> Result<(), StoreError>;
    /// Replace the check-in record of an installed license.
    fn save_checkin(&self, product: &str, record: &CheckinRecord) -> Result<(), StoreError>;
    /// Forget a product's license. `Ok(false)` when there was none.
    fn remove(&self, product: &str) -> Result<bool, StoreError>;
    /// Every product with a license, sorted.
    fn products(&self) -> Result<Vec<String>, StoreError>;
    /// The cached key set for an issuer.
    fn load_keys(&self, issuer: &str) -> Result<Option<Jwks>, StoreError>;
    /// Cache an issuer's key set.
    fn save_keys(&self, issuer: &str, keys: &Jwks) -> Result<(), StoreError>;
}

/// In-memory store for tests.
#[derive(Default)]
pub struct MemoryLicenseStore {
    licenses: Mutex<BTreeMap<String, StoredLicense>>,
    keys: Mutex<BTreeMap<String, Jwks>>,
}

impl MemoryLicenseStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

impl LicenseStore for MemoryLicenseStore {
    fn load(&self, product: &str) -> Result<Option<StoredLicense>, StoreError> {
        Ok(self.licenses.lock().unwrap().get(product).cloned())
    }
    fn save(&self, license: &StoredLicense) -> Result<(), StoreError> {
        product_dir_name(&license.product)?;
        self.licenses
            .lock()
            .unwrap()
            .insert(license.product.clone(), license.clone());
        Ok(())
    }
    fn save_checkin(&self, product: &str, record: &CheckinRecord) -> Result<(), StoreError> {
        if let Some(l) = self.licenses.lock().unwrap().get_mut(product) {
            l.checkin = record.clone();
        }
        Ok(())
    }
    fn remove(&self, product: &str) -> Result<bool, StoreError> {
        Ok(self.licenses.lock().unwrap().remove(product).is_some())
    }
    fn products(&self) -> Result<Vec<String>, StoreError> {
        Ok(self.licenses.lock().unwrap().keys().cloned().collect())
    }
    fn load_keys(&self, issuer: &str) -> Result<Option<Jwks>, StoreError> {
        Ok(self.keys.lock().unwrap().get(issuer_key(issuer)).cloned())
    }
    fn save_keys(&self, issuer: &str, keys: &Jwks) -> Result<(), StoreError> {
        self.keys
            .lock()
            .unwrap()
            .insert(issuer_key(issuer).to_string(), keys.clone());
        Ok(())
    }
}

fn issuer_key(issuer: &str) -> &str {
    issuer.trim_end_matches('/')
}

/// `owner/repo` → `owner__repo`, through `ModuleId` so the same rules apply as to a
/// module directory.
fn product_dir_name(product: &str) -> Result<String, StoreError> {
    ModuleId::new(product)
        .map(|id| id.dir_name())
        .map_err(|_| StoreError::BadProduct(product.to_string()))
}

/// A file name for an issuer URL: its host (sanitised) plus a hash prefix of the whole
/// URL, so `https://a.example` and `https://a.example/tenant` do not collide.
pub fn issuer_slug(issuer: &str) -> String {
    let issuer = issuer_key(issuer);
    let host = issuer
        .split("://")
        .nth(1)
        .unwrap_or(issuer)
        .split('/')
        .next()
        .unwrap_or("")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .take(40)
        .collect::<String>();
    let hash = Sha256::digest(issuer.as_bytes());
    let hex: String = hash.iter().take(6).map(|b| format!("{b:02x}")).collect();
    if host.is_empty() {
        hex
    } else {
        format!("{host}-{hex}")
    }
}

/// Write `bytes` to `path` owner-only and atomically: a fresh `0600` temp file beside
/// it, then a rename. Nothing here logs its arguments.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let dir = path.parent().ok_or_else(|| {
        io_at(
            path,
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "no parent dir"),
        )
    })?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("license"),
        uuid::Uuid::new_v4().simple()
    ));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let result = (|| {
        let mut f = opts.open(&tmp).map_err(|e| io_at(&tmp, e))?;
        f.write_all(bytes).map_err(|e| io_at(&tmp, e))?;
        f.sync_all().map_err(|e| io_at(&tmp, e))?;
        drop(f);
        #[cfg(windows)]
        crate::persistence::acl_windows::restrict_to_owner(&tmp).map_err(|e| io_at(&tmp, e))?;
        std::fs::rename(&tmp, path).map_err(|e| io_at(path, e))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn ensure_private_dir(dir: &Path) -> Result<(), StoreError> {
    crate::install::dirs::ensure_private_dir(dir).map_err(|e| match e {
        crate::install::InstallError::Io { path, source } => StoreError::Io { path, source },
        other => io_at(dir, std::io::Error::other(other.to_string())),
    })
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, StoreError> {
    match std::fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_at(path, e)),
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, StoreError> {
    match read_optional(path)? {
        None => Ok(None),
        Some(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| StoreError::Corrupt {
                path: path.to_path_buf(),
                reason: e.to_string(),
            }),
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), StoreError> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| StoreError::Corrupt {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;
    write_private(path, &bytes)
}

#[derive(Serialize, Deserialize)]
struct Meta {
    product: String,
    issuer: String,
}

/// A place to keep a product's bearer token *off the filesystem* — the OS keychain in
/// production. It is deliberately small: three verbs over an opaque string.
///
/// The `get`/`set`/`delete` return type separates two facts the store must act on
/// differently:
///
/// * `Ok(None)` from `get` — the backend answered and holds no entry for this product.
/// * `Err(_)` — the backend itself is unavailable (a headless Linux box with no Secret
///   Service, a locked login keychain, or a platform with no backend compiled in). This
///   is the signal to fall back to the owner-only `license.jwt` file, which is exactly
///   the pre-keychain behaviour, so nothing regresses where there is no keychain.
///
/// The token never appears in an error or a log line: [`VaultError`] carries only the
/// backend's own message, and the backend never echoes the secret.
pub(crate) trait TokenVault: Send + Sync {
    /// The token stored for this product, if the backend holds one.
    fn get(&self, product: &str) -> Result<Option<String>, VaultError>;
    /// Store (replace) this product's token.
    fn set(&self, product: &str, token: &str) -> Result<(), VaultError>;
    /// Forget this product's token. Absent is success.
    fn delete(&self, product: &str) -> Result<(), VaultError>;
}

/// The keychain backend was unreachable. Message only — never the secret.
#[derive(Debug)]
pub(crate) struct VaultError(String);

impl fmt::Display for VaultError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The keychain service name every license entry is filed under. The per-product key is
/// the `owner/repo` id, so one entry maps to one installed license.
const KEYRING_SERVICE: &str = "avada-terminal-license";

/// The production [`TokenVault`]: the platform keychain via the `keyring` crate.
struct KeyringVault;

impl KeyringVault {
    fn entry(product: &str) -> Result<::keyring::Entry, VaultError> {
        ::keyring::Entry::new(KEYRING_SERVICE, product).map_err(|e| VaultError(e.to_string()))
    }
}

impl TokenVault for KeyringVault {
    fn get(&self, product: &str) -> Result<Option<String>, VaultError> {
        match Self::entry(product)?.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(::keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(VaultError(e.to_string())),
        }
    }

    fn set(&self, product: &str, token: &str) -> Result<(), VaultError> {
        Self::entry(product)?
            .set_password(token)
            .map_err(|e| VaultError(e.to_string()))
    }

    fn delete(&self, product: &str) -> Result<(), VaultError> {
        match Self::entry(product)?.delete_credential() {
            Ok(()) | Err(::keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(VaultError(e.to_string())),
        }
    }
}

/// The file-backed store. Bearer tokens live in the OS keychain (`vault`); the directory
/// keeps only the non-secret record — `meta.json` and `checkin.json` — plus, as a
/// fallback where there is no keychain, the owner-only `license.jwt`.
pub struct FileLicenseStore {
    dir: PathBuf,
    vault: Box<dyn TokenVault>,
}

impl FileLicenseStore {
    /// The host's store: `<modules root>/licenses`.
    pub fn host() -> Self {
        Self::under(InstallPaths::host().root())
    }

    /// A store under this modules root.
    pub fn under(root: impl AsRef<Path>) -> Self {
        FileLicenseStore {
            dir: root.as_ref().join(LICENSES_DIR),
            vault: Box::new(KeyringVault),
        }
    }

    /// A store under this modules root with an explicit token vault — tests inject a
    /// fake so they never touch (or depend on) the real OS keychain.
    #[cfg(test)]
    pub(crate) fn with_vault(root: impl AsRef<Path>, vault: Box<dyn TokenVault>) -> Self {
        FileLicenseStore {
            dir: root.as_ref().join(LICENSES_DIR),
            vault,
        }
    }

    /// The `licenses` directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// A product's directory.
    pub fn product_dir(&self, product: &str) -> Result<PathBuf, StoreError> {
        Ok(self.dir.join(product_dir_name(product)?))
    }

    /// Where an issuer's keys are cached.
    pub fn keys_path(&self, issuer: &str) -> PathBuf {
        self.dir
            .join(KEYS_DIR)
            .join(format!("{}.json", issuer_slug(issuer)))
    }

    /// The token for a product: the keychain first, then the on-disk fallback. When a
    /// legacy token is found on disk and the keychain is reachable, it is migrated into
    /// the keychain and the plaintext file removed — so simply upgrading and running the
    /// app moves secrets off disk on first read. `None` means no token anywhere.
    fn read_token(&self, product: &str, dir: &Path) -> Result<Option<LicenseToken>, StoreError> {
        match self.vault.get(product) {
            Ok(Some(secret)) => {
                let token = LicenseToken::new(secret);
                return Ok((!token.expose_secret().is_empty()).then_some(token));
            }
            Ok(None) => {} // reachable, no entry — a legacy or fallback file may hold it
            Err(e) => {
                tracing::debug!(error = %e, "license keychain unavailable; reading the token file");
            }
        }
        let Some(bytes) = read_optional(&dir.join(TOKEN_FILE))? else {
            return Ok(None);
        };
        let token = LicenseToken::new(String::from_utf8_lossy(&bytes));
        if token.expose_secret().is_empty() {
            return Ok(None);
        }
        // Opportunistic migration: put the secret in the keychain, then drop the file.
        if self.vault.set(product, token.expose_secret()).is_ok() {
            let _ = std::fs::remove_file(dir.join(TOKEN_FILE));
            tracing::info!(
                product,
                "migrated license token from disk into the OS keychain"
            );
        }
        Ok(Some(token))
    }
}

impl LicenseStore for FileLicenseStore {
    fn load(&self, product: &str) -> Result<Option<StoredLicense>, StoreError> {
        let dir = self.product_dir(product)?;
        // `meta.json` is the presence marker now that the token may live off-disk.
        let Some(meta) = read_json::<Meta>(&dir.join(META_FILE))? else {
            return Ok(None);
        };
        let Some(token) = self.read_token(product, &dir)? else {
            return Ok(None);
        };
        let checkin: CheckinRecord = read_json(&dir.join(CHECKIN_FILE))?.unwrap_or(CheckinRecord {
            last_ok: 0,
            revoked: false,
        });
        Ok(Some(StoredLicense {
            product: meta.product,
            issuer: meta.issuer,
            token,
            checkin,
        }))
    }

    fn save(&self, license: &StoredLicense) -> Result<(), StoreError> {
        let dir = self.product_dir(&license.product)?;
        ensure_private_dir(&self.dir)?;
        ensure_private_dir(&dir)?;
        write_json(
            &dir.join(META_FILE),
            &Meta {
                product: license.product.clone(),
                issuer: license.issuer.clone(),
            },
        )?;
        write_json(&dir.join(CHECKIN_FILE), &license.checkin)?;
        match self
            .vault
            .set(&license.product, license.token.expose_secret())
        {
            Ok(()) => {
                // The secret is in the keychain now; never leave a plaintext copy behind.
                let _ = std::fs::remove_file(dir.join(TOKEN_FILE));
                Ok(())
            }
            Err(e) => {
                tracing::debug!(error = %e, "license keychain unavailable; writing the owner-only token file");
                write_private(
                    &dir.join(TOKEN_FILE),
                    license.token.expose_secret().as_bytes(),
                )
            }
        }
    }

    fn save_checkin(&self, product: &str, record: &CheckinRecord) -> Result<(), StoreError> {
        let dir = self.product_dir(product)?;
        if !dir.join(META_FILE).exists() {
            return Ok(());
        }
        write_json(&dir.join(CHECKIN_FILE), record)
    }

    fn remove(&self, product: &str) -> Result<bool, StoreError> {
        // Best-effort: a `NoEntry` backend already reads as success, and a keychain that
        // is merely unreachable must not block forgetting the on-disk record.
        let _ = self.vault.delete(product);
        let dir = self.product_dir(product)?;
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(io_at(&dir, e)),
        }
    }

    fn products(&self) -> Result<Vec<String>, StoreError> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io_at(&self.dir, e)),
        };
        let mut out = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| io_at(&self.dir, e))?;
            let dir = entry.path();
            if !dir.is_dir() || !dir.join(META_FILE).exists() {
                continue;
            }
            if let Some(meta) = read_json::<Meta>(&dir.join(META_FILE))? {
                out.push(meta.product);
            }
        }
        out.sort();
        Ok(out)
    }

    fn load_keys(&self, issuer: &str) -> Result<Option<Jwks>, StoreError> {
        read_json(&self.keys_path(issuer))
    }

    fn save_keys(&self, issuer: &str, keys: &Jwks) -> Result<(), StoreError> {
        ensure_private_dir(&self.dir)?;
        ensure_private_dir(&self.dir.join(KEYS_DIR))?;
        write_json(&self.keys_path(issuer), keys)
    }
}

/// Test doubles for [`TokenVault`], shared with the license integration tests so neither
/// suite ever touches (or depends on) the real OS keychain.
#[cfg(test)]
#[allow(dead_code)] // each double is used by one suite or the other, not both
pub(crate) mod test_vaults {
    use super::{TokenVault, VaultError};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// A keychain that works, backed by an in-process map. `Clone` shares one map, so a
    /// value set through one handle is visible through another — the way the real
    /// keychain outlives a single `FileLicenseStore` and survives a "restart".
    #[derive(Clone, Default)]
    pub(crate) struct MemVault(Arc<Mutex<HashMap<String, String>>>);

    impl MemVault {
        pub(crate) fn new() -> Self {
            Self::default()
        }
        /// A fresh boxed handle onto the same map.
        pub(crate) fn boxed(&self) -> Box<dyn TokenVault> {
            Box::new(self.clone())
        }
        /// How many tokens the keychain holds — for asserting migration and removal.
        pub(crate) fn count(&self) -> usize {
            self.0.lock().unwrap().len()
        }
    }

    impl TokenVault for MemVault {
        fn get(&self, product: &str) -> Result<Option<String>, VaultError> {
            Ok(self.0.lock().unwrap().get(product).cloned())
        }
        fn set(&self, product: &str, token: &str) -> Result<(), VaultError> {
            self.0
                .lock()
                .unwrap()
                .insert(product.to_string(), token.to_string());
            Ok(())
        }
        fn delete(&self, product: &str) -> Result<(), VaultError> {
            self.0.lock().unwrap().remove(product);
            Ok(())
        }
    }

    /// A keychain that is not there — every call fails, exactly as on a headless box with
    /// no Secret Service. Drives the store onto its owner-only-file fallback.
    pub(crate) struct UnavailableVault;

    impl TokenVault for UnavailableVault {
        fn get(&self, _: &str) -> Result<Option<String>, VaultError> {
            Err(VaultError("no keychain backend".into()))
        }
        fn set(&self, _: &str, _: &str) -> Result<(), VaultError> {
            Err(VaultError("no keychain backend".into()))
        }
        fn delete(&self, _: &str) -> Result<(), VaultError> {
            Err(VaultError("no keychain backend".into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_vaults::{MemVault, UnavailableVault};
    use super::*;
    use crate::license::verify::SigningKey;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "avada-license-store-{name}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample(product: &str) -> StoredLicense {
        StoredLicense {
            product: product.into(),
            issuer: "https://issuer.test".into(),
            token: LicenseToken::new("eyJ.test-token-value.sig"),
            checkin: CheckinRecord {
                last_ok: 42,
                revoked: false,
            },
        }
    }

    #[test]
    fn token_debug_is_redacted_and_the_stored_license_inherits_it() {
        let s = sample("acme/widget");
        let shown = format!("{s:?}");
        assert!(!shown.contains("test-token"), "{shown}");
        assert!(shown.contains("redacted"));
        assert!(shown.contains("acme/widget"));
        assert_eq!(s.token.expose_secret(), "eyJ.test-token-value.sig");
        assert_eq!(LicenseToken::new("  x \n").expose_secret(), "x");
    }

    #[test]
    fn memory_store_round_trips_lists_and_rejects_bad_products() {
        let s = MemoryLicenseStore::new();
        assert!(s.load("acme/widget").unwrap().is_none());
        s.save(&sample("acme/widget")).unwrap();
        s.save(&sample("acme/alpha")).unwrap();
        assert_eq!(s.products().unwrap(), ["acme/alpha", "acme/widget"]);
        s.save_checkin(
            "acme/widget",
            &CheckinRecord {
                last_ok: 99,
                revoked: true,
            },
        )
        .unwrap();
        let l = s.load("acme/widget").unwrap().unwrap();
        assert_eq!(l.checkin.last_ok, 99);
        assert!(l.checkin.revoked);
        assert!(matches!(
            s.save(&sample("no-slash")).unwrap_err(),
            StoreError::BadProduct(_)
        ));
        assert!(s.remove("acme/widget").unwrap());
        assert!(!s.remove("acme/widget").unwrap());
        assert!(s.load_keys("https://issuer.test/").unwrap().is_none());
        let keys = Jwks {
            keys: vec![SigningKey::generate_with_kid("k1").jwk()],
        };
        s.save_keys("https://issuer.test", &keys).unwrap();
        assert_eq!(s.load_keys("https://issuer.test/").unwrap().unwrap(), keys);
    }

    #[test]
    fn file_store_keeps_the_token_in_the_keychain_and_off_disk() {
        let root = scratch("keychain");
        let vault = MemVault::new();
        let s = FileLicenseStore::with_vault(&root, vault.boxed());
        assert!(s.load("acme/widget").unwrap().is_none());
        assert!(s.products().unwrap().is_empty());
        let l = sample("acme/widget");
        s.save(&l).unwrap();
        let dir = s.product_dir("acme/widget").unwrap();
        assert_eq!(dir, root.join("licenses").join("acme__widget"));
        // The non-secret record is on disk; the token is in the keychain, not on disk.
        assert!(dir.join(META_FILE).exists());
        assert!(dir.join(CHECKIN_FILE).exists());
        assert!(!dir.join(TOKEN_FILE).exists(), "token must not be on disk");
        assert_eq!(vault.count(), 1, "the token is in the keychain");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&dir.join(META_FILE)), 0o600);
            assert_eq!(mode(&dir.join(CHECKIN_FILE)), 0o600);
            assert_eq!(mode(&dir), 0o700);
            assert_eq!(mode(s.dir()), 0o700);
        }
        // The full license, token and all, round-trips out of the keychain.
        let back = s.load("acme/widget").unwrap().unwrap();
        assert_eq!(back, l);
        assert_eq!(s.products().unwrap(), ["acme/widget"]);
        s.save_checkin(
            "acme/widget",
            &CheckinRecord {
                last_ok: 7,
                revoked: true,
            },
        )
        .unwrap();
        assert!(s.load("acme/widget").unwrap().unwrap().checkin.revoked);
        // A check-in for a product with no license is a no-op, not a stray file.
        s.save_checkin(
            "acme/none",
            &CheckinRecord {
                last_ok: 1,
                revoked: false,
            },
        )
        .unwrap();
        assert!(!s.product_dir("acme/none").unwrap().exists());
        // Remove clears the keychain entry as well as the on-disk record.
        assert!(s.remove("acme/widget").unwrap());
        assert!(!s.remove("acme/widget").unwrap());
        assert!(s.load("acme/widget").unwrap().is_none());
        assert_eq!(vault.count(), 0, "remove forgets the keychain entry");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn file_store_falls_back_to_owner_only_files_with_no_keychain() {
        // No keychain backend (a headless box, a locked keychain): the token lands in the
        // owner-only file, exactly the pre-keychain behaviour, and everything still works.
        let root = scratch("fallback");
        let s = FileLicenseStore::with_vault(&root, Box::new(UnavailableVault));
        let l = sample("acme/widget");
        s.save(&l).unwrap();
        let dir = s.product_dir("acme/widget").unwrap();
        for f in [TOKEN_FILE, META_FILE, CHECKIN_FILE] {
            assert!(dir.join(f).exists(), "{f}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&dir.join(TOKEN_FILE)), 0o600);
            assert_eq!(mode(&dir.join(META_FILE)), 0o600);
            assert_eq!(mode(&dir), 0o700);
            assert_eq!(mode(s.dir()), 0o700);
        }
        // No temp file left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
        let back = s.load("acme/widget").unwrap().unwrap();
        assert_eq!(back, l);
        assert_eq!(s.products().unwrap(), ["acme/widget"]);
        assert!(s.remove("acme/widget").unwrap());
        assert!(s.load("acme/widget").unwrap().is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_legacy_on_disk_token_migrates_into_the_keychain_on_first_read() {
        // A store upgraded from the file-only era: the token is on disk. The first read
        // with a reachable keychain moves it in and deletes the plaintext file.
        let root = scratch("migrate");
        let l = sample("acme/widget");
        // Write the legacy on-disk layout via the no-keychain path.
        let legacy = FileLicenseStore::with_vault(&root, Box::new(UnavailableVault));
        legacy.save(&l).unwrap();
        let dir = legacy.product_dir("acme/widget").unwrap();
        assert!(dir.join(TOKEN_FILE).exists());

        let vault = MemVault::new();
        let s = FileLicenseStore::with_vault(&root, vault.boxed());
        let back = s.load("acme/widget").unwrap().unwrap();
        assert_eq!(back, l);
        assert_eq!(vault.count(), 1, "token migrated into the keychain");
        assert!(
            !dir.join(TOKEN_FILE).exists(),
            "plaintext token file removed after migration"
        );
        // A second read now comes straight from the keychain.
        assert_eq!(s.load("acme/widget").unwrap().unwrap(), l);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_token_survives_a_new_store_instance_via_the_shared_keychain() {
        // The keychain outlives any one store: save through one handle, and a second
        // store built later (a "restart") reads the token back with nothing on disk.
        let root = scratch("restart-keychain");
        let vault = MemVault::new();
        let l = sample("acme/widget");
        {
            let s1 = FileLicenseStore::with_vault(&root, vault.boxed());
            s1.save(&l).unwrap();
        }
        let dir = root.join("licenses").join("acme__widget");
        assert!(!dir.join(TOKEN_FILE).exists());
        let s2 = FileLicenseStore::with_vault(&root, vault.boxed());
        assert_eq!(s2.load("acme/widget").unwrap().unwrap(), l);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn file_store_caches_keys_per_issuer_owner_only() {
        let root = scratch("keys");
        let s = FileLicenseStore::with_vault(&root, Box::new(UnavailableVault));
        assert!(s.load_keys("https://issuer.test").unwrap().is_none());
        let keys = Jwks {
            keys: vec![SigningKey::generate_with_kid("k1").jwk()],
        };
        s.save_keys("https://issuer.test/", &keys).unwrap();
        assert_eq!(s.load_keys("https://issuer.test").unwrap().unwrap(), keys);
        let path = s.keys_path("https://issuer.test");
        assert!(path.starts_with(root.join("licenses").join("keys")));
        assert!(path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("issuer-test-"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        assert_ne!(
            issuer_slug("https://issuer.test"),
            issuer_slug("https://issuer.test/tenant")
        );
        assert_eq!(
            issuer_slug("https://issuer.test"),
            issuer_slug("https://issuer.test/")
        );
        assert!(issuer_slug("http://127.0.0.1:4455").starts_with("127-0-0-1-4455-"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_corrupt_meta_file_is_reported_not_swallowed() {
        let root = scratch("corrupt");
        // The no-keychain path so the token lives on disk — the empty-token-file case
        // below is exactly about reading that file.
        let s = FileLicenseStore::with_vault(&root, Box::new(UnavailableVault));
        s.save(&sample("acme/widget")).unwrap();
        let dir = s.product_dir("acme/widget").unwrap();
        std::fs::write(dir.join(META_FILE), b"{not json").unwrap();
        assert!(matches!(
            s.load("acme/widget").unwrap_err(),
            StoreError::Corrupt { .. }
        ));
        // An empty token file reads as no license.
        std::fs::write(
            dir.join(META_FILE),
            br#"{"product":"acme/widget","issuer":"x"}"#,
        )
        .unwrap();
        std::fs::write(dir.join(TOKEN_FILE), b"  \n").unwrap();
        assert!(s.load("acme/widget").unwrap().is_none());
        let _ = std::fs::remove_dir_all(&root);
    }
}
