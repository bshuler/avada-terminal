//! What this module knows about git: nothing it did not ask the host for.
//!
//! There is no `std::process::Command` in this crate, and that is the point. The porcelain
//! parsing lives in the host (`core/src/git.rs`), behind the `git.read` capability, where
//! it is scoped to the open workspace and where a human can see in the manifest that this
//! module was allowed to read a repository and nothing else. A module that shelled out to
//! `git` itself would be running arbitrary programs under a capability that says "read".
//!
//! So the whole of git, from here, is the [`Host`] trait: two questions, two answers.

use serde::Deserialize;

/// The event kind the host announces when a commit hash clicked in a pane wants listing.
///
/// It is a plain string rather than an SDK constant on purpose: the contract lets a host
/// announce a kind an older module never heard of, and a module subscribe to one the SDK
/// it was built against did not name.
pub const GIT_COMMIT_EVENT: &str = "git.commit";

/// `host.git.status`.
pub const HOST_GIT_STATUS: &str = "host.git.status";
/// `host.git.commit`.
pub const HOST_GIT_COMMIT: &str = "host.git.commit";

/// The host's section wire names, paired with the heading this module draws for each, in
/// the order a human reads a working tree: what is about to be committed, what is not yet,
/// and what git is not tracking at all.
pub const SECTIONS: [(&str, &str); 3] = [
    ("staged", "Staged"),
    ("changed", "Changed"),
    ("untracked", "Untracked"),
];

/// One line of `host.git.status`'s `rows`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct StatusRow {
    /// Repo-relative path, forward slashes on every platform.
    pub path: String,
    /// The file name.
    pub label: String,
    /// The parent directory, empty at the repo root.
    pub detail: String,
    /// git's single-letter status: `A` `M` `D` `R` `C` `T` `U` `?`.
    pub code: String,
    /// `staged`, `changed` or `untracked`.
    pub section: String,
}

/// The answer to `host.git.status`.
///
/// "Not a repository" arrives as `repo: false`, not as an RPC error — it is an answer to a
/// reasonable question, and drawing it as a failure would put a red toast in front of
/// every human who opened the panel in a plain directory.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Status {
    /// Whether the path is inside a git repository at all.
    pub repo: bool,
    /// The repository root, absolute, as the host resolved it.
    pub root: Option<String>,
    /// The current branch, or empty on a detached HEAD.
    pub branch: String,
    /// The upstream ref, when the branch has one.
    pub upstream: Option<String>,
    /// Commits ahead of the upstream.
    pub ahead: i64,
    /// Commits behind it.
    pub behind: i64,
    /// The host's one-line head summary, e.g. `main  ↑2  ↓1`.
    pub summary: String,
    /// The changed files.
    pub rows: Vec<StatusRow>,
}

/// One file of a commit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct CommitFile {
    /// Repo-relative path.
    pub path: String,
    /// The file name.
    pub label: String,
    /// The parent directory.
    pub detail: String,
    /// git's status letter for this file in this commit.
    pub code: String,
}

/// The answer to `host.git.commit`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Commit {
    /// Whether the revision resolved. A hash that does not exist is `found: false`.
    pub found: bool,
    /// The repository the commit was resolved in.
    pub root: Option<String>,
    /// The full object name.
    pub hash: String,
    /// The abbreviation to show a human.
    pub short: String,
    /// The subject line.
    pub subject: String,
    /// The author's name.
    pub author: String,
    /// The author date, already formatted by the host.
    pub date: String,
    /// The files the commit touched.
    pub files: Vec<CommitFile>,
}

/// git, as seen through the host.
///
/// Both methods return `Err` only when the *conversation* failed — a refused capability, a
/// path outside the workspace, a broken socket. "Not a repository" and "no such commit"
/// come back as `Ok` with `repo: false` / `found: false`, because they are answers.
pub trait Host {
    /// `host.git.status` for the given path, or for the workspace when `None`.
    fn status(&mut self, path: Option<&str>) -> Result<Status, String>;

    /// `host.git.commit` for a revision, resolved in the repository containing `path`.
    fn commit(&mut self, rev: &str, path: Option<&str>) -> Result<Commit, String>;
}

/// Join a repository root and a repo-relative path into the absolute path the host's own
/// file menu expects in `data.path`.
///
/// String work rather than `Path::join`, because git's paths use forward slashes on every
/// platform and the root came to us as the host's own display string: round-tripping it
/// through `PathBuf` on Windows would rewrite separators the host then has to undo.
pub fn abs(root: &str, rel: &str) -> String {
    if rel.is_empty() {
        return root.to_string();
    }
    let sep = if root.ends_with('/') || root.ends_with('\\') {
        ""
    } else {
        "/"
    };
    format!("{root}{sep}{rel}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The host answers "not a repository" in full, with every field present, and the
    /// module must read that as a state rather than as a missing answer.
    #[test]
    fn a_directory_that_is_not_a_repository_deserialises_to_a_quiet_status() {
        let v = serde_json::json!({
            "repo": false, "root": null, "branch": "", "upstream": null,
            "ahead": 0, "behind": 0, "summary": "", "rows": []
        });
        let s: Status = serde_json::from_value(v).expect("the host's own shape");
        assert!(!s.repo);
        assert_eq!(s, Status::default(), "an empty status is the default one");
    }

    /// A newer host may add fields this module has never heard of. Deserialising must not
    /// break on them, or a host upgrade would take every installed module down with it.
    #[test]
    fn an_unknown_field_from_a_newer_host_is_ignored() {
        let v = serde_json::json!({
            "repo": true, "root": "/proj", "branch": "main", "summary": "main",
            "rows": [{ "path": "a.rs", "label": "a.rs", "detail": "", "code": "M",
                       "section": "changed", "blame": "someone" }],
            "stash_count": 3
        });
        let s: Status = serde_json::from_value(v).expect("forwards compatible");
        assert_eq!(s.rows.len(), 1);
        assert_eq!(s.rows[0].code, "M");
    }

    /// `found: false` is the whole answer for an unresolvable revision: no root, no files,
    /// and nothing for the module to mistake for a real commit.
    #[test]
    fn an_unresolvable_revision_is_an_answer_not_an_error() {
        let c: Commit =
            serde_json::from_value(serde_json::json!({ "found": false })).expect("shape");
        assert!(!c.found);
        assert!(c.files.is_empty());
    }

    /// The host's file menu finds the repo-relative part again with `strip_prefix`, so the
    /// join has to be exactly one separator — never two, never none.
    #[test]
    fn joining_a_root_and_a_relative_path_yields_one_separator() {
        assert_eq!(abs("/proj", "src/a.rs"), "/proj/src/a.rs");
        assert_eq!(abs("/proj/", "src/a.rs"), "/proj/src/a.rs");
        assert_eq!(abs("/proj", ""), "/proj", "the root itself is the root row");
        assert_eq!(abs("C:\\proj", "a.rs"), "C:\\proj/a.rs");
    }
}
