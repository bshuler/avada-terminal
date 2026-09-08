//! The host's git service — the questions the app and its modules are allowed to ask git.
//!
//! Nothing outside this file shells out to `git`. A module in particular must not: it is
//! handed answers over `host.git.*` behind the `git.read` capability, the same way the file
//! explorer is handed directory listings instead of `std::fs`. Keeping the subprocess here
//! is what makes that capability mean something.
//!
//! Three entry points, split because they are asked at very different rates:
//!
//!   * [`resolve_commit`] runs on every hover over a hex-shaped token, so it is the cheapest
//!     question git can be asked (`rev-parse --verify`) and nothing else.
//!   * [`load_commit`] runs once, on the click that follows, and pays for the header and the
//!     file list.
//!   * [`status_for`] describes a working tree — branch, upstream divergence, and the
//!     staged / changed / untracked files — out of ONE `git status` run.
//!
//! Everything here is best-effort: git missing, not a repo, a hash that names a blob rather
//! than a commit, a repo whose object is not fetched yet — all of it yields `None`. A link
//! that cannot resolve simply does not light up, which is the same contract a path that is
//! not on disk already has.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Spawn a child without flashing a console window on Windows. Private, and the only copy
/// left: the whole point of this module is that nobody else builds a `git` command at all.
trait NoWindow {
    fn no_window(&mut self) -> &mut Self;
}
impl NoWindow for Command {
    #[cfg(windows)]
    #[tracing::instrument(level = "debug", ret)]
    fn no_window(&mut self) -> &mut Self {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        self.creation_flags(CREATE_NO_WINDOW)
    }
    #[cfg(not(windows))]
    fn no_window(&mut self) -> &mut Self {
        self
    }
}

/// How much of a `git show` we are willing to read into memory. A commit that renamed a
/// vendored tree can list tens of thousands of files, and the panel draws a few dozen.
const MAX_OUTPUT_BYTES: usize = 1 << 20;

/// Run `git -C dir <args>` and return its stdout, or `None` for any failure at all — a
/// missing git, a non-zero exit, output that is not UTF-8, or output past [`MAX_OUTPUT_BYTES`].
#[tracing::instrument(level = "debug", ret)]
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        // A commit link must never be the thing that pops a credential or GPG prompt: this
        // reads local objects only, and `core.askPass` staying empty keeps it that way.
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .no_window()
        .output()
        .ok()?;
    if !out.status.success() || out.stdout.len() > MAX_OUTPUT_BYTES {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// The work tree root containing `dir`, or `None` when it is in no repository.
#[tracing::instrument(level = "debug", ret)]
pub fn repo_root(dir: &Path) -> Option<PathBuf> {
    let out = git(dir, &["rev-parse", "--show-toplevel"])?;
    let line = out.trim();
    if line.is_empty() {
        None
    } else {
        Some(PathBuf::from(line))
    }
}

/// True when `rev` is shaped like something we are willing to hand to git as a revision.
/// Deliberately narrow — a hex object name and nothing else. `rev-parse` happily accepts
/// `HEAD@{yesterday}`, `:/fix the thing` and other forms whose text comes from a pane's
/// output, and none of them is what a clicked hash means.
#[tracing::instrument(level = "debug", ret)]
pub fn is_hex_rev(rev: &str) -> bool {
    let n = rev.len();
    (7..=40).contains(&n)
        && rev
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Find the one file `name` refers to, when it is not where the pane is standing.
///
/// A coding session says `b99_price.py` and means a file three directories away; the pane's
/// cwd resolves it to nothing, and the name stays dark. The repository is the right place to
/// look for it, and `git ls-files` is the cheap way: one subprocess over the index (plus
/// untracked-but-not-ignored files, which a just-written file is), instead of a walk of a
/// tree whose `node_modules` alone would dwarf the answer.
///
/// **Ambiguity is a miss, deliberately.** Four files named `mod.rs` and a guess would open
/// the wrong one, and a link that opens the wrong file is worse than a name that stays dark.
///
/// `name` may carry directories (`src/b99_price.py`); it matches on a whole-segment suffix,
/// so `price.py` never answers for `b99_price.py`.
#[tracing::instrument(level = "debug", ret)]
pub fn find_in_repo(dir: &Path, name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.starts_with('/') || name.contains('\\') || name.contains("..") {
        return None;
    }
    let root = repo_root(dir)?;
    // Two pathspecs: the name at the root, and the name anywhere under it. `*` in a git
    // pathspec crosses `/`, so `*b99_price.py` is the whole-tree search — done by git, so
    // the output that crosses the pipe is the handful of matches rather than the index.
    let anywhere = format!("*{name}");
    let out = git(
        &root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
            name,
            &anywhere,
        ],
    )?;
    let tail = format!("/{name}");
    let mut hit: Option<&str> = None;
    for p in out.split('\0').filter(|p| !p.is_empty()) {
        // The pathspec glob matched on characters; this is the segment boundary it ignored.
        if p != name && !p.ends_with(&tail) {
            continue;
        }
        match hit {
            Some(prev) if prev != p => return None,
            _ => hit = Some(p),
        }
    }
    Some(root.join(hit?))
}

/// True when `rev` is shaped like a branch or remote-tracking ref a pane might print —
/// `origin/main`, `feature/x-1`, `upstream/release/2.4`: two or more segments of word
/// characters, dots and dashes, joined by `/`. That is `git check-ref-format`'s shape pared
/// down to what prose contains: no `@`, `~`, `^`, `:` or `{` (the revision *operators*
/// `rev-parse` would otherwise evaluate), no segment starting with `-` (an option) or `.`,
/// no `..` and no `.lock` tail. A bare `main` is deliberately out — one word is a word.
#[tracing::instrument(level = "debug", ret)]
pub fn is_ref_name(rev: &str) -> bool {
    if rev.len() > 200 {
        return false;
    }
    let mut segments = 0;
    for seg in rev.split('/') {
        segments += 1;
        if seg.is_empty()
            || seg.starts_with('-')
            || seg.starts_with('.')
            || seg.ends_with(".lock")
            || seg.contains("..")
            || !seg
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
        {
            return false;
        }
    }
    segments >= 2
}

/// Resolve `rev` to the full hash of a **commit** in the repository containing `dir`, or
/// `None`. The `^{commit}` peel is what makes this an answer rather than a guess: a tree or
/// blob whose abbreviation happens to match is not something a commit link can show.
///
/// `rev` is either a hex object name ([`is_hex_rev`]) or a ref name ([`is_ref_name`]); a
/// ref answers with the commit at its tip, which is what "open `origin/main`" can mean to
/// a panel that shows commits.
#[tracing::instrument(level = "debug", ret)]
pub fn resolve_commit(dir: &Path, rev: &str) -> Option<String> {
    if !is_hex_rev(rev) && !is_ref_name(rev) {
        return None;
    }
    let out = git(
        dir,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{rev}^{{commit}}"),
        ],
    )?;
    let full = out.trim();
    (full.len() == 40 && is_hex_rev(full)).then(|| full.to_string())
}

/// One file a commit touched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitFile {
    /// Repo-relative path, exactly as git reported it (forward slashes on every platform).
    pub path: String,
    /// The file name — the part a 260px panel actually shows.
    pub label: String,
    /// The parent directory, drawn dimmed after the label. Empty at the repo root.
    pub detail: String,
    /// git's status letter: `A` `M` `D` `R` `C` `T`.
    pub code: char,
}

impl CommitFile {
    #[tracing::instrument(level = "debug", ret)]
    fn new(path: String, code: char) -> Self {
        let (detail, label) = match path.rsplit_once('/') {
            Some((dir, name)) => (dir.to_string(), name.to_string()),
            None => (String::new(), path.clone()),
        };
        Self {
            path,
            label,
            detail,
            code,
        }
    }
}

/// A commit as a link target: enough to caption the view, plus what it touched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    /// The work tree the paths in [`files`](Self::files) are relative to.
    pub root: PathBuf,
    /// Full 40-character hash — the identity, and what every follow-up command speaks.
    pub hash: String,
    /// git's own abbreviation, which is what the panel shows.
    pub short: String,
    pub subject: String,
    pub author: String,
    /// Author date, `YYYY-MM-DD`.
    pub date: String,
    pub files: Vec<CommitFile>,
}

/// The separator between header fields. A record separator rather than a newline because a
/// commit subject can (and in this repo does) contain almost anything else.
const FIELD: &str = "\u{1f}";

/// Load the commit `rev` names, as seen from `dir`. `None` when `dir` is in no repository or
/// `rev` is not a commit in it.
#[tracing::instrument(level = "debug", ret)]
pub fn load_commit(dir: &Path, rev: &str) -> Option<Commit> {
    let root = repo_root(dir)?;
    let hash = resolve_commit(&root, rev)?;

    let fmt = format!("--format=%h{FIELD}%an{FIELD}%ad{FIELD}%s");
    let head = git(&root, &["show", "--no-patch", "--date=short", &fmt, &hash])?;
    let mut parts = head.trim_end_matches('\n').splitn(4, FIELD);
    let short = parts.next().unwrap_or_default().to_string();
    let author = parts.next().unwrap_or_default().to_string();
    let date = parts.next().unwrap_or_default().to_string();
    let subject = parts.next().unwrap_or_default().to_string();

    Some(Commit {
        files: files_of(&root, &hash),
        root,
        hash,
        short,
        author,
        date,
        subject,
    })
}

/// The files `hash` touched. `-z` so a path containing a space, a quote or a newline arrives
/// verbatim — the same reason the working-tree view uses it. A merge commit legitimately
/// reports nothing here (`git show` diffs it against no parent), and that is not an error.
#[tracing::instrument(level = "debug", ret)]
fn files_of(root: &Path, hash: &str) -> Vec<CommitFile> {
    let Some(out) = git(root, &["show", "--name-status", "--format=", "-z", hash]) else {
        return Vec::new();
    };
    let mut fields = out.split('\0').filter(|f| !f.is_empty());
    let mut files = Vec::new();
    while let Some(status) = fields.next() {
        let code = status.chars().next().unwrap_or('M').to_ascii_uppercase();
        // A rename or copy spends TWO path fields — old then new — and the new one is the
        // file that now exists, so it is the only one a click could open.
        let path = if matches!(code, 'R' | 'C') {
            fields.next();
            fields.next()
        } else {
            fields.next()
        };
        let Some(path) = path else { break };
        files.push(CommitFile::new(path.to_string(), code));
    }
    files
}

/// Which of the three sections a working-tree row belongs to, in the order they are shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    /// Index differs from HEAD — `git commit` would include this.
    Staged,
    /// Working tree differs from the index, or the merge is unresolved.
    Changed,
    /// Not tracked and not ignored.
    Untracked,
}

impl Section {
    /// The heading a view draws above the section.
    #[tracing::instrument(level = "debug", ret)]
    pub fn title(self) -> &'static str {
        match self {
            Section::Staged => "Staged Changes",
            Section::Changed => "Changes",
            Section::Untracked => "Untracked",
        }
    }

    /// The wire name. Short, lower-case and stable: it crosses the RPC boundary into a
    /// module that has no way to see this enum, so its spelling is part of the contract.
    #[tracing::instrument(level = "debug", ret)]
    pub fn wire(self) -> &'static str {
        match self {
            Section::Staged => "staged",
            Section::Changed => "changed",
            Section::Untracked => "untracked",
        }
    }
}

/// One file in the working-tree view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusRow {
    /// Repo-relative path exactly as git reported it — the row's identity.
    pub path: String,
    /// File name, the part a narrow panel actually shows.
    pub label: String,
    /// Parent directory (empty at the repo root), drawn dimmed after the label.
    pub detail: String,
    /// The single-letter status badge: `A` `M` `D` `R` `C` `T` `U` `?`.
    pub code: char,
    /// Which section the row was reported under.
    pub section: Section,
}

impl StatusRow {
    #[tracing::instrument(level = "debug", ret)]
    fn new(path: String, code: char, section: Section) -> Self {
        // Split on `/`: git reports repo-relative paths with forward slashes on every
        // platform, so this is correct on Windows too and needs no `Path` round-trip.
        let (detail, label) = match path.rsplit_once('/') {
            Some((dir, name)) => (dir.to_string(), name.to_string()),
            None => (String::new(), path.clone()),
        };
        Self {
            path,
            label,
            detail,
            code,
            section,
        }
    }
}

/// One repo's head and its working tree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    /// The repo root the rows are relative to; `None` when there is no repo.
    pub root: Option<PathBuf>,
    /// Short branch name, or `HEAD detached` when there is no branch.
    pub branch: String,
    /// Configured upstream (`origin/main`), when there is one.
    pub upstream: Option<String>,
    /// Commits ahead of the upstream. 0 with no upstream.
    pub ahead: u32,
    /// Commits behind the upstream. 0 with no upstream.
    pub behind: u32,
    /// Every changed file, in git's own order.
    pub rows: Vec<StatusRow>,
}

impl Status {
    /// "There is nothing to show" — no repo, no git, or a git that failed.
    #[tracing::instrument(level = "debug", ret)]
    pub fn none() -> Self {
        Self::default()
    }

    /// Whether this describes a repository at all.
    #[tracing::instrument(level = "debug", ret)]
    pub fn is_repo(&self) -> bool {
        self.root.is_some()
    }

    /// Rows of one section, in git's own (path-sorted) order.
    #[tracing::instrument(level = "debug", ret)]
    pub fn section(&self, section: Section) -> impl Iterator<Item = &StatusRow> {
        self.rows.iter().filter(move |r| r.section == section)
    }

    /// `main ↑2 ↓1` — the header line, already assembled so no view does the formatting.
    #[tracing::instrument(level = "debug", ret)]
    pub fn head_summary(&self) -> String {
        let mut s = self.branch.clone();
        if self.ahead > 0 {
            s.push_str(&format!("  ↑{}", self.ahead));
        }
        if self.behind > 0 {
            s.push_str(&format!("  ↓{}", self.behind));
        }
        s
    }
}

/// The status letter for a porcelain v2 `XY` half. `.` (and a stray space, which v1 uses
/// in the same position) mean "unchanged in this half" and produce no row.
#[tracing::instrument(level = "debug", ret)]
fn code_of(c: u8) -> Option<char> {
    match c {
        b'.' | b' ' => None,
        b'A' | b'M' | b'D' | b'R' | b'C' | b'T' => Some(c as char),
        // Anything else is a status this build doesn't name; show it verbatim rather than
        // dropping the file — a file silently missing from the view is the worse failure.
        other => Some(other as char),
    }
}

/// Parse `git status --porcelain=v2 --branch -z` output.
///
///   * `-z` makes records NUL-terminated, so a path containing a space, a quote or a
///     newline arrives verbatim — the default porcelain output C-quotes those, and
///     un-quoting it correctly is a parser we would rather not own.
///   * `--porcelain=v2` splits the index state (X) from the working-tree state (Y), which
///     is exactly the "Staged Changes" / "Changes" split a view draws. v1 collapses rename
///     information the same way but reports no branch divergence.
///
/// A `2` (rename/copy) record carries its ORIGINAL path in the following NUL-field, so the
/// iterator consumes two fields for one row — the reason this is a hand-rolled loop.
#[tracing::instrument(level = "debug", ret)]
pub fn parse_status_v2(out: &str) -> Status {
    let mut st = Status {
        branch: "HEAD detached".to_string(),
        ..Default::default()
    };
    let mut fields = out.split('\0').filter(|f| !f.is_empty());
    while let Some(rec) = fields.next() {
        let Some((tag, rest)) = rec.split_once(' ') else {
            continue;
        };
        match tag {
            "#" => {
                let (key, val) = rest.split_once(' ').unwrap_or((rest, ""));
                match key {
                    // `(detached)` is git's own literal for "no branch"; keep the default.
                    "branch.head" if val != "(detached)" => st.branch = val.to_string(),
                    "branch.upstream" => st.upstream = Some(val.to_string()),
                    "branch.ab" => {
                        // `+N -M`, always both, always in that order.
                        for part in val.split_whitespace() {
                            let (sign, n) = part.split_at(1);
                            let n: u32 = n.parse().unwrap_or(0);
                            match sign {
                                "+" => st.ahead = n,
                                "-" => st.behind = n,
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
            "?" => st
                .rows
                .push(StatusRow::new(rest.to_string(), '?', Section::Untracked)),
            // Ignored entries are only emitted with `--ignored`, which we never pass.
            "!" => {}
            "1" | "2" => {
                // `<XY> <sub> <mH> <mI> <mW> <hH> <hI> [<Xscore> ]<path>`
                let mut it = rest.splitn(if tag == "2" { 9 } else { 8 }, ' ');
                let Some(xy) = it.next() else { continue };
                let path = match it.clone().last() {
                    Some(p) => p.to_string(),
                    None => continue,
                };
                if tag == "2" {
                    // Consume the original path so it is not mistaken for the next record.
                    let _ = fields.next();
                }
                let xy = xy.as_bytes();
                if xy.len() < 2 {
                    continue;
                }
                if let Some(c) = code_of(xy[0]) {
                    st.rows
                        .push(StatusRow::new(path.clone(), c, Section::Staged));
                }
                if let Some(c) = code_of(xy[1]) {
                    st.rows.push(StatusRow::new(path, c, Section::Changed));
                }
            }
            "u" => {
                // Unmerged: both halves describe the conflict, and it is one row, not two.
                let path = rest.rsplit(' ').next().unwrap_or_default().to_string();
                if !path.is_empty() {
                    st.rows.push(StatusRow::new(path, 'U', Section::Changed));
                }
            }
            _ => {}
        }
    }
    st
}

/// Run the status query in `root`. `None` on any failure — see the module note.
#[tracing::instrument(level = "debug", ret)]
pub fn status_in(root: &Path) -> Option<Status> {
    // Not [`git`]: this one decodes lossily. A single file whose name is not UTF-8 would
    // otherwise take the entire status with it, and a panel showing nothing is a worse
    // answer than a panel showing one row with a replacement character in it.
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "status",
            "--porcelain=v2",
            "--branch",
            // Every untracked file, not just the containing directory: the view lists
            // files, and `normal` would collapse a new directory into one unopenable row.
            "--untracked-files=all",
            "-z",
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .no_window()
        .output()
        .ok()?;
    if !out.status.success() || out.stdout.len() > MAX_OUTPUT_BYTES {
        return None;
    }
    let mut st = parse_status_v2(&String::from_utf8_lossy(&out.stdout));
    st.root = Some(root.to_path_buf());
    Some(st)
}

/// The whole read: find the repo enclosing `dir`, then describe it. `None` when `dir` is
/// not inside a repo.
#[tracing::instrument(level = "debug", ret)]
pub fn status_for(dir: &Path) -> Option<Status> {
    let root = repo_root(dir)?;
    status_in(&root)
}

/// Whether `rel` (repo-relative) is a file git tracks in `root`.
///
/// The one question a "Show Diff" menu entry has to answer before it offers itself:
/// `git diff HEAD -- <untracked file>` prints nothing, and a menu entry that opens an
/// empty pane is worse than no menu entry at all. `--error-unmatch` turns "not tracked"
/// into a non-zero exit, which is the shape [`git`] already reports as `None`.
#[tracing::instrument(level = "debug", ret)]
pub fn is_tracked(root: &Path, rel: &str) -> bool {
    !rel.is_empty() && git(root, &["ls-files", "--error-unmatch", "-z", "--", rel]).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    /// A throwaway repo with one commit, or `None` when this machine has no usable git —
    /// which is a skip, not a failure, for tests that are about git's output format.
    fn fixture() -> Option<(tempdir::Dir, String)> {
        let dir = tempdir::Dir::new()?;
        let run = |args: &[&str]| -> bool {
            Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        if !run(&["init", "-q", "-b", "main"]) {
            return None;
        }
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "T"]);
        run(&["config", "commit.gpgsign", "false"]);
        std::fs::create_dir_all(dir.path().join("sub")).ok()?;
        std::fs::write(dir.path().join("sub/a b.txt"), "one\n").ok()?;
        std::fs::write(dir.path().join("top.txt"), "two\n").ok()?;
        if !run(&["add", "-A"]) || !run(&["commit", "-q", "-m", "a subject: with punctuation"]) {
            return None;
        }
        let hash = git(dir.path(), &["rev-parse", "HEAD"])?.trim().to_string();
        Some((dir, hash))
    }

    /// A minimal scratch directory that removes itself — the crate has no dev-dependency on
    /// a tempdir crate and one test does not justify adding one.
    mod tempdir {
        use std::path::{Path, PathBuf};
        pub struct Dir(PathBuf);
        impl Dir {
            pub fn new() -> Option<Self> {
                let base = std::env::temp_dir().join(format!(
                    "avada-git-{}-{:?}",
                    std::process::id(),
                    std::thread::current().id()
                ));
                let _ = std::fs::remove_dir_all(&base);
                std::fs::create_dir_all(&base).ok()?;
                Some(Self(base))
            }
            pub fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn only_a_lowercase_hex_abbreviation_is_offered_to_git() {
        assert!(is_hex_rev("2d6909f1a"));
        assert!(is_hex_rev(&"a".repeat(40)));
        assert!(
            !is_hex_rev("2d6909"),
            "six characters is too ambiguous to link"
        );
        assert!(!is_hex_rev(&"a".repeat(41)));
        assert!(
            !is_hex_rev("2D6909F1A"),
            "uppercase is not how git prints one"
        );
        assert!(
            !is_hex_rev("HEAD~1"),
            "a revision expression is not a clicked hash"
        );
        assert!(!is_hex_rev(":/fix the thing"));
    }

    #[test]
    fn a_ref_name_is_two_or_more_plain_segments() {
        for ok in [
            "origin/main",
            "feature/x-1.2",
            "upstream/release/2.4",
            "a_b/c.d",
        ] {
            assert!(is_ref_name(ok), "{ok}");
        }
        for bad in [
            "main",
            "and/or/",
            "/usr/bin",
            "./x",
            "origin/-x",
            "a..b/c",
            "x/y.lock",
            "HEAD@{1}/x",
            "a/b:c",
            "a b/c",
        ] {
            assert!(!is_ref_name(bad), "{bad}");
        }
    }

    #[test]
    fn a_branch_resolves_to_the_commit_at_its_tip() {
        let Some((dir, hash)) = fixture() else { return };
        let made = Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["branch", "feature/x"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !made {
            return;
        }
        assert_eq!(
            resolve_commit(dir.path(), "feature/x").as_deref(),
            Some(hash.as_str())
        );
        assert_eq!(resolve_commit(dir.path(), "and/or"), None);
        // One word is a word, even when git would know it.
        assert_eq!(resolve_commit(dir.path(), "main"), None);
    }

    #[test]
    fn a_hash_printed_by_git_resolves_back_to_the_commit_it_names() {
        let Some((dir, hash)) = fixture() else { return };
        let short = &hash[..9];
        assert_eq!(
            resolve_commit(dir.path(), short).as_deref(),
            Some(&hash[..])
        );
        assert_eq!(
            resolve_commit(dir.path(), &hash).as_deref(),
            Some(&hash[..])
        );
    }

    #[test]
    fn a_hex_word_that_names_nothing_does_not_light_up() {
        let Some((dir, _)) = fixture() else { return };
        assert_eq!(resolve_commit(dir.path(), "defaced"), None);
        assert_eq!(resolve_commit(dir.path(), &"f".repeat(40)), None);
    }

    #[test]
    fn a_commit_reports_its_caption_and_every_file_it_touched() {
        let Some((dir, hash)) = fixture() else { return };
        let c = load_commit(dir.path(), &hash[..9]).expect("the commit we just made");
        assert_eq!(c.hash, hash);
        assert_eq!(c.subject, "a subject: with punctuation");
        assert_eq!(c.author, "T");
        assert_eq!(
            c.date.len(),
            10,
            "%ad --date=short is YYYY-MM-DD: {}",
            c.date
        );

        let mut paths: Vec<_> = c.files.iter().map(|f| f.path.clone()).collect();
        paths.sort();
        assert_eq!(
            paths,
            ["sub/a b.txt", "top.txt"],
            "a space in a path survives -z"
        );
        assert!(
            c.files.iter().all(|f| f.code == 'A'),
            "a first commit adds everything"
        );

        let deep = c.files.iter().find(|f| f.path.starts_with("sub/")).unwrap();
        assert_eq!(
            (deep.label.as_str(), deep.detail.as_str()),
            ("a b.txt", "sub")
        );
    }

    #[test]
    fn a_bare_name_finds_its_one_file_anywhere_in_the_repository() {
        let Some((dir, _)) = fixture() else { return };
        let root = dir.path();
        // What comes back is rooted at git's own `--show-toplevel`, which on macOS has
        // already walked the `/var` → `/private/var` symlink the temp dir sits behind.
        let real = repo_root(root).expect("the fixture is a repository");

        // The name is three directories from where we are standing, and still resolves.
        assert_eq!(
            find_in_repo(root, "a b.txt").as_deref(),
            Some(real.join("sub/a b.txt").as_path()),
            "a nested file answers to its bare name"
        );
        assert_eq!(
            find_in_repo(&root.join("sub"), "top.txt").as_deref(),
            Some(real.join("top.txt").as_path()),
            "the search is the repository, not the directory we asked from"
        );
        assert_eq!(
            find_in_repo(root, "sub/a b.txt").as_deref(),
            Some(real.join("sub/a b.txt").as_path()),
            "a partial path is a name too"
        );

        assert_eq!(
            find_in_repo(root, "op.txt"),
            None,
            "the match is by whole segment: `op.txt` is not `top.txt`"
        );
        assert_eq!(find_in_repo(root, "nothing-like-this.txt"), None);

        // A second `top.txt` — untracked, but not ignored, so the search sees it and now
        // cannot say which one was meant. Silence beats opening the wrong file.
        std::fs::write(root.join("sub/top.txt"), "three\n").unwrap();
        assert_eq!(
            find_in_repo(root, "top.txt"),
            None,
            "two files of that name is an ambiguity, not a pick"
        );
    }

    #[test]
    fn a_directory_outside_any_repository_has_no_commits_to_show() {
        let tmp = std::env::temp_dir();
        // Not asserting on `tmp` itself being repo-less would make this test lie on a
        // machine whose temp dir is inside a checkout; skip there rather than fail.
        if repo_root(&tmp).is_some() {
            return;
        }
        assert_eq!(load_commit(&tmp, &"a".repeat(40)), None);
    }

    /// Records are NUL-*terminated*, so the fixture ends with one too.
    fn z(lines: &[&str]) -> String {
        lines.iter().map(|l| format!("{l}\0")).collect::<String>()
    }

    #[test]
    fn status_reads_branch_upstream_and_divergence() {
        let st = parse_status_v2(&z(&[
            "# branch.oid 1111111111111111111111111111111111111111",
            "# branch.head main",
            "# branch.upstream origin/main",
            "# branch.ab +2 -1",
        ]));
        assert_eq!(st.branch, "main");
        assert_eq!(st.upstream.as_deref(), Some("origin/main"));
        assert_eq!((st.ahead, st.behind), (2, 1));
        assert_eq!(st.head_summary(), "main  ↑2  ↓1");
        assert!(st.rows.is_empty());
    }

    #[test]
    fn a_detached_head_is_named_not_left_blank() {
        let st = parse_status_v2(&z(&["# branch.head (detached)", "# branch.ab +0 -0"]));
        assert_eq!(st.branch, "HEAD detached");
        assert_eq!(st.head_summary(), "HEAD detached");
    }

    /// The XY split IS the section split: a file staged *and* then edited again shows up
    /// in both sections, which is what git itself reports and what the user has to see to
    /// understand why a commit would not include their latest edit.
    #[test]
    fn the_index_half_and_the_worktree_half_are_separate_rows() {
        let st = parse_status_v2(&z(&[
            "# branch.head main",
            "1 MM N... 100644 100644 100644 aaa bbb src/state.rs",
        ]));
        let staged: Vec<_> = st.section(Section::Staged).collect();
        let changed: Vec<_> = st.section(Section::Changed).collect();
        assert_eq!(staged.len(), 1);
        assert_eq!(changed.len(), 1);
        assert_eq!(staged[0].code, 'M');
        assert_eq!(staged[0].label, "state.rs");
        assert_eq!(staged[0].detail, "src");
        assert_eq!(staged[0].path, "src/state.rs");
    }

    #[test]
    fn an_unchanged_half_produces_no_row() {
        let st = parse_status_v2(&z(&[
            "# branch.head main",
            "1 .M N... 100644 100644 100644 aaa bbb only-in-worktree.txt",
            "1 A. N... 000000 100644 100644 000 bbb only-in-index.txt",
        ]));
        assert_eq!(st.section(Section::Staged).count(), 1);
        assert_eq!(st.section(Section::Changed).count(), 1);
        assert_eq!(
            st.section(Section::Staged).next().unwrap().path,
            "only-in-index.txt"
        );
        assert_eq!(
            st.section(Section::Changed).next().unwrap().path,
            "only-in-worktree.txt"
        );
    }

    /// A `2` record's original path is a field of its own. Miss it and the very next
    /// record is read as a path — the bug this test exists to pin.
    #[test]
    fn a_rename_consumes_its_original_path_field() {
        let st = parse_status_v2(&z(&[
            "# branch.head main",
            "2 R. N... 100644 100644 100644 aaa bbb R100 new/name.rs",
            "old/name.rs",
            "? untracked.txt",
        ]));
        let staged: Vec<_> = st.section(Section::Staged).collect();
        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].path, "new/name.rs");
        assert_eq!(staged[0].code, 'R');
        let untracked: Vec<_> = st.section(Section::Untracked).collect();
        assert_eq!(
            untracked.len(),
            1,
            "the record after a rename is not a path"
        );
        assert_eq!(untracked[0].path, "untracked.txt");
    }

    #[test]
    fn untracked_and_conflicted_land_in_their_own_sections() {
        let st = parse_status_v2(&z(&[
            "# branch.head main",
            "? new file.txt",
            "u UU N... 100644 100644 100644 100644 a b c conflict.rs",
        ]));
        let un: Vec<_> = st.section(Section::Untracked).collect();
        assert_eq!(un.len(), 1);
        // A space in the name survives because `-z` never quotes.
        assert_eq!(un[0].path, "new file.txt");
        assert_eq!(un[0].code, '?');
        let ch: Vec<_> = st.section(Section::Changed).collect();
        assert_eq!(ch.len(), 1, "a conflict is ONE row, not one per half");
        assert_eq!(ch[0].code, 'U');
        assert_eq!(ch[0].path, "conflict.rs");
    }

    #[test]
    fn a_root_level_file_has_no_detail() {
        let st = parse_status_v2(&z(&["# branch.head main", "? README.md"]));
        let r = st.section(Section::Untracked).next().unwrap();
        assert_eq!(r.label, "README.md");
        assert_eq!(r.detail, "");
    }

    #[test]
    fn empty_status_output_is_a_clean_tree_not_a_panic() {
        let st = parse_status_v2("");
        assert!(st.rows.is_empty());
        assert!(!st.is_repo());
        assert_eq!(Status::none(), Status::default());
    }

    /// The wire names cross into a module that cannot see the enum, so they are asserted
    /// here rather than left to whatever `Debug` happens to print.
    #[test]
    fn the_section_wire_names_are_the_contract() {
        assert_eq!(Section::Staged.wire(), "staged");
        assert_eq!(Section::Changed.wire(), "changed");
        assert_eq!(Section::Untracked.wire(), "untracked");
    }

    /// End to end against a real repo — the parser and the flags have to agree with the
    /// git that is actually installed, not with the fixture I wrote.
    #[test]
    fn a_real_repo_reports_its_branch_and_a_new_file() {
        let root = std::env::temp_dir().join(format!("avada-status-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let run = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .output()
                .expect("git runs")
        };
        if !run(&["init", "-b", "trunk"]).status.success() {
            return; // no git on this machine — nothing to assert against
        }
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(root.join("sub/added.txt"), "hi").unwrap();
        std::fs::write(root.join("loose.txt"), "hi").unwrap();
        run(&["add", "sub/added.txt"]);

        let st = status_in(&root).expect("status");
        assert_eq!(st.branch, "trunk");
        assert_eq!(st.root.as_deref(), Some(root.as_path()));
        let staged: Vec<_> = st.section(Section::Staged).collect();
        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].path, "sub/added.txt");
        assert_eq!(staged[0].code, 'A');
        let un: Vec<_> = st.section(Section::Untracked).map(|r| &r.path).collect();
        assert_eq!(un, vec!["loose.txt"]);

        // `status_for` finds the root from a subdirectory, which is what every caller
        // actually has — a pane's cwd, never the root itself.
        let from_sub = status_for(&root.join("sub")).expect("status from a subdirectory");
        assert_eq!(from_sub.branch, "trunk");

        assert!(is_tracked(&root, "sub/added.txt"));
        assert!(
            !is_tracked(&root, "loose.txt"),
            "an untracked file has no diff to show"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
