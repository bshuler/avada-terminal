# 2026-10-14 — "Avada looks stuck": the subagent scan runs on the main thread, once a second, over every hook record ever written

**Status:** fixed in code on `main` (2026-10-14, the three B8 commits listed under *Fix*);
**proof pending a GUI restart** on a build that carries them. The running 0.2.42 GUI still has
the old scan; its live state was left as found (the *Mitigation* below is now moot: the fixed
build archives those records itself).

## What Bert saw

Avada 0.2.42 (pid 63812, up since 2026-10-13 21:12) with 29 panes felt stuck: slow to repaint,
slow to take keys. It was not hung.

## What it is

The GUI is alive — the control API answers in ~40 ms, the log flows, the process is not
kernel-suspended (`pbi_flags` clear; compare the 2026-10-09 case in the Mac-slowdown notes) and
sandboxd is idle (3 threads). The main thread is simply busy: every second it re-reads **every
subagent hook record every pane has ever produced**, on the UI thread, synchronously.

| measurement | value |
|---|---|
| 5 s `sample` at 1 ms, main thread | 1130 samples; 397 in `State::reconcile_subagents`, 375 in `App::pane_subagents`, 368 in `claude_subagents::subagents_for_pane` |
| 2 s `sample`, main thread | 235 samples, 183 inside `reconcile_subagents` |
| root `fs_usage -f filesys` for 3 s | 6929 `open`, 5889 `stat64`, 8108 `read`, 4152 `close`, 4094 `fstat64` (~23 k filesystem syscalls / s) |
| top paths in that trace | `claude-subagents/<pane>/<agent>.json` hook records and `<project>/<session>/subagents/agent-<id>.meta.json` |
| hook records on disk | 1451 across 32 pane dirs; 1448 finished (`stoppedAt` set), 3 unfinished |
| records written in the last 24 h / 7 d | 886 / 1427 (by file mtime), so the cost grows by roughly 900 records a day |
| one full scan, emulated in Python with a warm cache | 2872 opens, ~357 ms |
| panes being scanned | 29 panes in state, 27 of them pty panes = orchestrator candidates |

So a scan costs on the order of a third of a second and runs every second, on the thread that
paints and handles keys. That alone makes the app feel stuck; the B7 focus loss
(`keyboardFocusPaneId = None` while a pane is selected, see
[2026-10-13-keyboard-focus](../2026-10-13-keyboard-focus/README.md)) was active at the same
time and compounds it. A restart does not help: the hook dirs persist and the first scan after
launch re-reads the same 1448 finished records.

Evidence files are in [`evidence/`](evidence/): the main-thread block of the 5 s sample
(`sample5-main-thread.txt`, mangled Rust symbols shortened), the per-frame counts
(`sample5-frame-hits.txt`), the fs_usage histogram and top paths (`fsusage-summary.txt`), the
hook-record census (`hook-record-census.txt`) and the pane count. Raw captures (full 1 MB sample,
7 MB fs_usage trace) are in the session scratchpad only.

## Why — the code path

* `App::tick` (`rs/crates/app/src/app.rs`, a Slint timer at `TICK_FAST_MS` = 8 ms) calls
  `w.state.borrow_mut().reconcile_subagents(&self.mgr, now, |uid| self.pane_subagents(uid))`.
* `State::reconcile_subagents` (`rs/crates/app/src/state.rs`, ~3200) early-returns unless
  `SUBAGENT_RECONCILE_EVERY` (1 s) has passed, then treats **every `kind.is_pty()` pane** as an
  orchestrator and calls the lookup for each.
* `App::pane_subagents` (`app.rs:1189`) reads the pane's session marker and calls
  `claude_subagents::subagents_for_pane(pane_id)`.
* `subagents_from(hook_dir, transcript_dir)` (`rs/crates/core/src/claude_subagents.rs:179`)
  `read_dir`s the pane's hook dir, `read_to_string` + serde-parses **every** `*.json` record,
  and for each one opens `agent-<id>.meta.json` and stats the transcript; then `read_dir`s the
  session's `subagents/` dir for hook-less records.
* Nothing forgets a finished record while its pane lives: `prune_dirs_in` only removes the dirs
  of panes that no longer exist. A pane that has run 463 subagents (the biggest dir today) pays
  for all 463 on every tick, forever.

The scan has no change detection: no directory mtime check, no cache keyed on (path, mtime), no
file watcher (no `notify`/kqueue/FSEvents dependency in either crate), and no event from the
hook — `resources/claude/hp-claude-session-hook.sh` writes the record and exits; it never tells
the GUI anything.

## Bert's question: why scan every second? Pushes, not pulls.

Agreed, and the pieces for a push already exist. Three independent parts, cheapest first; the
first one removes ~99.8 % of the work by itself and should not wait for the other two.

1. **Stop re-reading finished records.** When `reconcile_subagents` drops a pane because the
   record is finished, move the hook record into an `archive/` subdir of the pane's hook dir
   (or cache the parsed record by `(path, mtime)` and skip unchanged files). Today 1448 of 1451
   records are dead weight; after this a scan touches ~3 files. Also archive on the
   `fallback_only` close path and at startup for records whose `stoppedAt` is set.
2. **Never do this I/O on the UI thread.** Run the scan on a background thread that posts its
   result into a `Mutex<Vec<…>>` queue drained by `tick` — the same shape the code already uses
   for `pane_write_failed_uids` (`app.rs` ~1960), because `State` is `!Send`. The UI thread then
   does only the in-memory reconcile.
3. **Push instead of poll.** Avada's control host *is* the message bus: a loopback HTTP server
   with a master token discovered through `control.json`, and every pane shell already carries
   `HP_CTL` and the app dir on `PATH` (`state.rs` ~4347). So:
   * add a control verb, e.g. `avada ctl event subagent-start|subagent-stop|turn-end --pane $AVADA_PANE_ID --agent <id>` → `POST /events`;
   * the hook calls it after writing the record, **backgrounded with stdout/stderr to
     `/dev/null` and a short timeout**, so a slow or absent GUI can never block Claude Code and
     the hook still prints nothing (the "must print nothing on stdout" rule in the hook header);
   * the control host appends the event to a queue; `tick` drains it and rescans **only the
     named pane** (off-thread, part 2);
   * a separate broker process is not needed; an in-process event queue fed by the control
     socket is the whole "data bus", and the same `/events` endpoint can later carry other
     agent-to-GUI signals (done, needs-input, error).

   A slow fallback scan still has to stay, because the hook-less path (sessions that started
   before the hook was installed, whose subagents are known only from
   `<session>/subagents/agent-<id>.meta.json` written by Claude Code itself) has nothing that
   can push. Gate it on the directory mtime (a new file in a dir bumps the dir's mtime on APFS)
   and run it every 5–10 s off-thread; `GROWTH_IDLE` (10 s) and `FALLBACK_CLOSE_IDLE` (60 s)
   bound what cadence is semantically safe. Alternatively watch the two dirs with FSEvents or
   kqueue (`notify` crate), which makes even the fallback a push.

   The hook already exits 0 before doing anything when `AVADA_PANE_ID` is unset, so the new call
   adds no failure mode for Claude sessions launched outside Avada.

**Proof for the fix:** `sample <pid> 5` shows no `subagents_from` frame on the main thread, and a
3 s root `fs_usage` shows no `claude-subagents/` opens except within a second of a real
SubagentStart/Stop. Keep the uitest in `rs/crates/app/src/uitest/subagents.rs` green and add one
that starts a subagent, archives its record on stop, and re-opens nothing on the next scan.

## Fix (landed on `main` 2026-10-14)

Three commits, in the order B8 named them, each testable on its own:

1. **`B8 part 1: archive finished subagent hook records out of the scan path`** —
   `claude_subagents::subagents_from_archiving` moves a record whose `stoppedAt` is older than
   30 s into `<pane dir>/_archive/` (one rename each, once); the archive's file names stay
   known, so the transcript-directory fallback cannot bring an archived agent back as a
   "fallback-only" record. A scan now opens the live records plus a directory listing of
   `_archive/`, not 1448 finished JSON files.
2. **`B8 part 2: scan subagent hook records off the UI thread`** — new
   `app/src/subagent_scan.rs`. The tick only lists the pty panes and their marker ids; a
   short-lived `subagent-scan` thread does the reads and the archiving and swaps in a per-pane
   snapshot; `reconcile_subagents` reads the snapshot (same signature, so the uitests and the
   state tests are unchanged). One worker at a time, in-flight flag released by a drop guard, so
   a panicking scan cannot wedge it. Poll cadence **5 s**, not 1 s.
3. **`B8 part 3: the Claude hook pushes a subagentEvent instead of being polled`** — Bert's
   "pushes, no pulls". The session hook, after writing its record, runs
   `"$HP_CTL" ctl subagent-event "$AVADA_PANE_ID"` detached and silent; the control host parks
   it in the read-model (`subagentEvent` verb, one entry per pane between syncs), `sync` hands
   it to the App, and the App nudges the scanner so the pane is rescanned on the next tick.
   The 5 s poll stays as the fallback for the cases where the post cannot land: the Control API
   off, a scoped module token without rights to that pane (403), a hook with no `HP_CTL` in its
   environment (a claude started outside a pane), or a GUI too busy to answer within the
   client's 2 s timeout.

Not done, by design: a backstop that drops a subagent pane when its record is *absent* from a
scan. The snapshot persists until the next scan replaces it and `reconcile_subagents` is
idempotent, so nothing needs it; adding it would turn a transient read error into closed panes.

**Proof still owed** (needs the GUI restarted on a build ≥ this fix): `sample` of the GUI shows no
`subagents_from` under the main thread, root `fs_usage -f filesys` shows no
`claude-subagents/` opens between real subagent events, and the per-second syscall count from the
*Evidence* section drops from ~23 k to the handful the 5 s poll costs.

## Mitigation available today (reversible, not applied — superseded by part 1)

Moving the 1448 finished hook records out of the live dirs would cut the per-second cost to ~3
files immediately, without a restart. Only `claude_subagents.rs` reads these dirs (checked:
`claude_panes.rs` calls `prune_dirs_in`, the uitest fixture builds its own; nothing else reads
them), and `reconcile_subagents` ignores finished records anyway, so the GUI's behaviour does not
change. The two subagent panes still on screen whose agents are finished (`af2128c5ebf52a76b`,
`a72768208351b38f0`) are already closed-on-finish candidates; keep their records or not, the
result is the same. Undo = move the files back. This mutates live app state, so it waits for
Bert's yes.
