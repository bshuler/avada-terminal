//! The saved-workspace **library** and the **sets** drawer, as data.
//!
//! Both are directories of small JSON files under [`paths::data_dir`] —
//! [`paths::workspaces_dir`] and [`paths::sets_dir`] — and both are listed newest-first,
//! because they are recency drawers: the thing you just saved belongs at the top.
//!
//! This lives in core rather than beside the left panel that draws it for one reason: a
//! module cannot see either directory. `fs.read` is scoped to the *workspace root*, and
//! these files deliberately live outside it, so the panel's LIBRARY and SETS sections were
//! unreachable over the module contract until `host.workspace.list` could answer them from
//! here. See `crate::module::rpc`'s workspace arms.
//!
//! What is returned is **facts, not display strings**. The built-in panel renders
//! "3 panes · 2 tabs · 5m ago" from these counts; a module renders whatever it likes in
//! whatever locale it likes. Shipping the formatted line over the wire would have made the
//! host the arbiter of a module's own presentation.

use super::{io, sets};
use crate::persistence::paths;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// One saved workspace in the library.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryItem {
    /// The file on disk. Opening the row reads this back.
    pub path: PathBuf,
    /// The workspace's own `name` if it has one, else the file stem.
    pub name: String,
    /// How many panes the workspace's first window describes.
    pub panes: usize,
    /// How many tabs (pane groups) that window describes.
    pub tabs: usize,
    /// Last-modified, epoch ms; `0` when the filesystem would not say.
    pub modified_ms: u64,
}

/// One saved set in the sets drawer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetItem {
    /// The file on disk.
    pub path: PathBuf,
    /// The set's own name if it has one, else the file stem.
    pub name: String,
    /// How many member workspaces the set indexes.
    ///
    /// The set's OWN member list, not a count of members that still resolve: a set is a
    /// loose index of references, and a stale reference must not change the number the
    /// user saved.
    pub members: usize,
    /// Last-modified, epoch ms; `0` when the filesystem would not say.
    pub modified_ms: u64,
}

/// Last-modified in epoch ms, or `0` for anything the filesystem will not answer.
///
/// Zero rather than an `Option` because every caller treats "unknown" as "sorts last and
/// shows no age", which is exactly what 0 does — and it keeps the wire shape a number.
fn modified_ms(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Scan `dir` for `*.avada` / `*.json` workspaces, newest first.
///
/// Unreadable or malformed files are skipped rather than listed as broken rows: one corrupt
/// file must not cost the user the rest of the drawer.
#[tracing::instrument(level = "debug", ret)]
pub fn scan_library(dir: &Path) -> Vec<LibraryItem> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut rows: Vec<LibraryItem> = Vec::new();
    for ent in rd.flatten() {
        let path = ent.path();
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if ext != "avada" && ext != "json" {
            continue;
        }
        let Some(file) = io::read_workspace(&path) else {
            continue;
        };
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("workspace")
            .to_string();
        let name = match &file.name {
            Some(n) if !n.trim().is_empty() => n.trim().to_string(),
            _ => stem,
        };
        // The first window is the one the panel summarises; a workspace that describes
        // several is still opened whole, but its row speaks for the window you land in.
        let groups = io::windows_of(Some(&file))
            .into_iter()
            .next()
            .map(|w| w.groups)
            .unwrap_or_default();
        rows.push(LibraryItem {
            name,
            tabs: groups.len(),
            panes: groups.iter().map(|g| g.panes.len()).sum(),
            modified_ms: modified_ms(&path),
            path,
        });
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.modified_ms));
    rows
}

/// Scan `dir` for readable sets, newest first.
///
/// The ordering differs from [`sets::list_sets_in`] on purpose: that returns file-name
/// order, a stable index for programmatic use, while this is the drawer a person reads.
#[tracing::instrument(level = "debug", ret)]
pub fn scan_sets(dir: &Path) -> Vec<SetItem> {
    let mut rows: Vec<SetItem> = sets::list_sets_in(dir)
        .into_iter()
        .map(|(path, set)| {
            let name = if set.name.trim().is_empty() {
                path.file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "set".to_string())
            } else {
                set.name.clone()
            };
            SetItem {
                name,
                members: set.members.len(),
                modified_ms: modified_ms(&path),
                path,
            }
        })
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.modified_ms));
    rows
}

/// The library in its canonical home, [`paths::workspaces_dir`].
#[tracing::instrument(level = "debug", ret)]
pub fn library() -> Vec<LibraryItem> {
    scan_library(&paths::workspaces_dir())
}

/// The sets drawer in its canonical home, [`paths::sets_dir`].
#[tracing::instrument(level = "debug", ret)]
pub fn all_sets() -> Vec<SetItem> {
    scan_sets(&paths::sets_dir())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A scratch directory that removes itself. No `tempfile` in this workspace's
    /// dependency set, and the pid+counter pair is what keeps two concurrent test binaries
    /// off each other's directory.
    struct Dir(PathBuf);
    impl Dir {
        fn new(tag: &str) -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!(
                "avada-lib-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).expect("scratch dir");
            Dir(p)
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A workspace file with `name` and one window of `tabs` tabs holding `panes` panes each.
    fn workspace_json(name: Option<&str>, tabs: &[usize]) -> String {
        let groups: Vec<String> = tabs
            .iter()
            .map(|n| {
                let panes: Vec<String> = (0..*n)
                    .map(|i| format!(r#"{{"cwd":"/w","title":"p{i}"}}"#))
                    .collect();
                format!(r#"{{"panes":[{}]}}"#, panes.join(","))
            })
            .collect();
        let name = name
            .map(|n| format!(r#""name":"{n}","#))
            .unwrap_or_default();
        format!(
            r#"{{{name}"version":1,"windows":[{{"groups":[{}]}}]}}"#,
            groups.join(",")
        )
    }

    #[test]
    fn the_library_counts_panes_and_tabs_and_prefers_the_workspaces_own_name() {
        let d = Dir::new("named");
        std::fs::write(
            d.0.join("api.avada"),
            workspace_json(Some("API work"), &[2, 1]),
        )
        .unwrap();
        let rows = scan_library(&d.0);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].name, "API work",
            "the file's own name beats the stem"
        );
        assert_eq!(rows[0].tabs, 2);
        assert_eq!(rows[0].panes, 3);
    }

    #[test]
    fn a_nameless_workspace_falls_back_to_the_file_stem() {
        let d = Dir::new("stem");
        std::fs::write(d.0.join("scratch.avada"), workspace_json(None, &[1])).unwrap();
        let rows = scan_library(&d.0);
        assert_eq!(rows[0].name, "scratch");
        assert_eq!((rows[0].tabs, rows[0].panes), (1, 1));
    }

    /// A blank-named workspace is the same case as a missing one — the panel must never
    /// draw an empty row — and a whitespace-only name is trimmed to nothing, not kept.
    #[test]
    fn a_blank_name_is_treated_as_no_name_at_all() {
        let d = Dir::new("blank");
        std::fs::write(d.0.join("quiet.avada"), workspace_json(Some("   "), &[1])).unwrap();
        assert_eq!(scan_library(&d.0)[0].name, "quiet");
    }

    #[test]
    fn corrupt_and_foreign_files_are_skipped_rather_than_listed_broken() {
        let d = Dir::new("corrupt");
        std::fs::write(d.0.join("good.avada"), workspace_json(Some("good"), &[1])).unwrap();
        std::fs::write(d.0.join("bad.avada"), "{not json").unwrap();
        std::fs::write(d.0.join("notes.txt"), workspace_json(Some("x"), &[1])).unwrap();
        let rows = scan_library(&d.0);
        assert_eq!(rows.len(), 1, "only the readable workspace: {rows:?}");
        assert_eq!(rows[0].name, "good");
    }

    /// Newest-first is the contract both the panel and a module sort by, and it is the one
    /// thing a plain `read_dir` will not give you — the order there is the filesystem's.
    #[test]
    fn the_library_is_newest_first() {
        let d = Dir::new("order");
        for n in ["old", "mid", "new"] {
            std::fs::write(
                d.0.join(format!("{n}.avada")),
                workspace_json(Some(n), &[1]),
            )
            .unwrap();
        }
        let rows = scan_library(&d.0);
        assert_eq!(rows.len(), 3);
        for w in rows.windows(2) {
            assert!(
                w[0].modified_ms >= w[1].modified_ms,
                "not newest-first: {rows:?}"
            );
        }
    }

    #[test]
    fn a_missing_directory_is_an_empty_drawer_not_an_error() {
        let d = Dir::new("missing");
        let gone = d.0.join("nope");
        assert!(scan_library(&gone).is_empty());
        assert!(scan_sets(&gone).is_empty());
    }

    #[test]
    fn a_set_counts_its_own_members_even_when_they_no_longer_resolve() {
        let d = Dir::new("sets");
        std::fs::write(
            d.0.join("morning.json"),
            r#"{"version":1,"name":"Morning","members":[
                 {"name":"a","path":"/gone/a.avada"},
                 {"name":"b","path":"/gone/b.avada"}]}"#,
        )
        .unwrap();
        let rows = scan_sets(&d.0);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "Morning");
        assert_eq!(
            rows[0].members, 2,
            "the saved index, not a count of what still exists"
        );
    }
}
