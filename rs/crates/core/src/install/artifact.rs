//! The artifact hash: computed at install, pinned in the record and the lockfile, and
//! re-checked by the host before every spawn so a binary swapped under an installed
//! record never runs with that record's rights.

use super::{io_at, InstallError};
use avada_module_sdk::rights::InstallRecord;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;

/// SHA-256 of a file's bytes as lowercase hex, streamed so a large binary is never
/// held in memory whole.
pub fn hash_file(path: &Path) -> Result<String, InstallError> {
    let mut file = std::fs::File::open(path).map_err(|e| io_at(path, e))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|e| io_at(path, e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

/// Check that the artifact at `path` hashes to the record's `artifact_sha256`.
/// Comparison is case-insensitive on the hex so a hand-edited uppercase record still
/// matches its own bytes; the hash itself is what matters.
pub fn verify_artifact(record: &InstallRecord, path: &Path) -> Result<(), InstallError> {
    let actual = hash_file(path)?;
    let expected = record.artifact_sha256.to_ascii_lowercase();
    if expected.len() != actual.len() || !constant_time_eq(expected.as_bytes(), actual.as_bytes()) {
        return Err(InstallError::HashMismatch {
            path: path.to_path_buf(),
            expected: record.artifact_sha256.clone(),
            actual,
        });
    }
    Ok(())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    a.ct_eq(b).into()
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "avada-artifact-{tag}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn hashes_match_the_known_vector() {
        let dir = scratch("vector");
        let p = dir.join("abc");
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(
            hash_file(&p).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let empty = dir.join("empty");
        std::fs::write(&empty, b"").unwrap();
        assert_eq!(
            hash_file(&empty).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_large_file_streams_to_the_same_digest() {
        let dir = scratch("large");
        let p = dir.join("big");
        let bytes: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&p, &bytes).unwrap();
        let whole = hex(&Sha256::digest(&bytes));
        assert_eq!(hash_file(&p).unwrap(), whole);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_is_an_io_error_with_the_path() {
        let dir = scratch("missing");
        let p = dir.join("nope");
        match hash_file(&p).unwrap_err() {
            InstallError::Io { path, source } => {
                assert_eq!(path, p);
                assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
            }
            other => panic!("expected io error, got {other}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
