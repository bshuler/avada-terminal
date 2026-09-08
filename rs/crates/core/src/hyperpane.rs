//! The always-on **Hyperpane** tab's working directory.
//!
//! The tab runs the user's coding CLI in [`paths::hyperpane_dir`], and everything that agent
//! knows about driving this app — the skill, its reference, the README a human reads — is a
//! file in that directory. Those files ship with the binary under
//! `resources/claude/hyperpane/`; this module copies them out to the durable location on every
//! start, so an upgraded app teaches its agent the new verbs without the user doing anything.
//!
//! The copy is one-directional and additive: files the app ships are overwritten, and nothing
//! else in the directory is ever touched. That split is the whole contract — the agent keeps
//! notes there and the user drops files in, and neither can be clobbered by an upgrade, while
//! a locally-edited `SKILL.md` is app-owned and *will* be replaced.
//!
//! The shipped `skills/` subtree is not copied as-is: it is the built-in `avada/hyperpane`
//! module's skill source, and [`crate::skills::Materializer`] turns it into the fenced
//! `AGENTS.md` / `CLAUDE.md` sections and `.agents/skills/` directories every agent tool reads.
//! [`materialize`] is the shim the app calls; [`materialize_into`] is the same thing with every
//! input injectable so tests never touch the real home or the real machine's tools.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use avada_module_sdk::caps::Capability;
use avada_module_sdk::descriptor::SchemaDocument;
use avada_module_sdk::manifest::{ModuleId, SkillsSection};

use crate::persistence::paths;
use crate::skills::{Materializer, ModuleInput, Request, Tools};

/// Module id of the shipped Hyperpane skill. Built in, always accepted.
pub const MODULE_ID: &str = "avada/hyperpane";

/// The directory under the shipped tree that holds the module's skill units.
const SKILLS_SUBDIR: &str = "skills";

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
///
/// Claude Code is always written for here regardless of detection: the tab exists to run it.
#[tracing::instrument(level = "debug", ret)]
pub fn materialize() -> io::Result<PathBuf> {
    let tools = Tools::detect_here().with("claude-code");
    materialize_into(
        &paths::hyperpane_dir(),
        source_dir().as_deref(),
        &tools,
        None,
    )
}

/// [`materialize`] with every input explicit: copy the shipped tree at `src` (if any) onto
/// `dest`, then materialize the built-in module's skills into `dest` as a project root for
/// `tools`. `schema`, when given, also emits the `avada-modules` index skill.
#[tracing::instrument(level = "debug", skip(schema), ret)]
pub fn materialize_into(
    dest: &Path,
    src: Option<&Path>,
    tools: &Tools,
    schema: Option<&SchemaDocument>,
) -> io::Result<PathBuf> {
    fs::create_dir_all(dest)?;
    let Some(src) = src else {
        return Ok(dest.to_path_buf());
    };
    copy_over(src, dest)?;
    let module = ModuleInput {
        id: ModuleId::new(MODULE_ID).map_err(|e| io::Error::other(e.to_string()))?,
        name: "Hyperpane".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        version_dir: src.to_path_buf(),
        skills: SkillsSection {
            paths: vec![SKILLS_SUBDIR.into()],
        },
        accepted: BTreeSet::from([Capability::SkillsMaterialize]),
        enabled: true,
    };
    let roots = [dest.to_path_buf()];
    let mat = Materializer::new();
    let plan = mat.plan(&Request {
        modules: std::slice::from_ref(&module),
        roots: &roots,
        home: None,
        tools,
        schema,
        workspace: Some("hyperpane"),
    });
    for e in &plan.errors {
        tracing::warn!(path = %e.path.display(), error = %e.error, "hyperpane skill unit rejected");
    }
    let applied = mat.apply(&plan);
    for (path, err) in &applied.failed {
        tracing::warn!(path = %path.display(), error = %err, "hyperpane skill write failed");
    }
    Ok(dest.to_path_buf())
}

/// Recursively copy `src` onto `dest`, overwriting collisions and leaving everything else in
/// `dest` alone. Errors on individual entries are skipped rather than aborting the walk: a
/// single unreadable file should not cost the agent its whole skill set.
///
/// The top-level `skills/` directory is the materializer's source, not something the agent
/// reads, so it stays behind.
#[tracing::instrument(level = "debug", ret)]
fn copy_over(src: &Path, dest: &Path) -> io::Result<()> {
    copy_tree(src, dest, true)
}

fn copy_tree(src: &Path, dest: &Path, top: bool) -> io::Result<()> {
    for entry in fs::read_dir(src)? {
        let Ok(entry) = entry else { continue };
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if from.is_dir() {
            if top && entry.file_name() == SKILLS_SUBDIR {
                continue;
            }
            if fs::create_dir_all(&to).is_ok() {
                let _ = copy_tree(&from, &to, false);
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
    use avada_module_sdk::skills::{Activation, Skill, SkillKind};

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

    #[test]
    fn shipped_skill_unit_is_an_always_on_rule() {
        let path = shipped().join("skills").join("hyperpane").join("SKILL.md");
        let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let s = Skill::parse(&text).unwrap();
        assert_eq!(s.frontmatter.name, "hyperpane");
        assert_eq!(s.frontmatter.kind, SkillKind::Rule);
        assert_eq!(s.frontmatter.activation, Activation::Always);
        assert!(s.body.contains("avada ctl"));
        // build.rs (frozen) lists these five paths in rerun-if-changed; they must keep existing.
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

    #[test]
    fn materialize_into_copies_the_tree_and_fences_the_rule() {
        let tmp = scratch("into");
        let dest = tmp.join("dest");
        let tools = Tools::only(["claude-code"]);
        let out = materialize_into(&dest, Some(&shipped()), &tools, None).unwrap();
        assert_eq!(out, dest);

        // What it did before: the legacy tree copied, `skills/` source left behind.
        assert!(dest.join("README.md").is_file());
        assert!(dest
            .join(".claude")
            .join("skills")
            .join("avada")
            .join("SKILL.md")
            .is_file());
        assert!(!dest.join("skills").exists());

        // What it does now: the fenced rule in AGENTS.md and the import in CLAUDE.md.
        let agents = fs::read_to_string(dest.join("AGENTS.md")).unwrap();
        assert!(agents.starts_with("<!-- avada:module=avada/hyperpane -->\n"));
        assert!(agents.contains("avada ctl"));
        let claude = fs::read_to_string(dest.join("CLAUDE.md")).unwrap();
        assert!(claude.contains("<!-- avada:module=avada/modules -->\n@AGENTS.md\n"));

        // The agent's own notes survive a second run, which changes nothing.
        fs::write(dest.join("notes.md"), "mine").unwrap();
        let before_agents = agents.clone();
        materialize_into(&dest, Some(&shipped()), &tools, None).unwrap();
        assert_eq!(fs::read_to_string(dest.join("notes.md")).unwrap(), "mine");
        assert_eq!(
            fs::read_to_string(dest.join("AGENTS.md")).unwrap(),
            before_agents
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn materialize_into_without_a_source_still_makes_the_dir() {
        let tmp = scratch("nosrc");
        let dest = tmp.join("dest");
        materialize_into(&dest, None, &Tools::none(), None).unwrap();
        assert!(dest.is_dir());
        assert!(!dest.join("AGENTS.md").exists());
        let _ = fs::remove_dir_all(&tmp);
    }
}
