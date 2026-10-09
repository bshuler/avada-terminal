# Backlog — open items

GitHub issues are disabled on this repository, so this file is the tracker. One entry per
open item, newest first; an entry leaves when its fix ships and the commit names it. The
next-session command line at the end of every session points at the entry it will work.

## Open

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
