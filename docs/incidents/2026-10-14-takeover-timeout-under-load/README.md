# 2026-10-14 — "All tabs look empty": a live takeover timed out under load and the old daemon shut down anyway

**Status:** diagnosed, fix filed as **B11** in [`docs/backlog.md`](../../backlog.md). Nothing in
the app was changed; the live state was left as found. Follows B4: this is the failure mode
*after* B4's launchd fix did its job (the daemon survived the GUI quit and a handoff was
attempted) — the handoff itself lost every session.

## What Bert saw

At 18:38 local (22:38Z) every pane in every Avada tab went blank at once. Within about 90 s most
panes showed a fresh `claude` session resuming its conversation; three stayed as bare shells
(the `secret-broker serve` terminal, the Proton-on-hp2 pane, the divi-nft fork pane). The first
seconds of the new shells printed pyenv rehash lock errors — 33 zsh startups at the same instant.

## What it is

Another session installed **0.2.45** (binary written 18:37) and restarted the GUI
(`quit: event loop exited normally keep_alive=true restarting=true daemon=true`, 22:38:36.97Z).
The new GUI saw a daemon build mismatch and spawned a successor daemon to take the incumbent's
65 sessions over live. The Mac was at load ~300 at that moment (a resumed rclone upload at
~590 % CPU, the scheduled code-backup rclone, local whisper/cargo builds from the release).

Timeline, all from `~/Library/Application Support/avada/logs/` (UTC):

| time | who | line |
|---|---|---|
| 22:38:39.098 | GUI 82604 | `daemon build mismatch (client 0.0.0+d3806028010ce5da, daemon 0.0.0+6e611aaf6c0f4713); attempting live takeover (attempt 0)` |
| 22:38:39.136 | GUI | `spawned session daemon via launchd label="to.avada.terminal.daemon.b4488f5de313e99e.142ada13c915218"` |
| 22:38:39.194 | successor 82610 | `logging started role="daemon"` |
| 22:38:39.252 | incumbent 17498 | `takeover requested by a successor daemon conn=6` |
| 22:38:44.226 | successor | `WARN takeover: handoff read failed part way through sessions=0 error=Resource temporarily unavailable (os error 35)` |
| 22:38:44.227 | successor | `takeover declined; leaving the incumbent alone error=the daemon holding this salt did not answer the takeover` — successor exits |
| 22:38:45.277 | incumbent | `sessions handed off sessions=65` (`SessionManager::hand_off`, 6.0 s after the request) |
| 22:38:45.314 | incumbent | `handing sessions over to the successor sessions=65` |
| 22:38:45.445 | incumbent | `WARN takeover: descriptor send failed error=Broken pipe (os error 32)` |
| 22:38:45.446 | incumbent | `session daemon shutting down socket=…/avadad.b4488f5de313e99e.sock` |
| 22:38:47.301 | GUI | `build takeover failed against daemon 0.0.0+6e611aaf6c0f4713; driving it as-is — the terminals matter more than the upgrade` |
| 22:38:47.301 | GUI | `no session daemon listening; spawning one` |
| 22:38:47.540 | daemon 82783 | `session daemon started … took_over=false` |
| 22:38:49 → | daemon 82783 | 33 × `session created uid=pane-… command=/bin/zsh` |

Not one `adopt` / `handoff_info` line anywhere in the window: nothing was adopted. The three
daemons each believed a different story. The successor believed the incumbent was dead. The
incumbent believed the successor had the sessions. The GUI believed the incumbent was still
there to drive.

The incumbent needed **6 s** to answer; the successor waits **5 s**. The two earlier takeovers
today (19:14:56Z, 64 sessions; 20:55:42Z, 65 sessions) succeeded on an idle machine.

## Why — the code path

Both sides of the handoff assume a responsive machine, and neither side confirms the other
before giving something up. `rs/crates/core/src/session/daemon.rs` at `8031bd2`:

**Successor, `take_over` (line 358).** Sets `stream.set_read_timeout(Some(TAKEOVER_RECV_TIMEOUT))`
(`TAKEOVER_RECV_TIMEOUT` = 5 s, line 346) and loops over `recv_with_fds`. A read error of any
kind — including `EAGAIN`, which is what a timeout looks like on a Unix socket — logs
"handoff read failed part way through" and breaks. With `messages == 0` it returns
`ConnectionAborted` "the daemon holding this salt did not answer the takeover", and `run`
(≈ line 170–205) treats that as "takeover declined; leaving the incumbent alone" and exits with
`AddrInUse`. A slow incumbent and a dead incumbent are indistinguishable to it. The connection
was still open — the incumbent had accepted it and logged the request at 39.252 — so "did not
answer" was wrong, and the successor could have known it was wrong.

**Incumbent, `hand_over` (≈ line 1117–1163).** Runs `self.registry.hand_off()` *first*: every
session leaves the registry and relinquishes its pty before a single byte has been sent to
the successor. Then it logs "handing sessions over" and sends the descriptors in
`MAX_FDS_PER_MSG` batches. On a send error it logs "descriptor send failed" and breaks, with the
comment that "the sessions are already out of the registry and their ptys relinquished, so
there is nothing to roll back to — exiting is still right, and the shells stay alive, parented
to init, for the next daemon to discover" — and then unconditionally calls
`self.lifecycle.shutdown(|| {})`. The comment's premise did not hold: when the daemon exits,
the pty masters close with it, every shell behind them gets a hangup, and the next daemon
found nothing to discover. The log shows a fresh `/bin/zsh` for all 33 panes and no adoption.

**The 6 s.** `SessionManager::hand_off` took from 39.252 to 45.277 for 65 sessions. Under load
~300 the lock walk, the `handoff_info` snapshot per session and the log writes are enough;
`TAKEOVER_LOCK_BUDGET_HOLDING` (30 s, line 301) already acknowledges that the incumbent side
can be slow — the successor's 5 s read timeout does not.

**The GUI.** `daemon_client` fell back to "driving it as-is" at 47.301, two seconds after the
incumbent had already shut down, and then spawned a third daemon. Correct behaviour given what
it was told; it had no way to learn that the fallback target was gone.

## What happened to the panes

- 30 panes had resume markers and relaunched `claude --dangerously-skip-permissions --resume
  <uuid>` over ~90 s. That burst (30 Claude startups + 33 zsh) pushed the 1-minute load to 329.
- 3 panes came back as bare shells, because their process was not a marker-backed Claude
  session: `pane-6d32969f-…` (**`secret-broker serve`** — the one human-started terminal that
  pays the Touch ID for every `secret get`; until Bert restarts it, misses raise dialogs),
  `pane-28cd4982-…` (Proton on hp2), `pane-9fe12f45-…` (divi-nft fork).
- One orphan `claude` (pid 82883) for a pane that no longer exists (`pane-20de33e6`, 6 days
  old) survived — it was never behind a daemon pty.

## Fix direction (B11)

1. **Incumbent: nothing is relinquished before the successor has acknowledged.** Send a
   `HandoffBegin` frame (session count, batch count) and wait for the successor's ack *before*
   `registry.hand_off()`; or hand off and send batch by batch and, on the first send error,
   re-adopt the sessions still held (the masters are still in this process at that point) and
   carry on serving instead of shutting down. Either way `hand_over` must not reach
   `lifecycle.shutdown` after `EPIPE` with sessions in hand.
2. **Successor: a slow incumbent is not a dead one.** Keep reading while the connection is
   alive: extend the receive wait to `TAKEOVER_LOCK_BUDGET_HOLDING` (30 s), or make each
   `EAGAIN` re-check the peer (a zero-length write, or the incumbent's pid via `getpeereid` +
   `kill(pid, 0)` through the existing helper) and only give up when the peer is gone. Only a
   closed socket or a dead peer should produce "did not answer".
3. **GUI:** after a declined takeover, re-probe the socket before "driving it as-is" (it did,
   effectively — the second message is "no session daemon listening"), and log the three
   daemon pids together so the next reader of this log does not need a timeline table.
4. **Tests** (hp2, never local cargo): a takeover against an incumbent that delays its
   first frame by `TAKEOVER_RECV_TIMEOUT + 1 s` must succeed; an incumbent whose successor
   disconnects after `HandoffBegin` must keep all its sessions and keep serving; existing
   `takeover_transfers_live_sessions_and_stands_the_incumbent_down` (≈2455) and
   `takeover_against_a_daemon_that_does_not_answer_is_an_error` (≈2537) stay green (the
   latter needs a peer that really does not answer — one that never accepts, not one that is
   slow).

Until B11 ships: do not install a release and relaunch while the Mac is under heavy load
(`uptime` load > core count); `Avada ▸ Quit` and reopen once `uptime` is sane.
