//! The always-on **Hyperpane** tab's working directory.
//!
//! The tab runs the user's coding CLI in [`paths::hyperpane_dir`], and the files that make that
//! directory a useful place to start — the README a human reads, the `.claude/` settings and
//! reference material — ship with the binary under `resources/claude/hyperpane/`. This module
//! copies them out to the durable location on every start, so an upgraded app refreshes them
//! without the user doing anything.
//!
//! The copy is one-directional and additive: files the app ships are overwritten, and nothing
//! else in the directory is ever touched. That split is the whole contract — the agent keeps
//! notes there and the user drops files in, and neither can be clobbered by an upgrade, while
//! a locally-edited shipped file is app-owned and *will* be replaced.
//!
//! What this module deliberately no longer does is materialize skills. The rule that teaches an
//! agent about `avada ctl` belongs to the `bshuler/avada-hyperpane` module and travels with it;
//! [`crate::skills::Materializer`] runs over *installed modules*, from the registry, and has no
//! special case for this directory any more. Seeding a directory is a host job; deciding what
//! an agent is told is a module's.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::persistence::paths;

/// The shipped copy of the Hyperpane directory, or `None` when the app is running from a tree
/// that doesn't carry it.
///
/// Same packaged layouts [`crate::shell_integration::shell_integration_dir`] handles —
/// exe-relative (which also covers a dev `cargo run`, since `build.rs` stages resources next to
/// the binary), the macOS `.app` `Contents/Resources`, and the FHS `share`/`lib` prefixes.
#[tracing::instrument(level = "debug", ret)]
pub fn source_dir() -> Option<PathBuf> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))?;
    let rel = Path::new("resources").join("claude").join("hyperpane");
    let mut candidates = vec![exe_dir.join(&rel)];
    if let Some(prefix) = exe_dir.parent() {
        candidates.push(prefix.join("Resources").join("claude").join("hyperpane"));
        candidates.push(prefix.join("share").join("avada").join(&rel));
        candidates.push(prefix.join("lib").join("avada").join(&rel));
    }
    candidates.into_iter().find(|c| c.is_dir())
}

/// Refresh [`paths::hyperpane_dir`] from the shipped tree and return it.
///
/// The directory is created even when nothing ships (a stripped build, a dev binary run from a
/// tree without its resources): the tab still needs a cwd, and an empty one is a working — if
/// unhelpful — starting point, which is strictly better than the tab failing to open.
#[tracing::instrument(level = "debug", ret)]
pub fn materialize() -> io::Result<PathBuf> {
    materialize_into(&paths::hyperpane_dir(), source_dir().as_deref())
}

/// [`materialize`] with both paths explicit: copy the shipped tree at `src` (if any) onto
/// `dest`, creating `dest` either way. Injectable so tests never touch the real home.
#[tracing::instrument(level = "debug", ret)]
pub fn materialize_into(dest: &Path, src: Option<&Path>) -> io::Result<PathBuf> {
    fs::create_dir_all(dest)?;
    if let Some(src) = src {
        copy_over(src, dest)?;
    }
    Ok(dest.to_path_buf())
}

/// Recursively copy `src` onto `dest`, overwriting collisions and leaving everything else in
/// `dest` alone. Errors on individual entries are skipped rather than aborting the walk: a
/// single unreadable file should not cost the agent its whole starting directory.
#[tracing::instrument(level = "debug", ret)]
fn copy_over(src: &Path, dest: &Path) -> io::Result<()> {
    for entry in fs::read_dir(src)? {
        let Ok(entry) = entry else { continue };
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if from.is_dir() {
            if fs::create_dir_all(&to).is_ok() {
                let _ = copy_over(&from, &to);
            }
        } else {
            let _ = fs::copy(&from, &to);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let tmp = std::env::temp_dir().join(format!("hp-{tag}-{}", uuid::Uuid::new_v4()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        tmp
    }

    /// The tree as it ships: `resources/claude/hyperpane` at the repo root.
    fn shipped() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("..")
            .join("resources")
            .join("claude")
            .join("hyperpane")
    }

    #[test]
    fn shipped_files_are_refreshed_and_local_ones_survive() {
        let tmp = scratch("copy");
        let src = tmp.join("src");
        let dest = tmp.join("dest");
        fs::create_dir_all(src.join(".claude").join("skills").join("avada")).unwrap();
        fs::write(
            src.join(".claude")
                .join("skills")
                .join("avada")
                .join("SKILL.md"),
            "new",
        )
        .unwrap();
        fs::create_dir_all(dest.join(".claude").join("skills").join("avada")).unwrap();
        fs::write(
            dest.join(".claude")
                .join("skills")
                .join("avada")
                .join("SKILL.md"),
            "old",
        )
        .unwrap();
        fs::write(dest.join("notes.md"), "the agent's own notes").unwrap();

        copy_over(&src, &dest).unwrap();

        // App-owned file replaced, hidden `.claude/` subtree walked, user file untouched.
        let skill = fs::read_to_string(
            dest.join(".claude")
                .join("skills")
                .join("avada")
                .join("SKILL.md"),
        )
        .unwrap();
        assert_eq!(skill, "new");
        assert_eq!(
            fs::read_to_string(dest.join("notes.md")).unwrap(),
            "the agent's own notes"
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    /// `build.rs` (frozen) names these five paths in `rerun-if-changed`; a rename that left
    /// them behind would silently stop re-staging the resources.
    #[test]
    fn the_shipped_tree_still_holds_what_build_rs_watches() {
        for rel in [
            "README.md",
            ".claude/settings.json",
            ".claude/skills/avada/SKILL.md",
            ".claude/skills/avada/REFERENCE.md",
            ".claude/skills/avada/RECIPES.md",
        ] {
            let p = rel.split('/').fold(shipped(), |p, s| p.join(s));
            assert!(p.is_file(), "{} missing", p.display());
        }
    }

    /// The skill unit left with the `bshuler/avada-hyperpane` module. Nothing under the shipped
    /// tree may claim module ownership any more: a stray `skills/` directory here would be
    /// content the host ships and no registry knows about, which is exactly the confusion the
    /// extraction removed.
    #[test]
    fn the_host_ships_no_module_skills_of_its_own() {
        assert!(
            !shipped().join("skills").exists(),
            "skill units belong to the hyperpane module, not to resources/claude/hyperpane"
        );
    }

    #[test]
    fn materialize_into_copies_the_tree_and_leaves_local_files_be() {
        let tmp = scratch("into");
        let dest = tmp.join("dest");
        let out = materialize_into(&dest, Some(&shipped())).unwrap();
        assert_eq!(out, dest);
        assert!(dest.join("README.md").is_file());
        assert!(dest
            .join(".claude")
            .join("skills")
            .join("avada")
            .join("SKILL.md")
            .is_file());
        // No fences, no imports: materialization is the module registry's job now.
        assert!(!dest.join("AGENTS.md").exists());
        assert!(!dest.join("CLAUDE.md").exists());

        // The agent's own notes survive a second run, which changes nothing.
        fs::write(dest.join("notes.md"), "mine").unwrap();
        materialize_into(&dest, Some(&shipped())).unwrap();
        assert_eq!(fs::read_to_string(dest.join("notes.md")).unwrap(), "mine");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn materialize_into_without_a_source_still_makes_the_dir() {
        let tmp = scratch("nosrc");
        let dest = tmp.join("dest");
        materialize_into(&dest, None).unwrap();
        assert!(dest.is_dir());
        assert!(!dest.join("README.md").exists());
        let _ = fs::remove_dir_all(&tmp);
    }
}
