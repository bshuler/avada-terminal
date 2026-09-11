//! Turning one tool's conversation list into the host's tier-1 [`Row`] list.
//!
//! A tier-1 module draws nothing: it hands the host a flat list of rows and the host
//! decides what that looks like on this platform, in this theme, at this DPI. Everything
//! this module wants to say about a conversation therefore has to fit into `label`,
//! `detail`, `marks` and `data`.
//!
//! The shaping rules here are not new. They are the ones the built-in tool tabs used, kept
//! character-for-character — `session_label`, `session_detail` and [`relative_time`] are
//! the panel's own functions, moved to the side of the seam that owns presentation. A row
//! that read differently after the extraction would be a regression the human would notice
//! before any test did.

use crate::api::Session;
use crate::app::State;
use avada_module_sdk::rail::Row;
use serde_json::{json, Value};
use std::path::Path;

/// The mark the host scrolls to. `host.rows.set` carries no selection field on purpose
/// (see `docs/module-contract.md` §10.1); a mark is how a module says "this one".
pub const MARK_SELECTED: &str = "selected";
/// A conversation that cannot be resumed. The reason is on the detail line.
pub const MARK_BLOCKED: &str = "blocked";
/// The same conversation also exists in Claude Desktop.
pub const MARK_DESKTOP: &str = "desktop";
/// The project directory was decoded from a store path rather than recorded, so it is a
/// good guess and not a fact. The host refuses to resume into one of these.
pub const MARK_UNVERIFIED: &str = "unverified";

/// The longest a row's first line may be, in characters.
///
/// Characters, not bytes: a first message that opens with an emoji must not be cut
/// mid-codepoint, and `chars().take(n)` is the only slicing that cannot panic.
pub const LABEL_CAP: usize = 120;

/// The row id of a conversation.
pub fn session_row_id(id: &str) -> String {
    format!("s:{id}")
}

/// The row id of a project heading.
pub fn project_row_id(project: &str) -> String {
    format!("p:{project}")
}

/// The whole row list for one rail entry.
pub fn rows(state: &State, entry: &str) -> Vec<Row> {
    if let Some(why) = state.errors.get(entry) {
        return vec![note_row("error", why)];
    }
    let Some(sessions) = state.sessions.get(entry) else {
        // No error and no list: the entry exists but nothing has been read into it yet.
        return vec![note_row("idle", "Reading the conversation history…")];
    };
    let query = state.query(entry);
    let matched: Vec<&Session> = sessions.iter().filter(|s| matches(s, query)).collect();
    if matched.is_empty() {
        return vec![note_row("empty", &empty_reason(state, entry, query))];
    }

    let mut out: Vec<Row> = Vec::with_capacity(matched.len() + 4);
    // Group by project, in the order the projects first appear. The host sends
    // newest-first, so this puts the project you touched last at the top without the
    // module having to sort on a timestamp it would then have to keep agreeing with.
    let mut seen: Vec<&str> = Vec::new();
    for s in &matched {
        if !seen.contains(&s.project.as_str()) {
            seen.push(&s.project);
        }
    }
    let selected = state.selected.get(entry).map(String::as_str);
    for project in seen {
        let group: Vec<&&Session> = matched.iter().filter(|s| s.project == project).collect();
        // A filter is a search, and folding a search result away would hide the thing that
        // was searched for. So groups only stay shut while the whole list is showing.
        let collapsed = query.is_empty()
            && state
                .collapsed
                .contains(&(entry.to_string(), project.to_string()));
        out.push(project_row(entry, project, &group, collapsed));
        if collapsed {
            continue;
        }
        for s in group {
            out.push(session_row(entry, s, state.now, selected));
        }
    }
    out
}

/// The heading above one project's conversations.
fn project_row(entry: &str, project: &str, group: &[&&Session], collapsed: bool) -> Row {
    // The directory's own name, not the whole path: `hyperpanes` reads at 260 pixels and
    // `/Users/someone/code/hyperpanes` does not. The path is still on the detail line.
    let name = Path::new(project)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| project.to_string());
    let n = group.len();
    let mut marks = Vec::new();
    if group.iter().any(|s| !s.project_exact) {
        marks.push(MARK_UNVERIFIED.to_string());
    }
    Row {
        id: project_row_id(project),
        label: name,
        detail: format!(
            "{project} · {n} conversation{}",
            if n == 1 { "" } else { "s" }
        ),
        depth: 0,
        expandable: true,
        expanded: !collapsed,
        icon: Some("folder".into()),
        marks,
        data: json!({ "kind": "project", "tool": entry, "project": project }),
    }
}

/// One conversation.
fn session_row(entry: &str, s: &Session, now: u64, selected: Option<&str>) -> Row {
    let id = session_row_id(&s.id);
    let mut marks = Vec::new();
    if selected == Some(id.as_str()) {
        marks.push(MARK_SELECTED.to_string());
    }
    if s.blocked.is_some() {
        marks.push(MARK_BLOCKED.to_string());
    }
    if s.desktop.is_some() {
        marks.push(MARK_DESKTOP.to_string());
    }
    Row {
        // The blocked reason *replaces* the branch-and-age line rather than joining it.
        // Why a click will do nothing outranks how old the conversation is, and a row that
        // said both would bury the half that matters.
        detail: match &s.blocked {
            Some(why) => why.clone(),
            None => session_detail(s, now),
        },
        label: session_label(s),
        id,
        depth: 1,
        expandable: false,
        expanded: false,
        icon: Some(if s.blocked.is_some() { "lock" } else { "chat" }.into()),
        marks,
        data: json!({ "kind": "session", "tool": entry, "session": s.id }),
    }
}

/// The row's first line: the transcript's summary, its first user message, or — when a
/// transcript carries neither — the head of its id, so a row is never blank.
pub fn session_label(s: &Session) -> String {
    for candidate in [s.summary.trim(), s.first_user.trim()] {
        if !candidate.is_empty() {
            return candidate.chars().take(LABEL_CAP).collect();
        }
    }
    format!("session {}", s.id.chars().take(8).collect::<String>())
}

/// The row's second line for a resumable conversation: branch · messages · age. Each part
/// is dropped when unknown rather than shown empty.
pub fn session_detail(s: &Session, now: u64) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(b) = s.branch.as_deref().filter(|b| !b.is_empty()) {
        parts.push(b.to_string());
    }
    if s.message_count > 0 {
        parts.push(format!(
            "{} message{}",
            s.message_count,
            if s.message_count == 1 { "" } else { "s" }
        ));
    }
    let rel = relative_time(s.started_at, now);
    if !rel.is_empty() {
        parts.push(rel);
    }
    parts.join(" · ")
}

/// How long ago, in the panel's own words.
///
/// `saturating_sub` so a transcript stamped in the future — a machine whose clock was
/// wrong, or a file copied across a timezone — reads "just now" instead of underflowing
/// into a row that claims to be 584 million years old.
pub fn relative_time(started_at: Option<u64>, now: u64) -> String {
    let Some(t) = started_at else {
        return String::new();
    };
    let secs = now.saturating_sub(t) / 1000;
    if secs < 60 {
        return "just now".to_string();
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m ago");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let days = hours / 24;
    if days < 7 {
        return format!("{days}d ago");
    }
    let weeks = days / 7;
    if weeks < 5 {
        return format!("{weeks}w ago");
    }
    let months = days / 30;
    if months < 12 {
        return format!("{months}mo ago");
    }
    format!("{}y ago", days / 365)
}

/// Whether a conversation survives the filter box.
///
/// Matched against everything the row shows — its label, its project and its branch — so
/// what a human typed because they can see it is what finds it.
fn matches(s: &Session, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return true;
    }
    let hay = [
        session_label(s).to_lowercase(),
        s.project.to_lowercase(),
        s.branch.clone().unwrap_or_default().to_lowercase(),
    ];
    hay.iter().any(|h| h.contains(&q))
}

/// Why an entry has nothing in it. Three different silences, said three different ways,
/// because "no conversations" and "that tool is not on this machine" send the human to
/// very different places.
fn empty_reason(state: &State, entry: &str, query: &str) -> String {
    if !query.trim().is_empty() {
        return format!("Nothing matches “{}”", query.trim());
    }
    match state.tool(entry) {
        Some(t) if !t.installed() => format!("{} is not installed on this machine", t.name),
        Some(t) => format!("No {} conversations yet", t.name),
        None => "No conversations yet".to_string(),
    }
}

/// An inert message row: no data, so `module.row.activate` never even reaches the app.
fn note_row(id: &str, label: &str) -> Row {
    Row {
        id: id.into(),
        label: label.into(),
        detail: String::new(),
        depth: 0,
        expandable: false,
        expanded: false,
        icon: None,
        marks: Vec::new(),
        data: Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Resume;
    use crate::app::fake::{blocked, session, tool, FakeApi};
    use crate::app::App;

    const NOW: u64 = 1_000_000_000_000;

    fn built(api: &mut FakeApi) -> App {
        let mut app = App::new();
        app.activate(api);
        app
    }

    #[test]
    fn the_relative_time_buckets_are_the_panels_own() {
        assert_eq!(relative_time(None, NOW), "");
        assert_eq!(relative_time(Some(NOW), NOW), "just now");
        assert_eq!(relative_time(Some(NOW - 90_000), NOW), "1m ago");
        assert_eq!(relative_time(Some(NOW - 3 * 3_600_000), NOW), "3h ago");
        assert_eq!(relative_time(Some(NOW - 2 * 86_400_000), NOW), "2d ago");
        assert_eq!(relative_time(Some(NOW - 21 * 86_400_000), NOW), "3w ago");
        assert_eq!(relative_time(Some(NOW - 60 * 86_400_000), NOW), "2mo ago");
        assert_eq!(relative_time(Some(NOW - 800 * 86_400_000), NOW), "2y ago");
        // Clock skew reads "just now" rather than underflowing.
        assert_eq!(relative_time(Some(NOW + 5000), NOW), "just now");
    }

    #[test]
    fn a_label_falls_back_through_summary_then_prompt_then_id() {
        let mut s = session("abcdef0123456789", "/w/p", "Fix the parser");
        assert_eq!(session_label(&s), "Fix the parser");
        s.summary = "   ".into();
        assert_eq!(session_label(&s), "hello there");
        s.first_user = String::new();
        assert_eq!(session_label(&s), "session abcdef01");
    }

    #[test]
    fn a_long_prompt_is_cut_by_characters_and_never_mid_codepoint() {
        let mut s = session("s1", "/w/p", &"é".repeat(400));
        let label = session_label(&s);
        assert_eq!(label.chars().count(), LABEL_CAP);
        s.summary = format!("🙂{}", "a".repeat(400));
        assert!(session_label(&s).starts_with('🙂'));
    }

    #[test]
    fn a_detail_line_drops_the_parts_it_does_not_know() {
        let mut s = session("s1", "/w/p", "x");
        s.started_at = Some(NOW - 3 * 3_600_000);
        assert_eq!(session_detail(&s, NOW), "main · 7 messages · 3h ago");
        s.branch = Some(String::new());
        assert_eq!(session_detail(&s, NOW), "7 messages · 3h ago");
        s.message_count = 1;
        assert_eq!(session_detail(&s, NOW), "1 message · 3h ago");
        s.message_count = 0;
        s.started_at = None;
        assert_eq!(session_detail(&s, NOW), "");
    }

    #[test]
    fn a_blocked_reason_replaces_the_detail_rather_than_joining_it() {
        let mut api = FakeApi::ready().with(
            "claude",
            Ok(vec![blocked("s1", "/w/p", "project folder is missing")]),
        );
        let app = built(&mut api);
        let rows = rows(&app.state, "claude");
        let row = rows.iter().find(|r| r.depth == 1).unwrap();
        assert_eq!(row.detail, "project folder is missing");
        assert!(!row.detail.contains("message"), "{}", row.detail);
        assert!(row.marks.contains(&MARK_BLOCKED.to_string()));
    }

    #[test]
    fn conversations_group_under_the_projects_directory_name() {
        let mut api = FakeApi::ready().with(
            "claude",
            Ok(vec![
                session("s1", "/Users/me/code/hyperpanes", "Fix the parser"),
                session("s2", "/Users/me/code/hyperpanes", "Add a test"),
                session("s3", "/Users/me/code/other", "Something else"),
            ]),
        );
        let app = built(&mut api);
        let rows = rows(&app.state, "claude");
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0].label, "hyperpanes");
        assert_eq!(rows[0].depth, 0);
        assert!(rows[0].detail.contains("/Users/me/code/hyperpanes"));
        assert!(rows[0].detail.contains("2 conversations"));
        assert_eq!(rows[1].depth, 1);
        assert_eq!(rows[1].label, "Fix the parser");
        assert_eq!(rows[3].label, "other");
        assert!(rows[3].detail.contains("1 conversation"));
        assert!(!rows[3].detail.contains("1 conversations"));
    }

    #[test]
    fn folding_a_project_hides_its_conversations_and_only_its_own() {
        let mut api = FakeApi::ready().with(
            "claude",
            Ok(vec![
                session("s1", "/w/a", "one"),
                session("s2", "/w/b", "two"),
            ]),
        );
        let mut app = built(&mut api);
        app.toggle("claude", "/w/a");
        let rows = rows(&app.state, "claude");
        assert_eq!(rows.len(), 3);
        assert!(!rows[0].expanded);
        assert_eq!(rows[1].label, "b");
        assert_eq!(rows[2].label, "two");
    }

    #[test]
    fn a_filter_unfolds_everything_because_a_search_must_not_hide_its_own_hit() {
        let mut api =
            FakeApi::ready().with("claude", Ok(vec![session("s1", "/w/a", "the parser")]));
        let mut app = built(&mut api);
        app.toggle("claude", "/w/a");
        assert_eq!(rows(&app.state, "claude").len(), 1);
        app.set_query("claude", "parser");
        let rows = rows(&app.state, "claude");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].label, "the parser");
    }

    #[test]
    fn a_filter_matches_the_label_the_project_or_the_branch() {
        let mut s = session("s1", "/w/hyperpanes", "Fix the parser");
        s.branch = Some("feature/rail".into());
        for q in ["PARSER", "hyperpanes", "feature/rail", " parser "] {
            assert!(matches(&s, q), "{q}");
        }
        assert!(!matches(&s, "nothing like this"));
        assert!(matches(&s, "   "), "a blank filter matches everything");
    }

    #[test]
    fn an_empty_entry_says_which_kind_of_empty_it_is() {
        // Nothing yet, but the tool is here.
        let mut api = FakeApi::ready().with("claude", Ok(vec![]));
        let app = built(&mut api);
        assert_eq!(
            rows(&app.state, "claude")[0].label,
            "No Claude Code conversations yet"
        );

        // The tool is not on this machine. Its transcripts might still be.
        let mut api = FakeApi::ready().with("copilot", Ok(vec![]));
        let app = built(&mut api);
        assert_eq!(
            rows(&app.state, "copilot")[0].label,
            "Copilot is not installed on this machine"
        );

        // The filter is what emptied it.
        let mut api =
            FakeApi::ready().with("claude", Ok(vec![session("s1", "/w/a", "the parser")]));
        let mut app = built(&mut api);
        app.set_query("claude", "zzz");
        assert_eq!(rows(&app.state, "claude")[0].label, "Nothing matches “zzz”");
    }

    #[test]
    fn a_note_row_is_inert() {
        let mut api = FakeApi::ready().with("claude", Ok(vec![]));
        let app = built(&mut api);
        for row in rows(&app.state, "claude") {
            assert!(row.data.is_null(), "{row:?}");
            assert!(!row.expandable);
        }
    }

    #[test]
    fn a_refused_history_shows_the_hosts_reason_instead_of_looking_empty() {
        let mut api =
            FakeApi::ready().with("claude", Err("/tools/claude/sessions: HTTP 403".into()));
        let app = built(&mut api);
        let rows = rows(&app.state, "claude");
        assert_eq!(rows.len(), 1);
        assert!(rows[0].label.contains("403"));
        assert!(rows[0].data.is_null());
    }

    #[test]
    fn an_entry_nothing_has_been_read_into_says_so_rather_than_claiming_it_is_empty() {
        let app = App::new();
        let rows = rows(&app.state, "claude");
        assert_eq!(rows.len(), 1);
        assert!(rows[0].label.contains("Reading"));
    }

    #[test]
    fn a_guessed_project_directory_is_marked_on_its_heading() {
        let mut s = session("s1", "/w/a", "one");
        s.project_exact = false;
        let mut api = FakeApi::ready().with("claude", Ok(vec![s]));
        let app = built(&mut api);
        let rows = rows(&app.state, "claude");
        assert!(rows[0].marks.contains(&MARK_UNVERIFIED.to_string()));
        assert!(rows[1].marks.is_empty(), "the mark belongs to the project");
    }

    #[test]
    fn the_desktop_and_selected_marks_land_on_the_conversation() {
        let mut s = session("s1", "/w/a", "one");
        s.desktop = Some("conv-9".into());
        let mut api = FakeApi::ready().with("claude", Ok(vec![s]));
        let mut app = built(&mut api);
        app.state
            .selected
            .insert("claude".into(), session_row_id("s1"));
        let rows = rows(&app.state, "claude");
        assert_eq!(
            rows[1].marks,
            vec![MARK_SELECTED.to_string(), MARK_DESKTOP.to_string()]
        );
    }

    #[test]
    fn a_row_id_survives_a_conversation_that_looks_like_a_project() {
        // Row ids are only unique within an entry, and a conversation id is whatever the
        // tool chose. Prefixing keeps a session called `/w/a` from colliding with the
        // heading for the project `/w/a`.
        assert_ne!(session_row_id("/w/a"), project_row_id("/w/a"));
    }

    #[test]
    fn every_row_the_host_can_click_carries_what_the_click_needs() {
        let mut api = FakeApi::ready().with(
            "claude",
            Ok(vec![
                session("s1", "/w/a", "one"),
                blocked("s2", "/w/b", "no"),
            ]),
        );
        let app = built(&mut api);
        for row in rows(&app.state, "claude") {
            if row.data.is_null() {
                continue;
            }
            assert_eq!(row.data["tool"], "claude");
            match row.data["kind"].as_str().unwrap() {
                "project" => assert!(row.data["project"].is_string()),
                "session" => assert!(row.data["session"].is_string()),
                other => panic!("unknown row kind {other}"),
            }
        }
    }

    #[test]
    fn a_session_with_no_resume_and_no_reason_still_draws() {
        // Defensive: the host promises exactly one of `resume` and `blocked`, but a row
        // list is not the place to discover it broke that promise.
        let s = crate::api::Session {
            resume: None,
            blocked: None,
            ..session("s1", "/w/a", "one")
        };
        let mut api = FakeApi::ready().with("claude", Ok(vec![s]));
        let app = built(&mut api);
        assert_eq!(rows(&app.state, "claude").len(), 2);
        // And clicking it is a toast, not a panic.
        let mut app = app;
        let out = app.resume("claude", "s1").unwrap();
        assert!(out.spawn.is_none());
        assert!(out.toast.unwrap().contains("cannot be resumed"));
    }

    #[test]
    fn the_catalogue_is_only_consulted_for_the_empty_message() {
        // A tool the catalogue lost still lists whatever the host sent for it, because the
        // conversations are the point and the name is decoration.
        let mut api = FakeApi::ready().with("claude", Ok(vec![session("s1", "/w/a", "one")]));
        let mut app = built(&mut api);
        app.state.tools.clear();
        assert_eq!(rows(&app.state, "claude").len(), 2);
        // Only when it is empty does the missing name show, and then generically.
        app.state.sessions.insert("claude".into(), vec![]);
        assert_eq!(rows(&app.state, "claude")[0].label, "No conversations yet");
    }

    #[test]
    fn the_pane_a_resume_asks_for_wears_the_tools_own_colour() {
        let mut api = FakeApi::ready().with(
            "cursor-agent",
            Ok(vec![crate::api::Session {
                resume: Some(Resume {
                    command: "/usr/local/bin/cursor-agent".into(),
                    args: vec!["resume".into(), "s9".into()],
                    cwd: "/w/a".into(),
                }),
                ..session("s9", "/w/a", "Rename the widget")
            }]),
        );
        api.tools = Ok(vec![crate::api::Tool {
            brand: "#1f6feb".into(),
            ..tool("cursor-agent", "Cursor", true, true)
        }]);
        let mut app = built(&mut api);
        let spec = app.resume("cursor-agent", "s9").unwrap().spawn.unwrap();
        assert_eq!(spec.color, "#1f6feb");
        assert_eq!(spec.label, "Rename the widget");
    }
}
