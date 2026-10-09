# Subagent panes — lane contract (design gate)

Companion to [`subagent-panes-plan.md`](subagent-panes-plan.md) (the approved spec, option 1).
This file fixes the seams between the lanes of plan §5 so they can run in parallel without
inventing incompatible types. Coordinator branch: `subagent-panes`; lane branches `sp-<lane>`
in `worktrees/sp-<lane>`; the coordinator merges every lane back and pushes `main`.

Corrections to plan §2/§5 found while mapping the code:
- The Claude hook is `core/src/claude_hook.rs` + `resources/claude/hp-claude-session-hook.sh`,
  not `tools/session_hook.rs` (that file is cursor/copilot/codex/gemini only).
- Tab layout is `core/src/layout/presets.rs::compute_tiles` (called from `state.rs` and
  `paneview.rs`), not `gridpane.rs` (module grid panes).
- The feature matrix is 107/107, checked by `app/src/uitest/matrix.rs`.

## 1. Discovery lane (first; everything else branches from its merge)

**Hook.** `claude_hook.rs` registers `SubagentStart`, `SubagentStop` and `Stop` in addition
to `SessionStart`/`SessionEnd`. The script dispatches on `hook_event_name`:
- `SessionStart` → session marker, unchanged. `SessionEnd` → remove marker, unchanged.
  **Any other event must not rewrite the session marker** (it would bump its mtime and reset
  the resume ready-age check in `app.rs`).
- `SubagentStart`/`SubagentStop` → `<state>/claude-subagents/<pane-id>/<agent_id>.json`,
  `{agentId, agentType, transcript, event, startedAt, stoppedAt}` (merge, atomic replace).
- `Stop` (orchestrator turn ended) → `<state>/claude-subagents/<pane-id>/_turn.json`
  `{endedAt}`.
- The real payload field names are **captured first** with a logging hook from a live
  `claude_auto` session and pinned as a fixture; the script reads the captured names. The
  capture must answer: does `SubagentStart` carry the **subagent's** transcript path, or only
  the parent's? If only the parent's, `transcript` is derived as
  `<dir of parent transcript>/<session_id>/subagents/agent-<agent_id>.jsonl` and the fixture
  pins that derivation.
- **MUST print nothing on stdout** for any event. A `Stop`/`SubagentStop` hook that emits
  JSON can block Claude from ending its turn. Exit 0 always.

**Fallback.** From the pane's session marker (`sessionId`, `cwd`, `configDir`): scan
`<configDir or ~/.claude>/projects/<encode_path_str(cwd)>/<sessionId>/subagents/agent-*.meta.json`
(+ the sibling `.jsonl`). Also used to enrich hook records with `description`/`spawnDepth`.

**Model** — new `core/src/claude_subagents.rs`:

```rust
pub struct SubagentRecord {
    pub agent_id: String,
    pub agent_type: String,
    pub description: String,     // meta.json; falls back to agent_type
    pub spawn_depth: u32,
    pub transcript: PathBuf,     // agent-<id>.jsonl
    pub started_at: Option<SystemTime>,
    pub stopped_at: Option<SystemTime>,   // Some ⇒ finished (hook) 
    pub last_growth: Option<SystemTime>,  // transcript mtime
}
pub fn subagents_for_pane(pane_id: &str) -> Vec<SubagentRecord>;          // hook ∪ fallback, by agent_id, ordered by start
pub fn subagents_from(hook_dir: &Path, transcript_dir: Option<&Path>) -> Vec<SubagentRecord>; // pure-ish, testable
pub fn last_turn_end(pane_id: &str) -> Option<SystemTime>;
impl SubagentRecord { pub fn is_active(&self, now: SystemTime, idle: Duration) -> bool; }
// active ⇔ stopped_at.is_none() ∧ (hook-started ∨ last_growth within `idle`)
// (stopped ⇒ never active; fallback-only records rely on growth)
pub fn toggle_breathes(tab_is_current: bool, any_active: bool, hidden: bool) -> bool; // = current ∧ active; hidden ignored
```

Only records whose transcript lies under this session are returned; a session that began
before the hook was installed is covered by the fallback alone. Background subagents that
outlive the turn stay active by the same rule.

**Shared scaffolding the other lanes build on** (discovery lands it so they don't collide):
- `PaneKind::Subagent` in `core/src/tools/kind.rs`: `is_view()` true, not PTY, `ui_kind`
  = next free int, meta/ctl encoding `"subagent"` (not `view:subagent`).
- Control read model: `PaneInfo`/`PaneOut` gain `parentPaneId: Option<String>`;
  `kind: "subagent"`; `activity` for a subagent pane = `active`/`idle`/`finished`.
  `ctl tabs` prints subagent panes indented one level under their parent.
- `UiOp::ToggleSubagents { tab }` + `toggleSubagents` in `TAB_VERBS`; the app side is the
  chrome lane's.
- App fields (in `app/src/state.rs`, so neither parallel lane edits these structs):
  - `PaneState.parent_uid: Option<…>` and `PaneState.subagent: Option<SubagentRecord>`.
  - `Tab.hide_subagents: bool` (default false), round-tripped through the session file.
  - `Tab::visible_panes() -> impl Iterator<Item = (usize, &PaneState)>` — all panes, minus
    subagent panes when `hide_subagents`. Hidden panes **stay in `tab.panes`** so `focused`,
    `zoomed` and `sizes` keep indexing the same vector.
  - `Tab::set_hide_subagents(bool)`: when hiding, a focused or zoomed hidden pane moves
    focus to its parent (or 0) and clears zoom. Focus/zoom never target a hidden pane.
  - `State::tab_subagent_summary(tab_idx) -> (usize, bool)` (count, any active) over
    `PaneKind::Subagent` panes. Both parallel lanes consume it.
  - Unit tests for `visible_panes` and the focus/zoom rule.

Tests (plan §4 bullets 1, 2, 4): hook payload fixture → record; transcript-dir fixture finds
the same set; `toggle_breathes` truth table incl. hidden; `ctl state` carries `subagent` +
parent link.

## 2. Pane lane (parallel with chrome)

- `PaneState` gains `parent_uid: Option<…>` and `subagent: Option<SubagentRecord-ish>`.
- `State::reconcile_subagents(now)` called from the tick at ≤1 Hz: for each Claude pane,
  `subagents_for_pane`; create missing subagent panes **in the orchestrator's tab** (not the
  active tab) right after the orchestrator and its earlier subagents, label
  `<orchestrator label> › <description>`; update finished state; close a finished subagent
  pane once `last_turn_end` > its `stopped_at`. User-closed ones are remembered per
  `agent_id` and not re-created.
- Viewer: `tail_rows(transcript, max)` (assistant text / tool name / collapsed result) and a
  `rows_for` arm for `PaneKind::Subagent`; fingerprint covers the transcript mtime+len.
  Read-only.
- `to_session_file` skips subagent panes; on restart reconcile rebuilds them.
- Layout: every `compute_tiles` caller and every pane-model projection iterates
  `tab.visible_panes()`, so hidden subagent panes get no tile and the orchestrator reclaims
  the space; indices passed back from the UI are mapped through the visible list.

## 3. Chrome lane (parallel with pane)

- `TopBar` button right of the left-panel toggle (`ui/topbar.slint`), label
  "hide subagents"/"show subagents", visible only when the current tab has subagent panes.
- `Command::ToggleSubagents` → flips `tab.hide_subagents` of the current tab; `UiOp`
  application in `control_host.rs::apply_ui_ops`; `ctl toggle-subagents [tab]`.
- Breathing: a `Glow` owned by the window state, driven in `paneview::pump` with
  `IdleEffect::Pulse` while `toggle_breathes(...)`; projected to a TopBar `breath` property.
  Static otherwise. It breathes when the panes are hidden.
- Reads "has subagents / any active" only through `State::tab_subagent_summary`.
- Owns its matrix ratchet: a "reaches Rust" uitest naming the new callback (pattern
  `the_left_panel_toggle_reaches_rust`) and the `feature-test-matrix.md` row/counts, so its
  suite is green on its own branch.

## 4. Proof lane (last, on the merged branch)

- uitest: fake session dir with two subagents → two subagent panes under the orchestrator;
  toggle hides them (tile count drops), button `breath` still animates; toggle shows.
- `docs/feature-test-matrix.md` row + counts; plan status updated; backlog B2 closed.
- Coordinator: bundle + install, then live check in this tab with a real Agent call.
