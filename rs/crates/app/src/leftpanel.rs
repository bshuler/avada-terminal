//! The left slide-out panel's data side (mux plan M5) — everything `ui/leftpanel.slint`
//! draws that isn't already in [`crate::state::State`].
//!
//! The panel has four sections, and this module owns the three that need to look outside
//! the window's own state:
//!
//! * **WORKSPACE tree** — pure projection over `State.tabs`, done in [`crate::paneview`];
//!   the only thing needed here is the per-pane liveness ([`liveness`] / [`is_idle`]),
//!   which reads the SAME activity source the idle glow does
//!   (`SessionManager::last_output_at` vs [`crate::glow::now_epoch_ms`]) — there is no
//!   second activity clock in the app, and this module must never introduce one.
//! * **LIBRARY** — the saved workspaces under [`library_dir`]. Cached in a thread-local and
//!   rescanned on the panel's closed→open edge, the same shape `sidebar.rs` uses for its
//!   project scans, so the projection never stats the disk on every tick.
//! * **SETS** — the saved workspace *sets* under [`avada_core::persistence::paths::sets_dir`]
//!   (mux plan M6): a named group of workspaces opened as one batch. Cached and rescanned
//!   on exactly the same edge as the library, since the two directories are siblings and a
//!   set write drops member files into the library's.
//! * **DETACHED** — live sessions that no window is showing. Computed by subtracting the
//!   claimed uids from `SessionManager::uids()`; see [`detached`] for exactly how complete
//!   that answer is today and where M7 fills in the rest.
//!
//! Nothing here mutates `State`: the projection calls it (Seam #1) and the commands it
//! feeds all land in `command::dispatch` (Seam #2).

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::PathBuf;

use avada_core::persistence::paths;
use avada_core::session_manager::SessionManager;
use avada_core::tools::PaneKind;

/// How long after a session's last output its liveness dot stays fully lit before fading
/// to the floor. 30s matches the "is this thing doing something right now?" question the
/// dot answers — long enough that a pane between prompts still reads as live, short enough
/// that an abandoned one goes quiet.
pub const LIVE_WINDOW_MS: u64 = 30_000;

/// The 0..1 liveness of a session whose last output was at `last` (epoch ms), as of
/// `now_ms`. `1.0` = output this instant, falling linearly to `0.0` a [`LIVE_WINDOW_MS`]
/// later; a session that has never produced output (or whose clock reads in the future)
/// is `0.0`. The UI floors the dot's opacity so a quiet pane still shows its color chip.
#[tracing::instrument(level = "debug", ret)]
pub fn liveness(last: Option<u64>, now_ms: u64) -> f32 {
    let Some(last) = last else {
        return 0.0;
    };
    let age = now_ms.saturating_sub(last);
    if age >= LIVE_WINDOW_MS {
        return 0.0;
    }
    1.0 - (age as f32 / LIVE_WINDOW_MS as f32)
}

/// Whether a pane's idle alert has armed: the same gate `paneview::pump` uses to light the
/// glow ring (the feature is on, the pane runs an agent CLI, and it has been output-quiet
/// past the threshold). Reproduced here rather than read off `PaneState::glow` because the
/// pump only advances the glow for the ACTIVE tab's panes — the tree shows every tab, and
/// a background tab's stale `glow.alpha` would lie.
#[tracing::instrument(level = "debug", ret)]
pub fn is_idle(
    shell_title: &str,
    last: Option<u64>,
    now_ms: u64,
    on: bool,
    threshold_ms: u64,
) -> bool {
    on && crate::glow::is_ai_pane(shell_title)
        && match last {
            Some(ms) => now_ms.saturating_sub(ms) >= threshold_ms,
            None => false,
        }
}

/// The mark kinds the workspace tree draws for a pane that is **not** a tool this build
/// knows. The pane header is free to leave those blank — it is chrome sitting on a pane you
/// are already looking at — but a tree is a column, and a column of marks with holes
/// punched in it reads as ragged, so every row here gets one.
///
/// Negative on purpose: the positive half of this namespace belongs to the tool registry
/// (`ToolDef.icon`, allocated from [`crate::theme::menu_icon::TOOL_BASE`]) and is drawn by
/// the shared `ToolIcon`. Keeping the two halves on opposite sides of zero means a tool
/// added to the registry tomorrow can never collide with a view added here today.
pub mod pane_mark {
    /// A plain shell — a `>_` prompt. Also what a tool id this build has no mark for falls
    /// back to: such a pane really is a terminal running something, and an honest prompt
    /// beats a borrowed brand (the same call the header makes when it draws nothing).
    pub const TERMINAL: i32 = -1;
    /// The file-browser view — a folder.
    pub const FILE_BROWSER: i32 = -2;
    /// The file-viewer view — a page.
    pub const FILE_VIEWER: i32 = -3;
    /// The markdown preview — the markdown badge.
    pub const MARKDOWN: i32 = -4;
    /// The internal browser view — a globe.
    pub const BROWSER: i32 = -5;
    /// The highlighted source view — `</>`.
    pub const CODE: i32 = -6;
    /// The structured-data tree — a branching node.
    pub const DATA: i32 = -7;
    /// The delimited-file grid — a table.
    pub const TABLE: i32 = -8;
    /// The image view — a picture frame with a sun.
    pub const IMAGE: i32 = -9;
    /// A module-contributed surface — a plug. One mark for every module: the module's own
    /// icon (if it ships one) is a Wave 2 concern, and the panel needs a stable answer today.
    pub const MODULE: i32 = -10;
}

/// The mark one pane row carries, in the namespace `PaneMark` in `ui/leftpanel.slint`
/// switches on: the registry's own icon kind for a tool we know (>= `TOOL_BASE`, drawn by
/// the shared `ToolIcon` — the very component the pane header uses, which is the whole
/// point: a pane must be identifiable in the panel the same way it is on its chrome), and
/// one of the [`pane_mark`] negatives for everything else.
///
/// Takes the pane's EFFECTIVE kind (`State::effective_kind`), not `PaneState::kind`, so a
/// plain terminal that the title sniff caught running an agent is branded in the tree for
/// the same reason — and at the same moment — as it is in its header.
#[tracing::instrument(level = "debug", ret)]
pub fn pane_mark_kind(kind: &PaneKind) -> i32 {
    match kind {
        PaneKind::FileBrowser => pane_mark::FILE_BROWSER,
        PaneKind::FileViewer => pane_mark::FILE_VIEWER,
        PaneKind::Markdown => pane_mark::MARKDOWN,
        PaneKind::Browser => pane_mark::BROWSER,
        PaneKind::Code => pane_mark::CODE,
        PaneKind::Data => pane_mark::DATA,
        PaneKind::Table => pane_mark::TABLE,
        PaneKind::Image => pane_mark::IMAGE,
        PaneKind::Module(_) => pane_mark::MODULE,
        // `ui_icon` answers 0 for a plain shell AND for a tool id with no mark in this
        // build; both are PTY panes, so both get the prompt rather than a gap.
        PaneKind::Terminal | PaneKind::Tool(_) => match kind.ui_icon() {
            0 => pane_mark::TERMINAL,
            icon => icon,
        },
    }
}

/// The ink a pane row's mark is drawn in: the tool's own brand when the registry knows it —
/// the identical colour the header tints its mark with, so the two readings of the same
/// pane never disagree — and the pane's accent otherwise, which is the header's own
/// fallback and is never invisible against the panel.
#[tracing::instrument(level = "debug", ret)]
pub fn pane_mark_ink(kind: &PaneKind, accent: slint::Color) -> slint::Color {
    match kind.tool() {
        Some(t) => slint::Color::from_rgb_u8(t.brand.0, t.brand.1, t.brand.2),
        None => accent,
    }
}

/// How often the projection re-runs purely to age the liveness dots while the panel is
/// open. One resync a second is invisible next to the pump's own cadence and keeps a dot
/// from freezing at its last projected brightness on an otherwise-quiet workspace.
const HEARTBEAT: std::time::Duration = std::time::Duration::from_millis(1000);

/// Whether the panel's liveness heartbeat is due at `now` (and, if so, consume it by
/// stamping `last`). Called from the pump only while the panel is open.
///
/// `last` is PER WINDOW (`State::left_panel_beat`), not a module-global: `pump` runs once
/// per window, so a single shared stamp would be consumed by whichever window the app
/// happens to pump first and every other window's dots would freeze at their last
/// projected brightness.
#[tracing::instrument(level = "debug", ret)]
pub fn heartbeat_due(last: &mut Option<std::time::Instant>, now: std::time::Instant) -> bool {
    match *last {
        Some(t) if now.duration_since(t) < HEARTBEAT => false,
        _ => {
            *last = Some(now);
            true
        }
    }
}

// ===================== the saved-workspace library =====================
//
// Listing the library is `avada_core::workspace::library`'s job now — the panel no longer
// draws a LIBRARY drawer, and the module that does reaches the directory over
// `host.workspace.list`, which cannot see it any other way (`fs.read` is scoped to the
// workspace root and these files live outside it). What stays here is the *write* half,
// because saving is still a host action on the palette.

/// Write `file` into the library under `name` (sanitised, `.avada` appended), creating the
/// directory if needed. Returns the path written, or `None` if the directory or the file
/// could not be written. A name that collides gets `-2`, `-3`, … appended, so saving twice
/// never silently overwrites the earlier snapshot.
#[tracing::instrument(level = "debug", skip_all)]
pub fn save_to_library(
    name: &str,
    file: &avada_core::workspace::model::WorkspaceFile,
) -> Option<PathBuf> {
    let dir = paths::workspaces_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return None;
    }
    let base = sanitize_name(name);
    let mut path = dir.join(format!("{base}.avada"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{base}-{n}.avada"));
        n += 1;
        if n > 999 {
            return None;
        }
    }
    if !avada_core::workspace::io::write_workspace(&path, file) {
        return None;
    }
    Some(path)
}

/// Reduce a tab title to a safe file stem: path separators and the Windows-reserved
/// punctuation become `-`, runs collapse, and an empty result falls back to "workspace".
#[tracing::instrument(level = "debug", ret)]
fn sanitize_name(name: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in name.trim().chars() {
        let ok = ch.is_alphanumeric() || ch == '_' || ch == '.' || ch == ' ' || ch == '-';
        if ok && ch != ' ' && ch != '-' {
            out.push(ch);
            last_dash = false;
        } else {
            if !last_dash && !out.is_empty() {
                out.push('-');
            }
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "workspace".to_string()
    } else {
        out.chars().take(64).collect()
    }
}

// ===================== detached (adoptable) sessions =====================

/// One row of the DETACHED section: a live session no window is currently showing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetachedSession {
    pub uid: String,
    /// The row's title — the session's short uid (there is no server-side label yet).
    pub label: String,
    /// The second line: how much output it has buffered and when it last spoke.
    pub detail: String,
    /// Epoch-ms of its last output, for the liveness dot.
    pub last_output_at: Option<u64>,
}

thread_local! {
    /// Session uids claimed by the windows of THIS process — the union over every live
    /// window, republished by `app.rs` once per pump and subtracted in [`detached`], so a
    /// pane sitting in the window next door is never offered for adoption. A union (rather
    /// than a per-window "everyone else" set) is enough: a window also subtracts its own
    /// uids, and a session held anywhere is not detached from anyone's point of view.
    static WINDOW_CLAIMS: RefCell<HashSet<String>> = RefCell::new(HashSet::new());

    /// What this process last told the daemon it is hosting. Diffed against each new
    /// publish so the pump sends a `Claim`/`Release` frame only when the set actually
    /// changes — an unchanged frame costs nothing on the wire.
    static PUBLISHED_CLAIMS: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
}

/// Publish the uids every window in this process is hosting (laid out or parked). Called
/// from the app's pump, before the per-window renders that project the panel.
///
/// **M7:** this also registers those uids with the daemon's cross-process claim registry, so
/// that *other* avada processes stop offering them for adoption. Only the difference
/// from the previous publish goes on the wire, and it goes fire-and-forget: a claim on a
/// pane we already host is not contested, and the GUI pump must never block on the daemon.
/// The contested case — adopting an orphan — takes the blocking path in
/// [`SessionManager::claim_session`] and obeys its answer.
///
/// Releasing on the way out is a courtesy, not the safety net: the daemon drops every claim
/// a connection holds when that connection's socket closes, so a crash releases them too.
#[tracing::instrument(level = "debug", skip_all)]
pub fn publish_window_claims(mgr: &SessionManager, uids: impl IntoIterator<Item = String>) {
    let held: HashSet<String> = uids.into_iter().collect();
    PUBLISHED_CLAIMS.with(|prev| {
        let mut prev = prev.borrow_mut();
        for uid in held.difference(&prev) {
            mgr.announce_claim(uid);
        }
        for uid in prev.difference(&held) {
            mgr.release_session(uid);
        }
        *prev = held.clone();
    });
    set_window_claims(held);
}

/// The thread-local half of [`publish_window_claims`]: record what this process's windows
/// hold, with no daemon traffic. Split out because the daemon half needs a live
/// `SessionManager` (and therefore a real pty) while the subtraction it feeds does not.
#[tracing::instrument(level = "debug", ret)]
fn set_window_claims(held: HashSet<String>) {
    WINDOW_CLAIMS.with(|c| *c.borrow_mut() = held);
}

/// Uids claimed by a avada process *other than this one* — a session another window,
/// in another process, is currently hosting.
///
/// Answered from the claim snapshot the daemon pushes to every client whenever the picture
/// changes (M7), so this is a lock and a set filter: no I/O on the panel's paint path. Empty
/// for the in-process backend, where no other process can be holding one of our sessions.
///
/// A claim is scoped to the owner's daemon *connection*, so a process that dies — cleanly,
/// by panic, or by `SIGKILL` — has its claims dropped the moment the kernel closes its
/// socket, and its panes appear here no longer.
#[tracing::instrument(level = "debug", skip_all)]
pub fn claimed_by_other_processes(mgr: &SessionManager) -> HashSet<String> {
    mgr.sessions_claimed_elsewhere()
}

/// The adoptable sessions: everything the session manager knows about, minus everything
/// already claimed.
///
/// `claimed_here` is this window's own uids (its panes + its parked reminders). The other
/// two subtractions come from [`publish_window_claims`] (every window in this process) and
/// [`claimed_by_other_processes`] (every OTHER process, via the daemon's claim registry).
///
/// How complete this is: in daemon mode `SessionManager::uids()` answers from the client's
/// shadow table, which is seeded by `ListSessions` at connect and then kept current by the
/// `Exit` stream, this client's own creates, and (M7) the full `SessionsChanged` snapshot the
/// daemon pushes on every create/kill/exit — so a session another client made after we
/// connected shows up here without a reconnect, and a session it killed stops showing up.
/// In-process mode lists exactly the sessions this process spawned, which is the correct
/// answer for a single-process run.
#[tracing::instrument(level = "debug", skip_all)]
pub fn detached(mgr: &SessionManager, claimed_here: &HashSet<String>) -> Vec<DetachedSession> {
    let now = crate::glow::now_epoch_ms();
    let other_procs = claimed_by_other_processes(mgr);
    let mut rows: Vec<DetachedSession> = adoptable_uids(mgr.uids(), claimed_here, &other_procs)
        .into_iter()
        .map(|uid| {
            let last = mgr.last_output_at(&uid);
            DetachedSession {
                label: short_uid(&uid),
                detail: describe_session(mgr.output_bytes(&uid).unwrap_or(0), last, now),
                last_output_at: last,
                uid,
            }
        })
        .collect();
    // Most recently active first — the one you're most likely to be looking for.
    rows.sort_by_key(|r| std::cmp::Reverse(r.last_output_at));
    rows
}

/// The subtraction behind [`detached`], split out so it can be tested without a live
/// `SessionManager` (which can only be populated by actually spawning a PTY): `all` minus
/// this window's own claims, minus every other window in this process
/// ([`publish_window_claims`]), minus every other process (`other_procs`, which [`detached`]
/// fills from [`claimed_by_other_processes`] — passed in rather than fetched here, since
/// M7's source for it needs a live `SessionManager`). Input order is preserved; [`detached`]
/// re-sorts by last output.
#[tracing::instrument(level = "debug", ret)]
pub fn adoptable_uids(
    all: Vec<String>,
    claimed_here: &HashSet<String>,
    other_procs: &HashSet<String>,
) -> Vec<String> {
    let elsewhere = WINDOW_CLAIMS.with(|c| c.borrow().clone());
    all.into_iter()
        .filter(|uid| {
            !claimed_here.contains(uid) && !elsewhere.contains(uid) && !other_procs.contains(uid)
        })
        .collect()
}

/// A session uid shortened for display (uids are long and opaque; the head is enough to
/// tell two apart). Mirrors `sidebar::short_id`'s approach.
#[tracing::instrument(level = "debug", ret)]
fn short_uid(uid: &str) -> String {
    let head: String = uid.chars().take(12).collect();
    format!("session {head}")
}

/// The detached row's second line: buffered output size + relative last-output time.
#[tracing::instrument(level = "debug", ret)]
fn describe_session(bytes: u64, last: Option<u64>, now: u64) -> String {
    let size = if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{} KB", bytes / 1024)
    } else {
        format!("{bytes} B")
    };
    let rel = crate::sidebar::relative_time(last, now);
    if rel.is_empty() {
        format!("{size} buffered")
    } else {
        format!("{size} buffered · {rel}")
    }
}

// ---------------------------------------------------------------------------------
// The module rail (track H4): what each running module put on the left panel's strip.
// ---------------------------------------------------------------------------------

/// What every running module registered on the rail, plus which module entry (if any)
/// the panel is showing. Fed by [`avada_core::module::RailEvent`]s marshalled onto the UI
/// thread by `App::tick`; projected into `RailAdapter` by `paneview::resync`.
///
/// Named `ModuleRail` rather than `Rail` because `State` already has a right-edge "rail"
/// (the pane rail) and the two must never be confused in a grep.
///
/// Keys: an entry is addressed everywhere outside this struct by [`entry_key`] —
/// `<owner/repo>#<entry-id>` — so two modules that both register `files` never collide
/// and a click can be routed back to its module without a second lookup.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ModuleRail {
    /// Per-module rail state, in module-id order (a `BTreeMap` so the strip's order is
    /// stable across ticks even when two entries share an `order`).
    pub modules:
        std::collections::BTreeMap<avada_core::rights::ModuleId, avada_core::module::RailState>,
    /// The key of the module entry the panel is showing, if a module entry is active.
    pub active: Option<String>,
    /// Bumped whichever way whenever the row a module marks [`MARK_SELECTED`] changes.
    /// The view WATCHES this rather than binding to `scroll_y`, so the viewport moves on
    /// the reveal and never on a tick the human spent scrolling somewhere else.
    pub scroll_seq: i32,
    /// The last `(entry key, row id)` seen marked selected, so re-pushing the same list —
    /// which a module does on every keystroke of a filter — does not re-scroll.
    selection: Option<(String, String)>,
}

/// The mark a module puts on the one row it wants shown: highlighted, and scrolled to.
///
/// The host does not choose a selection; a tier-1 module owns its list entirely, and this
/// is the only vocabulary it has for "this row, of the four hundred I just sent you".
pub const MARK_SELECTED: &str = "selected";

/// The mark for a row that is listed but should not compete for the eye — a dotfile, an
/// ignored path. Drawn dimmed rather than filtered out, because a module that wanted it
/// gone would simply not have sent it.
pub const MARK_HIDDEN: &str = "hidden";

/// Whether `row` carries `mark`. Marks are an unordered set on the wire (a `Vec` only
/// because JSON has no set), so membership is the only question worth asking.
pub fn has_mark(row: &avada_core::module::Row, mark: &str) -> bool {
    row.marks.iter().any(|m| m == mark)
}

/// The row heights `RailRowView` lays out: a row with a detail line is taller.
///
/// Duplicated from the `.slint` because Slint cannot be asked, and pinned by
/// [`tests::the_scroll_offset_sums_the_rows_above_the_selection`] — the same arrangement
/// the built-in explorer used before it became a module.
const ROW_H: f32 = 20.0;
const ROW_H_DETAIL: f32 = 30.0;

/// How far down the list the first [`MARK_SELECTED`] row starts, in logical pixels, or
/// `None` when nothing is selected.
///
/// A sum rather than `index * height` because the rows are not all the same height: a row
/// the module gave a detail line to is 30px and one without is 20px, so counting rows
/// would drift by 10px for every detailed row above the target.
pub fn scroll_offset(rows: &[avada_core::module::Row]) -> Option<f32> {
    let mut y = 0.0;
    for r in rows {
        if has_mark(r, MARK_SELECTED) {
            return Some(y);
        }
        y += if r.detail.is_empty() {
            ROW_H
        } else {
            ROW_H_DETAIL
        };
    }
    None
}

/// One entry as the strip draws it: the module's [`RailEntry`](avada_core::module::RailEntry)
/// plus the key the UI hands back on click.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RailEntryView {
    /// `<owner/repo>#<entry-id>`.
    pub key: String,
    /// The owning module.
    pub module: avada_core::rights::ModuleId,
    /// The module's own entry.
    pub entry: avada_core::module::RailEntry,
}

/// How a rail row was activated.
///
/// A local echo of the SDK's `rail::Gesture`, kept because this is also what the `.slint`
/// hands back: [`RailGesture::parse`] turns the three words the UI sends into a value, and
/// `module_runtime::wire_gesture` maps that onto the contract's own `Gesture` on the way
/// out. One of the two could go, but not both — the UI edge needs a parse and the host
/// edge needs the SDK's type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RailGesture {
    /// Click / Enter.
    Open,
    /// Expand or collapse.
    Toggle,
    /// Right-click.
    Context,
}

impl RailGesture {
    /// Parse the wire spelling the Slint callback carries; `None` for anything else, so a
    /// typo in the UI is a dropped click rather than a wrong gesture sent to a module.
    pub fn parse(s: &str) -> Option<RailGesture> {
        Some(match s {
            "open" => RailGesture::Open,
            "toggle" => RailGesture::Toggle,
            "context" => RailGesture::Context,
            _ => return None,
        })
    }
}

/// Work a rail click leaves for whoever owns the module host. Queued on
/// `State::rail_requests` and drained by the controller after the dispatch has returned
/// its borrow — a module callback must never re-enter `State`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RailRequest {
    /// `Host::activate(module, entry, …)`: the user selected this entry.
    Activate {
        /// Which module.
        module: avada_core::rights::ModuleId,
        /// Its entry id (not the key).
        entry: String,
    },
    /// `Host::activate_row(module, RowActivate { .. })`.
    Row {
        /// Which module.
        module: avada_core::rights::ModuleId,
        /// Its entry id (not the key).
        entry: String,
        /// Which of the module's two tier-1 surfaces the row was on. Handed straight back
        /// so a module that projects the same id to both can tell the two clicks apart.
        target: avada_core::module::RowTarget,
        /// The row id the module gave.
        row: String,
        /// The payload the module hung off the row, handed straight back.
        data: serde_json::Value,
        /// Which gesture.
        gesture: RailGesture,
    },
}

/// The path-string a rail entry's `icon` yields for `RailEntryRow.icon`.
///
/// A module's manifest icon is a *path to an SVG file* relative to its install directory,
/// and reading that file is the host's job, not the panel's (docs/module-contract.md). A
/// tier-1 module may instead send the path data itself, which is what Slint's `Path`
/// wants. Telling them apart by the leading move command is enough: an SVG `d` always
/// starts with `M`/`m`, and no relative file path does.
pub fn icon_commands(icon: Option<&str>) -> &str {
    match icon {
        Some(s) if s.starts_with('M') || s.starts_with('m') => s,
        _ => "",
    }
}

/// The key the UI uses for a module entry: `<owner/repo>#<entry-id>`.
pub fn entry_key(module: &avada_core::rights::ModuleId, entry: &str) -> String {
    format!("{}#{entry}", module.as_str())
}

/// Split a key back into `(module, entry id)`; `None` if it is not a valid key.
pub fn split_key(key: &str) -> Option<(avada_core::rights::ModuleId, &str)> {
    let (module, entry) = key.split_once('#')?;
    if entry.is_empty() {
        return None;
    }
    let module = avada_core::rights::ModuleId::new(module).ok()?;
    Some((module, entry))
}

impl ModuleRail {
    /// Fold one event from the module host's rail channel into the rail. Returns whether
    /// the active entry disappeared with it, so the caller can put the panel back on a
    /// built-in section.
    ///
    /// The fold lives here rather than on `State` so the UI tests can drive the panel from
    /// the host's own event type without standing up a whole window.
    pub fn apply(&mut self, event: avada_core::module::RailEvent) -> bool {
        use avada_core::module::RailEvent;
        match event {
            RailEvent::Registered { module, entries } => self.register(module, entries),
            RailEvent::Rows {
                module,
                entry,
                target,
                rows,
            } => {
                // Pane rows are the other tier-1 surface and live in
                // `crate::module_ui::rows`; they reach the panel only as an event it must
                // step over. Filtering here rather than upstream keeps one host stream:
                // both projections see everything and each keeps what is its own.
                if target == avada_core::module::RowTarget::Rail {
                    self.set_rows(&module, &entry, rows);
                }
                false
            }
            RailEvent::Gone { module } => self.gone(&module),
        }
    }

    /// The module registered (or re-registered) its entries — replaces the earlier set.
    /// Returns true when the active entry disappeared with it (the panel must fall back).
    pub fn register(
        &mut self,
        module: avada_core::rights::ModuleId,
        entries: Vec<avada_core::module::RailEntry>,
    ) -> bool {
        self.modules.entry(module).or_default().register(entries);
        self.drop_stale_active()
    }

    /// The module replaced the rows under `entry`. Rows for an entry the module never
    /// registered are ignored (the host already refused them; this is belt and braces).
    pub fn set_rows(
        &mut self,
        module: &avada_core::rights::ModuleId,
        entry: &str,
        rows: Vec<avada_core::module::Row>,
    ) {
        // Taken BEFORE the move, and compared against what the entry last had selected:
        // a module that re-sends the same list with the same selection (which is what a
        // filter does on every keystroke) must not drag the viewport back each time.
        let selection = rows
            .iter()
            .find(|r| has_mark(r, MARK_SELECTED))
            .map(|r| (entry_key(module, entry), r.id.clone()));
        if let Some(state) = self.modules.get_mut(module) {
            let _ = state.set_rows(entry, rows);
            if selection.is_some() && selection != self.selection {
                self.scroll_seq = self.scroll_seq.wrapping_add(1);
            }
            self.selection = selection;
        }
    }

    /// Re-assert the current scroll target for one more frame.
    ///
    /// The frame that asks for a scroll is usually one on which the list cannot honour it —
    /// the panel is being instantiated, or the `ListView` has not measured the new model —
    /// so the request is repeated for a short window (`paneview::RAIL_SCROLL_HOLD`) rather
    /// than fired once into a view that is not there yet.
    pub fn bump_scroll(&mut self) {
        self.scroll_seq = self.scroll_seq.wrapping_add(1);
    }

    /// How far down the active entry's list its selected row starts, in logical pixels.
    pub fn scroll_y(&self) -> f32 {
        scroll_offset(self.active_rows()).unwrap_or(0.0)
    }

    /// The module is gone: its entries and rows leave the rail. Returns true when the
    /// active entry was one of them.
    pub fn gone(&mut self, module: &avada_core::rights::ModuleId) -> bool {
        self.modules.remove(module);
        self.drop_stale_active()
    }

    fn drop_stale_active(&mut self) -> bool {
        match &self.active {
            Some(key) if self.lookup(key).is_none() => {
                self.active = None;
                true
            }
            _ => false,
        }
    }

    /// Every entry across every module, sorted by `order` then module id then
    /// registration order — the order the strip draws them in.
    pub fn entries(&self) -> Vec<RailEntryView> {
        let mut out: Vec<RailEntryView> = self
            .modules
            .iter()
            .flat_map(|(module, state)| {
                state.entries.iter().map(move |e| RailEntryView {
                    key: entry_key(module, &e.id),
                    module: module.clone(),
                    entry: e.clone(),
                })
            })
            .collect();
        // `sort_by_key` is stable, and the flat_map already yields module order then
        // registration order, so ties keep exactly that.
        out.sort_by_key(|v| v.entry.order);
        out
    }

    /// The entry behind `key`, if a module registered it.
    pub fn lookup(&self, key: &str) -> Option<RailEntryView> {
        let (module, entry) = split_key(key)?;
        let state = self.modules.get(&module)?;
        let e = state.entries.iter().find(|e| e.id == entry)?;
        Some(RailEntryView {
            key: key.to_string(),
            module,
            entry: e.clone(),
        })
    }

    /// The rows under `key` (empty when the module has projected nothing yet).
    pub fn rows(&self, key: &str) -> &[avada_core::module::Row] {
        let Some((module, entry)) = split_key(key) else {
            return &[];
        };
        self.modules
            .get(&module)
            .and_then(|s| s.rows.get(entry))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Make `key` the active module entry. `false` (and no change) when no module
    /// registered it — a click on a button that has just been unregistered is a no-op,
    /// not a blank panel.
    pub fn activate(&mut self, key: &str) -> bool {
        if self.lookup(key).is_some() {
            self.active = Some(key.to_string());
            true
        } else {
            false
        }
    }

    /// Leave the module entry (a built-in was clicked).
    pub fn deactivate(&mut self) {
        self.active = None;
    }

    /// The active entry's rows, if a module entry is active.
    pub fn active_rows(&self) -> &[avada_core::module::Row] {
        self.active.as_deref().map(|k| self.rows(k)).unwrap_or(&[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A module's row list, with `which` marked `selected` and every `n`th row given a
    /// detail line (which makes it taller — the whole reason the offset is measured rather
    /// than counted).
    fn marked(n: usize, which: Option<usize>, detail_every: usize) -> Vec<avada_core::module::Row> {
        (0..n)
            .map(|i| avada_core::module::Row {
                id: format!("r{i}"),
                label: format!("r{i}"),
                detail: if detail_every > 0 && i % detail_every == 0 {
                    "src".into()
                } else {
                    String::new()
                },
                depth: 0,
                expandable: false,
                expanded: false,
                icon: None,
                marks: if which == Some(i) {
                    vec![MARK_SELECTED.into()]
                } else {
                    vec![]
                },
                data: serde_json::Value::Null,
            })
            .collect()
    }

    /// Reveal-in-files ends here: the module marks a row and the host has to work out where
    /// that row is. It is a SUM of the heights above it, not `index * ROW_H`, because a row
    /// with a detail line is half again as tall — counting instead of measuring puts the
    /// viewport progressively further off the longer the list is.
    #[test]
    fn the_scroll_offset_sums_the_rows_above_the_selection() {
        assert_eq!(scroll_offset(&marked(5, Some(0), 0)), Some(0.0));
        assert_eq!(scroll_offset(&marked(5, Some(3), 0)), Some(3.0 * ROW_H));
        // Rows 0 and 2 are tall, row 1 is short; the mark is on row 3.
        assert_eq!(
            scroll_offset(&marked(5, Some(3), 2)),
            Some(ROW_H_DETAIL + ROW_H + ROW_H_DETAIL)
        );
    }

    /// Nothing marked is not "the top": it means the module has not answered the reveal
    /// yet. `Some(0.0)` would jerk the list to the top of a tree the human was reading.
    #[test]
    fn no_mark_means_no_scroll_at_all_rather_than_scroll_to_the_top() {
        assert_eq!(scroll_offset(&marked(5, None, 0)), None);
        assert_eq!(scroll_offset(&[]), None);
    }

    /// A filter re-sends the whole list on EVERY keystroke, usually with the same row still
    /// selected. If that bumped the sequence the viewport would be yanked back to the mark
    /// between one letter and the next, so the bump is tied to the selection CHANGING.
    #[test]
    fn re_sending_the_same_selection_does_not_scroll_again() {
        let module = avada_core::rights::ModuleId::new("bshuler/avada-files").unwrap();
        let mut rail = ModuleRail::default();
        rail.apply(avada_core::module::RailEvent::Registered {
            module: module.clone(),
            entries: vec![serde_json::from_value(serde_json::json!({
                "id": "files", "label": "Files", "tier": 1, "order": 0
            }))
            .unwrap()],
        });

        rail.set_rows(&module, "files", marked(5, None, 0));
        let quiet = rail.scroll_seq;

        rail.set_rows(&module, "files", marked(5, Some(3), 0));
        let after_reveal = rail.scroll_seq;
        assert_ne!(after_reveal, quiet, "a new selection has to be scrolled to");

        rail.set_rows(&module, "files", marked(5, Some(3), 0));
        assert_eq!(
            rail.scroll_seq, after_reveal,
            "the same row selected again is not a new reveal"
        );

        rail.set_rows(&module, "files", marked(5, Some(1), 0));
        assert_ne!(rail.scroll_seq, after_reveal, "a different row is");

        assert!(rail.activate(&entry_key(&module, "files")));
        assert_eq!(
            rail.scroll_y(),
            ROW_H,
            "and the offset the panel reads back is the active entry's"
        );
    }

    #[test]
    fn liveness_decays_over_the_window() {
        let now = 1_000_000u64;
        assert_eq!(liveness(None, now), 0.0);
        assert_eq!(liveness(Some(now), now), 1.0);
        // half way through the window → about half lit
        let half = liveness(Some(now - LIVE_WINDOW_MS / 2), now);
        assert!((half - 0.5).abs() < 0.01, "half = {half}");
        assert_eq!(liveness(Some(now - LIVE_WINDOW_MS), now), 0.0);
        assert_eq!(liveness(Some(now - LIVE_WINDOW_MS * 10), now), 0.0);
        // a timestamp in the future (clock skew) must not exceed 1.0
        assert_eq!(liveness(Some(now + 5_000), now), 1.0);
    }

    #[test]
    fn a_pane_row_carries_the_mark_of_the_tool_it_runs() {
        // A tool the registry knows resolves to ITS icon kind — the registry's own number,
        // not one this module invents — so the tree hands `ToolIcon` exactly what the pane
        // header hands it and the two draw the same mark.
        for t in avada_core::tools::registry::TOOLS {
            let kind = PaneKind::Tool(t.id.to_string());
            assert_eq!(
                pane_mark_kind(&kind),
                t.icon as i32,
                "{} must carry the registry's own mark",
                t.id
            );
        }
    }

    #[test]
    fn every_pane_row_gets_a_mark_even_when_it_runs_no_tool() {
        // The whole point of the negatives: a column with a hole in it reads as ragged, so
        // no kind may ever come back as "draw nothing" (0, which is what the header's
        // `ui_icon` answers for all of these).
        let views = [
            (PaneKind::Terminal, pane_mark::TERMINAL),
            (PaneKind::FileBrowser, pane_mark::FILE_BROWSER),
            (PaneKind::FileViewer, pane_mark::FILE_VIEWER),
            (PaneKind::Markdown, pane_mark::MARKDOWN),
            (PaneKind::Browser, pane_mark::BROWSER),
            (PaneKind::Code, pane_mark::CODE),
            (PaneKind::Data, pane_mark::DATA),
            (PaneKind::Table, pane_mark::TABLE),
            (PaneKind::Image, pane_mark::IMAGE),
            (
                PaneKind::from_meta_value("module:acme/avada-files#tree"),
                pane_mark::MODULE,
            ),
            // A tool id from a build newer than this one: still a terminal running
            // something, so it gets the prompt rather than a gap or a borrowed brand.
            (
                PaneKind::Tool("tool-from-the-future".into()),
                pane_mark::TERMINAL,
            ),
        ];
        for (kind, want) in views {
            let got = pane_mark_kind(&kind);
            assert_eq!(got, want, "{kind:?}");
            assert_ne!(got, 0, "{kind:?} must never draw nothing");
        }
    }

    #[test]
    fn pane_marks_never_collide_with_the_registrys_icon_kinds() {
        // The two halves of the namespace are split at zero. If a view mark ever went
        // positive it would draw whichever tool happened to own that number.
        for m in [
            pane_mark::TERMINAL,
            pane_mark::FILE_BROWSER,
            pane_mark::FILE_VIEWER,
            pane_mark::MARKDOWN,
            pane_mark::BROWSER,
            pane_mark::CODE,
            pane_mark::DATA,
            pane_mark::TABLE,
            pane_mark::IMAGE,
            pane_mark::MODULE,
        ] {
            assert!(m < 0, "view mark {m} is inside the registry's half");
        }
        // …and no two built-in marks share a number, or two views would wear one glyph.
        let mut marks = vec![
            pane_mark::TERMINAL,
            pane_mark::FILE_BROWSER,
            pane_mark::FILE_VIEWER,
            pane_mark::MARKDOWN,
            pane_mark::BROWSER,
            pane_mark::CODE,
            pane_mark::DATA,
            pane_mark::TABLE,
            pane_mark::IMAGE,
            pane_mark::MODULE,
        ];
        let n = marks.len();
        marks.sort_unstable();
        marks.dedup();
        assert_eq!(marks.len(), n, "two pane marks share a number");
        // …and every registry kind stays in the other half, which is what lets
        // `PaneMark` dispatch on the sign alone.
        for t in avada_core::tools::registry::TOOLS {
            assert!(t.icon as i32 > 0);
        }
    }

    #[test]
    fn a_marks_ink_is_the_tools_brand_and_the_panes_accent_otherwise() {
        let accent = slint::Color::from_rgb_u8(1, 2, 3);
        let claude = avada_core::tools::registry::by_id("claude").unwrap();
        assert_eq!(
            pane_mark_ink(&PaneKind::Tool("claude".into()), accent),
            slint::Color::from_rgb_u8(claude.brand.0, claude.brand.1, claude.brand.2)
        );
        // No registry entry, so no brand: the pane's own accent, which is what the header
        // falls back to and is never invisible against the panel.
        for kind in [
            PaneKind::Terminal,
            PaneKind::FileBrowser,
            PaneKind::Markdown,
            PaneKind::Tool("tool-from-the-future".into()),
        ] {
            assert_eq!(pane_mark_ink(&kind, accent), accent, "{kind:?}");
        }
    }

    #[test]
    fn idle_gate_matches_the_glow_gate() {
        let now = 1_000_000u64;
        let thr = 30_000u64;
        // off → never idle, whatever the pane is
        assert!(!is_idle("claude", Some(now - 60_000), now, false, thr));
        // a plain shell never arms, however quiet
        assert!(!is_idle("zsh", Some(now - 60_000), now, true, thr));
        // an agent pane quiet past the threshold arms
        assert!(is_idle("claude", Some(now - 60_000), now, true, thr));
        // …but not before it
        assert!(!is_idle("claude", Some(now - 1_000), now, true, thr));
        // no output at all is not "idle" (the pane never started)
        assert!(!is_idle("claude", None, now, true, thr));
    }

    #[test]
    fn heartbeat_is_per_window_and_rate_limited() {
        let t0 = std::time::Instant::now();
        // A window that has never beaten fires immediately, then not again inside the window.
        let mut a: Option<std::time::Instant> = None;
        assert!(heartbeat_due(&mut a, t0));
        assert!(!heartbeat_due(&mut a, t0 + HEARTBEAT / 2));
        assert!(heartbeat_due(&mut a, t0 + HEARTBEAT));
        // A SECOND window keeps its own stamp: the first window consuming the beat must not
        // starve it (the bug a module-global stamp had — window 2's dots froze forever).
        let mut b: Option<std::time::Instant> = None;
        assert!(heartbeat_due(&mut b, t0 + HEARTBEAT));
        assert!(!heartbeat_due(&mut b, t0 + HEARTBEAT));
    }

    #[test]
    fn adoptable_subtracts_this_window_and_the_others() {
        let all = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let set = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<HashSet<_>>();

        let none = HashSet::new();

        set_window_claims(HashSet::new());
        // Nothing claimed → everything is adoptable, in the order given.
        assert_eq!(
            adoptable_uids(all(&["a", "b", "c"]), &set(&[]), &none),
            all(&["a", "b", "c"])
        );
        // This window's own panes are never offered back to it.
        assert_eq!(
            adoptable_uids(all(&["a", "b", "c"]), &set(&["b"]), &none),
            all(&["a", "c"])
        );

        // A pane sitting in the window next door is not detached either.
        set_window_claims(set(&["c"]));
        assert_eq!(
            adoptable_uids(all(&["a", "b", "c"]), &set(&["b"]), &none),
            all(&["a"])
        );

        // Republishing replaces (never accumulates) the cross-window claim set.
        set_window_claims(HashSet::new());
        assert_eq!(
            adoptable_uids(all(&["a", "b", "c"]), &set(&["b"]), &none),
            all(&["a", "c"])
        );

        // And the M7 subtraction: a uid another PROCESS has claimed in the daemon's registry
        // is not offered here either, even though nothing in this process holds it.
        assert_eq!(
            adoptable_uids(all(&["a", "b", "c"]), &set(&["b"]), &set(&["c"])),
            all(&["a"])
        );
    }

    #[test]
    fn sanitize_name_makes_a_safe_stem() {
        assert_eq!(sanitize_name("my project"), "my-project");
        assert_eq!(sanitize_name("a/b\\c"), "a-b-c");
        assert_eq!(sanitize_name("  spaced  out  "), "spaced-out");
        assert_eq!(sanitize_name(""), "workspace");
        assert_eq!(sanitize_name("///"), "workspace");
        assert_eq!(sanitize_name("keep_this.1"), "keep_this.1");
        assert!(sanitize_name(&"x".repeat(200)).chars().count() <= 64);
    }

    #[test]
    fn describe_session_formats_size_and_age() {
        let now = 1_000_000u64;
        assert_eq!(describe_session(512, None, now), "512 B buffered");
        assert_eq!(describe_session(2048, None, now), "2 KB buffered");
        assert_eq!(
            describe_session(3 * 1_048_576, Some(now - 120_000), now),
            "3.0 MB buffered · 2m ago"
        );
    }

    #[test]
    fn short_uid_is_stable_and_short() {
        assert_eq!(short_uid("abcdefghijklmnopqrst"), "session abcdefghijkl");
        assert_eq!(short_uid("abc"), "session abc");
    }

    // ===== the module rail =====

    mod rail_model {
        use super::super::*;
        use avada_core::module::{RailEntry, Row};
        use avada_core::rights::ModuleId;

        fn id(s: &str) -> ModuleId {
            ModuleId::new(s).unwrap()
        }

        /// Built through serde because `UiTier` is the SDK's type and the app crate
        /// does not depend on the SDK directly — exactly what a module's own
        /// `host.rail.register` wire payload looks like, tier 1 = rows.
        fn entry(id: &str, order: i32) -> RailEntry {
            serde_json::from_value(serde_json::json!({
                "id": id, "label": id.to_uppercase(), "tier": 1, "order": order
            }))
            .unwrap()
        }

        fn row(id: &str) -> Row {
            Row {
                id: id.into(),
                label: id.into(),
                detail: String::new(),
                depth: 0,
                expandable: false,
                expanded: false,
                icon: None,
                marks: vec![],
                data: serde_json::Value::Null,
            }
        }

        #[test]
        fn keys_round_trip_and_reject_garbage() {
            let k = entry_key(&id("acme/avada-files"), "tree");
            assert_eq!(k, "acme/avada-files#tree");
            let (m, e) = split_key(&k).unwrap();
            assert_eq!(m, id("acme/avada-files"));
            assert_eq!(e, "tree");
            assert!(split_key("acme/avada-files").is_none());
            assert!(split_key("acme/avada-files#").is_none());
            assert!(split_key("not a module#tree").is_none());
        }

        #[test]
        fn entries_sort_by_order_then_module_then_registration() {
            let mut rail = ModuleRail::default();
            rail.register(id("zed/late"), vec![entry("b", 0), entry("a", 0)]);
            rail.register(id("acme/early"), vec![entry("first", -5), entry("z", 0)]);
            let keys: Vec<String> = rail.entries().into_iter().map(|v| v.key).collect();
            assert_eq!(
                keys,
                vec![
                    "acme/early#first",
                    "acme/early#z",
                    "zed/late#b",
                    "zed/late#a"
                ]
            );
        }

        #[test]
        fn rows_land_under_their_entry_and_leave_with_the_module() {
            let mut rail = ModuleRail::default();
            let m = id("acme/avada-files");
            rail.register(m.clone(), vec![entry("tree", 0)]);
            rail.set_rows(&m, "tree", vec![row("src"), row("Cargo.toml")]);
            // rows for an entry the module never registered are dropped, not stored
            rail.set_rows(&m, "ghost", vec![row("x")]);
            assert_eq!(rail.rows("acme/avada-files#tree").len(), 2);
            assert!(rail.rows("acme/avada-files#ghost").is_empty());
            assert!(rail.activate("acme/avada-files#tree"));
            assert_eq!(rail.active_rows().len(), 2);
            assert!(rail.gone(&m), "the active entry left with the module");
            assert!(rail.active.is_none());
            assert!(rail.entries().is_empty());
            assert!(rail.active_rows().is_empty());
        }

        #[test]
        fn activating_an_unknown_key_is_a_no_op() {
            let mut rail = ModuleRail::default();
            rail.register(id("acme/avada-files"), vec![entry("tree", 0)]);
            assert!(!rail.activate("acme/avada-files#nope"));
            assert!(rail.active.is_none());
            assert!(rail.activate("acme/avada-files#tree"));
            rail.deactivate();
            assert!(rail.active.is_none());
        }

        #[test]
        fn reregistering_without_the_active_entry_drops_the_activation() {
            let mut rail = ModuleRail::default();
            let m = id("acme/avada-files");
            rail.register(m.clone(), vec![entry("tree", 0), entry("git", 1)]);
            assert!(rail.activate("acme/avada-files#git"));
            assert!(!rail.register(m.clone(), vec![entry("git", 1)]));
            assert_eq!(rail.active.as_deref(), Some("acme/avada-files#git"));
            assert!(rail.register(m, vec![entry("tree", 0)]));
            assert!(rail.active.is_none());
        }

        /// The three words the `.slint` sends land on the three gestures the contract
        /// names. A rename on either side has to break here rather than in a module.
        #[test]
        fn a_gesture_survives_the_wire_spelling() {
            for (word, gesture) in [
                ("open", RailGesture::Open),
                ("toggle", RailGesture::Toggle),
                ("context", RailGesture::Context),
            ] {
                assert_eq!(RailGesture::parse(word), Some(gesture));
            }
            assert_eq!(RailGesture::parse("wiggle"), None);
        }
    }
}
