//! The host service behind the `avada-tools` module: which AI CLIs this machine has, and
//! what resumable conversations each of them is holding.
//!
//! **Why this is host-side at all.** The tool *tabs* are a module's business — a rail entry,
//! a row list, a filter box, a click that opens a pane. None of that needs to be in the app.
//! The two things underneath it do:
//!
//! * **Detection** walks `PATH` and the well-known install directories, and it honours the
//!   human's per-tool override out of app settings ([`crate::tools::detect`]). A module has
//!   neither the settings map nor a reason to reimplement a `PATHEXT`-aware probe, and the
//!   contract already has a capability that names exactly this read: `settings.read`.
//! * **History** is ~4,000 lines of per-tool transcript parsing, one of them a SQLite
//!   reader, over stores that live in `~/.claude`, `~/.cursor` and `~/.copilot` — outside
//!   any workspace root, and shared with [`crate::speech::tailer`] and the session-inference
//!   machinery, which are unambiguously host code. Shipping a second copy into a module
//!   would fork the parsers, and handing a module the raw directories would need
//!   `fs.read_any` *and* the whole layout knowledge anyway.
//!
//! So the seam is: the host answers *what exists* and *how would I get back into it*; the
//! module decides what that looks like on the rail. This file is the "what exists" half,
//! served over `GET /tools` and `GET /tools/{tool}/sessions`.
//!
//! Nothing here spawns anything. A resume is a `newPane` through `POST /command`, which the
//! module asks for itself with `panes.spawn` — the same door the CLI uses.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock};

use serde::Serialize;

use crate::tools::detect::{self, Source};
use crate::tools::history::{ResumePlan, SessionProvider, ToolSession};
use crate::tools::registry::{self, ToolDef};

/// One row of the catalogue: a registry entry, plus whether this machine actually has it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolEntry {
    /// The stable registry id — what `/tools/{tool}/sessions` takes.
    pub id: String,
    /// Human-facing name.
    pub name: String,
    /// Brand accent as `#rrggbb`, so a tier-1 module can ask for its own colour without
    /// knowing the host's theme.
    pub brand: String,
    /// Whether a session-history provider exists for it. A tool with no provider still
    /// appears here — the honest answer is "installed, but nothing to list".
    #[serde(rename = "hasHistory")]
    pub has_history: bool,
    /// The resolved binary, absent when the tool was not found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// How it was found: `override`, `path`, `wellKnown` — or absent when it was not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<&'static str>,
}

/// One resumable conversation, as the control plane hands it out.
///
/// Deliberately *not* the panel's two shaped lines. Labels, relative times and the
/// "N messages" plural are presentation, and presentation is the module's half of the seam;
/// what travels is the facts it needs to shape them. The one thing the host does decide is
/// the resume verdict, because deciding it costs a binary probe and a `stat` per row and
/// only the host holds the override map that makes the probe honest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionRow {
    /// The tool's own resume key.
    pub id: String,
    /// The project directory the conversation happened in.
    pub project: String,
    /// Whether `project` was read out of the transcript or reconstructed. A guess is a
    /// label, never a directory to spawn in — and a blocked row says so.
    #[serde(rename = "projectExact")]
    pub project_exact: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Epoch milliseconds.
    #[serde(rename = "startedAt", skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    /// The transcript's own summary, when it wrote one.
    pub summary: String,
    /// The opening prompt, truncated. `full_text` is deliberately never sent: the row needs
    /// a label, not the conversation, and a rail entry is not a reason to stream a
    /// transcript across a socket.
    #[serde(rename = "firstUser")]
    pub first_user: String,
    #[serde(rename = "messageCount")]
    pub message_count: usize,
    /// How to get back in, when we can.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume: Option<Resume>,
    /// Why we cannot, when we cannot. Exactly one of `resume`/`blocked` is present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked: Option<String>,
    /// The Claude Desktop id for the same conversation, when its store also holds it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desktop: Option<String>,
}

/// A spawnable resume: exactly the `newPane` shape `POST /command` takes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Resume {
    /// An absolute program path, not a name — see [`crate::tools::history::ResumeCommand`].
    pub command: String,
    pub args: Vec<String>,
    pub cwd: String,
}

/// How long a transcript truncated for the row is allowed to be. Long enough to read as a
/// sentence, short enough that a thousand rows is still a small response.
const TEXT_CAP: usize = 240;

/// The catalogue, with this machine's answer for each entry.
#[tracing::instrument(level = "debug", ret)]
pub fn catalogue(overrides: &BTreeMap<String, String>) -> Vec<ToolEntry> {
    let found = detect::resolve_all(overrides);
    registry::TOOLS
        .iter()
        .map(|t| {
            let hit = found.get(t.id);
            ToolEntry {
                id: t.id.to_string(),
                name: t.name.to_string(),
                brand: brand_hex(t),
                has_history: t.has_history(),
                path: hit.map(|r| r.path.display().to_string()),
                source: hit.map(|r| source_name(r.source)),
            }
        })
        .collect()
}

/// `#rrggbb` for a registry entry's accent.
#[tracing::instrument(level = "debug", ret)]
fn brand_hex(t: &ToolDef) -> String {
    let (r, g, b) = t.brand;
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// The wire name for a detection source. camelCase, matching every other control-plane body.
#[tracing::instrument(level = "debug", ret)]
fn source_name(s: Source) -> &'static str {
    match s {
        Source::UserOverride => "override",
        Source::Path => "path",
        Source::WellKnown => "wellKnown",
    }
}

/// Why a `/tools/{tool}/sessions` request could not be answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionsError {
    /// No such id in [`registry::TOOLS`] — a typo, or a newer build's tool.
    UnknownTool,
    /// A real tool with no history provider. Not an error the caller can fix, and not the
    /// same thing as "no sessions": the distinction is what lets the module say
    /// "Avada cannot read Aider's history" instead of drawing an empty list that lies.
    NoProvider,
}

/// The provider serving `tool_id`, or `None` when none does.
///
/// The app's `history_scan::provider_for` is the same match; it stays there because that
/// thread owns its own long-lived providers and this one owns [`PROVIDERS`]. Both are three
/// lines of dispatch over the same three constructors, and neither can drift without the
/// registry's `HistoryKind` drifting first.
#[tracing::instrument(level = "debug")]
fn provider_for(
    tool_id: &str,
    overrides: &BTreeMap<String, String>,
) -> Option<Box<dyn SessionProvider + Send>> {
    use crate::tools::history::{claude, copilot, cursor};
    match tool_id {
        claude::TOOL_ID => Some(Box::new(claude::ClaudeProvider::with_overrides(
            overrides.clone(),
        ))),
        cursor::TOOL_ID => Some(Box::new(cursor::CursorProvider::with_overrides(
            overrides.clone(),
        ))),
        copilot::TOOL_ID => Some(Box::new(copilot::CopilotProvider::with_overrides(
            overrides.clone(),
        ))),
        _ => None,
    }
}

/// One provider per tool, kept alive across requests, keyed by the overrides it was built
/// with.
///
/// The fingerprint caches that make a warm re-scan cost milliseconds instead of seconds live
/// *inside* a provider, so a fresh one per request would make every poll a cold scan. Rebuilt
/// when the overrides change, because a provider decides resumability with the binary it was
/// handed and a human who edits that path must not keep getting the old verdict.
type Cached = (BTreeMap<String, String>, Box<dyn SessionProvider + Send>);
static PROVIDERS: OnceLock<Mutex<HashMap<String, Cached>>> = OnceLock::new();

fn providers() -> &'static Mutex<HashMap<String, Cached>> {
    PROVIDERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Every resumable conversation `tool_id` is holding, newest-first within each project.
///
/// Blocking and slow on a cold store (it walks a whole transcript tree); the control server
/// runs it on a blocking task for exactly that reason.
#[tracing::instrument(level = "debug")]
pub fn sessions(
    tool_id: &str,
    overrides: &BTreeMap<String, String>,
) -> Result<Vec<SessionRow>, SessionsError> {
    if registry::by_id(tool_id).is_none() {
        return Err(SessionsError::UnknownTool);
    }
    let mut guard = providers().lock().unwrap_or_else(|e| e.into_inner());
    if guard
        .get(tool_id)
        .is_some_and(|(built_with, _)| built_with != overrides)
    {
        guard.remove(tool_id);
    }
    if !guard.contains_key(tool_id) {
        let Some(p) = provider_for(tool_id, overrides) else {
            return Err(SessionsError::NoProvider);
        };
        guard.insert(tool_id.to_string(), (overrides.clone(), p));
    }
    let (_, provider) = guard.get_mut(tool_id).expect("just inserted");
    Ok(rows_from(provider.as_mut()))
}

/// Shape one provider's whole store into rows. Split out from [`sessions`] so the shaping —
/// the part with rules in it — is testable against a fake provider with no disk and no cache.
#[tracing::instrument(level = "debug", skip(provider))]
pub fn rows_from(provider: &mut dyn SessionProvider) -> Vec<SessionRow> {
    // One pass over Claude Desktop's store for the whole scan rather than one per row, and
    // only for the tool that has a desktop app at all.
    let desktop = if provider.id() == crate::tools::history::claude::TOOL_ID {
        crate::tools::claude_desktop::scan()
    } else {
        HashMap::new()
    };
    provider
        .scan()
        .iter()
        .map(|s| {
            let (resume, blocked) = match provider.resume(s) {
                ResumePlan::Ready(c) => (
                    Some(Resume {
                        command: c.program.display().to_string(),
                        args: c.args.clone(),
                        cwd: c.cwd.display().to_string(),
                    }),
                    None,
                ),
                ResumePlan::Blocked(b) => (None, Some(b.reason())),
            };
            row(s, resume, blocked, desktop.get(&s.id).cloned())
        })
        .collect()
}

#[tracing::instrument(level = "debug", ret)]
fn row(
    s: &ToolSession,
    resume: Option<Resume>,
    blocked: Option<String>,
    desktop: Option<String>,
) -> SessionRow {
    SessionRow {
        id: s.id.clone(),
        project: s.project.display().to_string(),
        project_exact: s.project_origin.is_exact(),
        branch: s.branch.clone().filter(|b| !b.is_empty()),
        started_at: s.started_at,
        summary: clip(&s.summary),
        first_user: clip(&s.first_user),
        message_count: s.message_count,
        resume,
        blocked,
        desktop,
    }
}

/// Trim and cap one line of transcript text. Counts *characters*, not bytes, so a truncation
/// can never split a multi-byte prompt down the middle.
#[tracing::instrument(level = "debug", ret)]
fn clip(s: &str) -> String {
    s.trim().chars().take(TEXT_CAP).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_history::{HistorySource, ProjectOrigin};
    use crate::tools::history::{ResumeBlocked, ResumeCommand};
    use std::path::PathBuf;

    fn session(id: &str) -> ToolSession {
        ToolSession {
            id: id.into(),
            source: HistorySource::Claude,
            project: PathBuf::from("/w"),
            project_origin: ProjectOrigin::TranscriptExact,
            branch: Some("main".into()),
            started_at: Some(1_700_000_000_000),
            summary: "  a summary  ".into(),
            first_user: "hello".into(),
            message_count: 3,
            full_text: "the whole conversation, which must not travel".into(),
        }
    }

    struct Fake {
        id: &'static str,
        sessions: Vec<ToolSession>,
        plan: ResumePlan,
    }

    impl SessionProvider for Fake {
        fn id(&self) -> &'static str {
            self.id
        }
        fn scan(&mut self) -> Vec<ToolSession> {
            self.sessions.clone()
        }
        fn resume(&self, _: &ToolSession) -> ResumePlan {
            self.plan.clone()
        }
    }

    fn ready() -> ResumePlan {
        ResumePlan::Ready(ResumeCommand {
            program: PathBuf::from("/opt/bin/claude"),
            args: vec!["--resume".into(), "abc".into()],
            cwd: PathBuf::from("/w"),
        })
    }

    #[test]
    fn a_resumable_row_carries_the_command_and_no_reason() {
        let mut p = Fake {
            // Not `claude`: the desktop store is a real disk read and this test is about
            // shaping, not about what is installed on the machine running it.
            id: "cursor-agent",
            sessions: vec![session("s1")],
            plan: ready(),
        };
        let rows = rows_from(&mut p);
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.id, "s1");
        assert_eq!(r.project, "/w");
        assert!(r.project_exact);
        assert_eq!(r.branch.as_deref(), Some("main"));
        assert_eq!(r.message_count, 3);
        assert!(r.blocked.is_none());
        let resume = r.resume.as_ref().expect("resumable");
        assert_eq!(resume.command, "/opt/bin/claude");
        assert_eq!(resume.args, vec!["--resume", "abc"]);
        assert_eq!(resume.cwd, "/w");
    }

    #[test]
    fn a_blocked_row_says_why_and_offers_nothing_to_spawn() {
        let mut p = Fake {
            id: "cursor-agent",
            sessions: vec![session("s1")],
            plan: ResumePlan::Blocked(ResumeBlocked::ToolNotInstalled {
                tool_id: "cursor-agent",
            }),
        };
        let rows = rows_from(&mut p);
        assert!(rows[0].resume.is_none());
        assert_eq!(
            rows[0].blocked.as_deref(),
            Some("cursor-agent was not found on this machine")
        );
    }

    #[test]
    fn the_transcript_itself_never_leaves_the_host() {
        let mut p = Fake {
            id: "cursor-agent",
            sessions: vec![session("s1")],
            plan: ready(),
        };
        let wire = serde_json::to_string(&rows_from(&mut p)).unwrap();
        assert!(
            !wire.contains("the whole conversation"),
            "full_text must not be serialized: {wire}"
        );
        // The label the module needs is there, trimmed.
        assert!(wire.contains("\"summary\":\"a summary\""), "{wire}");
    }

    #[test]
    fn a_long_prompt_is_clipped_by_characters_not_bytes() {
        let mut s = session("s1");
        s.first_user = "é".repeat(TEXT_CAP + 50);
        let mut p = Fake {
            id: "cursor-agent",
            sessions: vec![s],
            plan: ready(),
        };
        let rows = rows_from(&mut p);
        assert_eq!(rows[0].first_user.chars().count(), TEXT_CAP);
    }

    #[test]
    fn an_empty_branch_is_absent_rather_than_blank() {
        let mut s = session("s1");
        s.branch = Some(String::new());
        let mut p = Fake {
            id: "cursor-agent",
            sessions: vec![s],
            plan: ready(),
        };
        assert!(rows_from(&mut p)[0].branch.is_none());
    }

    #[test]
    fn the_catalogue_lists_every_registered_tool_and_names_an_override() {
        let overrides: BTreeMap<String, String> =
            [("claude".to_string(), "/opt/bin/claude".to_string())]
                .into_iter()
                .collect();
        let list = catalogue(&overrides);
        assert_eq!(list.len(), registry::TOOLS.len());
        let claude = list.iter().find(|t| t.id == "claude").expect("claude");
        assert_eq!(claude.path.as_deref(), Some("/opt/bin/claude"));
        assert_eq!(claude.source, Some("override"));
        assert!(claude.has_history);
        assert_eq!(claude.brand, "#d97757");
        // A registry entry with no provider is listed and honest about it.
        let aider = list.iter().find(|t| t.id == "aider").expect("aider");
        assert!(!aider.has_history);
    }

    #[test]
    fn an_unknown_tool_and_a_provider_less_tool_are_different_answers() {
        let ov = BTreeMap::new();
        assert_eq!(
            sessions("no-such-tool", &ov).unwrap_err(),
            SessionsError::UnknownTool
        );
        assert_eq!(
            sessions("aider", &ov).unwrap_err(),
            SessionsError::NoProvider
        );
    }
}
