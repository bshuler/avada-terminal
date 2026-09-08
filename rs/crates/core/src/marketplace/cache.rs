//! On-disk cache for GitHub answers: one JSON file per URL, keyed by the URL's SHA-256.
//!
//! Unauthenticated GitHub allows 10 searches a minute and 60 API calls an hour, so every
//! answer is kept with its `ETag` and fetch time. A fresh entry (younger than the TTL) is
//! served without a request at all; a stale one is revalidated with `If-None-Match`, and
//! a `304` refreshes its clock. The body is public data (repository listings, tags,
//! manifests) — never the token — but the files are still written owner-only, like
//! everything else under the modules root.

use crate::persistence::paths::write_atomic_private;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// One cached answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The `ETag` GitHub sent, for `If-None-Match`.
    #[serde(default)]
    pub etag: Option<String>,
    /// The response body.
    pub body: String,
    /// Unix seconds when the body was last confirmed current.
    pub fetched_at: u64,
}

impl Entry {
    /// Whether the entry is younger than `ttl`.
    pub fn is_fresh(&self, ttl: Duration, now: u64) -> bool {
        now.saturating_sub(self.fetched_at) < ttl.as_secs()
    }
}

/// The cache directory.
#[derive(Debug, Clone)]
pub struct Cache {
    dir: PathBuf,
}

/// Unix seconds now.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Cache {
    /// A cache rooted at `dir` (created on first write).
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Cache { dir: dir.into() }
    }

    /// Where the cache lives.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, key: &str) -> PathBuf {
        let digest = Sha256::digest(key.as_bytes());
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        self.dir.join(format!("{hex}.json"))
    }

    /// The entry for `key`, if one is on disk and parses.
    pub fn get(&self, key: &str) -> Option<Entry> {
        let text = std::fs::read_to_string(self.path(key)).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Store an entry. A cache write that fails is not an error the caller can act on:
    /// the answer is still returned, the next call simply refetches.
    pub fn put(&self, key: &str, entry: &Entry) {
        let path = self.path(key);
        let text = match serde_json::to_string(entry) {
            Ok(t) => t,
            Err(_) => return,
        };
        if let Err(e) = write_atomic_private(&path, text.as_bytes()) {
            tracing::debug!(path = %path.display(), error = %e, "marketplace cache write failed");
        }
    }

    /// Refresh the clock of an entry after a `304 Not Modified`.
    pub fn touch(&self, key: &str, now: u64) {
        if let Some(mut entry) = self.get(key) {
            entry.fetched_at = now;
            self.put(key, &entry);
        }
    }

    /// Drop every entry.
    pub fn clear(&self) -> std::io::Result<()> {
        match std::fs::remove_dir_all(&self.dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        std::env::temp_dir().join(format!(
            "avada-mp-cache-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ))
    }

    #[test]
    fn miss_then_hit_then_touch_then_clear() {
        let dir = scratch();
        let cache = Cache::new(&dir);
        let key = "https://api.github.com/search/repositories?q=topic:avada-module";
        assert!(cache.get(key).is_none());
        let entry = Entry {
            etag: Some("\"abc\"".into()),
            body: "{\"items\":[]}".into(),
            fetched_at: 100,
        };
        cache.put(key, &entry);
        assert_eq!(cache.get(key), Some(entry.clone()));
        assert!(entry.is_fresh(Duration::from_secs(60), 150));
        assert!(!entry.is_fresh(Duration::from_secs(60), 160));
        assert!(!entry.is_fresh(Duration::from_secs(60), 161));
        cache.touch(key, 500);
        assert_eq!(cache.get(key).unwrap().fetched_at, 500);
        // Another key does not collide.
        assert!(cache.get("https://api.github.com/other").is_none());
        cache.clear().unwrap();
        assert!(cache.get(key).is_none());
        cache.clear().unwrap();
    }

    #[test]
    fn touch_of_a_missing_key_writes_nothing() {
        let dir = scratch();
        let cache = Cache::new(&dir);
        cache.touch("nope", 1);
        assert!(!dir.exists());
    }
}
