//! The plan: every write and removal the materializer wants, computed without
//! touching the disk, sorted so two runs over the same inputs compare equal, and
//! applied in one place.

use std::fs;
use std::path::{Path, PathBuf};

use super::unit::{UnitError, UnitRef};

/// One file to write with exactly these bytes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct FileWrite {
    /// Absolute path.
    pub path: PathBuf,
    /// Content.
    pub bytes: Vec<u8>,
    /// Set the executable bit (unix) after writing, for copied `scripts/`.
    pub executable: bool,
}

/// One path to remove: a stale generated file, an emptied fenced file, or a
/// whole `<module>-<name>/` directory this materializer owns.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Removal {
    /// Absolute path.
    pub path: PathBuf,
    /// `true` removes a directory tree; `false` a single file.
    pub dir: bool,
}

/// A unit that was deliberately not written for one tool, with the reason a
/// user can read in the module UI.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Skipped {
    /// Which unit.
    pub unit: UnitRef,
    /// Which tool (`agents` for the shared layer).
    pub tool: String,
    /// Why.
    pub reason: String,
}

/// A unit written shorter than its source because the tool caps what it reads.
/// The written text ends with a notice saying so; nothing is dropped silently.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Truncated {
    /// Which unit.
    pub unit: UnitRef,
    /// Which tool.
    pub tool: String,
    /// The source body's size.
    pub bytes: usize,
    /// The tool's cap.
    pub cap: usize,
}

/// What [`super::Materializer::plan`] produces. Inspect it, show it, then hand it
/// to [`apply`]. Everything is sorted; a plan for an already-materialized tree
/// has no writes and no removals, which is what [`Plan::is_empty`] reports.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Files whose content on disk differs from what is wanted (or are missing).
    pub writes: Vec<FileWrite>,
    /// Paths that exist and should not.
    pub removals: Vec<Removal>,
    /// Units left out on purpose.
    pub skipped: Vec<Skipped>,
    /// Units that could not be read or failed validation.
    pub errors: Vec<UnitError>,
    /// Units cut to a tool's cap.
    pub truncated: Vec<Truncated>,
}

impl Plan {
    /// Nothing to write and nothing to remove. Skips and errors do not count:
    /// they describe the inputs, not the disk.
    pub fn is_empty(&self) -> bool {
        self.writes.is_empty() && self.removals.is_empty()
    }

    /// Sort and dedupe every list.
    pub(crate) fn normalize(&mut self) {
        self.writes.sort();
        self.writes.dedup();
        self.removals.sort();
        self.removals.dedup();
        self.skipped.sort();
        self.skipped.dedup();
        self.errors.sort();
        self.errors.dedup();
        self.truncated.sort();
        self.truncated.dedup();
    }

    /// Paths the plan writes, for tests and display.
    pub fn written_paths(&self) -> Vec<&Path> {
        self.writes.iter().map(|w| w.path.as_path()).collect()
    }

    /// Paths the plan removes, for tests and display.
    pub fn removed_paths(&self) -> Vec<&Path> {
        self.removals.iter().map(|r| r.path.as_path()).collect()
    }
}

/// What [`apply`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    /// Files written.
    pub written: Vec<PathBuf>,
    /// Paths removed.
    pub removed: Vec<PathBuf>,
    /// Paths that failed, with the OS error. The rest of the plan is still applied.
    pub failed: Vec<(PathBuf, String)>,
}

impl Applied {
    /// No failures.
    pub fn ok(&self) -> bool {
        self.failed.is_empty()
    }
}

/// Carry a plan out: removals first, then writes, creating parent directories.
/// A failure on one path is recorded and the rest proceeds.
#[tracing::instrument(level = "debug", skip(plan), fields(writes = plan.writes.len(), removals = plan.removals.len()))]
pub fn apply(plan: &Plan) -> Applied {
    let mut out = Applied::default();
    for r in &plan.removals {
        let res = if r.dir {
            fs::remove_dir_all(&r.path)
        } else {
            fs::remove_file(&r.path)
        };
        match res {
            Ok(()) => out.removed.push(r.path.clone()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => out.removed.push(r.path.clone()),
            Err(e) => {
                tracing::warn!(path = %r.path.display(), error = %e, "skills: removal failed");
                out.failed.push((r.path.clone(), e.to_string()));
            }
        }
    }
    for w in &plan.writes {
        match write_one(w) {
            Ok(()) => out.written.push(w.path.clone()),
            Err(e) => {
                tracing::warn!(path = %w.path.display(), error = %e, "skills: write failed");
                out.failed.push((w.path.clone(), e.to_string()));
            }
        }
    }
    out
}

fn write_one(w: &FileWrite) -> std::io::Result<()> {
    if let Some(parent) = w.path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&w.path, &w.bytes)?;
    #[cfg(unix)]
    if w.executable {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&w.path, fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}
