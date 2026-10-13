# Operating-model module - REPORT

Updated 2026-10-13. Branch `feat/operating-model-module` (worktree `avada-terminal-opmodel`), based on `feat/scheduled-tasks` 234c805. Not merged, not deployed, live Avada untouched.

## NEEDS BERT
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
