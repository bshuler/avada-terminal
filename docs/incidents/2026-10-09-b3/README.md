# 2026-10-09 · B3: GUI wedge under swap exhaustion, then a Force Quit that killed every pane

Two separate failures, one afternoon, one root-cause pass. Evidence in `evidence/` was
collected live, before anything was restarted, by scripts that read `/proc`-style state
(`ps`, `sample`, `lsof`, `netstat`, `proc_pidinfo`, `vm_stat`, the unified log). Nothing
was signalled, attached to, or restarted during collection.

## Timeline (local, EDT)

| When | What |
|---|---|
| 11:19:02 | GUI 76456 launched from `/Applications/Avada.app` (0.2.31). 11:19:03 it spawned daemon 76462 as a setsid child (`ppid 76456`). |
| 11:19 → 13:40 | Normal operation; 50 sessions in the daemon. |
| ~13:20 | Machine-wide swap thrash begins (parallel cargo lanes plus a Docker VM). |
| 13:40:47 → 13:55 | App log goes silent between two firings of the 15-minute status loop. The GUI stopped running here. |
| 14:26:51 | Evidence collected (`00-when`). Swap 8387 of 9216 MB used, load average 89, `vm_stat` pageins in the billions (`06-vm`, `23-memory-pressure`). |
| 14:40:46.089 | Bert chooses Force Quit from the Dock (`Calling force quit asn=…`; the Dock waits 1 s for a spindump, then terminates). |
| 14:40:47.189 | launchd: GUI 76456 `exited due to SIGTERM, sent by Dock`. |
| 14:40:47.194 | loginwindow: *"pid=76456 … has fully exited including subordinate processes"*. The daemon dies here, silently (it logs nothing: SIGTERM takes the default action). |
| 14:40:56 | Relaunched GUI spawns a fresh daemon, `took_over=false`. Every pane gets a new `/bin/zsh`; all 49 Claude sessions restart from scratch. |

## Root cause 1 · the GUI wedge

The GUI was **suspended by the kernel**, not stuck in its own code.

- `proc_pidinfo` on 76456 reports `pbi_flags` containing `PC_SUSP` and `PA_SUSP`
  (`26-procinfo-1`): the process was stopped by the memorystatus/freezer path, which macOS
  uses on resource starvation. `ps` shows `STAT S` with 0 % CPU and the `T` priority
  suffix on every thread (`25-delta2`).
- All 42 threads are `WAITING` with 0 CPU across repeated samples (`14-gui-thread-leaves`,
  `22-top-gui`). The `sample` output in the original B3 entry showed the main thread "in
  `render_screen → recv_timeout`" only because that is where it happened to be when it was
  frozen; it never ran again to be anywhere else.
- The daemon 76462 was healthy the whole time: `SLEADER`, no suspension flags, and fresh
  probes (`Hello`, `Ping`, `ListSessions`, `RenderScreen`) were answered in under 30 ms
  (`38-daemon-conn-lifecycle`, `16-daemon-thread-leaves`, `11-sample-daemon-header`).
- The "full 8160-byte receive queue with a sleeping reader" from the first write-up is the
  same thing seen from the socket side: the reader was frozen, so the queue filled.

Memory numbers at collection: swap 8387/9216 MB, swapfiles 3–8 present, load 89,
`memory_pressure` reporting 43 % free only because active pages had been paged out.

What B3 originally blamed (`request()` retrying `SCREEN_TIMEOUT` forever, a reader
watchdog) is hardening that still makes sense, but it would not have helped here: a
suspended process cannot run a watchdog. The real fix for this class is to stop the host
from exhausting swap (see `mac-slowdown-2026-10-08` in the ops notes) and, in the GUI, to
tolerate a daemon link that goes quiet without stalling the UI thread.

## Root cause 2 · the lost handoff

The original B3 remedy said "quit and reopen the GUI; the daemon and every pane survive".
That is true for **Avada ▸ Quit** and false for **Force Quit**.

Dock Force Quit sends SIGTERM to the application and then terminates it *including
subordinate processes* (`50-unified-log-force-quit`). The daemon was spawned with `setsid`
only, which makes it a session leader but leaves it a direct child of the GUI in the
process tree launchd/LaunchServices walk. It was therefore killed along with the GUI.
The daemon had no SIGTERM handler, so there is no line in its log; the first evidence of
its death is the new daemon starting with `took_over=false` nine seconds later.

This is a design bug in how the daemon is detached, not an operator mistake: Force Quit
is the only thing the OS offers for a kernel-suspended app, so the daemon has to survive it.

## Fix (0.2.42)

- `rs/crates/core/src/session/launchd.rs`: on macOS the daemon is bootstrapped as its own
  launchd job in `gui/<uid>` (unique label `to.avada.terminal.daemon.<salt-hash>.<nonce>`,
  `RunAtLoad`, `KeepAlive=false`, `AbandonProcessGroup=true`, environment limited to
  `TMPDIR`/`XDG_RUNTIME_DIR` plus the label). That is the macOS analogue of the Linux
  `systemd-run --scope` path and puts the daemon outside the GUI's subordinate tree.
  Pid-less stale jobs are booted out and their plists removed on the next spawn.
  `setsid` remains the fallback when `launchctl bootstrap` fails, with a warning.
- `rs/crates/core/src/session/signal_log.rs`: SIGTERM/SIGHUP/SIGINT are logged with the
  sender pid and uid before the default action runs, so a daemon that dies by signal says
  who killed it.
- The daemon logs its ppid and launchd label at start; the GUI logs a line on a normal
  quit so a missing line means it died by signal.
- `scripts/install-macos.sh` now says which quit hands over and which does not.

## Verification

Unit tests cover plist rendering, label shape, `launchctl print` parsing and the signal
log line formatter; an `#[ignore]` round-trip test bootstraps `/bin/sleep` as a job. The
real check is the one that failed today: after the daemon has rolled onto 0.2.42
(Avada ▸ Quit, reopen), Force Quit the GUI from the Dock and confirm the daemon pid is
unchanged, `launchctl print gui/501/<label>` still shows it, and the relaunched GUI logs
`took_over=true`.

## What is not in `evidence/`

The raw app-log tails and the wide unified-log windows (`31`, `35`, `41`–`43` in the
collection set) contain the control-API events URL, which embeds the session token, so
they stay in the scratchpad and were not copied. `50-unified-log-force-quit.txt` is the
short excerpt that carries the finding. The full `sample` dumps are summarised by their
headers (`10`, `11`) and the per-thread leaf tables (`14`–`16`).
