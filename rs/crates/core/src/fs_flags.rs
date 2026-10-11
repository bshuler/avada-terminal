//! BSD file flags on files the app owns.
//!
//! The installer locks the app bundle with `uchg`. On macOS `std::fs::copy` is a
//! `clonefile`, and a clone inherits the source's flags, so anything copied out of the
//! bundle that way arrived immutable: a later overwrite fails with EPERM, and so does
//! removing it. These helpers clear the flags first. Elsewhere they do nothing.

use std::io;
use std::path::Path;

/// Clear every user-settable flag on `path` (not following a symlink). A path that is
/// already flag-free, or does not exist, is left alone.
pub fn clear(path: &Path) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::macos::fs::MetadataExt;
        use std::os::unix::ffi::OsStrExt;
        let meta = match std::fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        // `chflags` follows a symlink, and a link's target is not ours to unlock.
        if meta.file_type().is_symlink() || meta.st_flags() == 0 {
            return Ok(());
        }
        let c = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        // SAFETY: `c` is a valid NUL-terminated path for the duration of the call.
        if unsafe { libc::chflags(c.as_ptr(), 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = path;
    Ok(())
}

/// [`clear`] on `path` and everything under it, without following symlinks. Errors on
/// single entries are skipped: the caller's removal reports whatever is still stuck.
pub fn clear_tree(path: &Path) {
    let _ = clear(path);
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    if !meta.is_dir() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        clear_tree(&entry.path());
    }
}

#[cfg(all(test, target_os = "macos"))]
pub(crate) mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Set a flag with the system tool, the way the installer does.
    pub(crate) fn chflags(flag: &str, p: &Path) {
        let ok = std::process::Command::new("/usr/bin/chflags")
            .arg(flag)
            .arg(p)
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "chflags {flag} {}", p.display());
    }

    fn scratch(tag: &str) -> PathBuf {
        let tmp = std::env::temp_dir().join(format!("fsflags-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        tmp
    }

    #[test]
    fn a_locked_tree_becomes_removable() {
        let root = scratch("tree");
        let file = root.join("a").join("SKILL.md");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "x").unwrap();
        chflags("uchg", &file);
        assert!(std::fs::remove_dir_all(&root).is_err(), "uchg blocks removal");
        clear_tree(&root);
        std::fs::remove_dir_all(&root).expect("removable once cleared");
    }

    #[test]
    fn a_missing_path_is_not_an_error() {
        assert!(clear(Path::new("/nonexistent/fsflags/x")).is_ok());
    }
}
