//! The optional GitHub bearer token: where it lives and how it is handled.
//!
//! The marketplace works unauthenticated; a token only lifts GitHub's rate limit and
//! reaches private repositories. It is a secret: never logged, never serialized into a
//! route answer, held in a [`Token`] whose `Debug` is redacted and whose bytes are wiped
//! on drop. The file-backed store keeps it owner-only under
//! [`InstallPaths::keys_dir`](crate::install::InstallPaths::keys_dir); the keyring swap
//! (OS keychain) is a recorded follow-up in `docs/marketplace.md`.

use crate::persistence::paths::write_atomic_private;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// A bearer token. `Debug` never prints it.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(Vec<u8>);

impl Token {
    /// Wrap a token string.
    pub fn new(s: impl Into<String>) -> Self {
        Token(s.into().into_bytes())
    }

    /// The token itself, for the one place that puts it on the wire (an
    /// `Authorization` header). Named so a reviewer sees every use.
    pub fn expose_secret(&self) -> &str {
        std::str::from_utf8(&self.0).unwrap_or("")
    }
}

impl Drop for Token {
    fn drop(&mut self) {
        for b in self.0.iter_mut() {
            // Volatile so the wipe survives dead-store elimination.
            unsafe { std::ptr::write_volatile(b, 0) };
        }
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Token([redacted; {} bytes])", self.0.len())
    }
}

/// Token store problems.
#[derive(Debug)]
pub enum TokenError {
    /// The file could not be read or written.
    Io {
        /// The file involved.
        path: PathBuf,
        /// The OS error.
        source: std::io::Error,
    },
}

impl fmt::Display for TokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenError::Io { path, source } => write!(f, "{}: {source}", path.display()),
        }
    }
}
impl std::error::Error for TokenError {}

/// Where the GitHub token lives.
pub trait TokenStore: Send + Sync {
    /// The stored token, if any.
    fn get(&self) -> Result<Option<Token>, TokenError>;
    /// Store (replace) the token.
    fn set(&self, token: &Token) -> Result<(), TokenError>;
    /// Forget the token.
    fn clear(&self) -> Result<(), TokenError>;
}

/// In-memory store for tests.
#[derive(Default)]
pub struct MemoryTokenStore(Mutex<Option<Token>>);

impl MemoryTokenStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

impl TokenStore for MemoryTokenStore {
    fn get(&self) -> Result<Option<Token>, TokenError> {
        Ok(self.0.lock().unwrap().clone())
    }
    fn set(&self, token: &Token) -> Result<(), TokenError> {
        *self.0.lock().unwrap() = Some(token.clone());
        Ok(())
    }
    fn clear(&self) -> Result<(), TokenError> {
        *self.0.lock().unwrap() = None;
        Ok(())
    }
}

/// One owner-only file (`0600`, atomic writes) holding the token.
pub struct FileTokenStore {
    path: PathBuf,
}

/// File name under the keys directory.
pub const TOKEN_FILE: &str = "github-token";

impl FileTokenStore {
    /// Store the token at `keys_dir/github-token`.
    pub fn new(keys_dir: &Path) -> Self {
        FileTokenStore {
            path: keys_dir.join(TOKEN_FILE),
        }
    }

    /// The file's path (for diagnostics; the content is never shown).
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl TokenStore for FileTokenStore {
    fn get(&self) -> Result<Option<Token>, TokenError> {
        match std::fs::read(&self.path) {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes);
                let trimmed = text.trim();
                Ok((!trimmed.is_empty()).then(|| Token::new(trimmed)))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(TokenError::Io {
                path: self.path.clone(),
                source: e,
            }),
        }
    }

    fn set(&self, token: &Token) -> Result<(), TokenError> {
        if let Some(dir) = self.path.parent() {
            crate::install::dirs::ensure_private_dir(dir).map_err(|e| TokenError::Io {
                path: dir.to_path_buf(),
                source: std::io::Error::other(e.to_string()),
            })?;
        }
        write_atomic_private(&self.path, token.expose_secret().as_bytes()).map_err(|e| {
            TokenError::Io {
                path: self.path.clone(),
                source: e,
            }
        })
    }

    fn clear(&self) -> Result<(), TokenError> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(TokenError::Io {
                path: self.path.clone(),
                source: e,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "avada-mp-token-{name}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn debug_never_shows_the_token() {
        let t = Token::new("ghp_secret_value");
        let shown = format!("{t:?}");
        assert!(!shown.contains("secret"), "{shown}");
        assert!(shown.contains("redacted"));
        assert_eq!(t.expose_secret(), "ghp_secret_value");
    }

    #[test]
    fn memory_store_round_trips_and_clears() {
        let s = MemoryTokenStore::new();
        assert!(s.get().unwrap().is_none());
        s.set(&Token::new("abc")).unwrap();
        assert_eq!(s.get().unwrap().unwrap().expose_secret(), "abc");
        s.clear().unwrap();
        assert!(s.get().unwrap().is_none());
    }

    #[test]
    fn file_store_is_owner_only_and_round_trips() {
        let dir = scratch("file");
        let keys = dir.join("keys");
        let s = FileTokenStore::new(&keys);
        assert!(s.get().unwrap().is_none(), "missing file is no token");
        s.set(&Token::new("tok-value")).unwrap();
        assert_eq!(s.get().unwrap().unwrap().expose_secret(), "tok-value");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(s.path()).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "token file is owner-only");
            let dmode = std::fs::metadata(&keys).unwrap().permissions().mode() & 0o777;
            assert_eq!(dmode, 0o700, "keys dir is owner-only");
        }
        s.clear().unwrap();
        assert!(s.get().unwrap().is_none());
        s.clear().unwrap();
        // An empty file is no token either.
        std::fs::write(s.path(), "  \n").unwrap();
        assert!(s.get().unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
