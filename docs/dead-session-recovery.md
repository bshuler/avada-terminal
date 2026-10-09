# Dead-session recovery and pane identity

What happens when the macOS GUI login session is torn down under Avada, why the
2026-10-08 recovery brought seven Claude lanes back as bare shells, and what 0.2.28
changed so it does not happen again.

## The mechanism

The session daemon owns every pty. It is `setsid`'d so it survives the app, which is the
whole point of it — but it also survives a GUI login-session restart (a WindowServer
watchdog kill, a logout, a crash of loginwindow). When that happens the daemon keeps the
*dead* session's Mach bootstrap port, and so does every process it ever spawned: in those
panes `id -un` fails, 1Password and ssh-agent are unreachable, `open`/`defaults`/`pgrep`
fail. See `core/src/session/namespace.rs`.

Since 0.2.20 the daemon's Hello reports `namespace_ok`. A healthy GUI that sees
`Some(false)` does a live takeover of the daemon's ptys and then calls
`App::service_dead_session_restart`, which **restarts every pty pane** — there is no way to
move a running process into the live session, so each one is replaced. The log line is
`dead login session escaped: restarting every pane`.

That restart is where a pane's *identity* matters: a pane that was running a Claude
conversation must come back as `claude --resume <id>` in the same directory, not as a
fresh `zsh`.

## 2026-10-08: what worked and what did not

- 17:32 local: WindowServer watchdog under swap starvation tore down the GUI session.
- 17:35:52: on relaunch the probe fired for the first time against a real dead session,
  the takeover handed over 45 sessions, and the recovery restarted 40 panes. The daemon
  escape itself worked exactly as designed.
- 18 tabs collapsed to 6 — that was a separate, deliberate `move-pane` regroup done by
  another session, not damage.
- The 25 `pane demoted to exited` warnings are the old processes being noticed as gone
  after their replacements were already running. Benign.
- **5 Claude panes resumed their conversations. 7 did not** — they came back as bare
  shells, with the conversation ids still sitting in their saved meta.

## Root cause: split identity

A pane carries two different answers to "what is running here":

| | `PaneState.kind` (the label) | `tool_session` mark / `sniffed_tool` (learned) |
|---|---|---|
| set by | the in-app tool launcher, at launch | the Claude SessionStart hook marker (adopted every 2 s); OSC title sniff |
| control API `new-pane` | always `Terminal` | learned later like any other pane |
| persisted as | `pane.kind` | `tool.session`, `tool.cwd`, `tool.id` |
| rewritten by detection | never | yes |

Every restart decision — `all_pty_panes`, the `let PaneKind::Tool(tool) = &p.kind else
{ return None }` gate in `restart_monitored_pane`, `monitored_panes` — read **only the
label**. The five panes that resumed had been launched from the in-app tool launcher and
so carried `PaneKind::Tool("claude")`. The seven that did not were lanes spawned through
the control API (`avada ctl new-pane --cmd "claude_auto …"`): their label is `Terminal`
forever, even though the app had long since learned their session id and tool from the
hook marker and persisted both. The recovery saw `Terminal`, called `refresh_pane_at`, and
replaced a live conversation with a shell.

This was not a crash-path bug. The same gate would have dropped those panes on any
`restart-pane`, and the periodic restart loop had never monitored them either. The dead
session just restarted everything at once, so it was the first time it showed.

## What 0.2.28 changes

- `State::restart_tool(p)` is the single resolver for "what does a restart of this pane
  have to put back": label, then the learned mark's tool, then the sniffed tool. Both
  `all_pty_panes` and `restart_monitored_pane` use it.
- `restart_monitored_pane` also treats a live Claude hook marker as proof on its own. The
  hook removes the marker on SessionEnd, so one that exists names a conversation that is
  open right now, whatever the pane has or has not learned yet.
- `service_dead_session_restart` reads the marker for every unlabelled pane (not only
  labelled Claude panes), adopts it so the mark is persisted for next time, and never
  leaves a pane un-restarted: anything that declines a tool restart still gets
  `refresh_pane_at`, because no pty pane may keep the dead bootstrap port.
- After a restart, a hand-started pane's mark keeps `tool`, so the *next* restart knows
  too.
- `monitored_panes` (the periodic restart loop) still reads the label only, on purpose: a
  Claude the user started by hand in a shell is not volunteered for scheduled restarts by
  having been noticed. Extending that is a separate decision.

Verified by unit tests against the exact meta shape found in `last-workspace.json`
(`tool.session` + `tool.cwd` + `tool.id`, no `pane.kind`), not by a live dead-session
event — there is no way to exercise the real path without another GUI-session crash. To
verify after a future incident: grep `avada-app.log` for `dead login session escaped`,
then check that every pane with `tool.id` in `last-workspace.json` came back with
`--resume`.

## A second root cause found on the way: `openpty(3)` is not thread-safe on macOS

The new tests restart several panes at once, and in parallel runs two of them failed
intermittently with `failed to restart <pane>: failed to openpty: Os { code: -6 }`. A
plain C probe (16 threads × 200 `openpty` calls, no Rust involved) reproduces it: a
handful of calls per run return no pty pair at all. Nothing in the production logs
shows this, because today every pty is created through one daemon connection whose
reader thread handles `Create` messages one at a time, so the calls never overlap. They
would overlap as soon as two clients create panes at the same moment (two windows, the
Control API next to the GUI), and a pane that loses this race is born dead.

0.2.28 serializes the `openpty` call inside `spawn_pty` (`core/src/session/pty.rs`)
behind a process-wide mutex. Only the microsecond-long libc call is held; cloning,
spawning the child and the reader thread stay unlocked. Ten consecutive parallel runs
of the tool-session tests and the full app and core suites pass with the lock in place.

## 0.2.29: the status loop typed into a bare shell

After 0.2.28 the Avada panel (the app's own Hyperpane pane) came back as a plain `zsh`,
and every fifteen minutes the status loop typed its prompt into that shell and pressed
Enter twice, because `fire_status_loop` only checked the pane's *label*, never what the
pane was running. 0.2.29 classifies the live foreground (`loops::classify_foreground`):
an agent gets the prompt as before, a shell is relaunched from the pane's recipe
(`relaunch_hyperpane_pane`, then `rekey_restarted_pane` so the loop follows the new pane
id), anything else skips the round with a warning. Dead-session recovery routes the
Hyperpane pane through the same relaunch, and `restart_monitored_pane` no longer dies on
a stale directory.

Verified live on 2026-10-09T01:55:47Z: the log shows `status loop: the Avada panel is a
bare shell; relaunching its agent instead of typing` followed by `Avada panel relaunched
old=… new=…`, and the new pane came up as Claude.

One more thing surfaced by that relaunch: the panel directory moved from
`~/Library/Application Support/hyperpanes/hyperpane` to `…/avada/hyperpane` on
2026-10-01, and Claude Code records folder trust per path, so the first launch in the new
directory parked on the trust dialog. The loop correctly skips every round while a form
is on screen, but it says nothing, so a parked form looks like a silent panel. Trust was
accepted by hand on 2026-10-09 and is now in `~/.claude.json`.

## 0.2.30: why the panes stayed bare after the 2026-10-08 crash, and the two-id marker lookup

### The sweep that ran was older than the fix

The dead-login escape fired at 21:35:52Z on 2026-10-08 (17:35 EDT). The GUI that ran it
had launched at 20:49Z with build `191ef33b` — before 0.2.28 (commit `6ff472b`, 20:22 EDT
= 00:22Z next day) existed. In that sweep (`service_dead_session_restart` at `6ff472b^`),
a pane whose `restart_tool` was `None` went straight to `refresh_pane_at`: a bare shell,
no marker read at all. Only panes already labelled or marked as Claude were resumed.
The sessions were handed over intact by the takeover and then killed by the sweep itself.

The markers were there. The six control-spawned Divi lanes had alias-keyed markers with
mtimes 00:00–01:24Z, hours before the sweep; the GUI panes had uid-keyed ones. The old
code simply never looked. 0.2.28 fixed exactly this path (marker read for every
`tool == None` pane), but the 23:45Z relaunch onto 0.2.28 was a takeover without a sweep,
so the 35 bare shells stayed bare until they were resumed by hand with
`recoverPane { action: "resume", sessionId }` on 2026-10-09.

### The remaining bug: one pane, two marker ids

A control-spawned lane inherits its control alias as `AVADA_PANE_ID`, so its hook writes
`<alias>.json`. After any GUI-side restart (`restart_pane_at`), the replacement inherits
the new session uid, so the new hook writes `<uid>.json`. Every lookup preferred the alias
(`pane_id_for_uid(uid).unwrap_or(uid)` in the app; `pane.id` in the dispatcher), so:

- a stale alias marker — never removed, because a killed process never runs
  `SessionEnd` — shadowed the live uid marker. Seen live: alias `3388d9f9` said cwd
  `…/IronDivi-swap-engine/crates/divi-swap` while the running session's uid marker and
  its transcript both said `…/IronDivi-swap-engine`; a resume through the alias would
  have landed where `--resume` cannot find the transcript;
- an aliased GUI pane with a uid-only marker (TG-A, alias `2eb875d2`) got no
  `claude.session` in the snapshot at all, so a cold restore would have lost it.

0.2.30 reads **both** ids and takes the newest marker by mtime
(`claude_panes::read_newest_pane_session`; `App::pane_claude_marker` on the GUI side,
`[pane.id, pane.session_uid]` in `restartPane`/`recoverPane`), and
`rekey_restarted_pane` now deletes the dead incarnation's markers (old uid and alias)
so a kill no longer leaves a marker that reads as live.

## 0.2.31: inferred marks, unlabelled panes, and the marker prune

Three of the 0.2.30 follow-ups below are closed here. All three were verified by unit
tests plus one install-and-relaunch on the live workspace (2026-10-09, 40 panes, 38 of
them in a Claude conversation): every pane came back with the same `sessionId` it
reported before the quit; the six panes whose legacy `tool.session` disagreed with the
hook's `claude.session` were migrated to the hook's id with `tool.inferred = 0`, and the
two shells that had only a guessed mark were demoted to `tool.inferred = 1`; a pane that
had been an unlabelled shell picked up its hook label ("VPS Health") within the first
tick; the 60 s prune deleted 46 Claude markers and 1 tool marker, none of whose ids was
among the 67 live uids and aliases (28 were the known-stale set, the other 18 were
orphans in no tab, alias map, or snapshot). The sweep path itself
(`restart_monitored_pane`) was only exercised by tests, no pane was killed to watch it.
One pre-existing quirk showed up in the check: right after a GUI relaunch every pane
reads `busy` to `inspect` (40/40 at T+2.5 min against 5/40 before the quit), because the
fresh GUI has no `last_output_at` for it yet (`activity_for`'s never-output rule,
unchanged here). At T+6 min 2 of 40 had cleared to `idle`, at T+10 min 4 of 40; the
pane in the front tab (`ctl state` reports `activeTabId`) was still `busy`, so tab
visibility is not what clears it. A Claude prompt that is sitting quiet prints nothing,
so such a pane keeps reading `busy` indefinitely, and anything gated on not-busy
(nudges, the speak-first queue) treats it as mid-turn until it next prints. Tracked as
issue #; not fixed here.

### Inferred marks are placeholders, never resume targets

`history_scan` guesses a mark for a bare shell from the newest transcript under its cwd.
That guess used to be stored exactly like a hook report, so a shell in `~` or `~/code`
carried some other project's conversation id and the next sweep would have resumed it
there (the six panes listed under *Inferred marks pollute parent-directory panes*,
including the deconstruct orchestrator whose `tool.session` was `114d7c4c` while its
Claude hook marker said `12cea8ab`).

`ToolSessionMark` now carries provenance (`inferred: bool`, persisted as
`tool.inferred` in the snapshot meta). `adopt_tool_session` keeps the stronger claim:
a hook report replaces an inferred mark, an inferred mark never replaces anything.
`restart_tool` and `restart_monitored_pane` ignore inferred marks, so a guessed
conversation is never passed to `--resume`. Legacy snapshots without the flag are
migrated by `migrate_tool_session`: when the Claude hook's own `claude.session` key is
present it wins and the mark is marked reported, otherwise the mark is marked inferred.
(`tool.id` cannot stand in for provenance: 0.2.30's `history_scan` inference wrote it
too.) The drop of a guess on restart lives in `restart_pane_at`, beside the reset of
`startup`, so a plain refresh sheds it as well; `restart_monitored_pane` re-sets a
reported mark to whatever conversation it put back. The gone-directory check in
`restart_monitored_pane` no longer waits for a session to be in play: a pane whose own
directory was removed used to be restarted as `cd '<gone>' && claude_auto`, which
fails the `cd` and leaves a bare shell.

### Every pty pane is polled for a marker

`tool_session_wanted` returns a `SessionWanted { tool, cwd, infer }` for every pty pane,
not only labelled or sniffed ones. The adoption pump (`read_any_pane_mark`) reads the
Claude and hooked-tool marker files for each pane, and `watch_pane` is armed when the
pane has nothing better than inference. A plain shell that later starts Claude learns
its conversation from the hook within a tick instead of at sweep time.

### Stale markers are pruned once per launch

Sixty seconds after startup (`MARKER_PRUNE_DELAY`), once the live pane set is known,
`prune_session_markers` deletes every marker under `claude-sessions/` and
`tool-sessions/<tool>/` whose pane id (GUI uid or control alias) is not in the
workspace. The prune is skipped when the live set is empty (a snapshot that failed to
load must not wipe the markers it needs) and logs a receipt naming each deleted file.
`pane-20de33e6.json` is expected in the receipt: that pane hosts two of the owner's
tmux Claude sessions (pids 15233 in `divi-chatbot` and 18483 in `divi_audit`), which
are not Avada panes and must be left running.

### `~/.claude/settings.json` hook entries

`SessionStart`/`SessionEnd` each listed the pane-mark hook twice, once from
`/Applications/Hyperpanes.app/…` and once from Avada. Hyperpanes 0.0.36 is in fact
still on disk, but its hook exits immediately because no pane sets
`HYPERPANES_PANE_ID`, so the duplicate cost one dead exec per Claude start and stop.
The two Hyperpanes entries (one `SessionStart`, one `SessionEnd`, both running
`/Applications/Hyperpanes.app/Contents/MacOS/resources/claude/hp-claude-session-hook.sh`)
were removed; the Avada entries and the `Bash(/Applications/Hyperpanes.app/…)`
permission allow entry were left alone.

### Still open after 0.2.31

- **No backoff on relaunch.** If the relaunched agent exits straight back to a shell,
  the loop relaunches it again every tick, forever. Add a per-pane failure count and stop
  (with a visible alert) after a few consecutive failures.
- **A parked option form is silent.** When `screen_shows_option_form` holds the loop
  for more than one tick, raise a notification naming the pane, instead of a log line.
- **`module.activate failed … the host did not provide a …`** WARN seen in
  `avada-app.log` right after the 0.2.29 relaunch. Not investigated; find which module
  and which host capability it wants.
- **Main checkout carries two uncommitted features.** `/Users/bshuler/code/avada/avada-terminal`
  (13 files: `move-pane … new`, Escape cancels tab rename, two-row tab strip) plus the
  memory-fix session's edits, including `github.rs` with an empty OAuth client id that
  must not be committed. Commit the strip on its own branch from a clean worktree.
- **Marker freshness is mtime-only.** `read_newest_pane_session` picks the newest of
  two files; it cannot tell a marker older than the pane's current pty session from a
  live one when only one file exists. Record the session start time in the manager and
  ignore markers older than it.
- **Three orphan claude processes** (pids 89932, 90268, 90270; sessions 68d6673a,
  e2cc5cb8, c16470b0; markers pane-7900f837, 31b547ed, d8dc156d from 2026-10-06
  13:15Z) belong to no pane. Decide with the owner whether to kill them.
- **Bare shells left after the manual resume**: term 3, IronDivi (term 6),
  Divi Browser Extension, divi-infrastructure, and the old Chat Bot pane in the Divi tab
  (its session now runs in the new Chat Bot pane). Close them once confirmed unwanted.
  `new-tab` also seeds one shell pane per new tab (the `Local` tab created on
  2026-10-09 has one such shell next to the Disk Space pane).

### Closed by 0.2.31 (kept for the record)

- Inferred marks pollute parent-directory panes → provenance flag, sweep ignores inferred.
- `tool_session_wanted` skips unlabelled panes → every pty pane is polled.
- Stale markers on disk (85 files for 34 live processes) → startup prune with receipt.
- Stale hook entries in `~/.claude/settings.json` → Hyperpanes entries removed.
- `pane-20de33e6` hosts two claude processes → both are the owner's tmux sessions,
  not a leak; left alone, documented above.
