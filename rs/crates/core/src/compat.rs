//! Compatibility with the product's previous name.
//!
//! Avada Terminal shipped as "hyperpanes" through 0.0.36. Everything that carried that
//! name outward is still on users' disks and in their scripts: the app-support
//! directory, the `HYPERPANES_*` variables the app injects into its panes (and that
//! agent hooks and MCP servers read back), the `.hyperpanes/` project directory, the
//! `.hyperpanes` workspace-file suffix, the `hyperpanes-set` format tag.
//!
//! This module is the one place that knows the old spellings. The policy
//! (docs/modules-fanout-plan.md, Wave 0.5 track R4) is:
//!
//! * **read both, write the new** — a legacy env var, project dir, suffix or format
//!   tag is accepted wherever the new one is; the app only ever writes the new one;
//! * **mirror the env for two releases** — every `AVADA_*` variable injected into a
//!   pane gets a `HYPERPANES_*` twin, so a user's existing hook script keeps working
//!   until they have had two release notes telling them to rename it;
//! * **copy the data directory once** — on first launch the old app-support directory
//!   is copied (never moved: the old install may still be running against it) into
//!   the new one, and a marker file makes the copy a one-time event.
//!
//! Delete this module, and every `compat::` call site, when the mirror period ends.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

/// The app-support directory name (and process name) the product had before the rename.
pub const LEGACY_PRODUCT_NAME: &str = "hyperpanes";
/// Prefix of every environment variable the product owns today.
pub const ENV_PREFIX: &str = "AVADA_";
/// Prefix those same variables had before the rename.
pub const LEGACY_ENV_PREFIX: &str = "HYPERPANES_";
/// The repo-local project directory before the rename (`workspace::project::PROJECT_DIR`).
pub const LEGACY_PROJECT_DIR: &str = ".hyperpanes";
/// The workspace-set `format` tag before the rename (`workspace::sets::SET_FORMAT`).
pub const LEGACY_SET_FORMAT: &str = "hyperpanes-set";
/// The positional workspace-file suffix before the rename (`avada ./dev.hyperpanes`).
pub const LEGACY_WORKSPACE_EXT: &str = ".hyperpanes";
/// Written into the new data directory once the old one has been copied in.
pub const MIGRATION_MARKER: &str = ".migrated-from-hyperpanes";

/// Entries of the old data directory that must not be carried over: live logs the old
/// install is still appending to, the old control plane's discovery files (the new
/// process writes its own), SQLite side files (`work.db` is copied without them and
/// opens consistently), and editor/OS droppings.
const NEVER_COPY: &[&str] = &["logs", "control.json", "control-pane-ids.json", ".DS_Store"];
const NEVER_COPY_SUFFIX: &[&str] = &["-shm", "-wal", ".bak"];

/// `AVADA_FOO` → `HYPERPANES_FOO`; `None` for a name outside the product's prefix.
pub fn legacy_env_name(name: &str) -> Option<String> {
    name.strip_prefix(ENV_PREFIX)
        .map(|rest| format!("{LEGACY_ENV_PREFIX}{rest}"))
}

/// Pure core of [`env_var`]: the new name wins, the legacy name is the fallback, and a
/// name outside the prefix is looked up as-is.
pub fn lookup<T>(name: &str, get: impl Fn(&str) -> Option<T>) -> Option<T> {
    get(name).or_else(|| legacy_env_name(name).and_then(|legacy| get(&legacy)))
}

/// `std::env::var(name)`, falling back to the pre-rename spelling. Use this for every
/// variable a user, a hook or an old pane might have set.
pub fn env_var(name: &str) -> Option<String> {
    lookup(name, |n| std::env::var(n).ok())
}

/// `std::env::var_os(name)`, falling back to the pre-rename spelling.
pub fn env_var_os(name: &str) -> Option<OsString> {
    lookup(name, |n| std::env::var_os(n))
}

/// `env_var` read as a flag: `1`, `true` or `yes`.
pub fn env_truthy(name: &str) -> bool {
    matches!(
        env_var(name).as_deref(),
        Some("1") | Some("true") | Some("yes")
    )
}

/// Give every `AVADA_*` entry a `HYPERPANES_*` twin with the same value. An existing
/// legacy entry is left alone: a caller that set it on purpose is saying something,
/// and "the new name wins" only applies to *reads*. Returns how many twins were added.
pub fn mirror_legacy_env(env: &mut HashMap<String, String>) -> usize {
    let twins: Vec<(String, String)> = env
        .iter()
        .filter_map(|(k, v)| legacy_env_name(k).map(|legacy| (legacy, v.clone())))
        .filter(|(legacy, _)| !env.contains_key(legacy))
        .collect();
    let n = twins.len();
    env.extend(twins);
    n
}

/// What one data-directory migration did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    /// The old directory that was read.
    pub from: PathBuf,
    /// The new directory that was filled.
    pub to: PathBuf,
    /// Files copied.
    pub copied: usize,
    /// Files left alone because the new directory already had them.
    pub kept: usize,
}

/// Copy `from` into `to` once. `Ok(None)` when there is nothing to do: no old
/// directory, the two are the same directory, or the marker says it already happened.
///
/// Existing files under `to` win — a user who launched the renamed app before this
/// code shipped keeps what that launch wrote, and only the gaps are filled. The copy
/// is a copy, not a rename, because the old install may still be running against
/// `from` (this machine is the proof). The marker is written last, so an interrupted
/// copy is retried on the next launch and finishes filling the gaps.
pub fn migrate_dir(from: &Path, to: &Path) -> io::Result<Option<MigrationReport>> {
    if !from.is_dir() || from == to || to.join(MIGRATION_MARKER).exists() {
        return Ok(None);
    }
    std::fs::create_dir_all(to)?;
    let mut report = MigrationReport {
        from: from.to_path_buf(),
        to: to.to_path_buf(),
        copied: 0,
        kept: 0,
    };
    copy_tree(from, to, &mut report)?;
    let note = format!(
        "from={}\ncopied={}\nkept={}\n",
        from.display(),
        report.copied,
        report.kept
    );
    std::fs::write(to.join(MIGRATION_MARKER), note)?;
    Ok(Some(report))
}

fn should_skip(name: &str) -> bool {
    NEVER_COPY.contains(&name)
        || name == MIGRATION_MARKER
        || NEVER_COPY_SUFFIX.iter().any(|s| name.ends_with(s))
}

fn copy_tree(from: &Path, to: &Path, report: &mut MigrationReport) -> io::Result<()> {
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if should_skip(name_str) {
            continue;
        }
        let src = entry.path();
        let dst = to.join(&name);
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            std::fs::create_dir_all(&dst)?;
            copy_tree(&src, &dst, report)?;
        } else if dst.exists() {
            report.kept += 1;
        } else {
            std::fs::copy(&src, &dst)?;
            report.copied += 1;
        }
    }
    Ok(())
}

/// What [`migrate_user_data`] did across every directory pair, kept as data so the
/// caller can log it *after* logging is up (the copy has to run before the first
/// settings read, which is before the log level is known).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MigrationOutcome {
    pub reports: Vec<MigrationReport>,
    /// `(from, to, error)` for each pair that failed; the app starts anyway.
    pub errors: Vec<(PathBuf, PathBuf, String)>,
}

impl MigrationOutcome {
    /// Emit one tracing line per report and per error.
    pub fn log(&self) {
        for r in &self.reports {
            tracing::info!(
                from = %r.from.display(),
                to = %r.to.display(),
                copied = r.copied,
                kept = r.kept,
                "copied the pre-rename data directory"
            );
        }
        for (from, to, e) in &self.errors {
            tracing::warn!(
                from = %from.display(),
                to = %to.display(),
                error = %e,
                "could not copy the pre-rename data directory; starting without it"
            );
        }
    }
}

/// Run [`migrate_dir`] for every (old, new) app-support pair the platform has. Call
/// once at process entry, after `--version`/`--help` have had their chance to exit
/// (the first launch after the rename may copy a few hundred megabytes — the dictation
/// model lives there — and that must not sit in front of `--version`) and before the
/// first settings read, so the copied settings are the ones that launch sees.
pub fn migrate_user_data() -> MigrationOutcome {
    let mut out = MigrationOutcome::default();
    for (from, to) in crate::persistence::paths::legacy_dir_pairs() {
        match migrate_dir(&from, &to) {
            Ok(Some(report)) => out.reports.push(report),
            Ok(None) => {}
            Err(e) => out.errors.push((from, to, e.to_string())),
        }
    }
    out
}

/// The path a legacy project directory would have beside the new one.
pub fn legacy_project_dir(root: &Path) -> PathBuf {
    root.join(LEGACY_PROJECT_DIR)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("avada-compat-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn legacy_env_name_swaps_the_prefix_and_nothing_else() {
        assert_eq!(
            legacy_env_name("AVADA_CONTROL_FILE").as_deref(),
            Some("HYPERPANES_CONTROL_FILE")
        );
        assert_eq!(legacy_env_name("AVADA_").as_deref(), Some("HYPERPANES_"));
        assert_eq!(legacy_env_name("PATH"), None);
        assert_eq!(legacy_env_name("XAVADA_FOO"), None);
    }

    #[test]
    fn lookup_prefers_the_new_name_and_falls_back_to_the_old() {
        let both = |n: &str| match n {
            "AVADA_PANE_ID" => Some("new"),
            "HYPERPANES_PANE_ID" => Some("old"),
            _ => None,
        };
        assert_eq!(lookup("AVADA_PANE_ID", both), Some("new"));
        let old_only = |n: &str| (n == "HYPERPANES_PANE_ID").then_some("old");
        assert_eq!(lookup("AVADA_PANE_ID", old_only), Some("old"));
        assert_eq!(lookup("AVADA_PANE_ID", |_| None::<&str>), None);
        // A name outside the prefix is looked up once, as-is.
        let calls = std::cell::RefCell::new(Vec::new());
        let _ = lookup("HOME", |n| {
            calls.borrow_mut().push(n.to_string());
            None::<&str>
        });
        assert_eq!(calls.into_inner(), ["HOME"]);
    }

    #[test]
    fn env_var_reads_the_legacy_spelling_from_the_process() {
        // Unique names so parallel tests cannot collide.
        let new = "AVADA_COMPAT_TEST_READ";
        let old = "HYPERPANES_COMPAT_TEST_READ";
        std::env::remove_var(new);
        std::env::set_var(old, "legacy");
        assert_eq!(env_var(new).as_deref(), Some("legacy"));
        assert_eq!(
            env_var_os(new).as_deref(),
            Some(OsString::from("legacy").as_os_str())
        );
        assert!(!env_truthy(new));
        std::env::set_var(new, "1");
        assert!(env_truthy(new));
        std::env::remove_var(new);
        std::env::remove_var(old);
    }

    #[test]
    fn mirror_adds_twins_without_clobbering_a_deliberate_legacy_value() {
        let mut env: HashMap<String, String> = [
            ("AVADA_PANE_ID", "pane-1"),
            ("AVADA_CONTROL_FILE", "/x/control.json"),
            ("AVADA_CONTROL_TOKEN", "new-tok"),
            ("HYPERPANES_CONTROL_TOKEN", "old-tok"),
            ("PATH", "/bin"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert_eq!(mirror_legacy_env(&mut env), 2);
        assert_eq!(env["HYPERPANES_PANE_ID"], "pane-1");
        assert_eq!(env["HYPERPANES_CONTROL_FILE"], "/x/control.json");
        assert_eq!(env["HYPERPANES_CONTROL_TOKEN"], "old-tok");
        assert_eq!(env.len(), 7);
        // Idempotent.
        assert_eq!(mirror_legacy_env(&mut env), 0);
    }

    #[test]
    fn migrate_copies_the_tree_once_and_skips_live_state() {
        let base = tmp("migrate");
        let from = base.join("hyperpanes");
        let to = base.join("avada");
        std::fs::create_dir_all(from.join("workspaces")).unwrap();
        std::fs::create_dir_all(from.join("logs")).unwrap();
        std::fs::write(from.join("projects.json"), "[1]").unwrap();
        std::fs::write(from.join("workspaces/a.json"), "{}").unwrap();
        std::fs::write(from.join("work.db"), "db").unwrap();
        std::fs::write(from.join("work.db-wal"), "wal").unwrap();
        std::fs::write(from.join("control.json"), "old-token").unwrap();
        std::fs::write(from.join("logs/app.log"), "log").unwrap();
        std::fs::write(from.join("last-workspace.json.pre-move.1.bak"), "x").unwrap();
        // The renamed app already ran once and wrote its own settings.
        std::fs::create_dir_all(&to).unwrap();
        std::fs::write(to.join("projects.json"), "[2]").unwrap();

        let report = migrate_dir(&from, &to)
            .unwrap()
            .expect("first run migrates");
        assert_eq!(report.copied, 2, "{report:?}"); // workspaces/a.json, work.db
        assert_eq!(report.kept, 1); // projects.json
        assert_eq!(
            std::fs::read_to_string(to.join("projects.json")).unwrap(),
            "[2]"
        );
        assert_eq!(
            std::fs::read_to_string(to.join("workspaces/a.json")).unwrap(),
            "{}"
        );
        assert!(to.join("work.db").exists());
        assert!(!to.join("work.db-wal").exists());
        assert!(!to.join("control.json").exists());
        assert!(!to.join("logs").exists());
        assert!(!to.join("last-workspace.json.pre-move.1.bak").exists());
        let marker = std::fs::read_to_string(to.join(MIGRATION_MARKER)).unwrap();
        assert!(marker.contains("copied=2"), "{marker}");

        // Second launch: the marker short-circuits, even though the old dir grew.
        std::fs::write(from.join("workspaces/b.json"), "{}").unwrap();
        assert_eq!(migrate_dir(&from, &to).unwrap(), None);
        assert!(!to.join("workspaces/b.json").exists());
        // The old directory is untouched.
        assert!(from.join("control.json").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn migrate_is_a_no_op_without_an_old_directory_or_onto_itself() {
        let base = tmp("noop");
        let to = base.join("avada");
        assert_eq!(migrate_dir(&base.join("hyperpanes"), &to).unwrap(), None);
        assert!(
            !to.exists(),
            "nothing to migrate must not create the target"
        );
        std::fs::create_dir_all(&to).unwrap();
        assert_eq!(migrate_dir(&to, &to).unwrap(), None);
        assert!(!to.join(MIGRATION_MARKER).exists());
        let _ = std::fs::remove_dir_all(&base);
    }
}
