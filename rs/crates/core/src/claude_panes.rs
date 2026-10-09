//! Per-pane Claude Code session markers — the bridge that lets a relaunch resume the
//! conversation each pane was having.
//!
//! A Claude Code `SessionStart` hook (`resources/claude/hp-claude-session-hook.sh`) runs
//! inside every `claude` a user launches in a pane. The pane's environment carries
//! `AVADA_PANE_ID`, and the hook's stdin carries the conversation's `session_id` —
//! so the hook writes `<state dir>/claude-sessions/<pane-id>.json`:
//!
//! ```json
//! { "sessionId": "0198c4a2-…", "cwd": "/home/me/dev/x" }
//! ```
//!
//! `SessionEnd` removes the marker, so a marker exists exactly while a conversation is
//! live in that pane. The GUI's relaunch snapshot ([`crate::workspace::model::PaneSpec`]
//! via `to_session_file`) embeds the id as pane meta (key [`META_KEY`]); on restore, a
//! pane whose live session did NOT survive re-spawns and resumes the conversation with
//! `claude --resume <id>` in its original cwd.
//!
//! The session id is embedded into a command line at restore, so [`read_pane_session`]
//! accepts only ids matching Claude's UUID shape (`[0-9a-fA-F-]`) — anything else is
//! treated as a corrupt/hostile marker and ignored.

use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

use serde::Deserialize;

use crate::persistence::paths;

/// Pane-meta key under which the snapshot records the pane's live Claude session id.
pub const META_KEY: &str = "claude.session";

/// Pane-meta key for the conversation's working directory. `claude --resume <id>` only
/// finds sessions belonging to the CURRENT directory's project, and a pane's own cwd
/// snapshot can be stale (a shell parked inside a TUI never re-emits OSC 7 after a GUI
/// re-attach) — so the hook-reported cwd is authoritative and restore must `cd` first.
pub const META_CWD_KEY: &str = "claude.cwd";

/// Pane-meta key for the conversation's `CLAUDE_CONFIG_DIR` (the account it was saved
/// under). `claude` stores transcripts in `$CLAUDE_CONFIG_DIR/projects`, so a relaunch must
/// set the SAME dir for `claude --resume <id>` to find the session — without it, a pane that
/// ran under a rotated/non-default account resumes against `~/.claude` and finds nothing.
/// Empty/absent ⇒ the default account (`~/.claude`), so restore sets nothing.
pub const META_CONFIG_DIR_KEY: &str = "claude.config_dir";

/// Is `cwd` safe to interpolate into a single-quoted `cd '<cwd>'`? Absolute, no control
/// characters, and no single quotes (rather than escaping, refuse — real project paths
/// never contain them, and refusing keeps the injection reasoning trivial).
#[tracing::instrument(level = "debug", ret)]
pub fn valid_resume_cwd(cwd: &str) -> bool {
    cwd.starts_with('/')
        && cwd.len() < 1024
        && !cwd.contains('\'')
        && !cwd.chars().any(|c| c.is_control())
}

/// Is `dir` safe to interpolate into a single-quoted `CLAUDE_CONFIG_DIR='<dir>'` prefix (or
/// pass as a spawn env value)? Same gate as [`valid_resume_cwd`] — absolute, bounded, no
/// single quotes, no control chars — since a marker is external, best-effort state that
/// lands on a command line. A non-empty but invalid dir is treated as "no config dir".
#[tracing::instrument(level = "debug", ret)]
pub fn valid_config_dir(dir: &str) -> bool {
    valid_resume_cwd(dir)
}

/// One pane's live Claude conversation, as reported by the session hook.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaneClaudeSession {
    /// The conversation's session id — what `claude --resume <id>` takes.
    pub session_id: String,
    /// The conversation's working directory at SessionStart (informational; the pane's
    /// own cwd snapshot is what restore actually uses).
    #[serde(default)]
    pub cwd: String,
    /// The `CLAUDE_CONFIG_DIR` this conversation was saved under (empty ⇒ the default
    /// `~/.claude` account). Restore must re-set it so `claude --resume` finds the session
    /// in the right per-account transcript store. Older markers without the field parse
    /// with an empty string (the default account), preserving pre-multi-account behaviour.
    #[serde(default)]
    pub config_dir: String,
}

/// The marker file for one pane id.
#[tracing::instrument(level = "debug", ret)]
fn marker_path(pane_id: &str) -> PathBuf {
    paths::claude_sessions_dir().join(format!("{pane_id}.json"))
}

/// Is `id` shaped like a Claude session id (UUID: hex + dashes, sane length)? The id is
/// later interpolated into a shell command line, so this is a safety gate, not a nicety.
/// Public because restore re-checks ids read back from `workspace.json` — that file is
/// user-editable, so the write-time validation here cannot be assumed to hold.
#[tracing::instrument(level = "debug", ret)]
pub fn valid_session_id(id: &str) -> bool {
    (8..=64).contains(&id.len()) && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// Read the live-session marker for `pane_id`, if one exists and is well-formed.
/// Missing file, unparseable JSON, or a malformed id all yield `None` — a marker is
/// best-effort state written by an external hook, never trusted blindly.
#[tracing::instrument(level = "debug", ret)]
pub fn read_pane_session(pane_id: &str) -> Option<PaneClaudeSession> {
    let text = fs::read_to_string(marker_path(pane_id)).ok()?;
    let parsed: PaneClaudeSession = serde_json::from_str(&text).ok()?;
    valid_session_id(&parsed.session_id).then_some(parsed)
}

/// The marker file's mtime — the only ordering two markers for one pane can be given.
#[tracing::instrument(level = "debug", ret)]
fn marker_mtime(pane_id: &str) -> Option<SystemTime> {
    fs::metadata(marker_path(pane_id)).ok()?.modified().ok()
}

/// The newest of several dated candidates; the first one wins a tie.
fn newest<T>(cands: impl IntoIterator<Item = (SystemTime, T)>) -> Option<T> {
    let mut best: Option<(SystemTime, T)> = None;
    for (at, item) in cands {
        best = Some(match best.take() {
            Some((best_at, kept)) if best_at >= at => (best_at, kept),
            _ => (at, item),
        });
    }
    best.map(|(_, item)| item)
}

/// Read the newest live-session marker among several ids that name the same pane.
///
/// One pane can own two marker files: a control-spawned lane's hook writes under the
/// control alias, and after a GUI-side restart the replacement process inherits the new
/// session uid instead. Only the newest describes the process running now — an older one
/// is a dead incarnation whose `SessionEnd` never fired (a kill skips it). Duplicate ids
/// and ids without a marker are skipped; on an equal mtime the earlier id wins.
#[tracing::instrument(level = "debug", ret, skip(ids))]
pub fn read_newest_pane_session<'a>(
    ids: impl IntoIterator<Item = &'a str>,
) -> Option<PaneClaudeSession> {
    let mut seen: Vec<&str> = Vec::new();
    let cands = ids.into_iter().filter_map(|id| {
        if seen.contains(&id) {
            return None;
        }
        seen.push(id);
        let session = read_pane_session(id)?;
        Some((marker_mtime(id).unwrap_or(SystemTime::UNIX_EPOCH), session))
    });
    newest(cands)
}

/// Remove the markers of ids whose process is gone. `SessionEnd` runs only on a clean
/// exit; a session a restart killed leaves its marker behind, where the next lookup would
/// read it as live and resume a stale conversation (or its stale cwd) into the pane.
/// A missing file is the normal case, not an error.
#[tracing::instrument(level = "debug", skip(ids))]
pub fn remove_pane_markers<'a>(ids: impl IntoIterator<Item = &'a str>) {
    for id in ids {
        match fs::remove_file(marker_path(id)) {
            Ok(()) => tracing::debug!(pane_id = id, "claude marker removed"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::debug!(pane_id = id, error = %e, "claude marker not removed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn newest_marker_wins_and_first_takes_a_tie() {
        let t0 = SystemTime::UNIX_EPOCH;
        let t1 = t0 + Duration::from_secs(10);
        // A stale alias marker (older) loses to the uid marker the restart wrote later.
        assert_eq!(newest([(t0, "alias"), (t1, "uid")]), Some("uid"));
        assert_eq!(newest([(t1, "alias"), (t0, "uid")]), Some("alias"));
        // Same second: the id asked for first (the alias) keeps its precedence.
        assert_eq!(newest([(t1, "alias"), (t1, "uid")]), Some("alias"));
        assert_eq!(newest(Vec::<(SystemTime, &str)>::new()), None);
    }

    #[test]
    fn accepts_uuid_shaped_ids_only() {
        assert!(valid_session_id("0198c4a2-1f2e-4d3c-8a5b-9e7f6c5d4b3a"));
        assert!(valid_session_id("deadbeef"));
        // Shell metacharacters, spaces, quotes — all refused.
        assert!(!valid_session_id("abc; rm -rf /"));
        assert!(!valid_session_id("abc def"));
        assert!(!valid_session_id("$(boom)"));
        assert!(!valid_session_id("ab")); // too short
        assert!(!valid_session_id(&"a".repeat(65))); // too long
    }

    #[test]
    fn resume_cwd_gate() {
        assert!(valid_resume_cwd("/home/me/dev/x"));
        assert!(valid_resume_cwd("/tmp/a b/c")); // spaces fine inside single quotes
        assert!(!valid_resume_cwd("relative/path"));
        assert!(!valid_resume_cwd("/has'quote"));
        assert!(!valid_resume_cwd("/has\nnewline"));
        assert!(!valid_resume_cwd(""));
    }

    #[test]
    fn parses_marker_json_shape() {
        let parsed: PaneClaudeSession = serde_json::from_str(
            r#"{ "sessionId": "0198c4a2-1f2e-4d3c-8a5b-9e7f6c5d4b3a", "cwd": "/w", "configDir": "/home/me/.claude-alt" }"#,
        )
        .unwrap();
        assert_eq!(parsed.session_id, "0198c4a2-1f2e-4d3c-8a5b-9e7f6c5d4b3a");
        assert_eq!(parsed.cwd, "/w");
        assert_eq!(parsed.config_dir, "/home/me/.claude-alt");
        // cwd + configDir are optional — an older/minimal hook payload (pre-multi-account)
        // still parses, defaulting to the default account.
        let bare: PaneClaudeSession =
            serde_json::from_str(r#"{ "sessionId": "deadbeef" }"#).unwrap();
        assert_eq!(bare.cwd, "");
        assert_eq!(bare.config_dir, "");
    }

    #[test]
    fn config_dir_gate() {
        assert!(valid_config_dir("/home/me/.claude-alt"));
        assert!(!valid_config_dir("")); // empty = no config dir, not "valid"
        assert!(!valid_config_dir("relative/.claude"));
        assert!(!valid_config_dir("/has'quote"));
    }
}
