# 2026-10-13 · B7: keyboard focus dies or wanders in the live GUI (0.2.41, pid 12185)

Diagnosed on the running process without restarting, attaching to, or typing into it.
Evidence in `evidence/` was read through `avada ctl state`/`ctl panes`, the app log, and the
Slint sources the binary was built from (git `9463a107`).

## For Bert — plain summary

**What is broken.** In the Orchestrator tab, the "cursor" that decides which pane your
keystrokes go to is not where you put it. Sometimes it is nowhere, so typing shows nothing.
Sometimes it silently moves to a neighbouring pane while you type. Clicking the Dock icon or
a tab does not reliably put it back.

**Why.** Your Orchestrator tab is a row of six panes: the orchestrator, then its three
subagent panes, then your two working panes ("repaint fix", "license agent"). Every time
the orchestrator opens or finishes a subagent, a pane is inserted or removed **in the middle
of that row**, and every pane after it shifts one slot. Avada tells the screen about this by
rewriting each on-screen slot with its new neighbour's data. The GUI toolkit (Slint) keeps
keyboard focus on a *slot*, not on a *pane*, so after a shift the slot you were typing into
now shows a different pane, and your keys go there. If the slot at the end of the row is
removed while it holds focus, focus is simply gone: Slint tells nobody, and Avada's "hand the
keyboard to the selected pane" retry has already stopped, so nothing puts it back. Subagent
churn in that tab ran at 23 to 25 events an hour this evening, so this happened constantly.

I watched it happen live: at 20:12 UTC the keyboard holder was your "repaint fix" pane; a
subagent closed and another opened in slot 3; one minute later the keyboard holder was that
new subagent pane, which had already exited. Nobody clicked it.

**What to do right now** (until the fix ships). Any of these forces Avada to re-run the
keyboard hand-off:

```bash
/Applications/Avada.app/Contents/MacOS/avada ctl focus-pane afa39087-9ab0-4b5a-972b-47835eadf51a && /Applications/Avada.app/Contents/MacOS/avada ctl focus-pane 7d41f732-8174-4a92-a377-2391c0706cdf
```

That selects "license agent" then "repaint fix" again. Selecting the pane you are already on
does **nothing** (Avada only re-arms when the selection changes), which is why clicking the
same pane or tab often fails. Switching to another tab and back also works. Hiding subagent
panes does not help: hidden rows still exist and still shift.

The fix belongs to the coding agent and is specified below. It is a small, contained change.

## Timeline (UTC)

| When | What |
|---|---|
| 2026-10-12 21:57:03 | GUI 12185 (0.2.41, commit d1e197b lineage) launched; "dead login session escaped: restarting every pane" count=35. |
| 2026-10-13 12:00 → 17:59 | Subagent pane open/close events: 1, 2, 5, 8, 4, 5 per hour (`evidence/churn-per-hour.txt`). |
| 18:00 → 19:59 | 23 and 25 events per hour, mostly in tab 0:0 (Orchestrator) and tab 0:3. |
| ~20:05 | Bert reports dead keys and wandering focus. `ctl state` shows `keyboardFocusPaneId: None` for the active tab. |
| ~20:10 | Bert clicks into "repaint fix": `keyboardFocusPaneId: 7d41f732…`. |
| 20:12:13 / 20:12:19 / 20:12:37 | Subagent `sub:a0962459…` opened then closed; `sub:a2afca1c…` opened, both in slot 3 of tab 0:0. |
| 20:13 | `keyboardFocusPaneId: sub:a2afca1c…`, an **exited** subagent pane. No click happened (`evidence/kbd-focus-samples.txt`). |

## How the keyboard hand-off works today

- `State::sync_pane_keyboard_focus` (state.rs) runs every pump tick. When the selected
  `(active tab, pane uid)` changes it arms `kbd_focus_tries = KBD_FOCUS_TRIES` (64) and
  on each tick bumps the selected pane's `refocus_seq` from the app-wide `refocus_clock`
  until the widget acknowledges. It stops for good when `kbd_focus_live` equals the selected
  uid, or when the tries run out. Nothing else ever re-arms it.
- `PaneState::refocus_seq` is **stored on the pane** and never reset; `paneview::pane_item`
  copies it into `PaneItem::refocus_seq` for every row, selected or not.
- `paneview.slint` binds `refocus-tick: pane.refocus-seq` on each row's `TerminalPane`;
  `widget.slint` runs `changed refocus-tick => { fs.focus(); root.focus-changed(fs.has-focus); }`
  and `fs` (the terminal `FocusScope`) also reports `changed has-focus`.
- The ack flows `pane-focus-changed(pane.uid, on)` → `Window::focus_acks` →
  `State::note_pane_focus` → `kbd_focus_live`, published by `/state` as `keyboardFocusPaneId`.
- Keystrokes are routed **by row index**: `key(m) => root.key(idx, m)` → `App::on_key(idx)`
  → `active_tab().panes[idx]`.
- `paneview::sync_model` updates the UI model incrementally: it removes rows **at the tail**
  until the count matches, `set_row_data`s every surviving row with the new occupant, and
  pushes new rows at the end. Elements are therefore keyed by slot, not by pane.
- Subagent panes are inserted right after their orchestrator's existing subagents
  (`insert_pane_at` in the "subagent pane opened" path), i.e. in the middle of the list,
  before any pane the user opened later. `take_pane_inner` removes them in place and shifts
  `Tab::focused` down when the removed index is below it.

## Slint facts the design relies on (internal/core/window.rs, items/input_items.rs, model/repeater.rs)

1. `WindowInner::focus_item` is a **weak** reference. When a Repeater drops an element
   instance (row removed), the focused item simply vanishes: no `FocusOut`, no
   `changed has-focus`, no callback of any kind. The window is then focus-less.
2. A freshly created element takes its initial property values **without** firing `changed`.
3. `process_key_input`: if the focus item is not visible, focus is cleared before the key is
   delivered. If nothing handles a Tab/Shift+Tab, the window runs tree-order
   `focus_next_item()`/`focus_previous_item()` and the next focusable element takes focus
   (FocusScope accepts this unless `focus-on-tab-navigation: false`).
4. `set_active(false)` on window deactivation sends `FocusOut(WindowActivation)` to a live
   focus item only; `set_active(true)` restores only if a focus item is still alive.

## Root causes

### RC1 (primary) · The focus trigger travels with the row data, so every pane shift re-targets or drops focus

Because `refocus_seq` is per pane, persistent, and projected for every row, a mid-list
insert or removal makes `set_row_data` change `refocus-tick` on **every row after the shift
point** (each slot now holds its neighbour's seq, and any pane that was ever selected has a
non-zero seq). Each of those rows runs `fs.focus()` in the same pump; the last one processed
wins Slint focus, and its `focus-changed(pane.uid, true)` ack overwrites `kbd_focus_live`
with that pane's uid. That is the 20:13 observation: the inserted subagent row at slot 3
and the shifted rows 4 and 5 all fired, and an exited subagent pane ended up as the holder.

Consequences, in the order Bert sees them:

- **Focus moves while typing.** Keys are routed by `idx`, so the slot that holds Slint focus
  now feeds a different pane's PTY. `Tab::focused` was adjusted for the shift but Slint
  focus was not, and `kbd_focus_on` is unchanged, so no re-arm happens. `kbd_focus_live`
  either goes stale (names the old pane) or names the wrong one.
- **Dead keyboard.** When the pane count shrinks, `sync_model` destroys the **last** slot's
  element. If that element held focus (common: the user's newest panes sit at the end of the
  list, after the subagents), Slint focus is gone with no notification. `kbd_focus_live`
  still names the pane, `kbd_focus_tries` is 0, the selection has not changed, so
  `sync_pane_keyboard_focus` never bumps again. Clicking the Dock icon re-activates the
  window with no focus item to restore. `keyboardFocusPaneId: None` at 20:05 is the variant
  where a `WindowActivation` FocusOut had cleared it first.
- **Clicking the tab does not land.** A tab click re-arms and bumps the selected pane, but a
  row that was just created (tab switch with a different pane count) takes the bumped seq as
  its initial value and never sees `changed`. If the selected pane's `TerminalPane` is not
  visible (`visible: pane.kind < 2`, or a hidden subagent row), programmatic focus succeeds and
  the first keypress drops it (Slint fact 3).

### RC2 · Nothing re-arms the hand-off on the events that invalidate it

The retry only re-arms on a selection change. The three events that actually invalidate the
hand-off (active-tab row set changed, window re-activated, focus holder reported as a pane
other than the selected one) are not watched. The winit hook in `filedrop.rs` sees
`WindowEvent::Focused(on)` and only counts it.

### RC3 · Slint's own Tab traversal can move focus between panes

With no focus item (after RC1), a Tab or Shift+Tab from the shell reaches the window
unhandled and Slint moves focus to the next focusable element in tree order: another pane's
`FocusScope` (via `forward-focus: fs`), a `TextInput` in a rename field, or a search box.
`focus-on-tab-navigation` is left at its default `true` on the terminal `FocusScope`.

## Fix instructions for the coding agent

Work in function names, not line numbers: the running binary is 0.2.41 and the source has
moved. All paths are under `rs/crates/`.

### F1 · Make the refocus trigger a property of the selected slot, not of the pane

- In `app/src/paneview.rs`, `pane_item` (called from the panes projection in `resync`):
  project `refocus_seq` as `if is_focused { ps.refocus_seq } else { 0 }`. Only the selected
  row may ever carry a non-zero tick.
- In `terminal-widget/ui/widget.slint`, the `changed refocus-tick` handler: guard with
  `if (root.refocus-tick != 0 && !root.search-open) { fs.focus(); }` and only then report
  `root.focus-changed(fs.has-focus)`. A row whose tick drops to 0 must not touch focus.
- Optionally stop storing `refocus_seq` on `PaneState` at all: keep one
  `State::refocus_seq_for_selected` and let `pane_item` read it. Fewer places to drift.

### F2 · Re-arm the hand-off whenever it can have been invalidated

In `app/src/state.rs` add a helper `rearm_kbd_focus(&mut self)` that sets
`kbd_focus_live = None`, `kbd_focus_tries = KBD_FOCUS_TRIES`, `dirty = true`, and call it:

1. From `take_pane_inner` and `insert_pane_at` (and any other place that edits
   `Tab::panes` order: `close_subagents_of`, `adopt_pane`, moves/reorders) when the edited tab
   is the active tab. A cheap alternative that catches everything: in `resync` compare the
   active tab's `Vec<uid>` against the one projected last tick and re-arm when the **sequence
   of uids** differs. Compare uids, not the count: a close plus an open in one tick keeps the
   count equal.
2. On `WindowEvent::Focused(true)`. The hook in `app/src/filedrop.rs` already receives it;
   route it into `State` (a flag the pump drains, like `focus_acks`) instead of only bumping
   `FOCUS_GEN`. Window deactivation is also where a `WindowActivation` FocusOut cleared
   `kbd_focus_live`, so a re-arm on re-activation is exactly right.
3. In `note_pane_focus(uid, on)`: if `on` is true and `uid` is not the selected pane's uid
   (and no overlay, editor, or search box owns the keyboard), treat it as a stolen focus and
   re-arm instead of recording it. Today the ack from the wrong pane is accepted as success.
4. In `sync_pane_keyboard_focus`: when `kbd_focus_tries == 0` and `kbd_focus_live` is `None`
   and the window is active, keep retrying on a slow cadence (for example once per second)
   instead of returning forever. A bounded retry is the backstop for any invalidation this
   list missed.

### F3 · Keep Slint from moving focus on its own

- `terminal-widget/ui/widget.slint`, the `fs := FocusScope`: add
  `focus-on-tab-navigation: false;`. Do the same on every other `FocusScope`/`TextInput` in
  `app/ui/*.slint` that is not meant to be reached by Tab (rename fields, search boxes, the
  New-goal box). `key-pressed` already `accept`s Tab, so this only matters when no pane
  holds focus, which is exactly the failure state.
- Review `TerminalPane { visible: pane.kind < 2 }` and hidden subagent rows: the selected
  pane must never be a row whose `TerminalPane` is invisible. If `Tab::focused` can point at a
  hidden subagent after `take_pane_inner`/`hide_subagents` toggles, move it to the nearest
  visible pane there (the `visible_layout` helper already knows which rows are visible).

### F4 · Focus on creation

Elements created with the bumped seq as their initial value never see `changed`. In
`widget.slint` add `init => { if (root.refocus-tick != 0 && !root.search-open) { fs.focus();
root.focus-changed(fs.has-focus); } }` so a freshly created selected row takes focus itself.
With F1 only the selected row has a non-zero tick, so this is safe.

### F5 · Observability and a control verb

- Log at `info` in `note_pane_focus` (uid, on, whether it matched the selected pane) and in
  `rearm_kbd_focus` (reason). Commit 1dab2c4 already logs `window focus event`; keep it.
- Add `selectedPaneId` (from `Tab::focused`) next to `keyboardFocusPaneId` in `/state`
  (`control_host.rs`, the window snapshot). The bug is visible as the two disagreeing, which
  makes it testable from `ctl state` without a human at the keyboard.
- Add `avada ctl refocus [pane]` that calls `rearm_kbd_focus` for the active tab (today
  `focus-pane` on the already-selected pane is a no-op).

### F6 · Tests

In `state.rs` next to `a_late_acknowledgement_confirms_the_pane_it_names_not_a_slot`:

- Inserting a subagent pane before the selected pane in the active tab re-arms the hand-off
  (`kbd_focus_live == None`, `kbd_focus_tries == KBD_FOCUS_TRIES`) and the next tick bumps
  the selected pane only.
- Removing a pane below the selected one leaves `Tab::focused` on the same uid **and**
  re-arms.
- An ack naming a pane other than the selected one does not set `kbd_focus_live` and re-arms.
- A `WindowEvent::Focused(true)` flag drained by the pump re-arms.
- `pane_item` projects `refocus_seq == 0` for every non-selected row.

### F7 · Ship

Build from the current `fix/repaint-on-show` lineage so the installed binary matches the
source (the live 0.2.41 predates 1dab2c4's focus logging). After install, reproduce with the
Orchestrator tab active and a subagent-heavy session running, and confirm via
`avada ctl state` that `keyboardFocusPaneId` tracks `selectedPaneId` across at least ten
subagent open/close events.

## Evidence files

| File | What |
|---|---|
| `evidence/ctl-state.json` | `/state` snapshot at 20:13 UTC: active tab 0:0, keyboard holder an exited subagent pane. |
| `evidence/ctl-panes.txt` | Pane order per tab: orchestrator, three subagents, then the user's panes. |
| `evidence/kbd-focus-samples.txt` | The three `keyboardFocusPaneId` values seen during diagnosis and what happened between them. |
| `evidence/subagent-pane-churn-2026-10-13.log` | Every subagent open/close line from the app log today. |
| `evidence/churn-per-hour.txt` | The same, counted per hour. |
| `evidence/process.txt`, `evidence/process-start.log` | Pid 12185, launch time, version. |
