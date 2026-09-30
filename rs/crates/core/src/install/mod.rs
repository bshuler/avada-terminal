//! Install store (docs/modules-fanout-plan.md, track H5): module directories,
//! `modules.lock`, the HMAC'd install record, the keychain-held MAC key and the
//! artifact hash check at every spawn.
//!
//! Layout, all under `<data_dir>/modules` ([`dirs::InstallPaths`]):
//!
//! ```text
//! modules/
//!   modules.lock                     one entry per module: the ACTIVE version
//!   keys/<key_id>                    the HMAC key (FileKeyStore fallback, owner-only)
//!   <owner>__<repo>/<version>/
//!     record.json                    SignedInstallRecord: rights + manifest + artifact hash
//!     bin/<name>                     the artifact (module binary)
//!     data/                          the module's writable state (AVADA_MODULE_DATA)
//! ```
//!
//! Versions sit side by side; the lockfile pins the active one and a workspace may pin
//! another (`WorkspaceModuleState::pins`, stored by `persistence::lockfile`). Trust flows
//! only from `record.json`: the host (track H1) reads it through [`InstallStore`], which
//! verifies the MAC, and re-hashes the binary ([`verify_artifact`]) before every spawn.
//!
//! The key that signs records comes from a [`KeyStore`]. The OS keychain is the intended
//! home; until the `keyring` crate can be added, [`FileKeyStore`] keeps it owner-only on
//! disk. Key bytes are never logged, printed or placed in an error.

pub mod artifact;
pub mod dirs;
pub mod keyring;
#[cfg(any(windows, test))]
pub mod keyring_windows;
pub mod lock;
pub mod record;
pub mod resolver;
pub mod seed;
pub mod store;

pub use artifact::{hash_file, hash_skills, verify_artifact, verify_skills};
pub use dirs::InstallPaths;
pub use seed::{ledger_path, seed_bundled, seed_modules_dir, SeedOutcome};
pub use keyring::{FileKeyStore, KeyError, KeyStore, MemoryKeyStore, SecretKey, DEFAULT_KEY_ID};
pub use store::{InstallStore, Installed, RecordStatus};

use crate::persistence::lockfile::LockfileIoError;
use avada_module_sdk::rights::RightsError;
use avada_module_sdk::ModuleId;
use semver::Version;
use std::fmt;
use std::path::PathBuf;

/// Why an install-store operation failed. Carries paths and ids, never key bytes.
#[derive(Debug)]
pub enum InstallError {
    /// The filesystem said no.
    Io {
        /// The path involved.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The record's MAC did not verify, or its accepted set exceeds the manifest's
    /// request.
    Rights(RightsError),
    /// `modules.lock` could not be read or written.
    Lockfile(LockfileIoError),
    /// The signing key could not be obtained.
    Key(KeyError),
    /// `record.json` is present but unreadable as a signed record.
    Record {
        /// The file involved.
        path: PathBuf,
        /// What went wrong.
        reason: String,
    },
    /// The artifact on disk does not hash to what the record says.
    HashMismatch {
        /// The artifact.
        path: PathBuf,
        /// The record's `artifact_sha256`.
        expected: String,
        /// What the bytes on disk hash to.
        actual: String,
    },
    /// The record given to `install` disagrees with its own manifest.
    Inconsistent(String),
    /// No such module (at that version) is installed.
    NotInstalled {
        /// The module.
        id: ModuleId,
        /// The version asked for, when one was.
        version: Option<Version>,
    },
    /// A version directory exists but cannot be trusted: tampered record, missing
    /// binary, or a record that does not belong in that directory.
    Broken {
        /// The version directory.
        dir: PathBuf,
        /// Why.
        reason: String,
    },
}

impl fmt::Display for InstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InstallError::Io { path, source } => write!(f, "{}: {source}", path.display()),
            InstallError::Rights(e) => write!(f, "install record: {e}"),
            InstallError::Lockfile(e) => write!(f, "lockfile: {e}"),
            InstallError::Key(e) => write!(f, "signing key: {e}"),
            InstallError::Record { path, reason } => {
                write!(f, "{}: unreadable install record: {reason}", path.display())
            }
            InstallError::HashMismatch {
                path,
                expected,
                actual,
            } => write!(
                f,
                "{}: artifact hash mismatch (record {expected}, on disk {actual})",
                path.display()
            ),
            InstallError::Inconsistent(why) => write!(f, "install record inconsistent: {why}"),
            InstallError::NotInstalled { id, version } => match version {
                Some(v) => write!(f, "{} {v} is not installed", id.as_str()),
                None => write!(f, "{} is not installed", id.as_str()),
            },
            InstallError::Broken { dir, reason } => {
                write!(f, "{}: broken install: {reason}", dir.display())
            }
        }
    }
}

impl std::error::Error for InstallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            InstallError::Io { source, .. } => Some(source),
            InstallError::Rights(e) => Some(e),
            InstallError::Lockfile(e) => Some(e),
            InstallError::Key(e) => Some(e),
            _ => None,
        }
    }
}

impl From<RightsError> for InstallError {
    fn from(e: RightsError) -> Self {
        InstallError::Rights(e)
    }
}

impl From<LockfileIoError> for InstallError {
    fn from(e: LockfileIoError) -> Self {
        InstallError::Lockfile(e)
    }
}

impl From<KeyError> for InstallError {
    fn from(e: KeyError) -> Self {
        InstallError::Key(e)
    }
}

/// Attach a path to an io error.
pub(crate) fn io_at(path: &std::path::Path, source: std::io::Error) -> InstallError {
    InstallError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::lockfile::LockfileIoError;
    use avada_module_sdk::rights::RightsError;
    use std::error::Error as _;

    fn io_err() -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::PermissionDenied, "nope")
    }

    #[test]
    fn every_wrapping_error_exposes_its_underlying_cause() {
        // `source()` is what `?`-propagation, `anyhow`'s `{:#}`, and a log's error chain
        // walk to reach the real cause. Each wrapping variant must hand back its inner
        // error; dropping any arm (or blanking the whole method to `None`) would sever
        // the chain and hide, e.g., the io errno or the MAC-verification failure behind a
        // bare top-line. Assert a source is present for each wrapping variant, and that
        // the leaf/plain variants correctly report none.
        let wrapping: Vec<InstallError> = vec![
            InstallError::Io {
                path: PathBuf::from("/x"),
                source: io_err(),
            },
            InstallError::Rights(RightsError::BadSignature),
            InstallError::Lockfile(LockfileIoError::Io {
                path: PathBuf::from("/x/modules.lock"),
                source: io_err(),
            }),
            InstallError::Key(KeyError::BadKeyId("bad".into())),
        ];
        for e in &wrapping {
            assert!(
                e.source().is_some(),
                "a wrapping variant must expose its cause: {e:?}"
            );
        }
        // A variant that carries only strings/paths has no deeper Error to point at.
        let plain = InstallError::Inconsistent("no manifest".into());
        assert!(
            plain.source().is_none(),
            "a plain variant has no underlying error to expose"
        );
    }
}
