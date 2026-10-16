//! The subagents a pane's Claude session has spawned — the model behind subagent panes
//! (`docs/subagent-panes-design.md` §1).
//!
//! A Claude Code subagent (the Agent tool) is in-process: it has no terminal, only a transcript.
//! Two sources describe it:
//!
//! - **The hook.** `resources/claude/hp-claude-session-hook.sh` writes
//!   `<state>/claude-subagents/<pane-id>/<agent-id>.json` on `SubagentStart`/`SubagentStop`:
//!
//!   ```json
//!   { "agentId": "a401…", "agentType": "Explore", "transcript": "…/agent-a401….jsonl",
//!     "event": "SubagentStop", "startedAt": 1791561624010, "stoppedAt": 1791561624058 }
//!   ```
//!
//!   (epoch ms), and `_turn.json` = `{ "endedAt": … }` on `Stop`, the orchestrator's turn end.
//! - **The transcript directory** — the fallback for a session that began before the hook was
//!   installed, and the source of `description`/`spawnDepth` for hook records:
//!   `<configDir>/projects/<encoded cwd>/<sessionId>/subagents/agent-<id>.meta.json` beside
//!   `agent-<id>.jsonl`.
//!
//! [`subagents_for_pane`] joins both by agent id, scoped to the pane's live conversation (its
//! session marker): records left in the pane's hook directory by an earlier conversation are
//! dropped, and a pane with no marker has no subagents. Records finished for longer than
//! [`ARCHIVE_AFTER`] are moved to `<pane-id>/_archive/` so a scan reopens only the live few
//! (B8, `docs/incidents/2026-10-14-subagent-scan-main-thread/`).

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::claude_history::{claude_projects_root, encode_path_str};
use crate::claude_panes::read_pane_session;
use crate::persistence::paths;

/// How recently a fallback-only subagent's transcript must have grown for it to count as
/// active. The plan leaves N open; a subagent between tool calls is silent for a few seconds
/// at most, and a hook-tracked one never consults this.
pub const GROWTH_IDLE: Duration = Duration::from_secs(10);

/// One subagent of a pane's Claude session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentRecord {
    pub agent_id: String,
    pub agent_type: String,
    /// From `meta.json`; falls back to `agent_type`.
    pub description: String,
    pub spawn_depth: u32,
    /// The subagent's own transcript, `agent-<id>.jsonl`.
    pub transcript: PathBuf,
    /// When the hook saw `SubagentStart`. `None` for a fallback-only record.
    pub started_at: Option<SystemTime>,
    /// When the hook saw `SubagentStop`. `Some` ⇒ finished.
    pub stopped_at: Option<SystemTime>,
    /// The transcript's mtime.
    pub last_growth: Option<SystemTime>,
}

impl SubagentRecord {
    /// Whether the subagent is still working: not stopped, and either hook-started or its
    /// transcript grew within `idle`. A stopped subagent is never active, however recently its
    /// transcript was written; a fallback-only record (no hook) has only growth to go on.
    /// Background subagents that outlive the orchestrator's turn stay active by the same rule.
    #[tracing::instrument(level = "debug", ret)]
    pub fn is_active(&self, now: SystemTime, idle: Duration) -> bool {
        if self.stopped_at.is_some() {
            return false;
        }
        if self.started_at.is_some() {
            return true;
        }
        self.last_growth.is_some_and(|g| {
            // A transcript stamped in the future (clock skew) counts as just grown.
            now.duration_since(g).map_or(true, |age| age <= idle)
        })
    }
}

/// Whether the tab's "hide subagents" toggle breathes: the tab is current and any of its
/// subagents is active. `hidden` is deliberately ignored — the point of breathing is to show
/// work going on in panes the user cannot see.
#[tracing::instrument(level = "debug", ret)]
pub fn toggle_breathes(tab_is_current: bool, any_active: bool, hidden: bool) -> bool {
    let _ = hidden;
    tab_is_current && any_active
}

/// The hook's per-agent record, as written by the script.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HookRecord {
    agent_id: String,
    #[serde(default)]
    agent_type: String,
    #[serde(default)]
    transcript: String,
    #[serde(default)]
    started_at: Option<u64>,
    #[serde(default)]
    stopped_at: Option<u64>,
}

/// The fields of `agent-<id>.meta.json` this model uses.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct AgentMeta {
    agent_type: String,
    description: String,
    spawn_depth: u32,
}

/// The hook's `_turn.json`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TurnRecord {
    ended_at: u64,
}

fn from_ms(ms: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(ms)
}

fn mtime(p: &Path) -> Option<SystemTime> {
    fs::metadata(p).ok()?.modified().ok()
}

/// `agent-<id>.jsonl` → `agent-<id>.meta.json`.
fn meta_path(transcript: &Path) -> PathBuf {
    transcript.with_extension("meta.json")
}

fn read_meta(transcript: &Path) -> Option<AgentMeta> {
    serde_json::from_str(&fs::read_to_string(meta_path(transcript)).ok()?).ok()
}

/// The session id a subagent transcript belongs to: `…/<sessionId>/subagents/agent-<id>.jsonl`.
fn transcript_session(transcript: &Path) -> Option<&str> {
    let subagents = transcript.parent()?;
    if subagents.file_name()? != "subagents" {
        return None;
    }
    subagents.parent()?.file_name()?.to_str()
}

/// Build a record for `agent_id` whose transcript is `transcript`, enriched from its meta.
fn record(agent_id: String, agent_type: String, transcript: PathBuf) -> SubagentRecord {
    let meta = read_meta(&transcript).unwrap_or_default();
    let agent_type = if agent_type.is_empty() {
        meta.agent_type
    } else {
        agent_type
    };
    let description = if meta.description.is_empty() {
        agent_type.clone()
    } else {
        meta.description
    };
    SubagentRecord {
        agent_id,
        agent_type,
        description,
        spawn_depth: meta.spawn_depth,
        last_growth: mtime(&transcript),
        transcript,
        started_at: None,
        stopped_at: None,
    }
}

/// The subagents described by a hook directory (`<state>/claude-subagents/<pane-id>`) and a
/// transcript directory (`…/<sessionId>/subagents`), joined by agent id and ordered by start
/// (hook start time, else the meta file's mtime — written once, at spawn).
///
/// With a `transcript_dir`, hook records whose transcript belongs to a different session are
/// dropped: the pane id is stable, so a second conversation in the same pane would otherwise
/// inherit the first one's subagents. With `None` the hook records are returned unfiltered —
/// scoping is then the caller's business. Missing directories and malformed files are skipped.
#[tracing::instrument(level = "debug", ret)]
pub fn subagents_from(hook_dir: &Path, transcript_dir: Option<&Path>) -> Vec<SubagentRecord> {
    scan(hook_dir, transcript_dir, None)
}

/// The subdirectory of a hook directory that holds finished records, moved there by
/// [`subagents_from_archiving`]. The `_` prefix keeps every scan from reading it as a record.
pub const ARCHIVE_DIR: &str = "_archive";

/// How long a finished record stays in the hook directory before it is archived: long enough
/// for every consumer of a scan to have seen it `finished` once (the GUI reconciles at least
/// once a second and rescans within a few seconds), short enough that a busy session does not
/// pile up hundreds of records that every scan reopens (B8: 1 448 of 1 451 records on disk
/// were finished, and the 1 Hz scan of them held the UI thread a third of the time).
pub const ARCHIVE_AFTER: Duration = Duration::from_secs(30);

/// [`subagents_from`], and move every hook record whose `stoppedAt` is before `archive_before`
/// into `hook_dir/_archive/`. The record is still part of this scan's result (it is finished,
/// and the consumer closes its pane on seeing that); the next scan no longer opens it. The
/// archive's file names stay known to the transcript-directory fallback, so an archived
/// subagent is not rediscovered from its meta file every scan. One `rename` per record,
/// once in its life, replaces an `open`+`read`+`close` per record per scan.
#[tracing::instrument(level = "debug", ret)]
pub fn subagents_from_archiving(
    hook_dir: &Path,
    transcript_dir: Option<&Path>,
    archive_before: SystemTime,
) -> Vec<SubagentRecord> {
    scan(hook_dir, transcript_dir, Some(archive_before))
}

/// The agent ids whose hook records sit in `hook_dir/_archive/` — file names only, no opens.
#[tracing::instrument(level = "debug", ret)]
fn archived_ids(hook_dir: &Path) -> HashSet<String> {
    fs::read_dir(hook_dir.join(ARCHIVE_DIR))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            let id = name.to_str()?.strip_suffix(".json")?;
            (!id.is_empty()).then(|| id.to_string())
        })
        .collect()
}

/// Move `path` (a hook record) into `hook_dir/_archive/`, creating the directory on first use.
fn archive_record(hook_dir: &Path, path: &Path) {
    let Some(name) = path.file_name() else {
        return;
    };
    let dir = hook_dir.join(ARCHIVE_DIR);
    if let Err(e) = fs::create_dir_all(&dir).and_then(|()| fs::rename(path, dir.join(name))) {
        tracing::debug!(path = %path.display(), error = %e, "subagent record not archived");
    }
}

#[tracing::instrument(level = "debug", ret)]
fn scan(
    hook_dir: &Path,
    transcript_dir: Option<&Path>,
    archive_before: Option<SystemTime>,
) -> Vec<SubagentRecord> {
    let session = transcript_dir
        .and_then(|d| (d.file_name()? == "subagents").then_some(d)?.parent()?.file_name())
        .and_then(|s| s.to_str());
    let mut by_id: BTreeMap<String, (Option<SystemTime>, SubagentRecord)> = BTreeMap::new();

    for entry in fs::read_dir(hook_dir).into_iter().flatten().flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('_') || path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(h) = fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<HookRecord>(&s).ok())
        else {
            continue;
        };
        // Finished long enough ago that every consumer has seen it: out of the scan path.
        // Records of an earlier conversation in this pane and Claude Code's bare internal
        // stops are finished too, and leave with the rest.
        if let (Some(before), Some(stopped)) = (archive_before, h.stopped_at.map(from_ms)) {
            if stopped < before {
                archive_record(hook_dir, &path);
            }
        }
        let transcript = PathBuf::from(&h.transcript);
        if transcript_dir.is_some() && transcript_session(&transcript) != session {
            continue;
        }
        let mut r = record(h.agent_id.clone(), h.agent_type, transcript);
        r.started_at = h.started_at.map(from_ms);
        r.stopped_at = h.stopped_at.map(from_ms);
        // Claude Code fires a bare SubagentStop for its own internal agents: no start, no type,
        // no transcript. They are not the user's subagents, so they get no pane.
        if r.started_at.is_none() && r.agent_type.is_empty() && r.last_growth.is_none() {
            continue;
        }
        by_id.insert(h.agent_id, (r.started_at, r));
    }

    if let Some(dir) = transcript_dir {
        // An archived subagent is finished and already known; its meta file must not bring
        // it back as a "fallback-only" record (one open per archived agent per scan).
        let archived = archived_ids(hook_dir);
        for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
            let name = entry.file_name();
            let Some(id) = name
                .to_str()
                .and_then(|n| n.strip_prefix("agent-")?.strip_suffix(".meta.json"))
            else {
                continue;
            };
            if id.is_empty() || by_id.contains_key(id) || archived.contains(id) {
                continue;
            }
            let r = record(id.to_string(), String::new(), dir.join(format!("agent-{id}.jsonl")));
            by_id.insert(id.to_string(), (mtime(&entry.path()), r));
        }
    }

    let mut out: Vec<(Option<SystemTime>, SubagentRecord)> = by_id.into_values().collect();
    // Stable sort over an id-ordered list: equal (or unknown) starts keep id order.
    out.sort_by_key(|(start, _)| *start);
    out.into_iter().map(|(_, r)| r).collect()
}

/// The transcript directory of `pane_id`'s live conversation, from its session marker:
/// `<configDir or ~/.claude>/projects/<encoded cwd>/<sessionId>/subagents`.
#[tracing::instrument(level = "debug", ret)]
fn transcript_dir_for(pane_id: &str) -> Option<PathBuf> {
    let s = read_pane_session(pane_id)?;
    let projects = if s.config_dir.is_empty() {
        claude_projects_root()?
    } else {
        PathBuf::from(&s.config_dir).join("projects")
    };
    Some(
        projects
            .join(encode_path_str(&s.cwd))
            .join(&s.session_id)
            .join("subagents"),
    )
}

/// The subagents of the Claude conversation live in `pane_id` — hook records ∪ the transcript
/// directory, by agent id, ordered by start. Empty when the pane has no session marker (no
/// live conversation, so nothing to show).
#[tracing::instrument(level = "debug", ret)]
pub fn subagents_for_pane(pane_id: &str) -> Vec<SubagentRecord> {
    let Some(dir) = transcript_dir_for(pane_id) else {
        return Vec::new();
    };
    let cutoff = SystemTime::now()
        .checked_sub(ARCHIVE_AFTER)
        .unwrap_or(UNIX_EPOCH);
    subagents_from_archiving(&paths::claude_subagents_dir().join(pane_id), Some(&dir), cutoff)
}

/// Whether a finished subagent's transcript ends in an error: its last assistant record is
/// Claude Code's API-error message (`isApiErrorMessage`) or carries an `error`. Hooks report no
/// subagent failure of their own, so this is what "a helper hit an error" means for the
/// attention glow (`docs/attention-glow-spec.md` §7, Bert ruling 2026-10-16). Reads only the
/// tail; an unreadable transcript is not an error.
#[tracing::instrument(level = "debug", ret)]
pub fn transcript_ended_in_error(path: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    const TAIL: u64 = 64 * 1024;
    let Ok(mut f) = fs::File::open(path) else {
        return false;
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if f.seek(SeekFrom::Start(len.saturating_sub(TAIL))).is_err() {
        return false;
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return false;
    }
    let text = String::from_utf8_lossy(&buf);
    text.lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
        .find(|v| v.get("type").and_then(|t| t.as_str()) == Some("assistant"))
        .is_some_and(|v| {
            v.get("isApiErrorMessage").and_then(|b| b.as_bool()) == Some(true)
                || v.get("error").is_some_and(|e| !e.is_null())
        })
}

/// When the orchestrator's last turn in `hook_dir` ended (`_turn.json`), if the hook recorded one.
#[tracing::instrument(level = "debug", ret)]
pub fn turn_end_in(hook_dir: &Path) -> Option<SystemTime> {
    let text = fs::read_to_string(hook_dir.join("_turn.json")).ok()?;
    Some(from_ms(serde_json::from_str::<TurnRecord>(&text).ok()?.ended_at))
}

/// When `pane_id`'s orchestrator last ended a turn (the hook's `_turn.json`). No longer gates
/// closing a finished subagent pane — that closes at once — but the marker is still written.
#[tracing::instrument(level = "debug", ret)]
pub fn last_turn_end(pane_id: &str) -> Option<SystemTime> {
    turn_end_in(&paths::claude_subagents_dir().join(pane_id))
}

/// Remove the hook directories of panes that no longer exist (see
/// [`crate::claude_panes::prune_markers`], which calls this with the same `live` set). Returns
/// the pane ids pruned.
#[tracing::instrument(level = "debug", ret, skip(live))]
pub fn prune_dirs_in(dir: &Path, live: &HashSet<String>) -> Vec<String> {
    let mut removed = Vec::new();
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        let Some(pane) = path.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
            continue;
        };
        if !path.is_dir() || live.contains(&pane) {
            continue;
        }
        match fs::remove_dir_all(&path) {
            Ok(()) => removed.push(pane),
            Err(e) => tracing::debug!(path = %path.display(), error = %e, "subagent dir not pruned"),
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_transcript_ending_in_an_api_error_is_an_error_and_a_normal_one_is_not() {
        let dir = std::env::temp_dir().join(format!("hp-sub-err-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let ok = dir.join("ok.jsonl");
        fs::write(
            &ok,
            "{\"type\":\"user\",\"message\":{}}\n{\"type\":\"assistant\",\"message\":{\"content\":[]}}\n",
        )
        .unwrap();
        let bad = dir.join("bad.jsonl");
        fs::write(
            &bad,
            "{\"type\":\"assistant\",\"message\":{}}\n{\"type\":\"assistant\",\"isApiErrorMessage\":true,\"message\":{}}\n{\"type\":\"system\"}\n",
        )
        .unwrap();
        let errfield = dir.join("err.jsonl");
        fs::write(&errfield, "{\"type\":\"assistant\",\"error\":\"rate_limit\"}\n").unwrap();
        assert!(!transcript_ended_in_error(&ok));
        assert!(transcript_ended_in_error(&bad));
        assert!(transcript_ended_in_error(&errfield));
        assert!(!transcript_ended_in_error(&dir.join("missing.jsonl")));
        let _ = fs::remove_dir_all(&dir);
    }

    const AGENT: &str = "a401878d07b43ff1c";
    const SESSION: &str = "fd3804e5-9743-4a44-afde-65ea8705ee3c";

    fn fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude-hooks")
    }

    fn fixture_json(name: &str) -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(fixtures().join(name)).unwrap()).unwrap()
    }

    /// The captured session's transcript directory (read-only fixture).
    fn fixture_transcripts() -> PathBuf {
        fixtures().join(format!("projects/-home-u-proj/{SESSION}/subagents"))
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "hp-subagents-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(path: &Path, v: serde_json::Value) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, v.to_string()).unwrap();
    }

    fn rec(started: bool, stopped: bool, grew_secs_ago: Option<u64>, now: SystemTime) -> SubagentRecord {
        SubagentRecord {
            agent_id: "a".into(),
            agent_type: "Explore".into(),
            description: "Explore".into(),
            spawn_depth: 1,
            transcript: PathBuf::from("/x/agent-a.jsonl"),
            started_at: started.then(|| now - Duration::from_secs(60)),
            stopped_at: stopped.then(|| now - Duration::from_secs(5)),
            last_growth: grew_secs_ago.map(|s| now - Duration::from_secs(s)),
        }
    }

    #[test]
    fn the_capture_pins_the_transcript_derivation() {
        // SubagentStart carries only the parent's transcript; the subagent's own is derived
        // from it, and SubagentStop's agent_transcript_path is exactly that derivation.
        let start = fixture_json("subagent-start.json");
        let stop = fixture_json("subagent-stop.json");
        assert!(start.get("agent_transcript_path").is_none());
        let parent = Path::new(start["transcript_path"].as_str().unwrap());
        let derived = parent
            .parent()
            .unwrap()
            .join(start["session_id"].as_str().unwrap())
            .join("subagents")
            .join(format!("agent-{}.jsonl", start["agent_id"].as_str().unwrap()));
        assert_eq!(
            derived,
            Path::new(stop["agent_transcript_path"].as_str().unwrap())
        );
        assert!(fixture_json("stop.json").get("agent_id").is_none());
    }

    #[test]
    fn the_transcript_dir_fallback_reads_meta() {
        let empty = tmp("nohook");
        let got = subagents_from(&empty, Some(&fixture_transcripts()));
        assert_eq!(got.len(), 1);
        let r = &got[0];
        assert_eq!(r.agent_id, AGENT);
        assert_eq!(r.agent_type, "Explore");
        assert_eq!(r.description, "List files in current directory");
        assert_eq!(r.spawn_depth, 1);
        assert_eq!(r.transcript, fixture_transcripts().join(format!("agent-{AGENT}.jsonl")));
        assert!(r.started_at.is_none() && r.stopped_at.is_none());
        assert!(r.last_growth.is_some());
        let _ = fs::remove_dir_all(&empty);
    }

    #[test]
    fn a_hook_record_is_enriched_from_meta_and_matches_the_fallback_set() {
        let hook = tmp("hook");
        let transcript = fixture_transcripts().join(format!("agent-{AGENT}.jsonl"));
        write(
            &hook.join(format!("{AGENT}.json")),
            serde_json::json!({
                "agentId": AGENT, "agentType": "Explore",
                "transcript": transcript.to_string_lossy(), "event": "SubagentStop",
                "startedAt": 1_000, "stoppedAt": 2_000,
            }),
        );
        write(&hook.join("_turn.json"), serde_json::json!({ "endedAt": 3_000 }));

        let both = subagents_from(&hook, Some(&fixture_transcripts()));
        let fallback = subagents_from(&tmp("empty"), Some(&fixture_transcripts()));
        let hook_only = subagents_from(&hook, None);
        let ids = |v: &[SubagentRecord]| v.iter().map(|r| r.agent_id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&both), ids(&fallback), "hook and fallback find the same set");
        assert_eq!(ids(&hook_only), ids(&fallback));

        let r = &both[0];
        assert_eq!(r.description, "List files in current directory");
        assert_eq!(r.started_at, Some(from_ms(1_000)));
        assert_eq!(r.stopped_at, Some(from_ms(2_000)));
        assert_eq!(turn_end_in(&hook), Some(from_ms(3_000)));
        let _ = fs::remove_dir_all(&hook);
    }

    #[test]
    fn hook_records_from_another_session_are_dropped() {
        let hook = tmp("stale");
        write(
            &hook.join("old.json"),
            serde_json::json!({
                "agentId": "old", "agentType": "Plan",
                "transcript": "/home/u/.claude/projects/-home-u-proj/00000000-0000-0000-0000-000000000000/subagents/agent-old.jsonl",
                "startedAt": 1_000,
            }),
        );
        let got = subagents_from(&hook, Some(&fixture_transcripts()));
        assert_eq!(
            got.iter().map(|r| r.agent_id.as_str()).collect::<Vec<_>>(),
            [AGENT],
            "only the live session's subagents"
        );
        // Without a session to scope by, the caller gets everything.
        assert_eq!(subagents_from(&hook, None).len(), 1);
        let _ = fs::remove_dir_all(&hook);
    }

    #[test]
    fn records_are_ordered_by_start_and_a_missing_meta_falls_back_to_the_type() {
        let hook = tmp("order");
        for (id, at) in [("late", 5_000u64), ("early", 1_000)] {
            write(
                &hook.join(format!("{id}.json")),
                serde_json::json!({
                    "agentId": id, "agentType": "general-purpose",
                    "transcript": format!("/nowhere/{SESSION}/subagents/agent-{id}.jsonl"),
                    "startedAt": at,
                }),
            );
        }
        let got = subagents_from(&hook, None);
        assert_eq!(got.iter().map(|r| r.agent_id.as_str()).collect::<Vec<_>>(), ["early", "late"]);
        assert_eq!(got[0].description, "general-purpose");
        assert_eq!(got[0].last_growth, None);
        let _ = fs::remove_dir_all(&hook);
    }

    #[test]
    fn a_bare_stop_with_no_type_or_transcript_is_an_internal_agent_and_dropped() {
        let hook = tmp("bare-stop");
        write(
            &hook.join("internal.json"),
            serde_json::json!({
                "agentId": "internal", "agentType": "", "event": "SubagentStop",
                "transcript": format!("/nowhere/{SESSION}/subagents/agent-internal.jsonl"),
                "startedAt": null, "stoppedAt": 2_000,
            }),
        );
        assert!(subagents_from(&hook, None).is_empty());
        let _ = fs::remove_dir_all(&hook);
    }

    #[test]
    fn missing_dirs_and_garbage_yield_nothing() {
        let hook = tmp("garbage");
        fs::write(hook.join("bad.json"), "{not json").unwrap();
        fs::write(hook.join("x.json.123.tmp"), "{}").unwrap();
        assert!(subagents_from(&hook, None).is_empty());
        assert!(subagents_from(&hook.join("nope"), Some(&hook.join("nope"))).is_empty());
        assert_eq!(turn_end_in(&hook), None);
        let _ = fs::remove_dir_all(&hook);
    }

    #[test]
    fn is_active_follows_the_hook_window_then_growth() {
        let now = SystemTime::now();
        let idle = Duration::from_secs(10);
        // (started, stopped, grew secs ago) → active
        let table = [
            (true, false, None, true),       // hook-started, running
            (true, false, Some(600), true),  // hook-started, quiet transcript: still running
            (true, true, Some(0), false),    // just stopped, transcript grew 0s ago: finished
            (false, true, Some(1), false),   // stop without a start: finished
            (false, false, Some(3), true),   // fallback-only, growing
            (false, false, Some(30), false), // fallback-only, quiet
            (false, false, None, false),     // fallback-only, no transcript
        ];
        for (started, stopped, grew, want) in table {
            assert_eq!(
                rec(started, stopped, grew, now).is_active(now, idle),
                want,
                "started={started} stopped={stopped} grew={grew:?}"
            );
        }
        // A transcript dated in the future (clock skew) counts as just grown.
        let mut r = rec(false, false, None, now);
        r.last_growth = Some(now + Duration::from_secs(5));
        assert!(r.is_active(now, idle));
    }

    #[test]
    fn toggle_breathes_truth_table() {
        for current in [false, true] {
            for active in [false, true] {
                for hidden in [false, true] {
                    assert_eq!(
                        toggle_breathes(current, active, hidden),
                        current && active,
                        "current={current} active={active} hidden={hidden}"
                    );
                }
            }
        }
        // The case the feature exists for: panes hidden, work going on, still breathing.
        assert!(toggle_breathes(true, true, true));
    }

    #[test]
    fn prune_removes_only_dead_pane_dirs() {
        let root = tmp("prune");
        fs::create_dir_all(root.join("live")).unwrap();
        fs::create_dir_all(root.join("dead")).unwrap();
        fs::write(root.join("stray.json"), "{}").unwrap();
        let live: HashSet<String> = ["live".to_string()].into();
        assert_eq!(prune_dirs_in(&root, &live), ["dead"]);
        assert!(root.join("live").is_dir() && !root.join("dead").exists());
        assert!(root.join("stray.json").exists(), "files are not pane dirs");
        let _ = fs::remove_dir_all(&root);
    }

    /// B8: a record finished before the cutoff is moved to `_archive/` by the archiving scan —
    /// still reported that once (so a consumer sees it finished), gone from the next scan,
    /// and not rediscovered from its meta file by the transcript-directory fallback. A record
    /// that finished after the cutoff, or never, stays. The plain scan moves nothing.
    #[test]
    fn finished_records_past_the_cutoff_are_archived_once_and_stay_known() {
        let root = tmp("archive");
        let hook = root.join("hook");
        let transcripts = root.join("projects/-x").join(SESSION).join("subagents");
        fs::create_dir_all(&hook).unwrap();
        fs::create_dir_all(&transcripts).unwrap();
        let ms = |t: SystemTime| t.duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        let now = SystemTime::now();
        for (id, start, stop) in [
            ("done", ms(now) - 120_000, Some(ms(now) - 90_000)),
            ("fresh", ms(now) - 5_000, Some(ms(now) - 1_000)),
            ("live", ms(now) - 2_000, None),
        ] {
            let transcript = transcripts.join(format!("agent-{id}.jsonl"));
            fs::write(&transcript, "").unwrap();
            fs::write(
                transcripts.join(format!("agent-{id}.meta.json")),
                r#"{"agentType":"Explore","description":"d","spawnDepth":1}"#,
            )
            .unwrap();
            write(
                &hook.join(format!("{id}.json")),
                serde_json::json!({
                    "agentId": id, "agentType": "Explore",
                    "transcript": transcript.to_string_lossy(),
                    "startedAt": start, "stoppedAt": stop,
                }),
            );
        }
        let ids = |v: &[SubagentRecord]| v.iter().map(|r| r.agent_id.clone()).collect::<Vec<_>>();

        // The plain scan reads and reports everything, and moves nothing.
        assert_eq!(ids(&subagents_from(&hook, Some(&transcripts))), ["done", "fresh", "live"]);
        assert!(hook.join("done.json").exists() && !hook.join(ARCHIVE_DIR).exists());

        let cutoff = now - ARCHIVE_AFTER;
        let first = subagents_from_archiving(&hook, Some(&transcripts), cutoff);
        assert_eq!(ids(&first), ["done", "fresh", "live"], "the archiving scan still reports it once");
        assert!(first[0].stopped_at.is_some());
        assert!(!hook.join("done.json").exists(), "moved out of the scan path");
        assert!(hook.join(ARCHIVE_DIR).join("done.json").exists());
        assert!(hook.join("fresh.json").exists(), "finished after the cutoff: not yet");

        let second = subagents_from_archiving(&hook, Some(&transcripts), cutoff);
        assert_eq!(ids(&second), ["fresh", "live"], "archived, and not back via its meta file");
        assert_eq!(ids(&subagents_from(&hook, Some(&transcripts))), ["fresh", "live"]);
        let _ = fs::remove_dir_all(&root);
    }

    /// The real hook script, fed the captured payloads, under a scratch `$HOME`.
    #[cfg(unix)]
    mod script {
        use super::*;
        use std::io::Write;
        use std::process::{Command, Stdio};

        fn script() -> PathBuf {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../../resources/claude/hp-claude-session-hook.sh")
        }

        /// The state dir the script resolves under `home` (mirrors `paths::state_dir`).
        fn state(home: &Path) -> PathBuf {
            if cfg!(target_os = "macos") {
                home.join("Library/Application Support/avada")
            } else {
                home.join(".local/state/avada")
            }
        }

        /// Run the hook with `payload` on stdin; returns its stdout.
        fn run(home: &Path, payload: &str) -> String {
            let mut child = Command::new("sh")
                .arg(script())
                .env("HOME", home)
                .env_remove("XDG_STATE_HOME")
                .env_remove("CLAUDE_CONFIG_DIR")
                .env_remove("HP_CTL")
                .env("AVADA_PANE_ID", "p7")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .expect("sh runs");
            child.stdin.take().unwrap().write_all(payload.as_bytes()).unwrap();
            let out = child.wait_with_output().unwrap();
            assert!(out.status.success(), "the hook always exits 0");
            String::from_utf8(out.stdout).unwrap()
        }

        fn fixture(name: &str) -> String {
            fs::read_to_string(fixtures().join(name)).unwrap()
        }

        #[test]
        fn captured_payloads_become_a_record_silently() {
            let home = tmp("script");
            let hook_dir = state(&home).join("claude-subagents/p7");

            assert_eq!(run(&home, &fixture("subagent-start.json")), "", "stdout must stay empty");
            let started = subagents_from(&hook_dir, None);
            assert_eq!(started.len(), 1);
            let stop = fixture_json("subagent-stop.json");
            assert_eq!(
                started[0].transcript,
                Path::new(stop["agent_transcript_path"].as_str().unwrap()),
                "the Start-time derivation is the path Stop reports"
            );
            assert_eq!(started[0].agent_type, "Explore");
            assert!(started[0].started_at.is_some() && started[0].stopped_at.is_none());

            assert_eq!(run(&home, &fixture("subagent-stop.json")), "");
            let stopped = subagents_from(&hook_dir, None);
            assert_eq!(stopped[0].started_at, started[0].started_at, "Stop merges, keeps the start");
            assert!(stopped[0].stopped_at.is_some());

            assert_eq!(run(&home, &fixture("stop.json")), "");
            assert!(turn_end_in(&hook_dir).is_some());
            let _ = fs::remove_dir_all(&home);
        }

        #[test]
        fn only_session_start_touches_the_session_marker() {
            let home = tmp("marker");
            let marker = state(&home).join("claude-sessions/p7.json");
            let mut start = fixture_json("subagent-start.json");
            start["hook_event_name"] = "SessionStart".into();
            assert_eq!(run(&home, &start.to_string()), "");
            let bytes = fs::read(&marker).expect("SessionStart writes the marker");
            let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(v["sessionId"], SESSION);
            let before = fs::metadata(&marker).unwrap().modified().unwrap();

            std::thread::sleep(Duration::from_millis(20));
            for f in ["subagent-start.json", "subagent-stop.json", "stop.json"] {
                assert_eq!(run(&home, &fixture(f)), "");
            }
            let mut other = fixture_json("stop.json");
            other["hook_event_name"] = "PreToolUse".into();
            assert_eq!(run(&home, &other.to_string()), "");

            assert_eq!(fs::read(&marker).unwrap(), bytes, "marker bytes untouched");
            assert_eq!(fs::metadata(&marker).unwrap().modified().unwrap(), before, "mtime untouched");

            let mut end = fixture_json("stop.json");
            end["hook_event_name"] = "SessionEnd".into();
            assert_eq!(run(&home, &end.to_string()), "");
            assert!(!marker.exists(), "SessionEnd removes the marker");
            let _ = fs::remove_dir_all(&home);
        }

        #[test]
        fn a_hostile_agent_id_writes_nothing() {
            let home = tmp("hostile");
            let mut start = fixture_json("subagent-start.json");
            start["agent_id"] = "../../escape".into();
            assert_eq!(run(&home, &start.to_string()), "");
            assert!(!state(&home).join("claude-subagents/p7").exists());
            assert!(!state(&home).join("claude-subagents/escape.json").exists());
            let _ = fs::remove_dir_all(&home);
        }
    }
}
