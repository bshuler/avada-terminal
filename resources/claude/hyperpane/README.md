# Hyperpane

This directory is the working directory of the **Hyperpane** tab — the always-on tab Avada
keeps open for its own agent.

What the agent needs to know is materialized here on every start from the shipped
`avada/hyperpane` module: an always-on rule in `AGENTS.md` (imported by `CLAUDE.md`) and the
`avada` skill under `.claude/skills/avada/` with its `REFERENCE.md` and `RECIPES.md`. Those
files sit inside `<!-- avada:module=avada/hyperpane -->` fences or are app-owned copies, and
the app replaces them on upgrade.

Everything else is yours. Notes, scratch files and scripts you leave here — and any text you
add to `AGENTS.md` or `CLAUDE.md` outside a fence — survive upgrades untouched.

The agent drives the app with `avada ctl <verb>`; `avada ctl help` lists the verbs.
