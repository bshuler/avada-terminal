//! The tree model: an IDE-style project explorer plus a fuzzy file finder.
//!
//! Two views over one root, because that is how every editor a developer already knows
//! behaves: a tree you expand a directory at a time, and — the moment you type — a flat
//! ranked list of everything under the root whose path matches what you typed.
//!
//! ## The disk is behind a trait
//!
//! A module never calls `std::fs`. Every listing goes through [`Fs`], which the running
//! module implements over `host.fs.list` — so the host's workspace scoping, not the
//! module's good intentions, is what keeps a read inside the project. The same trait is
//! what lets every rule below be tested against an in-memory tree with no temp
//! directories and no I/O at all.
//!
//! ## The tree shows everything; the finder does not
//!
//! An explorer that hid `node_modules` would be lying about what is on disk, so the tree
//! lists every entry. The finder walks the same tree with a skip-list and a hard budget,
//! because indexing a 200k-file dependency directory to answer "where is main.rs" costs a
//! second and finds nothing anyone wanted.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// A tree row that is a directory.
pub const KIND_DIR: i32 = 0;
/// A tree row that is a regular file.
pub const KIND_FILE: i32 = 1;
/// A row that is not a filesystem entry at all — an empty-directory note, a truncation
/// notice, "no matches". Inert: it has no path and cannot be activated.
pub const KIND_NOTE: i32 = 2;

/// Most rows the flattened tree will hand to the host. A human who expands the whole of a
/// large monorepo gets a truncation note rather than a 200k-row list.
pub const MAX_ROWS: usize = 5_000;

/// Most entries listed from any one directory.
pub const MAX_ENTRIES: usize = 2_000;

/// Most results the finder returns. Past this, ranking stops mattering — nobody scrolls to
/// the 300th fuzzy match; they type another character.
pub const MAX_RESULTS: usize = 200;

/// Most filesystem entries the finder will look at for one query, across the whole walk.
pub const MAX_SCAN: usize = 40_000;

/// How deep the finder walks below the root. The tree itself has no depth limit — it only
/// ever lists what a human explicitly expanded.
pub const MAX_FIND_DEPTH: usize = 12;

/// Directories the *finder* walks past. Not hidden from the tree: an explorer that hid a
/// directory would be lying about what is on disk. These are simply never worth indexing —
/// they hold generated or vendored files nobody is searching for by name, and they are
/// where a fuzzy walk's entire budget goes if you let it.
pub const FINDER_SKIP: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    ".venv",
    "venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    ".next",
    ".turbo",
    ".gradle",
    "DerivedData",
];

/// One entry as `host.fs.list` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The entry's own name, not a path.
    pub name: String,
    /// `dir`, `file`, `symlink` or `other`, straight off the wire.
    pub kind: String,
}

impl Entry {
    /// Whether this entry is a directory the tree may descend into.
    ///
    /// Only a literal `dir`: the host reports a symlink as `symlink` whatever it points
    /// at, and following one would let a link out of the workspace pull the whole tree
    /// back in — the same reason the old built-in browser used `DirEntry::file_type`,
    /// which does not follow links either.
    pub fn is_dir(&self) -> bool {
        self.kind == "dir"
    }
}

/// The one filesystem operation this module needs.
///
/// `&mut self` because the real implementation borrows the module's single connection to
/// the host to make the call.
pub trait Fs {
    /// One directory's entries in any order, or a human-readable reason it could not be
    /// read. Implementations do not sort: [`read_children`] owns the ordering.
    fn list(&mut self, dir: &Path) -> Result<Vec<Entry>, String>;

    /// Whether `path` is a directory, answered the only way a module can answer it: by
    /// trying to list it. There is no `host.fs.stat`, and inventing one for this would add
    /// a wire method to serve a question the existing one already answers.
    fn is_dir(&mut self, path: &Path) -> bool {
        self.list(path).is_ok()
    }
}

/// One row of the Files view, in the order it is drawn.
#[derive(Debug, Clone, PartialEq)]
pub struct FileRow {
    /// Indent level. `0` for a child of the root; always `0` in finder results, which are
    /// a flat ranked list rather than a tree.
    pub depth: i32,
    /// [`KIND_DIR`] · [`KIND_FILE`] · [`KIND_NOTE`].
    pub kind: i32,
    /// Whether a directory row is currently expanded (drives the twisty).
    pub expanded: bool,
    /// The entry's own file name — never the whole path, which does not fit in a 260px panel.
    pub label: String,
    /// The dim trailing column: the containing directory, relative to the root, for a finder
    /// result. Empty in the tree, where the indent already says where a row lives.
    pub detail: String,
    /// The absolute path this row acts on, or empty for a [`KIND_NOTE`].
    pub path: PathBuf,
}

impl FileRow {
    /// An inert message row.
    pub fn note(text: impl Into<String>) -> Self {
        FileRow {
            depth: 0,
            kind: KIND_NOTE,
            expanded: false,
            label: text.into(),
            detail: String::new(),
            path: PathBuf::new(),
        }
    }

    /// Whether clicking this row does anything — a [`KIND_NOTE`] ("no matches", "not a
    /// directory") is a message, not a target, and carries no path to act on.
    pub fn activatable(&self) -> bool {
        !self.path.as_os_str().is_empty()
    }
}

/// One directory's entries, directories first then files, each group sorted
/// case-insensitively — the ordering every file explorer uses, and the one a human
/// scanning for a name expects.
///
/// The host already sorts by name, but it sorts *all* entries together and by raw name;
/// the two-group, case-folded order is this view's own and is applied here.
pub fn read_children(fs: &mut impl Fs, dir: &Path) -> Result<Vec<(PathBuf, bool)>, String> {
    let entries = fs.list(dir)?;
    let mut dirs: Vec<(PathBuf, bool)> = Vec::new();
    let mut files: Vec<(PathBuf, bool)> = Vec::new();
    for ent in entries {
        let p = dir.join(&ent.name);
        if ent.is_dir() {
            dirs.push((p, true));
        } else {
            files.push((p, false));
        }
        if dirs.len() + files.len() >= MAX_ENTRIES {
            break;
        }
    }
    let key = |p: &PathBuf| {
        p.file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    };
    dirs.sort_by_key(|(p, _)| key(p));
    files.sort_by_key(|(p, _)| key(p));
    dirs.append(&mut files);
    Ok(dirs)
}

/// The last component of `p`, or the whole path when it has none.
pub fn name_of(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

/// Flatten the tree under `root` into draw order, descending only into directories present
/// in `expanded`. This is the whole tree model: there is no node graph to keep in sync with
/// the disk, because the disk is read at the moment the rows are built.
pub fn flatten(fs: &mut impl Fs, root: &Path, expanded: &BTreeSet<PathBuf>) -> Vec<FileRow> {
    let mut out = Vec::new();
    walk(fs, root, expanded, 0, &mut out);
    if out.is_empty() {
        out.push(FileRow::note("Empty directory"));
    }
    out
}

fn walk(
    fs: &mut impl Fs,
    dir: &Path,
    expanded: &BTreeSet<PathBuf>,
    depth: i32,
    out: &mut Vec<FileRow>,
) {
    if out.len() >= MAX_ROWS {
        return;
    }
    let children = match read_children(fs, dir) {
        Ok(c) => c,
        Err(e) => {
            out.push(FileRow {
                depth,
                ..FileRow::note(format!("Cannot read: {e}"))
            });
            return;
        }
    };
    for (path, is_dir) in children {
        if out.len() >= MAX_ROWS {
            out.push(FileRow::note("… more entries not shown"));
            return;
        }
        let open = is_dir && expanded.contains(&path);
        out.push(FileRow {
            depth,
            kind: if is_dir { KIND_DIR } else { KIND_FILE },
            expanded: open,
            label: name_of(&path),
            detail: String::new(),
            path: path.clone(),
        });
        if open {
            walk(fs, &path, expanded, depth + 1, out);
        }
    }
}

/// Every ancestor directory of `path` strictly below `root`, so revealing a deep file can
/// expand exactly the directories that lead to it and no others. Empty when `path` is not
/// under `root`.
pub fn ancestors_within(root: &Path, path: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut cur = path.parent();
    while let Some(d) = cur {
        if d == root {
            break;
        }
        if !d.starts_with(root) {
            return Vec::new();
        }
        out.push(d.to_path_buf());
        cur = d.parent();
    }
    out
}

/// Fuzzy-match `query` against `cand`, returning a score (higher is better) or `None` when
/// the query's characters do not appear in order.
///
/// The scoring is the small set of rules that make a subsequence match feel like an IDE's:
/// a run of adjacent characters is worth more than the same characters scattered, a
/// character starting a path segment or a word is worth more than one in the middle, and a
/// match inside the file name is worth more than one in the directories above it — typing
/// `stat` should find `state.rs` before `crates/statusline/thing.rs`.
pub fn score(query: &str, cand: &str) -> Option<i32> {
    let q: Vec<char> = query.chars().filter(|c| !c.is_whitespace()).collect();
    if q.is_empty() {
        return Some(0);
    }
    let c: Vec<char> = cand.chars().collect();
    // Everything below the last separator is the file name.
    let name_start = c
        .iter()
        .rposition(|&ch| ch == '/' || ch == '\\')
        .map_or(0, |i| i + 1);
    let mut total = 0i32;
    let mut ci = 0usize;
    let mut run = 0i32;
    for &qc in &q {
        let ql = qc.to_ascii_lowercase();
        let mut hit = None;
        while ci < c.len() {
            if c[ci].to_ascii_lowercase() == ql {
                hit = Some(ci);
                break;
            }
            ci += 1;
            run = 0;
        }
        let at = hit?;
        let mut s = 1;
        run += 1;
        s += run * 3;
        let prev = if at == 0 { None } else { Some(c[at - 1]) };
        let boundary = match prev {
            None => true,
            Some(p) => p == '/' || p == '\\' || p == '-' || p == '_' || p == '.' || p == ' ',
        };
        if boundary {
            s += 8;
        }
        if at >= name_start {
            s += 6;
        }
        total += s;
        ci = at + 1;
    }
    // A shorter candidate that matched the same query is the better answer.
    Some(total - (c.len() as i32) / 8)
}

/// Walk `root` and return the best [`MAX_RESULTS`] matches for `query`, best first.
/// Bounded by [`MAX_SCAN`] entries and [`MAX_FIND_DEPTH`] levels, skipping
/// [`FINDER_SKIP`] — a finder that takes a second to answer is one nobody types into.
///
/// Directories match too: "where is the parser crate" is the same question as "where is
/// `parser.rs`", and an explorer that could only find leaf files would answer half of it.
pub fn find(fs: &mut impl Fs, root: &Path, query: &str) -> Vec<FileRow> {
    let q = query.trim();
    if q.is_empty() {
        return Vec::new();
    }
    let mut scanned = 0usize;
    let mut hits: Vec<(i32, PathBuf, bool)> = Vec::new();
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    let mut truncated = false;
    while let Some((dir, depth)) = stack.pop() {
        let children = match read_children(fs, &dir) {
            Ok(c) => c,
            Err(_) => continue,
        };
        for (path, is_dir) in children {
            scanned += 1;
            if scanned > MAX_SCAN {
                truncated = true;
                stack.clear();
                break;
            }
            let name = name_of(&path);
            if is_dir && FINDER_SKIP.contains(&name.as_str()) {
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if let Some(s) = score(q, &rel) {
                hits.push((s, path.clone(), is_dir));
            }
            if is_dir && depth + 1 < MAX_FIND_DEPTH {
                stack.push((path, depth + 1));
            }
        }
    }
    // Ties break on the path itself, never on directory-read order: the same query must
    // produce the same list twice in a row, or a click races the list under the cursor.
    hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let shown = hits.len().min(MAX_RESULTS);
    let mut out: Vec<FileRow> = hits[..shown]
        .iter()
        .map(|(_, path, is_dir)| {
            let parent = path
                .parent()
                .and_then(|p| p.strip_prefix(root).ok())
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            FileRow {
                depth: 0,
                kind: if *is_dir { KIND_DIR } else { KIND_FILE },
                expanded: false,
                label: name_of(path),
                detail: parent,
                path: path.clone(),
            }
        })
        .collect();
    if out.is_empty() {
        out.push(FileRow::note("No matching files"));
    } else if hits.len() > shown {
        out.push(FileRow::note(format!(
            "… {} more matches — keep typing",
            hits.len() - shown
        )));
    } else if truncated {
        out.push(FileRow::note("… search stopped at the scan limit"));
    }
    out
}

#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::collections::BTreeMap;

    /// An in-memory tree standing in for `host.fs.list`.
    ///
    /// The old `filetree.rs` tests wrote real files into `std::env::temp_dir()` and
    /// removed them afterwards; a module cannot do that (it owns no `std::fs`) and should
    /// not want to — the rules under test are about ordering, depth and scoring, none of
    /// which involve a disk.
    #[derive(Default)]
    pub struct FakeFs {
        /// Directory → the entries it lists.
        pub dirs: BTreeMap<PathBuf, Vec<Entry>>,
        /// Directories that answer with this error instead of a listing.
        pub errors: BTreeMap<PathBuf, String>,
        /// Every `list` call, in order — the tests that care about the *walk* assert on it.
        pub calls: Vec<PathBuf>,
    }

    impl FakeFs {
        /// Build a tree from `path -> is_dir` pairs, creating every parent directory on
        /// the way. Paths are absolute and slash-separated.
        pub fn tree(paths: &[(&str, bool)]) -> Self {
            let mut fs = FakeFs::default();
            for (p, is_dir) in paths {
                fs.add(Path::new(p), *is_dir);
            }
            fs
        }

        /// Record one entry and every directory above it.
        pub fn add(&mut self, path: &Path, is_dir: bool) {
            if is_dir {
                self.dirs.entry(path.to_path_buf()).or_default();
            }
            let Some(parent) = path.parent() else { return };
            let kind = if is_dir { "dir" } else { "file" };
            let name = name_of(path);
            let slot = self.dirs.entry(parent.to_path_buf()).or_default();
            if !slot.iter().any(|e| e.name == name) {
                slot.push(Entry {
                    name,
                    kind: kind.into(),
                });
            }
            if parent.parent().is_some() {
                self.add(parent, true);
            }
        }
    }

    impl Fs for FakeFs {
        fn list(&mut self, dir: &Path) -> Result<Vec<Entry>, String> {
            self.calls.push(dir.to_path_buf());
            if let Some(e) = self.errors.get(dir) {
                return Err(e.clone());
            }
            self.dirs
                .get(dir)
                .cloned()
                .ok_or_else(|| format!("No such file or directory: {}", dir.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeFs;
    use super::*;

    #[test]
    fn a_collapsed_root_lists_only_its_own_children() {
        let mut fs = FakeFs::tree(&[("/r/b.txt", false), ("/r/sub/deep.txt", false)]);
        let rows = flatten(&mut fs, Path::new("/r"), &BTreeSet::new());
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        // Directories first, then files — and `deep.txt` is not there, because `sub` is shut.
        assert_eq!(labels, vec!["sub", "b.txt"]);
        assert_eq!(rows[0].kind, KIND_DIR);
        assert!(!rows[0].expanded);
        // A collapsed directory is never even listed: the walk is the read budget.
        assert_eq!(fs.calls, vec![PathBuf::from("/r")]);
    }

    #[test]
    fn expanding_a_directory_inlines_its_children_one_level_deeper() {
        let mut fs = FakeFs::tree(&[("/r/sub/deep.txt", false)]);
        let mut open = BTreeSet::new();
        open.insert(PathBuf::from("/r/sub"));
        let rows = flatten(&mut fs, Path::new("/r"), &open);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].expanded);
        assert_eq!(rows[1].label, "deep.txt");
        assert_eq!(rows[1].depth, 1);
    }

    #[test]
    fn a_directory_that_cannot_be_read_says_so_where_it_sits() {
        let mut fs = FakeFs::tree(&[("/r/sub", true)]);
        fs.errors
            .insert(PathBuf::from("/r/sub"), "Permission denied".into());
        let mut open = BTreeSet::new();
        open.insert(PathBuf::from("/r/sub"));
        let rows = flatten(&mut fs, Path::new("/r"), &open);
        // The note is indented under the directory it belongs to, and is inert.
        assert_eq!(rows[1].label, "Cannot read: Permission denied");
        assert_eq!(rows[1].depth, 1);
        assert!(!rows[1].activatable());
    }

    #[test]
    fn an_empty_root_says_so_rather_than_drawing_nothing() {
        let mut fs = FakeFs::tree(&[("/r", true)]);
        let rows = flatten(&mut fs, Path::new("/r"), &BTreeSet::new());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "Empty directory");
        assert!(!rows[0].activatable());
    }

    #[test]
    fn a_symlink_is_a_leaf_however_it_was_made() {
        // The host reports a symlink as `symlink` whatever it points at. Descending into
        // one is how a link to `/` turns a project tree into the whole filesystem.
        let mut fs = FakeFs::default();
        fs.dirs.insert(
            PathBuf::from("/r"),
            vec![Entry {
                name: "link".into(),
                kind: "symlink".into(),
            }],
        );
        let rows = flatten(&mut fs, Path::new("/r"), &BTreeSet::new());
        assert_eq!(rows[0].kind, KIND_FILE);
        assert!(!rows[0].expanded);
    }

    #[test]
    fn ancestors_within_names_exactly_the_directories_to_open() {
        let root = PathBuf::from("/a/b");
        let got = ancestors_within(&root, Path::new("/a/b/c/d/e.txt"));
        assert_eq!(
            got,
            vec![PathBuf::from("/a/b/c/d"), PathBuf::from("/a/b/c")]
        );
        // A path outside the root expands nothing rather than walking to `/`.
        assert!(ancestors_within(&root, Path::new("/x/y.txt")).is_empty());
    }

    #[test]
    fn the_finder_ranks_a_name_match_above_a_directory_match() {
        let name = score("state", "state.rs").unwrap();
        let dir = score("state", "state/lib/other.rs").unwrap();
        assert!(name > dir, "name {name} should beat directory {dir}");
    }

    #[test]
    fn a_query_whose_characters_are_out_of_order_does_not_match() {
        assert!(score("zq", "state.rs").is_none());
        assert!(score("ts", "state.rs").is_some());
    }

    #[test]
    fn the_finder_walks_the_tree_and_skips_vendored_directories() {
        let mut fs = FakeFs::tree(&[
            ("/r/crates/core/src/state.rs", false),
            ("/r/node_modules/pkg/state.rs", false),
        ]);
        let rows = find(&mut fs, Path::new("/r"), "state.rs");
        let paths: Vec<String> = rows
            .iter()
            .filter(|r| r.activatable())
            .map(|r| r.path.display().to_string())
            .collect();
        assert_eq!(paths.len(), 1, "got {paths:?}");
        assert!(paths[0].ends_with("crates/core/src/state.rs"));
        // The result carries where it lives, since the flat list has no indent to say so.
        assert_eq!(rows[0].detail, "crates/core/src");
        // The skip-list is a *read* budget, not just a filter: the directory is never listed.
        assert!(!fs.calls.contains(&PathBuf::from("/r/node_modules")));
    }

    #[test]
    fn an_empty_query_finds_nothing_rather_than_everything() {
        let mut fs = FakeFs::tree(&[("/r/a.txt", false)]);
        assert!(find(&mut fs, Path::new("/r"), "   ").is_empty());
        // And it costs no reads at all.
        assert!(fs.calls.is_empty());
    }

    #[test]
    fn no_match_says_so_with_an_inert_row() {
        let mut fs = FakeFs::tree(&[("/r/a.txt", false)]);
        let rows = find(&mut fs, Path::new("/r"), "zzzzq");
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].activatable());
    }

    #[test]
    fn results_are_stable_across_two_identical_queries() {
        // Ties break on the path, so a directory listing that comes back in a different
        // order twice must not move a row under the cursor between the two paints.
        let mut fs = FakeFs::tree(&[("/r/a/state.rs", false), ("/r/b/state.rs", false)]);
        let first: Vec<PathBuf> = find(&mut fs, Path::new("/r"), "state.rs")
            .into_iter()
            .map(|r| r.path)
            .collect();
        for v in fs.dirs.values_mut() {
            v.reverse();
        }
        let second: Vec<PathBuf> = find(&mut fs, Path::new("/r"), "state.rs")
            .into_iter()
            .map(|r| r.path)
            .collect();
        assert_eq!(first, second);
    }
}
