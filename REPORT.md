# Scheduled tasks — builder report

## NEEDS HUMAN
- **Wordmark overlap fix (B9) — one visual check, needs a GUI restart.** After the next Avada ▸ Quit and relaunch of a build containing this commit: with few tabs the gold "✦ AvadaTerminal ✦" sits dead-centre of the top strip; as tabs fill the strip it disappears before any chip touches it. Not done by the agent: a restart would take Bert's keyboard focus.
- Also parked from earlier: whether the 27 panes survived the earlier restart, and the B7 live `selectedPaneId == keyboardFocusPaneId` check.

## Wordmark overlap (B9) — fixed, 2026-10-14
Root cause: the `visible:` guard on the centred wordmark in `ui/topbar.slint` read `strip.absolute-position.x`, which is not reactive to layout moves, so it went stale and the mark showed over the tabs. Fix: wordmark moved inside `strip`; guard is now `mark-left > occupied-end + 12px && mark-left + width <= strip.width`, all strip-local reactive values. Wordmark text, colour, size and weight untouched. Verified on hp3: app tests 1018 passed; the new test fails (4 tabs: wordmark [576,704] overlaps tab [491,619]) when the guard is loosened.

## NEEDS BERT
- Nothing blocking. FYI: the janitor is now registered on this Mac's launchd (`to.avada.task.janitor`, every 15 min) and stays until removed with `avada ctl task-rm janitor` (needs the new build) or `launchctl bootout gui/$(id -u)/to.avada.task.janitor`.

## Status: LANDED on origin/main (2026-10-13)
Cherry-picked onto origin/main d9462ba (not onto the stale local main, whose working tree holds another feature's uncommitted session/daemon work; untouched). `feat/scheduled-tasks` carried ~59 unrelated unmerged commits, so it was not merged. The only conflicts were import/export lines in `ui/app.slint` and `ui/types.slint` (kept both sides). Compile-verified and tested on hp2 after the pick: 400 core `tasks::`/`control::` tests, 2 `schedtasks` app tests, `cargo check --bins --tests` clean.

## What was built
- `core::tasks` (`rs/crates/core/src/tasks/`): `Task` model, `Schedule` (interval / calendar / cron-ish string), `TaskScheduler` trait
  (create, list, get, update, set_enabled, run_now, remove), one backend per OS:
  - **macOS: launchd — working end to end.** Plist in `~/Library/LaunchAgents`, `launchctl bootstrap|bootout|kickstart|print`.
  - Linux (systemd user units) and Windows (`schtasks`) backends are written but only unit-tested, not run on a real host. **TODO:** cron/`at` fallback when systemd is absent; `at` fallback on Windows; calendar shapes beyond daily/weekly/monthly.
- A wrapper script `scheduled-tasks/run/<id>.sh` records `start end exit` in `scheduled-tasks/status/<id>` and logs to `logs/<id>.log`, so `last_run` / `last_status` are uniform across backends. `env` is an allow-list of names only, resolved at run time.
- Control routes (master-only, since they register commands that run unattended): `GET/POST /scheduled-tasks`, `DELETE /scheduled-tasks/{id}`, `POST /scheduled-tasks/{id}/enable`, `/run`. Descriptor ids `scheduled.*` (the existing `tasks.*` is the work queue).
- `avada ctl` verbs: `tasks`, `task-add --name --cmd "PROG ARGS" --schedule [--cwd] [--env A,B] [--id] [--disabled]`, `task-rm`, `task-enable [--off]`, `task-run`.
- Slint panel (`ui/scheduledtasks.slint`, overlay kind 9, palette entry "Scheduled tasks"): list with enable switch, schedule, last/next run, status; Run now, Remove, New-task form, Refresh. Self-contained, uses `TasksAdapter` only.

## Evidence (live Mac, real launchd, 2026-10-13)
Janitor registered through the real `Scheduler<Launchd>` code: `python3 janitor.py`, cwd `~/Library/Application Support/avada/orchestrator`, `StartInterval 900`.
- Run-now at 12:02:52 → `launchctl print`: `runs = 1`, `last exit code = 0`, `run interval = 900 seconds`.
- Status file `1791907378 1791909103 0` (start, end, exit 0); task list shows `lastRun 1791909103`, `nextRun 1791910003`, `lastStatus 0`.
- `janitor-report.md` regenerated: mtime 11:32 → 12:31, header `# Janitor report — 2026-10-13 12:31`.
- Disable → service gone from `gui/501`, `enabled:false`, `nextRun:null`; enable → back, `run interval = 900`. Left enabled.
- **Natural firing:** after the re-enable at ~12:32, launchd started the job itself at 12:47 with no kickstart (`runs = 0` → `runs = 1`, pid spawned by the `StartInterval 900` timer). That run's status file/report update lands when it finishes (slow, see below); the 12:31 run-now already showed the full `last_run` + report regeneration.
- The run took 29 min only because the Mac was at load ~100–180 and the janitor does one `avada ctl read` per pane (~40 s each). Not a scheduler fault.

## Verification
- hp2 (Linux): `cargo test -p avada-core --lib -- tasks:: control::` 400 passed; app `cargo test --bins schedtasks` 2 passed; `cargo clippy` clean for all touched files; rustfmt clean on new files.
- macOS launchd path proved by the live run above (built a tiny serde-only scratch crate with `LOCAL_CARGO_OK=1`).
- Not done: a headless UI e2e track in `src/uitest/` for the panel (the 106/106 matrix ratchet is untouched); the panel is only compile-checked and unit-tested at the view-model level. The live Avada was not rebuilt or relaunched.
- Findings: hp3 has no Rust toolchain; the live Mac is heavily loaded.
