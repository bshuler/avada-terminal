# Chrome lane — report

Branch `sp-chrome`, commit `c7c774f` (signed, Bert Shuler), on top of `e8d9ae0`.
Contract `docs/subagent-panes-design.md` §3 held as written. Nothing diverges from it.

## What was built

| Piece | Where |
|---|---|
| `Command::ToggleSubagents` + dispatch | `rs/crates/app/src/command.rs:443`, `:1102` |
| Palette entry "Toggle subagent panes" | `rs/crates/app/src/palette.rs:219` |
| `State::toggle_subagents_in(ti)` / `toggle_subagents()` (current tab) | `rs/crates/app/src/state.rs:5805` |
| `State.subagent_glow: Glow` (own seed) | `rs/crates/app/src/state.rs:1625`, init `:1925` |
| AppWindow callback binding `cb0!(on_toggle_subagents, …)` | `rs/crates/app/src/app.rs:3908` |
| AppWindow props `has-subagents`, `subagents-hidden`, `subagent-breath` + `toggle-subagents()`, forwarded to TopBar | `rs/crates/app/ui/app.slint` |
| TopBar props/callback | `rs/crates/app/ui/topbar.slint:346` |
| TopBar button (right of the left-panel toggle, `if root.has-subagents`) | `rs/crates/app/ui/topbar.slint:462` |
| `paneview::drive_subagent_breath(app, state, now)` | `rs/crates/app/src/paneview.rs:2178` |
| Called from `pump` (self-contained block before the HUD) | `rs/crates/app/src/paneview.rs:2151` |
| Control plane: `UiOp::ToggleSubagents` → `resolve_tab` → `toggle_subagents_in` (stub replaced) | `rs/crates/app/src/control_host.rs:1138` |
| `avada ctl toggle-subagents [tab]` (USAGE, `HAND_VERBS`, arm) | `rs/crates/app/src/ctl_cli.rs:230` |
| Current-tab resolver `current_tab_id(state, $AVADA_PANE_ID)` | `rs/crates/app/src/ctl_cli.rs:1073` |
| Skill verb lists | `resources/claude/hyperpane/.claude/skills/avada/REFERENCE.md`, `SKILL.md` |
| Feature matrix row, Command row and Counts | `docs/feature-test-matrix.md` |

**Button.**
- The chip reads "hide subagents" or "show subagents".
- It is a drag handle like its neighbours: press and move drags the window.
- At rest it has a `Theme.border` outline. While breathing, the background is `brand × breath × 0.35` and the border is `brand × breath`.

**Breath.**
- One Pulse `Glow`, idle while `toggle_breathes(true, any_active, hidden)` is true.
- Because of `toggle_breathes`, it breathes whether the panes are hidden or shown.
- When it stops, it resets to 0 and the chip goes static.

**ctl.**
- With no tab argument, the target is chosen in this order:
  1. The tab holding `$AVADA_PANE_ID`, matched on pane `id` or `sessionUid`.
  2. The `activeTabId` of the window that has `keyboardFocusPaneId`.
  3. The first window's `activeTabId`.
- It reads `/state`, the same path the `state`, `tabs` and `panes` verbs use.

## Tests (all through `cargo-locked.sh`, run one after another)

| Command | Result |
|---|---|
| `cd rs/crates/app && cargo test --bin avada` | **947 passed**, 0 failed |
| `cd rs && cargo test -p avada-core --lib -- --skip permissions` | **2027 passed**, 0 failed, 15 ignored |
| `cd rs/crates/app && cargo clippy --bin avada --all-targets` | clean; `avada-core` was also checked and is clean |

New tests:
- `uitest::the_subagent_toggle_reaches_rust` (`src/uitest/mod.rs:387`):
  - No button when there are no subagents.
  - Clicking Hide reaches `on_toggle_subagents`.
  - When hidden, the button reads Show.
- `uitest::the_subagent_toggle_breathes_only_while_a_subagent_works` (`src/uitest/mod.rs:415`):
  - Drives the real helper over advancing instants.
  - Active + hidden and active + shown both breathe: at least 3 distinct values, all above 0.
  - A stopped subagent stays at exactly 0.
- `state::subagent_tab_tests::toggle_subagents_flips_the_named_tab_and_ignores_a_bad_index`
- `ctl_cli::tests::toggle_subagents_defaults_to_the_calling_panes_tab`
- `ctl_cli::tests::toggle_subagents_falls_back_to_the_focused_window_then_the_first`
- Matrix ratchet `uitest::matrix` is green. Counts: callbacks 184 / bound 108 / proven 108 / unproven 0 / ui-local 76 / Command variants 149 / keyed 15 / keybindings 19.

## Deviations / judgement calls

- **Accessible label vs visible text.** The accessible label is the tooltip ("Hide this tab's subagent panes. Drag to move the window" / "Show …"), not the visible chip text. This is the same `TipArea` convention the left-panel toggle and its test use.
- **`toggle_breathes(true, …)`.** It is called with a literal `true` for "tab is current", because only the active tab is ever projected onto the TopBar.
- **Cadence.** `drive_subagent_breath` returns `true` on every tick that a subagent is working, not only when alpha moved. That keeps `pump` on the fast cadence from the arming tick onward.
- **No keybinding.** The convention doesn't require one (`ToggleSidebar` has none). The command is reachable from the button, the palette and ctl.
- **Windows/Linux position.** This holds by construction: the TopBar uses one `HorizontalLayout` for every platform. I did not run a separate per-platform check.

## Notes for the next lanes

- I removed the `expect(dead_code)` attributes from `set_hide_subagents`, `tab_subagent_summary` and `tab_subagent_summary_at`. **`Tab::visible_panes` still has its own.** The pane lane should drop it when reconcile starts calling it.
- `toggle_subagents_in` only flips the flag and sets `dirty`. Hiding and showing in the grid is the pane lane's `visible_panes` and reconcile work. Until that lands, the button and ctl flip the state, but the grid still draws every pane.
- I did not touch `viewpane.rs` or any reconcile/lifecycle code. In `paneview::pump` my only change is one call to `drive_subagent_breath` just before the HUD block. Expect at most a trivial merge with the pane lane there.
- `ctl toggle-subagents`, like the other tab verbs, answers 202 "queued". REFERENCE.md now reads "All seven".

LANE DONE
