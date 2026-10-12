# Discovery lane — report

Branch `sp-discovery`, contract `docs/subagent-panes-design.md` §1.

## What was built

Commits on top of `fe78f43` (the contract):

| Commit | Seam |
|---|---|
| `0e09731` | Hook: `SubagentStart`/`SubagentStop`/`Stop` registered and recorded; captured payload fixtures |
| `318eccb` | `claude_subagents` model: hook records ∪ transcript-dir fallback, `is_active`, `toggle_breathes`, pruning |
| `437ed4d` | Shared scaffolding: `PaneKind::Subagent`, read model `parentPaneId` + activity, `toggleSubagents` verb, `ctl tabs` outline |
| `437ed4d` | App fields: `parent_uid`/`subagent`/`hide_subagents`, `visible_panes`, `set_hide_subagents`, `tab_subagent_summary`, session round-trip |
| `e8d9ae0` | Doc-comment fix; `expect(dead_code)` (non-test only) on the four scaffolding methods so the warning returns once a lane calls them — **remove those attributes when you add the first caller** (an unfulfilled `expect` warns). Deliberate departure from the codebase's `allow(dead_code)` idiom so the marker can't outlive its reason; MSRV is 1.85 (app) / 1.89 (core), above `expect`'s 1.81 |

### Key pieces (file:line)

Hook
- `resources/claude/hp-claude-session-hook.sh:86` — subagent start/stop record (merge, atomic replace, agent-id allow-list); `:118` — `Stop` → `_turn.json`. Prints nothing; exit 0 always.
- `rs/crates/core/src/claude_hook.rs:23` — `HOOK_EVENTS` (5 events).
- `rs/crates/core/tests/fixtures/claude-hooks/` — captured `subagent-start.json`, `subagent-stop.json`, `stop.json`, a transcript tree, README. **Gitignored dir — add with `git add -f`.**
- Capture answer: `SubagentStart` carries only the **parent's** `transcript_path`; the script derives `<dir>/<session_id>/subagents/agent-<id>.jsonl`, which equals `SubagentStop`'s `agent_transcript_path` (pinned by `the_capture_pins_the_transcript_derivation`).

Model — `rs/crates/core/src/claude_subagents.rs`
- `:39` `GROWTH_IDLE` = 10 s (the plan left N open).
- `:43` `SubagentRecord`; `:65` `is_active`; `:83` `toggle_breathes`.
- `:179` `subagents_from(hook_dir, transcript_dir)`; `:253` `subagents_for_pane`; `:262`/`:270` turn end; `:278` `prune_dirs_in` (called from `claude_panes.rs:214` `prune_markers`).

Shared scaffolding
- `core/src/tools/kind.rs:138` `PaneKind::Subagent` — meta `"subagent"` (`:227`), `ui_kind` 11 (`:285`), `ui_name` "Subagent" (`:315`), `is_view`.
- `core/src/control/readmodel.rs:113` `PaneInfo.parent_pane_id`, `:121` `SubagentActivity {Active, Idle, Finished}`, `:907` `PaneOut.parentPaneId` (omitted when `None`); `activity` is overridden for subagent panes in `pane_out`.
- `core/src/control/uiops.rs:90` `UiOp::ToggleSubagents { tab_id }`; `core/src/control/routes.rs:2023` in `TAB_VERBS`, handler `:2160` (requires `tabId`, 404s on an unknown tab).
- `app/src/control_host.rs:1140` applies `ToggleSubagents` as a **debug-log stub** (chrome lane wires it); `:1505` `subagent_activity(record, now)`; the publisher maps `parent_uid` → parent control id.
- `app/src/ctl_cli.rs:803` `outline_lines` — `ctl tabs` indents children under `parentPaneId`, shows `activity`.
- `app/src/leftpanel.rs` `pane_mark_kind`: Subagent reuses Claude's mark (no Slint `PaneMark` change).
- `app/src/viewpane.rs` `rows_for`: Subagent is in the empty arm — the viewer is the pane lane's.
- `app/src/uitest/mod.rs`: `view_flag` / `_covered` / `all` include kind 11.

App state — `rs/crates/app/src/state.rs`
- `:167-168` `DetachedPane.parent_uid/subagent`; `:193` `DetachedTab.hide_subagents`.
- `:1129`/`:1132` `PaneState.parent_uid/subagent`; `:1415` `Tab.hide_subagents`.
- `:1437` `Tab::visible_panes()`; `:1451` `Tab::set_hide_subagents()`.
- `:3251` `State::tab_subagent_summary(tab_idx) -> (count, any_active)` (uses `GROWTH_IDLE`).
- `:8392` `to_session_file` writes `hideSubagents: true` only when on; `:8757` `append_tab_from_group` restores it. Detach/reattach (tab move) carries it.
- `core/src/workspace/model.rs:151` `GroupSpec.hide_subagents`; `"hideSubagents"` in `GROUP_KEYS` (`workspace/project.rs`).
- Tests: `mod subagent_tab_tests` at `state.rs:14395`.

## Tests

All with `export SDKROOT=$(xcrun --sdk macosx --show-sdk-path)`, on `e8d9ae0` unless a row says otherwise (`e8d9ae0` touched only `app/src/state.rs`).

| Command (from `rs/` unless noted) | Result |
|---|---|
| `cargo test -p avada-core --lib -- --skip permissions` | **2027 passed**, 0 failed, 15 ignored |
| `cargo test -p avada-core --lib -- module::host_tests` (alone) | 21 passed. One earlier full run under a concurrent app build failed `crash_restarts_with_toasts_then_disables_after_the_cap` (known load-sensitive); green alone and in the final full run |
| `cargo test -p avada-core --test workspace_format` | 6 passed |
| `cargo test -p avada-core --test control_parity --test readmodel_publish` | 5 / 7 passed |
| `cargo test -p avada-core --lib -- workspace::project` | 21 passed |
| `rs/crates/app`: `cargo test --bin avada` | **942 passed** on `437ed4d`; on `e8d9ae0` 941 passed + 1 flake: `uitest::docpane::a_doc_takes_precedence_over_rows_on_one_surface` (passes 6/6 run alone; failed 1 of 3 runs of `uitest::docpane`). Not this lane's: the four docpane tests share global `module_ui` state keyed by one module id and race when run in parallel; this lane did not touch docpane or module_ui |
| `cargo clippy -p avada-core --all-targets` | clean except pre-existing `marketplace/module_e2e.rs:58` |
| `rs/crates/app`: `cargo clippy --all-targets` | clean (only the widget crate's Slint build-script notes) |
| `scripts/check-windows.sh` | **green** (core, module-sdk, widget and app for `x86_64-pc-windows-gnu`) |
| `scripts/check-linux-docker.sh` | **not run** — the Docker daemon returned 500 on every API call. No `cfg(target_os)` code was touched. The image pins `rust:1.96` |

New tests: `claude_subagents.rs` tests (fixtures, fallback, ordering, `is_active`, `toggle_breathes` truth table incl. hidden, prune, hook script run against the captured payloads incl. "prints nothing" and "only SessionStart touches the marker"); `kind.rs:404`; `routes.rs:4635`, `:4685`; `ctl_cli.rs:1431`; `state.rs:14395` (7 tests).

## Deviations from the contract

1. **Focus-hidden-but-zoom-visible.** The contract says hiding moves focus to the parent and clears zoom. When the *zoomed* pane is visible but focus sits on a hidden pane, `set_hide_subagents` moves focus to the zoomed pane and **keeps** the zoom (clearing it would unzoom a pane the user can still see). In every other case focus goes to the parent and zoom is cleared, as the contract says. Pinned by the 6-case table in `hiding_moves_focus_and_zoom_off_a_hidden_pane`.
2. **Nested subagents.** "Its parent" is resolved by climbing `parent_uid` to the first *visible* ancestor (bounded by `panes.len()`, so a cycle can't loop), then pane 0. Needed because a subagent's parent can itself be a hidden subagent.
3. **`toggleSubagents` requires `tabId`.** The contract writes `ToggleSubagents { tab }`; the field is `tab_id` and the HTTP verb rejects a missing id rather than defaulting to the current tab. `ctl toggle-subagents [tab]` (chrome lane) should fill the current tab itself.
4. **Mark.** Subagent panes reuse Claude's left-panel mark instead of a new Slint case.

## Notes for the next lanes

Pane lane
- **Give every subagent pane a unique, non-empty `uid`.** View panes may have `uid == ""`, but the control `paneId` is derived from the uid and `parentPaneId` is resolved by uid → id (`control_host.rs` publisher). Use the agent id, e.g. `sub:<agent_id>`. `parent_uid` must be the orchestrator's (or parent subagent's) uid.
- When `to_session_file` skips subagent panes, **remap `focused`/`zoomed`/`sizes`** to the surviving indices — they currently index the full `tab.panes`.
- `PaneState.subagent` is the record the summary and read-model activity read; refresh it in `reconcile_subagents`.
- Every `compute_tiles` caller and pane projection must iterate `visible_panes()`; nothing does yet.
- `Tab::set_hide_subagents` is the only place enforcing "focus/zoom never target a hidden pane"; any new focus path (click, keyboard nav, `ctl focus`) on a hidden pane needs the same guard.
- `duplicate_tab` does not copy `hide_subagents` (deliberate — a duplicate has no subagents), and does not copy subagent panes' `parent_uid`.

Chrome lane
- Replace the stub at `control_host.rs:1140` with `tab.set_hide_subagents(!tab.hide_subagents)` on the tab whose control id matches `tab_id`.
- `toggle_breathes(current, any_active, hidden)` — `any_active` comes from `tab_subagent_summary`.

All lanes
- The `"subagent"` meta string is bare (not `view:subagent`); `a_subagent_pane_is_a_bare_subagent_view` guards it against colliding with a tool named `subagent`.
- Every cargo command needs `export SDKROOT=$(xcrun --sdk macosx --show-sdk-path)` (whisper-rs-sys).
- Pre-existing clippy warning `core/src/marketplace/module_e2e.rs:58` (`Iterator::last`) is not this lane's.
