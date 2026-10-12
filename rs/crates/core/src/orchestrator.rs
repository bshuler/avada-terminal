//! The always-on **Orchestrator** tab's working directory.
//!
//! The tab runs the user's coding CLI in [`paths::orchestrator_dir`], and the files that make that
//! directory a useful place to start — the README a human reads, the `.claude/` settings and
//! reference material — ship with the binary under `resources/claude/orchestrator/`. This module
//! copies them out to the durable location on every start, so an upgraded app refreshes them
//! without the user doing anything.
//!
//! The copy is one-directional and additive: files the app ships are overwritten, and nothing
//! else in the directory is ever touched. That split is the whole contract — the agent keeps
//! notes there and the user drops files in, and neither can be clobbered by an upgrade, while
//! a locally-edited shipped file is app-owned and *will* be replaced.
//!
//! The copy-out deliberately does not write skills. The rule that teaches an agent about
//! `avada ctl` belongs to the `bshuler/avada-orchestrator` module and travels with it;
//! [`materialize_modules`] hands the registry's enabled modules to
//! [`crate::skills::Materializer`] with this directory as the only project root, so whatever
//! those modules ship lands here like it would in any project. Seeding a directory is a host
//! job; deciding what an agent is told is a module's.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::persistence::paths;
use crate::skills::{Applied, Materializer, ModuleInput, Request, Tools};

/// The shipped copy of the Orchestrator tab's directory, or `None` when the app is running from a tree
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
    let rel = Path::new("resources").join("claude").join("orchestrator");
    let mut candidates = vec![exe_dir.join(&rel)];
    if let Some(prefix) = exe_dir.parent() {
        candidates.push(prefix.join("Resources").join("claude").join("orchestrator"));
        candidates.push(prefix.join("share").join("avada").join(&rel));
        candidates.push(prefix.join("lib").join("avada").join(&rel));
    }
    candidates.into_iter().find(|c| c.is_dir())
}

/// Refresh [`paths::orchestrator_dir`] from the shipped tree and return it.
///
/// The directory is created even when nothing ships (a stripped build, a dev binary run from a
/// tree without its resources): the tab still needs a cwd, and an empty one is a working — if
/// unhelpful — starting point, which is strictly better than the tab failing to open.
#[tracing::instrument(level = "debug", ret)]
pub fn materialize() -> io::Result<PathBuf> {
    let dest = paths::orchestrator_dir();
    move_legacy_dir(&paths::legacy_orchestrator_dir(), &dest);
    materialize_into(&dest, source_dir().as_deref())
}

/// Carry the directory over from its pre-0.2.36 name (`hyperpane`) by renaming it in place.
///
/// A rename keeps the inode, so an agent already running in the old directory keeps a valid
/// cwd and its notes come along. When both directories exist neither is touched: merging
/// two copies would have to pick a winner per file, and that is how data gets lost.
#[tracing::instrument(level = "debug")]
pub fn move_legacy_dir(legacy: &Path, dest: &Path) {
    if !legacy.is_dir() {
        return;
    }
    if dest.exists() {
        tracing::warn!(
            legacy = %legacy.display(),
            dest = %dest.display(),
            "both the old and the new orchestrator directory exist; leaving the old one for the user"
        );
        return;
    }
    match fs::rename(legacy, dest) {
        Ok(()) => {
            tracing::info!(from = %legacy.display(), to = %dest.display(), "moved the orchestrator directory")
        }
        Err(e) => {
            tracing::warn!(error = %e, from = %legacy.display(), "could not move the orchestrator directory")
        }
    }
}

/// [`materialize`] with both paths explicit: copy the shipped tree at `src` (if any) onto
/// `dest`, creating `dest` either way. Injectable so tests never touch the real home.
#[tracing::instrument(level = "debug", ret)]
pub fn materialize_into(dest: &Path, src: Option<&Path>) -> io::Result<PathBuf> {
    fs::create_dir_all(dest)?;
    drop_legacy_fences(dest);
    if let Some(src) = src {
        copy_over(src, dest)?;
    }
    Ok(dest.to_path_buf())
}

/// The materializer's view of one install: its record's manifest and accepted capabilities,
/// with `enabled` deciding whether it writes here or is swept away.
pub fn module_input(installed: &crate::install::Installed, enabled: bool) -> ModuleInput {
    let record = installed.rights();
    ModuleInput {
        id: installed.id.clone(),
        name: record.manifest.module.name.clone(),
        version: installed.version.to_string(),
        version_dir: installed.version_dir.clone(),
        skills: record.manifest.skills.clone(),
        accepted: record.accepted.clone(),
        enabled,
    }
}

/// Write `modules`' skills into [`paths::orchestrator_dir`], and sweep what a module that is no
/// longer among them left there. See [`materialize_modules_into`].
pub fn materialize_modules(modules: &[ModuleInput], tools: &Tools) -> Applied {
    let dest = paths::orchestrator_dir();
    // The runtime can get here before the tab's own refresh does; creating `dest` first
    // would strand a pre-0.2.36 directory under its old name.
    move_legacy_dir(&paths::legacy_orchestrator_dir(), &dest);
    materialize_modules_into(&dest, modules, tools)
}

/// Run the materializer with `dest` as the only project root and the user scope left alone.
///
/// Only files carrying an `avada:module` fence are ever changed or removed, so the user's
/// notes and the shipped `.claude/skills/avada/` survive. There is no index skill (`schema`
/// is `None`): the agent here already has the full `avada ctl` reference. Problems are
/// logged, never returned — a module with a bad skill must not keep the tab from opening.
pub fn materialize_modules_into(dest: &Path, modules: &[ModuleInput], tools: &Tools) -> Applied {
    let roots = [dest.to_path_buf()];
    let materializer = Materializer::new();
    let plan = materializer.plan(&Request {
        modules,
        roots: &roots,
        home: None,
        tools,
        schema: None,
        workspace: None,
    });
    for e in &plan.errors {
        tracing::warn!(module = %e.module.as_str(), file = %e.path.display(), error = %e.error, "orchestrator: a module skill was not materialized");
    }
    for s in &plan.skipped {
        tracing::info!(unit = ?s.unit, tool = %s.tool, reason = %s.reason, "orchestrator: skill skipped");
    }
    let applied = materializer.apply(&plan);
    for (path, e) in &applied.failed {
        tracing::warn!(file = %path.display(), error = %e, "orchestrator: could not write a module skill");
    }
    if !applied.written.is_empty() || !applied.removed.is_empty() {
        tracing::info!(
            written = applied.written.len(),
            removed = applied.removed.len(),
            "orchestrator: module skills refreshed"
        );
    }
    applied
}

/// Remove what the host fenced into this directory before the tab's rule moved into the
/// orchestrator module (September 2026): the rule itself, under `avada/hyperpane` in
/// `AGENTS.md`, and the `@AGENTS.md` import under the host id in `CLAUDE.md`. Nothing has
/// written either since, so no sweep would ever take them away, and the agent kept reading
/// the old product's name. Text outside the fences is the user's and stays; the import goes
/// only once `AGENTS.md` is gone, so a user's own `AGENTS.md` is still imported.
fn drop_legacy_fences(dest: &Path) {
    unfence(&dest.join("AGENTS.md"), "avada/hyperpane");
    if !dest.join("AGENTS.md").exists() {
        unfence(&dest.join("CLAUDE.md"), crate::skills::HOST_ID);
    }
}

/// Drop fence `id` from the file at `path`, removing the file when nothing but whitespace
/// is left. A missing file, or one without that fence, is left alone.
fn unfence(path: &Path, id: &str) {
    let Ok(text) = fs::read_to_string(path) else {
        return;
    };
    let next = avada_module_sdk::skills::splice_fenced(&text, id, None);
    if next == text {
        return;
    }
    let done = if next.trim().is_empty() {
        fs::remove_file(path)
    } else {
        fs::write(path, next)
    };
    if let Err(e) = done {
        tracing::warn!(error = %e, file = %path.display(), "could not drop a legacy fence");
    }
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
        } else if let Err(e) = copy_file(&from, &to) {
            tracing::warn!(error = %e, file = %to.display(), "could not refresh a shipped orchestrator file");
        }
    }
    Ok(())
}

/// Copy one file's bytes onto `to`, unlocking a stale copy first. Not `fs::copy`: on macOS
/// that clones the `uchg` flag off the locked bundle, and the next refresh then fails (see
/// [`crate::fs_flags`]). Copies made that way before 0.2.37 are healed here.
fn copy_file(from: &Path, to: &Path) -> io::Result<()> {
    crate::fs_flags::clear(to)?;
    let mut src = fs::File::open(from)?;
    let mut dst = fs::File::create(to)?;
    io::copy(&mut src, &mut dst)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_legacy_dir_is_renamed_in_place_with_its_contents() {
        let root = scratch("legacy-move");
        let legacy = root.join("hyperpane");
        let dest = root.join("orchestrator");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("notes.md"), "mine").unwrap();
        move_legacy_dir(&legacy, &dest);
        assert!(!legacy.exists(), "the old name is gone");
        assert_eq!(fs::read_to_string(dest.join("notes.md")).unwrap(), "mine");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn when_both_dirs_exist_neither_is_touched() {
        let root = scratch("legacy-both");
        let legacy = root.join("hyperpane");
        let dest = root.join("orchestrator");
        fs::create_dir_all(&legacy).unwrap();
        fs::create_dir_all(&dest).unwrap();
        fs::write(legacy.join("a"), "old").unwrap();
        fs::write(dest.join("a"), "new").unwrap();
        move_legacy_dir(&legacy, &dest);
        assert_eq!(fs::read_to_string(legacy.join("a")).unwrap(), "old");
        assert_eq!(fs::read_to_string(dest.join("a")).unwrap(), "new");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn no_legacy_dir_is_a_no_op() {
        let root = scratch("legacy-none");
        let dest = root.join("orchestrator");
        move_legacy_dir(&root.join("hyperpane"), &dest);
        assert!(!dest.exists());
        let _ = fs::remove_dir_all(&root);
    }

    fn scratch(tag: &str) -> PathBuf {
        let tmp = std::env::temp_dir().join(format!("hp-{tag}-{}", uuid::Uuid::new_v4()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        tmp
    }

    /// The tree as it ships: `resources/claude/orchestrator` at the repo root.
    fn shipped() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("..")
            .join("resources")
            .join("claude")
            .join("orchestrator")
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

    /// A shipped file cloned off the locked bundle before 0.2.37 is immutable; the refresh
    /// must unlock it and replace it, and the new copy must not carry the lock.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_locked_stale_copy_is_refreshed_and_unlocked() {
        use std::os::macos::fs::MetadataExt;
        let tmp = scratch("locked");
        let (src, dest) = (tmp.join("src"), tmp.join("dest"));
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dest).unwrap();
        fs::write(src.join("README.md"), "new").unwrap();
        fs::write(dest.join("README.md"), "old").unwrap();
        crate::fs_flags::tests::chflags("uchg", &src.join("README.md"));
        crate::fs_flags::tests::chflags("uchg", &dest.join("README.md"));

        copy_over(&src, &dest).unwrap();

        assert_eq!(fs::read_to_string(dest.join("README.md")).unwrap(), "new");
        let flags = fs::metadata(dest.join("README.md")).unwrap().st_flags();
        assert_eq!(flags, 0, "the copy inherits no flags");
        crate::fs_flags::clear_tree(&tmp);
        let _ = fs::remove_dir_all(&tmp);
    }

    /// The pre-module rule and its import are removed; the user's own text around them stays,
    /// and a second run changes nothing.
    #[test]
    fn the_pre_module_fences_are_dropped_and_the_users_text_kept() {
        let tmp = scratch("fences");
        let dest = tmp.join("dest");
        fs::create_dir_all(&dest).unwrap();
        fs::write(
            dest.join("AGENTS.md"),
            "<!-- avada:module=avada/hyperpane -->\n# Hyperpane\nold rule\n<!-- /avada:module=avada/hyperpane -->\n",
        )
        .unwrap();
        fs::write(
            dest.join("CLAUDE.md"),
            "my notes\n<!-- avada:module=avada/modules -->\n@AGENTS.md\n<!-- /avada:module=avada/modules -->\n",
        )
        .unwrap();

        materialize_into(&dest, None).unwrap();
        assert!(!dest.join("AGENTS.md").exists());
        assert_eq!(
            fs::read_to_string(dest.join("CLAUDE.md")).unwrap(),
            "my notes\n"
        );

        materialize_into(&dest, None).unwrap();
        assert_eq!(
            fs::read_to_string(dest.join("CLAUDE.md")).unwrap(),
            "my notes\n"
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    /// A user's own `AGENTS.md` keeps the import that pulls it in.
    #[test]
    fn a_users_own_agents_md_keeps_its_import() {
        let tmp = scratch("own-agents");
        let dest = tmp.join("dest");
        fs::create_dir_all(&dest).unwrap();
        let claude = "<!-- avada:module=avada/modules -->\n@AGENTS.md\n<!-- /avada:module=avada/modules -->\n";
        fs::write(dest.join("AGENTS.md"), "mine\n").unwrap();
        fs::write(dest.join("CLAUDE.md"), claude).unwrap();

        materialize_into(&dest, None).unwrap();
        assert_eq!(
            fs::read_to_string(dest.join("AGENTS.md")).unwrap(),
            "mine\n"
        );
        assert_eq!(fs::read_to_string(dest.join("CLAUDE.md")).unwrap(), claude);
        let _ = fs::remove_dir_all(&tmp);
    }

    /// `build.rs` (frozen) names these five paths in `rerun-if-changed`; a rename that left
    /// them behind would silently stop re-staging the resources.
    //
    // Reads the shipped resource tree off disk, anchored at the repo root. cargo-mutants
    // tests each mutant in a `.git`-less copy where that path does not resolve, so this is
    // excluded from mutation builds via `RUSTFLAGS='--cfg avada_mutants'` (see rs/.cargo/mutants.toml).
    #[cfg(not(avada_mutants))]
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

    /// The skill unit left with the `bshuler/avada-orchestrator` module. Nothing under the shipped
    /// tree may claim module ownership any more: a stray `skills/` directory here would be
    /// content the host ships and no registry knows about, which is exactly the confusion the
    /// extraction removed.
    //
    // Materializes the shipped resource tree, anchored at the repo root; unavailable in
    // cargo-mutants' `.git`-less tree copy. Excluded from mutation builds via
    // `RUSTFLAGS='--cfg avada_mutants'` (see rs/.cargo/mutants.toml).
    #[cfg(not(avada_mutants))]
    #[test]
    fn the_host_ships_no_module_skills_of_its_own() {
        assert!(
            !shipped().join("skills").exists(),
            "skill units belong to the orchestrator module, not to resources/claude/orchestrator"
        );
    }

    // Materializes the shipped resource tree, anchored at the repo root; that path does not
    // resolve in cargo-mutants' `.git`-less tree copy, so this is excluded from mutation builds
    // via `RUSTFLAGS='--cfg avada_mutants'` (see rs/.cargo/mutants.toml).
    #[cfg(not(avada_mutants))]
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

    /// A module dir shipping one always-on rule, as `bshuler/avada-orchestrator` does.
    fn rule_module(root: &Path, enabled: bool) -> ModuleInput {
        let dir = root.join("module");
        let unit = dir.join("skills").join("orchestrator");
        fs::create_dir_all(&unit).unwrap();
        fs::write(
            unit.join("SKILL.md"),
            "---\nname: orchestrator\ndescription: \"Drive the app with avada ctl.\"\nkind: rule\nactivation: always\n---\n# Orchestrator\n\nUse `avada ctl`.\n",
        )
        .unwrap();
        ModuleInput {
            id: avada_module_sdk::manifest::ModuleId::new("bshuler/avada-orchestrator").unwrap(),
            name: "Orchestrator".into(),
            version: "0.1.0".into(),
            version_dir: dir,
            skills: avada_module_sdk::manifest::SkillsSection {
                paths: vec!["skills".into()],
            },
            accepted: std::collections::BTreeSet::from([
                avada_module_sdk::caps::Capability::SkillsMaterialize,
            ]),
            enabled,
        }
    }

    /// The directory as a long-lived orchestrator folder has it: shipped reference files,
    /// the user's notes, and a pre-module Aider fence.
    fn lived_in(dest: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let avada = dest.join(".claude").join("skills").join("avada");
        fs::create_dir_all(&avada).unwrap();
        fs::write(avada.join("SKILL.md"), "---\nname: avada\n---\nshipped\n").unwrap();
        fs::write(avada.join("REFERENCE.md"), "reference\n").unwrap();
        fs::create_dir_all(dest.join("supervise")).unwrap();
        fs::write(dest.join("supervise").join("notes.md"), "mine\n").unwrap();
        fs::write(dest.join("README.md"), "readme\n").unwrap();
        [
            avada.join("SKILL.md"),
            avada.join("REFERENCE.md"),
            dest.join("supervise").join("notes.md"),
            dest.join("README.md"),
        ]
        .into_iter()
        .map(|p| {
            let b = fs::read(&p).unwrap();
            (p, b)
        })
        .collect()
    }

    fn untouched(files: &[(PathBuf, Vec<u8>)]) {
        for (p, b) in files {
            assert_eq!(&fs::read(p).unwrap(), b, "{} changed", p.display());
        }
    }

    /// B6: an enabled module's always-on rule reaches the folder, fenced, with Claude Code's
    /// import; nothing else there changes, and a second run has nothing to do.
    #[test]
    fn an_enabled_modules_rule_is_written_and_nothing_else_is_touched() {
        let tmp = scratch("b6-on");
        let dest = tmp.join("dest");
        let before = lived_in(&dest);
        let modules = [rule_module(&tmp, true)];
        let tools = Tools::only(["claude-code"]);

        let applied = materialize_modules_into(&dest, &modules, &tools);
        assert!(applied.ok(), "{:?}", applied.failed);
        let agents = fs::read_to_string(dest.join("AGENTS.md")).unwrap();
        assert!(
            agents.contains("<!-- avada:module=bshuler/avada-orchestrator -->"),
            "{agents}"
        );
        assert!(agents.contains("Use `avada ctl`."), "{agents}");
        let claude = fs::read_to_string(dest.join("CLAUDE.md")).unwrap();
        assert!(claude.contains("@AGENTS.md"), "{claude}");
        untouched(&before);

        let again = materialize_modules_into(&dest, &modules, &tools);
        assert!(
            again.written.is_empty() && again.removed.is_empty(),
            "{again:?}"
        );
        // The copy-out's legacy cleanup must not take the import away from the module's rule.
        materialize_into(&dest, None).unwrap();
        assert!(fs::read_to_string(dest.join("CLAUDE.md"))
            .unwrap()
            .contains("@AGENTS.md"));
        let _ = fs::remove_dir_all(&tmp);
    }

    /// B6: disabling the module sweeps its rule and the import, and still nothing else moves.
    /// A pre-module fence in `CONVENTIONS.md` is swept on the way.
    #[test]
    fn a_disabled_modules_rule_is_swept_and_nothing_else_is_touched() {
        let tmp = scratch("b6-off");
        let dest = tmp.join("dest");
        let before = lived_in(&dest);
        let tools = Tools::only(["claude-code"]);
        materialize_modules_into(&dest, &[rule_module(&tmp, true)], &tools);
        fs::write(
            dest.join("CONVENTIONS.md"),
            "<!-- avada:module=avada/hyperpane -->\n# Hyperpane\n<!-- /avada:module=avada/hyperpane -->\n",
        )
        .unwrap();

        let applied = materialize_modules_into(&dest, &[rule_module(&tmp, false)], &tools);
        assert!(applied.ok(), "{:?}", applied.failed);
        assert!(!dest.join("AGENTS.md").exists());
        assert!(!dest.join("CLAUDE.md").exists());
        assert!(!dest.join("CONVENTIONS.md").exists());
        untouched(&before);
        let _ = fs::remove_dir_all(&tmp);
    }
}
