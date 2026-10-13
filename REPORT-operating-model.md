# Operating-model module — REPORT

Updated 2026-10-13.

## NEEDS BERT
- Go received 2026-10-13. Nothing blocking yet.
- FYI base: `feat/scheduled-tasks` has the materializer caller but NOT the manager spine (`131ff9d`, the `PaneInfo.meta.role` source) which is only on `main`. B5 sets `AVADA_ROLE` from a spawn-time field; merging this branch with the spine needs a small reconcile. Tracked `REPORT.md` there belongs to scheduled-tasks, so this report lives in `REPORT-operating-model.md`.

## State
- Design rev 3 plus recorded rulings; branch `feat/operating-model-module` (worktree `/Users/bshuler/code/avada/avada-terminal-opmodel`), now rebased onto `feat/scheduled-tasks` (234c805). Live Avada untouched.
- On go: rebase onto `feat/scheduled-tasks`, then builders in this order: B4a safety fixes, B4c exclusive shape, B4b opt-in per-workspace roots, B1-B3 module + tests, B5 `AVADA_ROLE` at pane spawn, B6 Gemini row, manager reads both headings.
