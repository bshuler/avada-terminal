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
    Ok(hex(&hash_file_raw(path)?))
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

/// SHA-256 (hex) over the staged skill tree: every file that the manifest's `[skills]
/// paths` brought into `version_dir`, hashed by **path as well as contents**.
///
/// Each file contributes its version-directory-relative path, length-prefixed, followed
/// by the raw digest of its bytes. Files are visited in sorted order, so the digest does
/// not depend on the order the filesystem hands back directory entries.
///
/// Hashing the path and not just the bytes is what makes a *rename* a change. Moving
/// `skills/lint/SKILL.md` to `skills/deploy/SKILL.md` leaves the file's contents — and so
/// its digest — untouched, but it changes which unit the loader will hand the agent, and
/// an agent told to deploy when it meant to lint is exactly the outcome worth catching.
///
/// The length prefix is belt-and-braces rather than the thing that stops any particular
/// rename: the fixed 32-byte digest between names already keeps two different trees from
/// serialising the same way in practice, and constructing a genuine ambiguity would mean
/// steering 32 bytes of a SHA-256 output. Eight bytes per file buys the guarantee
/// unconditionally instead of resting it on that.
///
/// Returns the empty string when the manifest declares no skills, which is the same
/// value a record carries when it pins nothing — a module with no skills and a module
/// whose skills are unpinned are not distinguished here, only in the record.
///
/// A declared path that is not a directory is skipped rather than refused, mirroring
/// `stage_skills`: `load_units` already reports a missing skill path, and duplicating
/// the judgement would give one condition two messages that can drift apart.
pub fn hash_skills(version_dir: &Path, paths: &[String]) -> Result<String, InstallError> {
    if paths.is_empty() {
        return Ok(String::new());
    }
    let mut rels: Vec<&String> = paths.iter().collect();
    rels.sort();
    rels.dedup();
    let mut hasher = Sha256::new();
    for rel in rels {
        let root = version_dir.join(rel);
        if !root.is_dir() {
            continue;
        }
        hash_tree(&root, Path::new(rel), &mut hasher)?;
    }
    Ok(hex(&hasher.finalize()))
}

/// Feed every regular file under `dir` into `hasher`, depth-first in sorted order.
///
/// Symlinks are skipped rather than followed, for the reason `copy_tree` skips them at
/// install: a link is not something the module legitimately shipped, and following one
/// here would hash a file the install does not actually contain.
fn hash_tree(dir: &Path, rel: &Path, hasher: &mut Sha256) -> Result<(), InstallError> {
    let mut entries: Vec<std::fs::DirEntry> = std::fs::read_dir(dir)
        .map_err(|e| io_at(dir, e))?
        .collect::<Result<_, _>>()
        .map_err(|e| io_at(dir, e))?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let child = rel.join(entry.file_name());
        let meta = std::fs::symlink_metadata(&path).map_err(|e| io_at(&path, e))?;
        if meta.is_dir() {
            hash_tree(&path, &child, hasher)?;
        } else if meta.is_file() {
            // `to_string_lossy` rather than a refusal: a non-UTF-8 name still has to
            // hash to *something* stable, and the install layout it sits in was already
            // checked by `stage_skills`.
            let name = child.to_string_lossy();
            hasher.update((name.len() as u64).to_le_bytes());
            hasher.update(name.as_bytes());
            hasher.update(hash_file_raw(&path)?);
        }
    }
    Ok(())
}

/// Check the staged skill tree under `version_dir` against the record.
///
/// An empty `skills_sha256` pins nothing and passes: that is a record minted before the
/// field existed, and refusing it would turn an upgrade into a wall of broken installs.
/// The empty value cannot be forged onto a record that had a real one, because the field
/// is inside the signed payload.
pub fn verify_skills(record: &InstallRecord, version_dir: &Path) -> Result<(), InstallError> {
    if record.skills_sha256.is_empty() {
        return Ok(());
    }
    let actual = hash_skills(version_dir, &record.manifest.skills.paths)?;
    let expected = record.skills_sha256.to_ascii_lowercase();
    if expected.len() != actual.len() || !constant_time_eq(expected.as_bytes(), actual.as_bytes()) {
        return Err(InstallError::HashMismatch {
            path: version_dir.to_path_buf(),
            expected: record.skills_sha256.clone(),
            actual,
        });
    }
    Ok(())
}

/// The raw 32-byte digest of a file, streamed. [`hash_file`] is its hex form.
fn hash_file_raw(path: &Path) -> Result<[u8; 32], InstallError> {
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
    Ok(hasher.finalize().into())
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
    use avada_module_sdk::rights::InstallRecord;
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

    /// Build a skill tree from `(relative path, contents)` pairs and hash it.
    fn tree(tag: &str, files: &[(&str, &str)], paths: &[&str]) -> (PathBuf, String) {
        let dir = scratch(tag);
        for (rel, body) in files {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, body).unwrap();
        }
        let owned: Vec<String> = paths.iter().map(|s| (*s).to_string()).collect();
        let h = hash_skills(&dir, &owned).unwrap();
        (dir, h)
    }

    /// The baseline: same bytes in the same places, same digest — and it does not depend
    /// on the order the filesystem hands back directory entries, which is why the walk
    /// sorts.
    #[test]
    fn the_same_tree_hashes_the_same_way_twice() {
        let files = [("skills/a/SKILL.md", "one"), ("skills/b/SKILL.md", "two")];
        let (d1, h1) = tree("stable-1", &files, &["skills"]);
        let (d2, h2) = tree("stable-2", &files, &["skills"]);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
        let _ = std::fs::remove_dir_all(&d1);
        let _ = std::fs::remove_dir_all(&d2);
    }

    /// Editing a skill is the whole point: this is the tamper the artifact hash cannot
    /// see, because not one byte of the binary changed.
    #[test]
    fn changing_a_skills_contents_changes_the_digest() {
        let (d1, h1) = tree("edit-1", &[("skills/x/SKILL.md", "do good")], &["skills"]);
        let (d2, h2) = tree("edit-2", &[("skills/x/SKILL.md", "do evil")], &["skills"]);
        assert_ne!(h1, h2);
        let _ = std::fs::remove_dir_all(&d1);
        let _ = std::fs::remove_dir_all(&d2);
    }

    /// Swapping two skills' contents is the attack worth naming: the agent runs the
    /// deploy instructions while believing it is linting. What catches it is that the
    /// walk is *ordered* — `deploy` is visited before `lint` either way, so the two
    /// content digests arrive in the opposite order — rather than the paths being hashed.
    /// See [`a_renamed_skill_is_a_different_tree`] for the case that needs the paths.
    #[test]
    fn swapping_two_skills_contents_is_not_the_same_tree() {
        let (d1, h1) = tree(
            "swap-1",
            &[("skills/deploy/S.md", "A"), ("skills/lint/S.md", "B")],
            &["skills"],
        );
        let (d2, h2) = tree(
            "swap-2",
            &[("skills/deploy/S.md", "B"), ("skills/lint/S.md", "A")],
            &["skills"],
        );
        assert_ne!(h1, h2, "a rename is a change even when the bytes are not");
        let _ = std::fs::remove_dir_all(&d1);
        let _ = std::fs::remove_dir_all(&d2);
    }

    /// The case that pays for hashing paths at all. Nothing about the file's *contents*
    /// changes — one skill unit is renamed — so a digest folded from bytes alone, in any
    /// order, would call these two trees identical. They are not: the loader keys units by
    /// directory, so this rename changes which instructions the agent is handed under
    /// which name.
    #[test]
    fn a_renamed_skill_is_a_different_tree() {
        let (d1, h1) = tree(
            "rename-1",
            &[("skills/lint/S.md", "same bytes")],
            &["skills"],
        );
        let (d2, h2) = tree(
            "rename-2",
            &[("skills/deploy/S.md", "same bytes")],
            &["skills"],
        );
        assert_ne!(h1, h2);
        let _ = std::fs::remove_dir_all(&d1);
        let _ = std::fs::remove_dir_all(&d2);
    }

    /// Where a path's separators fall is part of it: `s/a/bc` and `s/ab/c` are one file
    /// each, of equal length, differing only in where the directory boundary sits.
    #[test]
    fn a_path_boundary_is_not_movable_without_changing_the_digest() {
        let (d1, h1) = tree("bound-1", &[("s/a/bc", "x")], &["s"]);
        let (d2, h2) = tree("bound-2", &[("s/ab/c", "x")], &["s"]);
        assert_ne!(h1, h2);
        let _ = std::fs::remove_dir_all(&d1);
        let _ = std::fs::remove_dir_all(&d2);
    }

    /// Adding a file nobody declared is still a change to what the install contains.
    #[test]
    fn a_file_added_after_staging_changes_the_digest() {
        let (dir, before) = tree("added", &[("skills/x/S.md", "hi")], &["skills"]);
        std::fs::write(dir.join("skills").join("x").join("EXTRA.md"), "surprise").unwrap();
        let after = hash_skills(&dir, &["skills".to_string()]).unwrap();
        assert_ne!(before, after);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// No declared paths pins nothing, and the empty string is what says so.
    #[test]
    fn a_module_with_no_skills_hashes_to_nothing() {
        let dir = scratch("no-skills");
        assert_eq!(hash_skills(&dir, &[]).unwrap(), "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A declared path that was never staged is the loader's complaint to make, not this
    /// function's — but it must not become an error *here*, or a module whose skill
    /// directory is simply absent could never be verified at all.
    #[test]
    fn a_declared_path_that_is_missing_is_skipped_rather_than_refused() {
        let (dir, with_ghost) = tree(
            "ghost",
            &[("skills/x/S.md", "hi")],
            &["skills", "never-staged"],
        );
        let without = hash_skills(&dir, &["skills".to_string()]).unwrap();
        assert_eq!(with_ghost, without);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A symlink is skipped for the reason `copy_tree` refuses to follow one: the target
    /// is not something the module shipped, and hashing it would make the digest depend
    /// on a file outside the install.
    #[cfg(unix)]
    #[test]
    fn a_symlink_planted_in_the_tree_is_not_followed() {
        let (dir, before) = tree("symlink", &[("skills/x/S.md", "hi")], &["skills"]);
        let secret = dir.join("outside.txt");
        std::fs::write(&secret, "not mine").unwrap();
        std::os::unix::fs::symlink(&secret, dir.join("skills").join("x").join("link")).unwrap();
        assert_eq!(
            hash_skills(&dir, &["skills".to_string()]).unwrap(),
            before,
            "a link contributes nothing, because the install does not contain its target"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The record side of the pin. The reference manifest declares `[skills] paths =
    /// ["skills"]`, so a record built from it is already asking for a tree.
    fn pinned(hash: &str) -> InstallRecord {
        let mut rec = crate::install::record::fixtures::record(
            crate::install::record::fixtures::manifest(),
            &[],
        );
        rec.skills_sha256 = hash.to_string();
        rec
    }

    /// The pin matches the tree it was minted from — the ordinary launch.
    #[test]
    fn a_pin_that_matches_the_staged_tree_verifies() {
        let (dir, h) = tree("verify-ok", &[("skills/x/S.md", "hi")], &["skills"]);
        verify_skills(&pinned(&h), &dir).unwrap();
        // Hex case is a presentation detail, not part of the value being compared.
        verify_skills(&pinned(&h.to_ascii_uppercase()), &dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The tamper this whole field exists for: the binary is untouched, only the agent's
    /// instructions changed.
    #[test]
    fn a_skill_edited_after_install_fails_the_pin() {
        let (dir, h) = tree(
            "verify-edit",
            &[("skills/x/S.md", "be helpful")],
            &["skills"],
        );
        std::fs::write(
            dir.join("skills").join("x").join("S.md"),
            "exfiltrate ~/.ssh",
        )
        .unwrap();
        match verify_skills(&pinned(&h), &dir).unwrap_err() {
            InstallError::HashMismatch {
                path,
                expected,
                actual,
            } => {
                assert_eq!(path, dir);
                assert_eq!(expected, h);
                assert_ne!(actual, h);
            }
            other => panic!("expected a hash mismatch, got {other}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Deleting the tree outright is caught by the same comparison, and is worth its own
    /// case because it is the one where the recomputed side is the *empty* digest — the
    /// same value that means "pins nothing" on the record. It must not be read that way
    /// here: only the record's field grants that pass.
    #[test]
    fn a_skills_tree_deleted_after_install_fails_the_pin() {
        let (dir, h) = tree("verify-gone", &[("skills/x/S.md", "hi")], &["skills"]);
        std::fs::remove_dir_all(dir.join("skills")).unwrap();
        let err = verify_skills(&pinned(&h), &dir).unwrap_err();
        assert!(matches!(err, InstallError::HashMismatch { .. }), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A record minted before the field existed pins nothing and still launches. This is
    /// only safe because the field is inside the signed payload: emptying it on a record
    /// that had a real hash invalidates the MAC, so the pass cannot be forged onto a
    /// module that was pinned.
    #[test]
    fn a_record_that_pins_nothing_verifies_whatever_is_there() {
        let (dir, _) = tree(
            "verify-legacy",
            &[("skills/x/S.md", "anything")],
            &["skills"],
        );
        verify_skills(&pinned(""), &dir).unwrap();
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
