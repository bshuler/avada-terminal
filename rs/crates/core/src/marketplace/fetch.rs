//! Getting a module's source: `git` as a subprocess, the manifest, the free-build gate.
//!
//! No git library is available to this crate, so `git` on `PATH` (or a path the caller
//! injects) does the work: `ls-remote --tags` says which commit a tag names, a shallow
//! `clone --branch <tag>` fetches exactly that, and `rev-parse HEAD` proves the clone
//! landed where `ls-remote` said. Output is streamed line by line to the caller's log
//! sink so an install job shows progress while it runs.

use crate::edition::{Edition, COMMERCIAL_URL};
use avada_module_sdk::manifest::{DistributionKind, Manifest};
use semver::Version;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The git binary and the `PATH` it runs with.
#[derive(Debug, Clone)]
pub struct Git {
    program: PathBuf,
    path: Option<std::ffi::OsString>,
}

/// Git problems, with the tail of what git said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitError {
    /// git could not be started.
    Spawn(String),
    /// git exited non-zero.
    Exit {
        /// The subcommand.
        what: String,
        /// Exit status (None when killed by a signal).
        status: Option<i32>,
        /// The last few lines git wrote.
        tail: String,
    },
    /// Output was not what was expected.
    Parse(String),
}

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GitError::Spawn(e) => write!(f, "could not run git: {e}"),
            GitError::Exit { what, status, tail } => write!(
                f,
                "git {what} exited with {}: {tail}",
                status.map_or("signal".to_string(), |s| format!("status {s}"))
            ),
            GitError::Parse(e) => write!(f, "git output did not parse: {e}"),
        }
    }
}
impl std::error::Error for GitError {}

impl Git {
    /// Run `program` with `path` as the `PATH` (None keeps the process one).
    pub fn new(program: impl Into<PathBuf>, path: Option<&OsStr>) -> Self {
        Git {
            program: program.into(),
            path: path.map(|p| p.to_os_string()),
        }
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        // Never let git open a credential prompt from a background thread.
        cmd.env("GIT_TERMINAL_PROMPT", "0");
        cmd.env("GIT_CONFIG_NOSYSTEM", "1");
        if let Some(p) = &self.path {
            cmd.env("PATH", p);
        }
        cmd
    }

    /// Every tag of `url` → the commit it names (a peeled `tag^{}` entry wins over
    /// the tag object itself, so annotated tags resolve to their commit).
    pub fn ls_remote_tags(&self, url: &str) -> Result<BTreeMap<String, String>, GitError> {
        let mut cmd = self.command();
        cmd.args(["ls-remote", "--tags", "--", url]);
        let out = run_capture(cmd, "ls-remote")?;
        let mut tags = BTreeMap::new();
        for line in out.lines() {
            let mut parts = line.split_whitespace();
            let (Some(commit), Some(reference)) = (parts.next(), parts.next()) else {
                continue;
            };
            let Some(name) = reference.strip_prefix("refs/tags/") else {
                continue;
            };
            if let Some(peeled) = name.strip_suffix("^{}") {
                tags.insert(peeled.to_string(), commit.to_string());
            } else {
                tags.entry(name.to_string())
                    .or_insert_with(|| commit.to_string());
            }
        }
        Ok(tags)
    }

    /// `git clone --depth 1 --branch <tag> <url> <dest>`, streaming git's chatter to
    /// `log`.
    pub fn clone_tag(
        &self,
        url: &str,
        tag: &str,
        dest: &Path,
        log: &mut dyn FnMut(&str),
    ) -> Result<(), GitError> {
        let mut cmd = self.command();
        cmd.args(["clone", "--depth", "1", "--branch", tag, "--progress", "--"])
            .arg(url)
            .arg(dest);
        run_streaming(cmd, "clone", log)
    }

    /// The commit `dir` is checked out at.
    pub fn head_commit(&self, dir: &Path) -> Result<String, GitError> {
        let mut cmd = self.command();
        cmd.current_dir(dir).args(["rev-parse", "HEAD"]);
        let out = run_capture(cmd, "rev-parse")?;
        let commit = out.trim();
        if commit.len() < 7 || !commit.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(GitError::Parse(format!("HEAD is `{commit}`")));
        }
        Ok(commit.to_string())
    }
}

fn run_capture(mut cmd: Command, what: &str) -> Result<String, GitError> {
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .map_err(|e| GitError::Spawn(e.to_string()))?;
    if !out.status.success() {
        return Err(GitError::Exit {
            what: what.to_string(),
            status: out.status.code(),
            tail: tail_of(&String::from_utf8_lossy(&out.stderr)),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run `cmd`, feeding every stdout/stderr line to `log`; error carries the tail.
pub fn run_streaming(
    mut cmd: Command,
    what: &str,
    log: &mut dyn FnMut(&str),
) -> Result<(), GitError> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| GitError::Spawn(e.to_string()))?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    // stderr is where git and cargo talk; drain stdout on a helper so neither pipe
    // fills while the other is read.
    let stdout_lines = std::thread::spawn(move || {
        BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
            .collect::<Vec<_>>()
    });
    let mut tail: Vec<String> = Vec::new();
    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
        // git's progress lines are `\r`-separated on one line; show the last piece.
        let line = line.rsplit('\r').next().unwrap_or("").to_string();
        if line.trim().is_empty() {
            continue;
        }
        if tail.len() >= 10 {
            tail.remove(0);
        }
        tail.push(line.clone());
        log(&line);
    }
    for line in stdout_lines.join().unwrap_or_default() {
        log(&line);
    }
    let status = child.wait().map_err(|e| GitError::Spawn(e.to_string()))?;
    if !status.success() {
        return Err(GitError::Exit {
            what: what.to_string(),
            status: status.code(),
            tail: tail.join(" | "),
        });
    }
    Ok(())
}

fn tail_of(text: &str) -> String {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .rev()
        .take(5)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join(" | ")
}

/// The newest semver tag among `tags` (only `vX.Y.Z` tags count).
pub fn newest_tag<'a>(tags: impl IntoIterator<Item = &'a str>) -> Option<(String, Version)> {
    tags.into_iter()
        .filter_map(|t| {
            Version::parse(t.strip_prefix('v')?)
                .ok()
                .map(|v| (t.to_string(), v))
        })
        .max_by(|a, b| a.1.cmp(&b.1))
}

/// Manifest problems in a fetched checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestReadError {
    /// No `avada.toml` at the checkout root.
    Missing(PathBuf),
    /// The file is not a valid manifest.
    Invalid(String),
}

impl std::fmt::Display for ManifestReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ManifestReadError::Missing(p) => write!(f, "no avada.toml at {}", p.display()),
            ManifestReadError::Invalid(e) => write!(f, "avada.toml: {e}"),
        }
    }
}
impl std::error::Error for ManifestReadError {}

/// The manifest file name every module carries at its root.
pub const MANIFEST_FILE: &str = "avada.toml";

/// Read and validate `<dir>/avada.toml`.
pub fn read_manifest(dir: &Path) -> Result<Manifest, ManifestReadError> {
    let path = dir.join(MANIFEST_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ManifestReadError::Missing(path))
        }
        Err(e) => return Err(ManifestReadError::Invalid(e.to_string())),
    };
    let manifest = Manifest::parse(&text).map_err(|e| ManifestReadError::Invalid(e.to_string()))?;
    manifest
        .validate()
        .map_err(|e| ManifestReadError::Invalid(e.to_string()))?;
    Ok(manifest)
}

/// Whether `edition` may install `manifest` at all, before any work is done.
///
/// The free edition builds from source and cannot license, so a prebuilt or commercial
/// module is refused by name with the commercial edition pointed at — the caller
/// prefixes the id when the refusal is about a dependency rather than the module that
/// was asked for. The commercial edition allows both; what it does *with* a prebuilt
/// artifact is the loader's business (`marketplace::loader`), not this gate's.
pub fn check_edition(edition: Edition, manifest: &Manifest) -> Result<(), String> {
    if edition.is_commercial() {
        return Ok(());
    }
    if manifest.distribution.kind == DistributionKind::Binary {
        return Err(format!(
            "{} ships as a prebuilt binary; the free tier installs source modules only \u{2014} {COMMERCIAL_URL}",
            manifest.id().as_str()
        ));
    }
    if manifest.distribution.commercial {
        return Err(format!(
            "{} is a commercial module and needs a license; the free tier cannot install it \u{2014} {COMMERCIAL_URL}",
            manifest.id().as_str()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_tag_prefers_semver_and_ignores_other_names() {
        let got = newest_tag([
            "v1.2.0",
            "v1.10.0",
            "v0.9.9",
            "latest",
            "1.11.0",
            "v1.10.0-rc1",
        ]);
        assert_eq!(got, Some(("v1.10.0".to_string(), Version::new(1, 10, 0))));
        assert_eq!(newest_tag(["main", "release"]), None);
        assert_eq!(newest_tag([]), None);
    }

    fn manifest(extra: &str) -> Manifest {
        Manifest::parse(&format!(
            r#"
capabilities = []
[module]
id = "acme/avada-files"
name = "Files"
version = "1.0.0"
description = "d"
publisher = "acme"
contract = "^1"
[distribution]
{extra}
"#
        ))
        .unwrap()
    }

    const COMMERCIAL_MANIFEST: &str =
        "kind = \"source\"\ncommercial = true\nissuer = \"https://x.example\"";

    #[test]
    fn free_build_refuses_binary_and_commercial_and_says_where_to_get_them() {
        assert!(check_edition(Edition::Free, &manifest("kind = \"source\"")).is_ok());
        let bin = check_edition(Edition::Free, &manifest("kind = \"binary\"")).unwrap_err();
        assert!(bin.contains("prebuilt binary"), "{bin}");
        assert!(bin.contains("acme/avada-files"));
        // Naming the module without saying what to do about it is half an answer.
        assert!(bin.contains(COMMERCIAL_URL), "{bin}");
        let com = check_edition(Edition::Free, &manifest(COMMERCIAL_MANIFEST)).unwrap_err();
        assert!(com.contains("commercial"), "{com}");
        assert!(
            com.contains("acme/avada-files") && com.contains(COMMERCIAL_URL),
            "{com}"
        );
    }

    #[test]
    fn the_commercial_edition_installs_what_the_free_one_refuses() {
        // The branch the private crate turns on, tested from the build that does not
        // have it: the edition is an argument, not a `cfg!` read at the point of use.
        for extra in [
            "kind = \"source\"",
            "kind = \"binary\"",
            COMMERCIAL_MANIFEST,
        ] {
            let m = manifest(extra);
            assert!(check_edition(Edition::Commercial, &m).is_ok(), "{extra}");
            assert_eq!(
                check_edition(Edition::Free, &m).is_ok(),
                extra == "kind = \"source\"",
                "{extra}"
            );
        }
    }

    #[test]
    fn read_manifest_reports_missing_and_invalid() {
        let dir = std::env::temp_dir().join(format!("avada-mp-fetch-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        match read_manifest(&dir) {
            Err(ManifestReadError::Missing(p)) => assert!(p.ends_with(MANIFEST_FILE)),
            other => panic!("{other:?}"),
        }
        std::fs::write(dir.join(MANIFEST_FILE), "this is = not [toml").unwrap();
        assert!(matches!(
            read_manifest(&dir),
            Err(ManifestReadError::Invalid(_))
        ));
        std::fs::write(
            dir.join(MANIFEST_FILE),
            manifest("kind = \"source\"").to_toml(),
        )
        .unwrap();
        assert_eq!(
            read_manifest(&dir).unwrap().id().as_str(),
            "acme/avada-files"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tail_keeps_the_last_five_non_empty_lines() {
        let t = tail_of("a\n\nb\nc\nd\ne\nf\n");
        assert_eq!(t, "b | c | d | e | f");
    }

    #[test]
    fn errors_read_well() {
        let e = GitError::Exit {
            what: "clone".into(),
            status: Some(128),
            tail: "fatal: no such ref".into(),
        };
        assert_eq!(
            e.to_string(),
            "git clone exited with status 128: fatal: no such ref"
        );
        let k = GitError::Exit {
            what: "clone".into(),
            status: None,
            tail: String::new(),
        };
        assert!(k.to_string().contains("signal"));
    }
}
