# Operating-model module - REPORT

Updated 2026-10-13. Branch `feat/operating-model-module` (worktree `avada-terminal-opmodel`), based on `feat/scheduled-tasks` 234c805. Not merged, not deployed, live Avada untouched.

## NEEDS BERT
- **MERGED to main (2026-10-13)** on Bert's decision, onto origin/main c512ef3. Verified on hp1: avada-core 2117 lib + integration tests, module-sdk, operating-model, app `cargo check --tests`. No typed role field exists on main, so the three `meta["role"]` reads were NOT re-pointed. Untested live: supervised-restart role fix, new ctl verb.
- **Matrix test verdict:** `uitest::matrix::the_feature_matrix_still_describes_this_app` was already red at c512ef3 (run alone on hp1, before this merge), not reddened by it; cause was the scheduled-tasks callbacks/Commands missing from `docs/feature-test-matrix.md`, regenerated and green on hp1.
- Review the branch and say merge / change. Nothing else blocks.
- Merge note: `feat/scheduled-tasks` lacks the manager spine (`131ff9d`, only on `main`). Reconcile points: `pane_origin::extract`, `pane_role::apply_to_spec`, `ctl_cli.rs` and one dispatch test read the role from `meta["role"]`; point them at the typed field if 131ff9d has one.
- Manager must read both `## NEEDS HUMAN` and `## NEEDS BERT`: the manager code is only on `main`, so this is a one-line change at that merge (not done here).

## Built (all signed, as Bert Shuler, tests on hp1)
- 99b167d safety: foreign fences survive, unterminated fence untouched, no user-scope writes for disabled modules
- 738a7d8 + d5169b1 exclusive shape `avada.agent.operating_model` (underscore: validator rejects hyphen), uninstall sweeps via ledger
- ecb31b9 module `rs/modules/operating-model/` (rule + orchestrator/builder playbooks)
- c09d2dd, c90876d Gemini CLI adapter (`GEMINI.md` imports `@AGENTS.md`)
- 1dbe7da, 77697b7, 6fd1f55 `new-pane --role` -> `AVADA_ROLE`, survives restart, not spoofable via attach/restart/setMeta/workspace env
- 3d74a3f opt-in per-workspace project-root writes (default off, project scope only)
- 71b5fd3 shared roots materialized once with union; blocked sweeps retried; non-UTF-8 files left alone
- ae5b92d 10 end-to-end tests on the real shipped module

## Known limits
- App knows only the launch workspace's one project root; per-pane directories are a follow-up.
- A workspace running alone can sweep a module another workspace sharing the root enabled (ledgers do not record enabled state); closing it needs the app to pass all workspaces together.
- Supervised-restart role re-derive (`control/server.rs`) and the generated `ctl` verb for the opt-in have no test.
- Aider only loads `CONVENTIONS.md` with `--read`; Cursor CLI/Cody/Continue AGENTS.md reading undocumented.

## Merge notes (complete)
- Base is `feat/scheduled-tasks` 234c805, 61 commits ahead of `main`; `main` has the manager spine 131ff9d, which this branch lacks. Merge order matters: land scheduled-tasks/spine first, then this.
- Panel-rename (pane->panel, `pane-to-panel-rename`) touches the same files: expect conflicts in `control/dispatch.rs`, `control/routes.rs`, `session/spawn.rs`, `app/src/state.rs`, `pane_role.rs`, `pane_origin.rs`, `ctl_cli.rs`, and the avada skill `REFERENCE.md` (`new-pane --role`). Rebase after the rename lands; do not merge ahead of it.
- Role field: this branch keeps role in the `meta` map under `role`. If 131ff9d makes it typed, change `pane_origin::extract`, `pane_role::apply_to_spec`, `ctl_cli.rs`, and the dispatch tests.
- Manager must read both `## NEEDS HUMAN` and `## NEEDS BERT`: not done (manager code is only on `main`).
- `rs/Cargo.lock` gained only the operating-model package entry; `rs/modules/versions.lock` has its hash (verified by `module_versions`).

## Test state
- Last full run on hp1 at 71b5fd3 code: avada-core lib 2114 pass / 0 fail, all avada-core integration tests pass, module-sdk 69+6, operating-model 2, app crate `cargo check --tests` clean. Local Mac cargo is guarded; nothing was run on the Mac.
- Not run: the app binary, Windows/Linux check scripts, any live Avada.

## Untested / unverified
- Supervised-restart role re-derive (`control/server.rs`) and GUI restart/relaunch role survival: compile-checked, no process-level test of the app paths.
- Generated `avada ctl` verb for the opt-in instruction-files setting: route tested, verb not run.
- App-side wiring in `module_runtime.rs` (project-root pass, defaults load): compiled only.
- No golden-file test per adapter; Cline/Kiro/Augment/Continue untested for this module.
- Never exercised against a real harness reading the injected file.
