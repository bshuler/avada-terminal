# Pane lane — report

Branch `sp-pane`, on top of `e8d9ae0`. Three signed commits:

- `5a4a366` adds the subagent viewer.
- `c521e95` adds reconcile and lifecycle, the hidden-pane layout, persistence, and the app wiring.
- The third renames one test so its name says what it checks.

## What was built

### Viewer (`rs/crates/app/src/viewpane.rs`)
- `rows_for` gained a `PaneKind::Subagent` arm (:291) that calls `tail_rows(&path, TAIL_ROWS)`.
- `tail_rows` (:506) reads at most `TAIL_BYTES` (256 KiB, :499) from the end of the transcript.
  - It drops the first line, which is probably cut in half, and keeps the last `TAIL_ROWS` (400, :495).
  - It puts a notice at the top when earlier messages were cut off.
  - A missing file shows "Waiting for the subagent to start…" and an empty file shows "No messages yet".
- `transcript_rows` (:550) turns each transcript entry into rows:

  | Transcript entry | Row |
  |---|---|
  | User prompt | QUOTE |
  | Assistant text | LINE |
  | `tool_use` | CODE `▸ <tool name>` |
  | `tool_result` | NOTICE `↳ <first line>` |
  | Thinking | skipped |

- Refresh needs no new code. The view-pane fingerprint (path + mtime + len) already reloads the pane when the transcript grows.

### Reconcile (`rs/crates/app/src/state.rs`)
- `State::reconcile_subagents(mgr, now, lookup)` (:2942) runs at most once a second (`SUBAGENT_RECONCILE_EVERY`).
- It asks `lookup` about every pty pane and never about view panes. `lookup` returns `None` for a pane without a Claude marker.
- For each `SubagentRecord` it does one of these:
  - **New, still running:** opens a `PaneKind::Subagent` pane.
    - uid `sub:<agent_id>`, `parent_uid` = the orchestrator's uid, `cwd` = the transcript path.
    - Label is `<orch label> › <description>`.
    - It goes in the orchestrator's tab, after the orchestrator and any subagents it already has.
    - It does not take focus: `insert_pane_at` (:2910) shifts `focused` and `zoomed` instead.
  - **Already open:** refreshes `PaneState.subagent` and the subtitle (`subagent_subtitle`, :1593) — `<agent_type>`, plus ` · finished` once it has stopped.
  - **Done:** closes the pane. Done means the orchestrator's `last_turn_end` is after the subagent's `stopped_at`.
  - **Closed by the user** (`State::closed_subagents`, :1914): never opened again.
- At the end it closes any subagent pane whose orchestrator is no longer in the window.
- `adopt_pane_at` was split into `pane_from_detached` (:2830) and `insert_pane_at`. The behaviour of every existing caller is unchanged.
- App wiring (`rs/crates/app/src/app.rs`):
  - `App::pane_subagents` (:1195) picks the pane id whose marker carries the newest session. It returns `subagents_for_pane(id)` and `last_turn_end(id)`.
  - `tick()` calls reconcile for every window (:2064).

### Lifecycle
- `close_pane_in` (:2691):
  - Closing a subagent pane adds its id to `closed_subagents`.
  - Closing any other pane first closes its subagent panes (`close_subagents_of`, :3065).
- `close_pane_menu` (:2728) parks the orchestrator as before, but closes its subagent panes first. A subagent pane's own close is final; it is not parked.
- `ends_window_pane` (:2759): an orchestrator that shares its window's last tab only with its own subagents counts as the window's last pane.

### Hidden panes and layout
- `Tab::is_hidden` (:1447) and `Tab::visible_layout` (:1455) return the visible indices, their renormalized sizes, and focus mapped into that list.
- Every caller of `compute_tiles` and `compute_dividers` now goes through them (checked with grep):
  - `paneview.rs` `relayout_active` (:649) and `relayout_background` (:707)
  - `State::dividers`
  - `focus_dir` (state.rs:3684)
  - `effective`
  - `taskbar_visible`
- `resize_divider` keeps the hidden panes' shares as they are.
- No focus path lands on a hidden pane: `focus_pane`, `focus_dir` (which walks the visible list), and `toggle_zoom` all check.
- The UI model keeps one row per `Tab::panes` entry. `PaneItem.hidden` (`ui/types.slint:126`) collapses that pane's taskbar entry (`ui/paneview.slint:142-146`).

### Persistence
- `to_session_file` (:8663) leaves subagent panes out:
  - A tab that holds only subagent panes is dropped.
  - `SavedPanes::of` (:1550) renormalizes the sizes of the panes that are kept.
  - Focus on a subagent moves to the nearest kept pane before it; zoom is remapped, or cleared if it was on a subagent.
- `library_workspace_of` filters subagent panes out as well.

## Tests

All of them ran from `rs/crates/app` through `cargo-locked.sh`.

| Command | Result |
|---|---|
| `cargo test --bin avada` | **959 passed, 0 failed** |
| `cargo clippy --bin avada --tests` | clean |
| `cargo clippy --bin avada` | clean (forced re-lint) |

- **Viewer:** four tests in `viewpane::tests`, starting `a_subagent_transcript_…`, `tool_results_collapse_…`, `a_long_transcript_…`, `a_transcript_not_yet_written_…`.
- **State:** 13 tests in `state::subagent_pane_tests` (state.rs:14938). They cover:
  - where a pane opens, and that it doesn't take focus
  - the 1 Hz throttle
  - that every pty pane is asked and view panes never are
  - the seam to the real fixture transcript directory
  - auto-close
  - the user-close memory
  - cascade close (both the park path and the plain close)
  - the window's-last-pane rule
  - the orphan sweep
  - persistence remapping
  - a hidden pane giving its space back (area and divider count)
  - focus and zoom never landing on a hidden pane
  - resizing with hidden panes

Core was not touched, so the core suite was not run.

## Deviations from the contract
- **Reconcile signature.** It is `reconcile_subagents(mgr, now: Instant, lookup)`. The throttle is per window and lives in `State`. The file reads stay in `App`, behind the `lookup` closure, so `State` remains testable.
- **The Claude-marker filter is in `App::pane_subagents`, not in `State`.** Every pty pane is offered to `lookup` once a second, and a pane with no marker gets `None` (marker reads are cheap). The test `every_terminal_pane_is_asked_and_view_panes_never` checks exactly this.
- **`parent_uid` is always the orchestrator's pane uid**, never another subagent. The hook payload has no parent agent id, so nesting shows only as `spawn_depth` in the record. The contract already says this.
- **The user-close memory is not saved.** `closed_subagents` lives only while the app runs. After a relaunch, a subagent the user closed can reappear if it is still running. Closes that come from a cascade or the orphan sweep are not recorded as user closes.
- **Moving an orchestrator to another window** makes its subagent panes orphans in the old window, so they close there. The new window's reconcile opens them again within about a second.
- **`App::pane_subagents` has no unit test.** It is a thin join over `claude_panes` and `claude_subagents`, which are both tested. `the_seam_reads_the_fixture_transcript_dir` covers how the record and the viewer meet.

## Notes for the next lanes
- **Chrome lane:**
  - `Tab::set_hide_subagents` still has `cfg_attr(not(test), expect(dead_code))`, and `tab_subagent_summary(_at)` still has `expect(dead_code)`. Remove both once the toggle and badge call them.
  - Hiding needs no extra layout work. Calling `set_hide_subagents` and setting `state.dirty` is enough, because the model resync recomputes `PaneItem.hidden`.
- **Glow:** a subagent pane is `PaneKind::Subagent` with `subagent: Some(rec)`. `rec.last_growth` and `stopped_at` are refreshed on every reconcile.
- **Merging:** `adopt_pane_at` was refactored, and `close_pane_in`, `close_pane_menu` and `ends_window_pane` changed. Any lane that edits those functions will hit merge conflicts here.

LANE DONE
