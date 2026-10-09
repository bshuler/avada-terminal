# Backlog — open items

GitHub issues are disabled on this repository, so this file is the tracker. One entry per
open item, newest first; an entry leaves when its fix ships and the commit names it. The
next-session command line at the end of every session points at the entry it will work.

## Open

### B3 · GUI wedges in `render_screen` and the control API starves (2026-10-09)
Seen on the 0.2.31 GUI (pid 76456, daemon 76462) after a machine-wide swap thrash; it did not
recover once memory was freed. Evidence gathered without restarting anything:
- `sample` of the GUI: all 712 samples of the main thread sit in
  `App::tick → SessionManager::render_screen → DaemonSessionManager::render_screen →
  request() → recv_timeout` (2 s per call, `SCREEN_TIMEOUT`,
  `rs/crates/core/src/session/daemon_client.rs`). The Slint loop never gets back to the
  control server, so every `avada ctl` call times out and the window stops painting. The
  app log stops dead between the 17:40 and 17:55 firings of the status loop.
- The GUI's only daemon reader (`hp-daemon-sm-reader`, `reader_loop`) sleeps in
  `read_frame → recvfrom` with 0 % CPU, while `netstat -f unix` shows that same socket
  holding a full 8160-byte receive queue, static for over an hour. The daemon's writer
  thread for that connection (`hp-daemon-writer`, `writer_loop → write_all → sendto`) is
  blocked because the GUI is not draining. A sleeping reader on a full queue has no
  userland explanation found so far (no `SO_RCVLOWAT`, one socket, fds 11/12 are dups of
  it, no second reader); root needs `dtrace`/`fs_usage` to settle it.
- The daemon is healthy: a fresh client (`Hello`, `Ping`, `ListSessions`, `RenderScreen`)
  gets answers in under 30 ms, 50 sessions listed, pane screens render. Pane PTYs and
  their programs are untouched.
Remedy today: quit and reopen the GUI (the daemon and every pane survive; it also moves the
GUI onto the installed 0.2.32). Fix direction:
1. `request()` must not retry forever: after N consecutive `SCREEN_TIMEOUT`s on one
   connection, mark the link dead (`connected = false`), log it, and let the redial path
   rebuild the connection — the reader thread is not the only place the process may learn
   the daemon link is gone.
2. Keep `render_screen` off the UI thread (cache the last screen, refresh asynchronously)
   so a slow or dead daemon link can never stall the Slint loop or the control server.
3. A reader watchdog: if the socket has bytes queued (or the daemon's `Ping` goes
   unanswered) for longer than a few seconds while the reader reports idle, drop and redial.
4. Reproduce under memory pressure (`sample` + `netstat -f unix` script) before closing.

### B2 · Subagent panes and the breathing "hide subagents" toggle (2026-10-09)
Spec: [`subagent-panes-plan.md`](subagent-panes-plan.md). §6 decided (viewer panes); build via the
lanes in §5.

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

(none yet)
