# Backlog — open items

GitHub issues are disabled on this repository, so this file is the tracker. One entry per
open item, newest first; an entry leaves when its fix ships and the commit names it. The
next-session command line at the end of every session points at the entry it will work.

## Open

### B13 — the Open Questions relay does work itself instead of handing it to the Manager (2026-10-15)

Bert's direction (`manager-architecture-spec.md` §1a, MUST 9–11): he talks to the Manager only
through the **Open Questions** agent, which relays in both directions and never builds, fixes,
spawns or messages anyone but the Manager; work goes Manager → Orchestrator → Builder. Audit of
the live fleet on 2026-10-15 against that:

- The Open Questions pane (`pane-990350ae…`, tab 0:10, opened by the Orchestrator tab
  2026-10-14 11:40) runs its Claude session in **`~/code/health`** (project
  `-Users-bshuler-code-health`, session `2d2e6a48…`), so it carries health's memory and
  tools, not the orchestrator data directory's operating-model rules.
- Its brief (`…/283de732…/scratchpad/open-questions-concierge-brief.md`, written by the Manager
  2026-10-14 11:39) says read-only / inform Bert / peer not manager, but **never says to forward
  Bert's instructions to the Manager**, and it lives under `/private/tmp`, so it dies with the
  session store.
- Consequence, from the pane's transcript: Bert used the pane as his instruction inbox
  (minecraft backups, iPhone backup, hp1 move, screen captures, `avada://` links …). The pane's
  session answered or did those itself: on 2026-10-15 11:31 it spawned a worktree subagent
  `agent-aaf802ef449040b55` "Integrate avada:// scheme into clickable terminal links" and at
  10:44 Bert asked it "you seem to be sending messages to different agents, do you not know the
  correct path?". The Manager itself has complied with the consolidation ruling it recorded at 08:14 EDT
  2026-10-15: 278 Manager turns between then and 11:32 EDT with zero `AskUserQuestion` calls
  (75 before the ruling).
- Nothing in the repo enforces the relay: no code, no module rule, no test. Phase 3 (right
  Q/A panel) is the native fix and has no code yet.

Fix direction, in order: (1) **done 2026-10-15** (by Bert's research session, on his choice,
not by the Manager): the brief is `docs/open-questions-brief.md`, copied to
`~/Library/Application Support/avada/open-questions/AGENTS.md`, a sibling of `orchestrator/`,
with a PreToolUse guard hook (`.claude/hooks/relay_guard.py`, tested against sample tool
calls) that refuses agent spawning, writes outside the relay dir and `orchestrator/inbox/`,
session launches, worktrees, commits, builds and every pane-driving `ctl` verb; the pane was
relaunched there and its first act was to carry Bert's "implement these goals" instruction
down to the Manager (`orchestrator/inbox/`). Not the orchestrator data directory, because a
session there inherits the Manager's memory namespace and the operating-model fences only
bind an accepted `AVADA_ROLE`. The old pane stays until its background agent finishes, then
the Manager parks it. (2) the operating-model module grows a relay role (`AVADA_ROLE=relay`,
accepted by `rs/crates/core/src/pane_role.rs`) whose fenced rule is the brief, so it travels
with Avada and the hook becomes a module file; (3) phase 3 of `manager-architecture-plan.md`
replaces the pane with the right Q/A panel carrying all three flows, built as an Avada
**module** in `rs/modules/` (spec MUST 5 "Shape", Bert 2026-10-15): it surfaces the
questions the relay pane carries and sends the answers back; every question shows four
options with one marked recommended plus a write-in area, matching Claude Code's
pick-a-number prompt, and a "More context" button that fetches that question's source
material on demand. Related: B7 (Orchestrator
tab focus), `operating-model-module.md`.

### B11 — live daemon takeover times out under load and the old daemon shuts down anyway, losing every pane (2026-10-14)

Installing 0.2.45 and relaunching at 18:38 local, with the Mac at load ~300, blanked all 33
panes: the successor daemon's `take_over` gave up after `TAKEOVER_RECV_TIMEOUT` (5 s) and
declared "did not answer", while the incumbent needed 6 s to run `registry.hand_off()`; the
incumbent then hit `EPIPE` sending to the dead successor and called `lifecycle.shutdown`
regardless, closing 65 pty masters — every shell got a hangup and nothing was adopted (zero
adopt lines; 33 fresh `/bin/zsh`). 30 Claude panes resumed from markers; the broker's launch
terminal, Proton-on-hp2 and the divi-nft fork pane came back as bare shells (the
`secret-broker serve` process itself survived: it was already detached, ppid 1). Both sides of the handoff
assume a responsive machine and neither confirms the other before giving something up
(`rs/crates/core/src/session/daemon.rs`: `take_over` ≈358, `hand_over` ≈1117–1163). Write-up,
log timeline and the fix spec (incumbent relinquishes nothing before an ack and re-adopts on a
failed send; successor treats a slow peer as alive, up to `TAKEOVER_LOCK_BUDGET_HOLDING`; tests
on hp2): [`incidents/2026-10-14-takeover-timeout-under-load/`](incidents/2026-10-14-takeover-timeout-under-load/README.md).
Related: B4 (the launchd fix worked — the daemon survived the GUI quit; this is the handoff
after it). Until fixed: never install-and-relaunch while `uptime` load exceeds the core count.

### B10 — avada:// links: one live click on an installed build

**Filed 2026-10-14. Shipped in `1f1bfed` (released in 0.2.48).** `avada://ssh|terminal|doc` and
`ssh://` links ([`links.md`](links.md)) are unit-tested (core `cli::link`, app `url_cli`).

**Verified live 2026-10-15 on the installed 0.2.48:** LaunchServices maps `avada://` to
`/Applications/Avada.app` from the Info.plist alone. `open "avada://terminal?title=…"` and
`open "avada://doc?path=~/code/avada/avada-terminal/docs/links.md&title=…"` each added a tab to the
running window (tabs 15 and 16 in `avada-app.log`). The doc opened rendered as Markdown, and both
panes carry the link's title in the pane header and in `last-workspace.json`.

**Open:**
- Fixed after 0.2.48, not yet in a release: the tab strip labelled these tabs `term 2` / `term 3`.
  A tab a link opens is now named after the link: its `title=`, the file name, or `ssh host`
  (`cli/parse.rs`; test `a_link_tab_is_named_after_the_link_not_term_n`).
- `avada://ssh/…` has not been clicked live.
- `ssh://` still opens Terminal.app. It goes to Avada only once Bert picks Avada under "Open ssh URLs
  with" or runs `avada url register --ssh`, which is his setting to change.

### B9 — title-strip wordmark overlaps tab titles — SUPERSEDED 2026-10-14

The wordmark no longer shares a row with the tabs. The Arcane mockup puts the centred gold
"AvadaTerminal" in its own 38px title bar (`TitleBar` in `ui/topbar.slint`) with the tab row
beneath, so the overlap cannot occur. Test:
`uitest::tabs::the_title_bar_sits_above_the_tab_row_with_the_title_centred`.

### B7 · Keyboard focus dies or wanders in the Orchestrator tab under subagent churn (2026-10-13)
Slint keeps keyboard focus on a *row slot*; `paneview::sync_model` rewrites slots in place
and drops the tail row, and `PaneState::refocus_seq` is projected for every row, so each
mid-list subagent insert/remove re-fires `changed refocus-tick` on every shifted row (last
one wins, keys route by `idx` to the wrong pane) or destroys the focused element silently
(dead keyboard, never re-armed). Observed live on 0.2.41 pid 12185: the holder jumped from
"repaint fix" to an exited subagent pane with no click. Write-up, evidence and the fix spec
(F1–F7): [`incidents/2026-10-13-keyboard-focus/`](incidents/2026-10-13-keyboard-focus/README.md).
Workaround: `avada ctl focus-pane <other pane>` then back (same-pane focus is a no-op).

### B4 · Force Quit of the GUI killed the session daemon and every pane (2026-10-09)
Dock Force Quit of the 0.2.31 GUI (pid 76456) at 14:40:46 ended with loginwindow's
"has fully exited including subordinate processes" at 14:40:47.194, and the daemon (76462,
a setsid child of the GUI) died with it, silently. The relaunched GUI spawned a fresh daemon
(`took_over=false`) and all 49 Claude sessions restarted from a bare shell. Write-up and
evidence: [`incidents/2026-10-09-b3/`](incidents/2026-10-09-b3/README.md), root cause 2.
Fix shipped in 0.2.42: on macOS the daemon is bootstrapped as its own launchd job
(`rs/crates/core/src/session/launchd.rs`, the analogue of the Linux `systemd-run --scope`
path), SIGTERM/SIGHUP/SIGINT are logged with the sender pid (`signal_log.rs`), the daemon
logs its ppid and label at start, and the GUI logs a line on a normal quit.
**Open until verified:** once the daemon has rolled onto 0.2.42 (Avada ▸ Quit, reopen),
Force Quit the GUI from the Dock and confirm the daemon pid is unchanged and the new GUI
logs `took_over=true`. Until that rollover, use Avada ▸ Quit, never Force Quit.

### B3 · GUI wedged (kernel-suspended) under swap exhaustion; control API starved (2026-10-09)
Seen on the 0.2.31 GUI (pid 76456, daemon 76462) during a machine-wide swap thrash (8.4 of
9 GB swap used, load 89); it did not recover once memory was freed. Write-up and evidence:
[`incidents/2026-10-09-b3/`](incidents/2026-10-09-b3/README.md), root cause 1.
- Root cause: the kernel **suspended the GUI process** (`proc_pidinfo` flags `PC_SUSP` and
  `PA_SUSP`, every thread `WAITING` at 0 % CPU, `T` priority suffix in `ps`). The `sample`
  that showed the main thread in `render_screen → request() → recv_timeout` was a frozen
  snapshot, not a loop: the earlier reading that `request()` "retries forever" and that the
  reader thread sleeps on a full receive queue is withdrawn. The queue filled because its
  reader was frozen.
- The daemon was healthy throughout: fresh probes answered in under 30 ms, 50 sessions
  listed, every pane PTY intact.
- The remedy "quit and reopen the GUI; the daemon and every pane survive" held only for
  Avada ▸ Quit. A suspended GUI cannot be quit normally; the Force Quit that was the only
  option killed the daemon as a subordinate process. That is B4, fixed in 0.2.42.
Hardening still worth doing, none of which would have helped a suspended process:
1. Keep `render_screen` off the UI thread (cache the last screen, refresh asynchronously)
   so a slow daemon link can never stall the Slint loop or the control server.
2. After N consecutive `SCREEN_TIMEOUT`s on one connection, mark the link dead, log it,
   and let the redial path rebuild it.
3. Reproduce under memory pressure (`sample` + `proc_pidinfo` flags) before closing; the
   first thing to check on a "wedged" GUI is now `pbi_flags` for `PC_SUSP`/`PA_SUSP`.

### B1 · Pane activity reads `busy` after a GUI relaunch until the pane prints (2026-10-09)
Pre-existing `activity_for` rule (`rs/crates/core/src/control/server.rs`): no liveness marker
seen and `last_output_at == None` → Busy. Observed on the 0.2.31 relaunch: 40/40 busy at
T+2.5 min, 36/40 at T+10 min, front tab's pane still busy, so visibility is not the trigger;
a quiet Claude prompt prints nothing and stays busy indefinitely, which blinds anything gated
on not-busy (nudges, speak-first). Fix direction: seed `last_output_at` at attach/resume, keep
the brand-new-pane rule, add the resumed-pane test beside
`activity_is_busy_for_a_never_output_running_pane`. Context:
[`dead-session-recovery.md`](dead-session-recovery.md) §0.2.31 last paragraph.

## Closed

### B12 · A daemon handoff orphans every pane's TCC attribution; screen recording only works in panes opened after the relaunch (2026-10-14, fixed 2026-10-15)

**Fix landed on `main` 2026-10-15** (commit `f9cc6c7` "B12: the retired daemon execs into a linger
stub so pre-upgrade panes keep their TCC responsibility", plus the 0.2.50 bump that closes this
entry). **Proven 2026-10-15 on 0.2.49 → 0.2.50** — two witness panes opened under the 0.2.49
daemon (pid 98226; shells 98791 and 99060) captured before the upgrade (`CAP_OK_P1/P2`, 12:03),
then `install-macos.sh` 0.2.50 + `osascript quit` + `open`: the 0.2.49 daemon handed 46
sessions to the successor (pid 1655) and exec'd into
`/Applications/.avada-attic/Avada-0.2.49-20261015120814.app/Contents/MacOS/avada --daemon-linger …`
keeping pid 98226 (`ps`: ppid 1, 05:32 CPU carried over; `launchctl print` of the old daemon's
job still names pid 98226; daemon log "linger stub started: holding TCC responsibility"). Both
witness shells still report `PPID=98226` and `screencapture -x` succeeds in both
(`CAP_OK2_P1/P2`, 12:08). tccd: six `Handling access request to kTCCServiceScreenCapture …
Resp:{… pid=98226 … responsible_path=/Applications/.avada-attic/Avada-0.2.49-…/Contents/MacOS/avada}`
lines, all `Allow`. The one-upgrade lag held as predicted: on the 0.2.48 → 0.2.49 hop the
retired 0.2.48 daemon exited with no stub. Both witness panes were left open. **Closed.**

**Filed 2026-10-14.** macOS attributes a pane shell's privacy requests (Screen Recording, etc.) to the process that was
*responsible* for it when it was spawned: the session daemon it was born under. After a GUI quit +
reopen, the successor daemon adopts the ptys but the kernel's responsibility link still points at
the retired daemon; once that daemon exits, each old shell is reparented to launchd and becomes
responsible for itself (`responsibility_get_pid_responsible_for_pid` returns its own pid). A bare
`zsh` has no TCC row and cannot prompt, so `screencapture` in every pre-relaunch pane fails with
"could not create image from display" while the identical command in a pane opened after the
relaunch succeeds. Observed 2026-10-14 21:06 after the 0.2.48 relaunch: test pane opened at
21:03 under daemon 58577 → `Allowed (System Set)`; test pane from 20:5x under the retired daemon
51411 → denied, `ps` ppid 1. This is independent of code signing (0.2.48 fixed the *other* half:
ad-hoc signatures made the grant itself die on every rebuild — commit `3b63dc7`). Every
install + relaunch therefore still forces users to reopen every pane that needs screen capture,
including the Avada UI pane's Claude session. Fix direction: the adopted pty's shell cannot be
re-parented, so the successor must either (a) keep the retired daemon alive as a responsible
stub for its old shells (it runs from the attic bundle, which the install script already keeps),
or (b) respawn the shell under the new daemon on request (`avada ctl respawn <pane>`), or (c) use
`responsibility_spawnattrs_setdisclaim` so panes are responsible for themselves and get their own
TCC prompt (then the grant is per `zsh`, which macOS shows as "zsh" — worse UX). Proof artefacts:
`/tmp/tcc6.log` (tccd `AttributionChain` lines) and this session's write-up. Related: B11 (the
handoff itself), B4.

**Decision 2026-10-15 — (a), as an `exec` into a linger stub.** The retired daemon, after
`hand_over` has sent every pty to the successor, no longer `exit`s: it unlinks its socket, marks
every descriptor close-on-exec and `execv`s its *current image* (`proc_pidpath`, i.e. the attic
copy `install-macos.sh` keeps — never the freshly installed binary) as `avada --daemon-linger
<salt>` (`rs/crates/core/src/session/linger.rs`). `exec` keeps the pid, the parent/child links,
the launchd job and the TCC responsibility and drops every thread, fd and destructor, so the
stub holds no socket, lock or port and cannot type at a shell; it only `waitpid(-1)`s until
`ECHILD` and exits. Why not (b): `respawn` loses the shell's state (jobs, history, a running
Claude session) and still needs the user to notice and act; why not (c): a per-`zsh` grant means
a Screen Recording prompt labelled "zsh" for every pane and a TCC row per shell, worse than the
bug. Cost of (a): one extra idle process per upgrade (`ps` shows `avada --daemon-linger …`
with `ppid 1`; `launchctl print gui/$UID/<label>` still names its pid) until the last
pre-upgrade shell closes, and one upgrade of lag: panes born under a pre-0.2.49 daemon are not
rescued, because that daemon has no stub to exec into. Unit tests in `linger.rs` and `main.rs`
(flag parsing, cloexec marking, drain/EINTR, "not a `--session-daemon` superstring" so the
install script's daemon census and attic prune stay correct).

### B8 · Subagent scan on the main thread, 1 Hz, over every hook record ever written (2026-10-14, fixed 2026-10-14)

**Fix landed on `main` 2026-10-14** (three commits: `B8 part 1` archive finished records,
`B8 part 2` scan off the UI thread at 5 s, `B8 part 3` the hook pushes `ctl subagent-event`;
details in the report's *Fix* section). **Proven 2026-10-14 on 0.2.43** — `sample`: main-thread
scan frames 539 → 0, `reconcile_subagents` 563 → 1 of the tick's samples, the poll's reads on a
worker thread at ≈6 ms/s; 1067 finished records archived at first launch (report, *Proof*).
Not re-measured: root `fs_usage` (needs a human terminal). **Closed.**

**Filed 2026-10-14.** Bert: "it looks stuck" / "why scan every second? maybe avada could have its
own message broker data bus. So pushes, no pulls." Report with evidence:
[`docs/incidents/2026-10-14-subagent-scan-main-thread/README.md`](incidents/2026-10-14-subagent-scan-main-thread/README.md).

`State::reconcile_subagents` runs every second on the UI thread and, through
`App::pane_subagents` → `claude_subagents::subagents_from`, re-reads every hook record of every
pty pane: 1451 records (1448 finished) across 29 panes, ~23 k filesystem syscalls/s, a third of
the main thread's samples, growing ~900 records/day. Fix in three independent parts, cheapest
first: (1) archive/cached-skip finished hook records so a scan touches only live ones;
(2) run the scan off the UI thread and hand results back through a tick-drained queue
(`pane_write_failed_uids` pattern); (3) push: `avada ctl event …` → `POST /events` from the hook
(backgrounded, silent), control host queues it, tick rescans only that pane; keep a slow
mtime-gated fallback for hook-less sessions, or FSEvents. Proof: `sample` shows no
`subagents_from` on the main thread; root `fs_usage` shows no `claude-subagents/` opens between
real subagent events.

### B6 · The orchestrator module's always-on rule never reaches its folder (2026-10-11, fixed 2026-10-11)
`skills::Materializer` had no runtime caller, so the `bshuler/avada-orchestrator` module's rule
never landed in `~/Library/Application Support/avada/orchestrator/AGENTS.md`; 0.2.38 only removed
the stale pre-module `avada/hyperpane` block.

**Fixed (0.2.39):** `ModuleRuntime::new` and `ModuleRuntime::reconcile` pass the running modules to
`orchestrator::materialize_modules`, which plans the orchestrator folder alone (no home, workspace,
schema or index skill) with `Tools::detect_here_with(disabled).with("claude-code")`; errors are
logged, never fatal. Only fenced files are written or removed, so the shipped
`.claude/skills/avada/` and the human's own notes stay byte-identical. Tests:
`orchestrator::tests::{an_enabled_modules_rule_is_written_and_nothing_else_is_touched,
a_disabled_modules_rule_is_swept_and_nothing_else_is_touched}`. Left behind on purpose: an
unfenced September copy at `orchestrator/skills/hyperpane/SKILL.md`, which no sweep owns.

### B2 · Subagent panes and the breathing "hide subagents" toggle (2026-10-09, built 2026-10-10)
Spec: [`subagent-panes-plan.md`](subagent-panes-plan.md), design contract
[`subagent-panes-design.md`](subagent-panes-design.md). Built in four lanes (discovery e8d9ae0,
panes c521e95/5a4a366, chrome c7c774f, proof), merged with main at a79f05e.

**Built:** a Claude hook plus the session's `subagents/` transcript directory give each pane's
subagents (`avada_core::claude_subagents`); `State::reconcile_subagents` (1 Hz) opens a read-only
`sub:<agent id>` viewer pane after the orchestrator, closes it as soon as the subagent is finished,
never reopens one the user closed, and closes the lot with the orchestrator.
Subagent panes are not saved in the session file.

**Revised 2026-10-10 (the pane flood):** the installed 0.2.33 opened every subagent a resumed
session had *ever* run — 297 panes across the window, 193 in the TPLX tab — because the close rule
needed both a hook `stopped_at` and the orchestrator's `last_turn_end`, and a pre-hook session has
neither. `reconcile_subagents` now closes a finished pane at once (Bert: "finished should be
closed"), treats a hook-less record as done when its transcript is quiet (`GROWTH_IDLE` 10 s to
open, `FALLBACK_CLOSE_IDLE` 60 s to close), and caps a tab at `SUBAGENT_TAB_CAP` (4) panes: the
next subagent goes to `<tab> 2`, `<tab> 3`, … inserted after its family, and an emptied overflow
tab drops with its last pane (Bert: "4 panels per tab max … increment name number tab"). Tests:
`subagent_pane_tests::{a_stopped_subagent_closes_at_once_and_is_never_reopened,
a_quiet_fallback_only_subagent_opens_no_pane,
an_open_fallback_only_pane_closes_once_its_transcript_goes_quiet,
the_fifth_pane_overflows_into_a_numbered_tab_that_closes_when_emptied}`. Same-day relief in the
running app was `ctl toggle-subagents` on the four flooded tabs. Note: `main` has not merged B2;
main's 304825a applies the same `MAX_PANES_PER_TAB` rule to the control API's `new-pane`. The TopBar chip (`Command::ToggleSubagents`,
`ctl toggle-subagents [tab]`) hides them per tab — no tile, the rest take the room — and breathes
on the Pulse curve while any is working, hidden or not.

**Open hole (2026-10-10):** the four-pane cap is applied at *placement* time, and a tab with `hide_subagents` set places without a cap because hidden panes take no room. `hide_subagents` is persisted, so a tab hidden under 0.2.33 with dozens of still-running subagents can expose more than four at once when Show is clicked after the relaunch. Re-apply the cap (move the excess into "<tab> N" tabs) on Show, or cap at placement even while hidden.

**Acceptance (plan §4):**
- hook payload → record, fallback finds the same set: `claude_subagents::tests::captured_payloads_become_a_record_silently`,
  `a_hook_record_is_enriched_from_meta_and_matches_the_fallback_set`, `the_transcript_dir_fallback_reads_meta`;
- breathing predicate: `claude_subagents::tests::toggle_breathes_truth_table`,
  `uitest::the_subagent_toggle_breathes_only_while_a_subagent_works`;
- UI end to end: `uitest::subagents::a_sessions_subagents_open_beside_the_orchestrator_and_the_toggle_hides_them`;
- control API: `control::routes::tests::state_carries_a_subagent_pane_and_its_parent_link`;
- matrix row `toggle_subagents` (`e2e`) and the `ToggleSubagents` command row.

### B5 · Recently Closed is emptied by a GUI relaunch, orphaning parked sessions (2026-10-10, fixed 2026-10-10)
`State::closed` (`rs/crates/app/src/state.rs`) is not persisted, on the stale premise "sessions
don't survive a relaunch". Since the session daemon they do: the 15:28Z relaunch on 2026-10-10
started with an empty list, so the parked Subagents tab could not be reopened. Its two
`pane-*` sessions kept running with no pane and no way back (their Claude markers were then
pruned as stale), and the self-heal re-hosted the tab's two control panes into the *active* tab,
which `gui_uids_with_parked` says must not happen. Fix direction: persist the history (ids + session
uids) with the workspace, re-adopt on restore, kill only what the daemon no longer has; add a
relaunch test beside the restore-workspace tests.

**Fixed:** the relaunch snapshot now carries the history (`WorkspaceFile.closed`, one
`ClosedSpec` per entry: kind, close time, and the pane/tab as a `GroupSpec` with session uids).
`App::session_snapshot` merges every window's entries; the restore seeds them back after the
tabs load (`State::restore_closed_from`), keeping only panes whose session the daemon still runs
and no restored tab already shows, and ends whatever falls past the 20-entry cap. It never spawns, so without the daemon (or after a
plain window close, which kills parked sessions) nothing comes back. Restored entries sit in
`st.closed`, so the self-heal's `gui_uids_with_parked` exclusion covers them again.

