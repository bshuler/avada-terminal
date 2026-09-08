//! Disk cache of the last `GET /schema` per control URL, so `--help` and completions
//! work with no instance running and a running one is asked only when its version moved.
//!
//! Layout: `<data_dir>/cli-schema/<sanitised control url>.json`, one file per instance
//! the CLI has talked to. Freshness is *both* the control URL and `host_version`
//! matching; a corrupt or missing file is a miss, never a panic.

use std::fs;
use std::path::{Path, PathBuf};

use avada_module_sdk::descriptor::SchemaDocument;
use serde::{Deserialize, Serialize};

use crate::persistence::paths;

/// Subdirectory of the data dir that holds the cache files.
pub const DIR_NAME: &str = "cli-schema";

/// What one cache file holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cached {
    /// Base URL of the control API the schema came from.
    pub control_url: String,
    /// `host_version` the instance reported when the schema was fetched.
    pub host_version: String,
    /// The document itself.
    pub schema: SchemaDocument,
}

/// The cache directory: `<data_dir>/cli-schema`.
pub fn default_dir() -> PathBuf {
    paths::data_dir().join(DIR_NAME)
}

/// The file a control URL maps to inside `dir`. Every byte outside `[A-Za-z0-9._-]`
/// becomes `_`, so `http://127.0.0.1:4041` → `http___127.0.0.1_4041.json`.
pub fn file_for(dir: &Path, control_url: &str) -> PathBuf {
    let name: String = control_url
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    dir.join(format!("{name}.json"))
}

/// Read one cache file. Any failure (absent, unreadable, not JSON, not a document) is
/// `None`: the CLI then falls back to its built-in table.
pub fn load(path: &Path) -> Option<Cached> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Write one cache file atomically, creating the directory.
pub fn store(path: &Path, cached: &Cached) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec(cached).map_err(std::io::Error::other)?;
    paths::write_atomic(path, &bytes)
}

/// The freshness rule: a cached schema serves a request only when it came from the same
/// control URL *and* the instance still reports the same `host_version`.
pub fn is_fresh(cached: &Cached, control_url: &str, host_version: &str) -> bool {
    cached.control_url == control_url && cached.host_version == host_version
}

/// With no instance to name a URL, the most recently written cache file in `dir` (by
/// mtime) still drives `--help` and completions.
pub fn latest(dir: &Path) -> Option<Cached> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        if best.as_ref().is_none_or(|(t, _)| modified > *t) {
            best = Some((modified, path));
        }
    }
    load(&best?.1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::schema_cli::tests::sample;

    fn tmp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("avada-cli-cache-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn cached() -> Cached {
        Cached {
            control_url: "http://127.0.0.1:4041".into(),
            host_version: "9.9.9".into(),
            schema: sample(),
        }
    }

    #[test]
    fn freshness_needs_both_url_and_version() {
        let c = cached();
        assert!(is_fresh(&c, "http://127.0.0.1:4041", "9.9.9"));
        assert!(!is_fresh(&c, "http://127.0.0.1:4041", "9.9.10"));
        assert!(!is_fresh(&c, "http://127.0.0.1:4042", "9.9.9"));
    }

    #[test]
    fn store_then_load_round_trips_and_missing_or_corrupt_files_miss() {
        let dir = tmp("roundtrip");
        let path = file_for(&dir, "http://127.0.0.1:4041");
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            "http___127.0.0.1_4041.json"
        );
        assert_eq!(load(&path), None, "missing file is a miss");
        let c = cached();
        store(&path, &c).unwrap();
        assert_eq!(load(&path), Some(c.clone()));
        fs::write(&path, b"{ this is not json").unwrap();
        assert_eq!(load(&path), None, "corrupt file is a miss, not a panic");
        fs::write(&path, br#"{"control_url":"x"}"#).unwrap();
        assert_eq!(load(&path), None, "wrong shape is a miss");
        // A rewrite replaces the corrupt file.
        store(&path, &c).unwrap();
        assert_eq!(load(&path), Some(c));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn latest_picks_the_newest_file_and_tolerates_an_absent_dir() {
        let dir = tmp("latest");
        assert_eq!(latest(&dir), None);
        let old = Cached {
            control_url: "http://a".into(),
            ..cached()
        };
        let new = Cached {
            control_url: "http://b".into(),
            host_version: "1.0.0".into(),
            ..cached()
        };
        store(&file_for(&dir, &old.control_url), &old).unwrap();
        // mtime granularity on some filesystems is a second: push the newer one ahead.
        store(&file_for(&dir, &new.control_url), &new).unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        let f = fs::File::options()
            .write(true)
            .open(file_for(&dir, &new.control_url))
            .unwrap();
        f.set_modified(later).unwrap();
        fs::write(dir.join("notes.txt"), b"ignored").unwrap();
        assert_eq!(latest(&dir).map(|c| c.control_url), Some("http://b".into()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_dir_is_under_the_data_dir() {
        let d = default_dir();
        assert!(d.starts_with(paths::data_dir()));
        assert_eq!(d.file_name().and_then(|n| n.to_str()), Some(DIR_NAME));
    }
}
