# Hyperpane

This directory is the working directory of the **Hyperpane** tab — the always-on tab Avada
keeps open for its own agent.

The app copies its own files here on every start: this README and the `avada` skill under
`.claude/skills/avada/` with its `REFERENCE.md` and `RECIPES.md`. They are app-owned copies and
are replaced on upgrade.

The `bshuler/avada-hyperpane` module adds the rest. Installing it materializes an always-on
rule into `AGENTS.md` (imported by `CLAUDE.md`), inside `<!-- avada:module=bshuler/avada-hyperpane -->`
fences that the module owns and rewrites; uninstalling it takes the rule back out.

Everything else is yours. Notes, scratch files and scripts you leave here — and any text you
add to `AGENTS.md` or `CLAUDE.md` outside a fence — survive upgrades untouched.

The agent drives the app with `avada ctl <verb>`; `avada ctl help` lists the verbs.
