---
name: hyperpane
description: "You are running in the Hyperpane tab, Avada Terminal's always-on agent tab. Drive the app with `avada ctl <verb>`: read any window, tab or pane, type into terminals, manage panes and tabs, change preferences."
kind: rule
activation: always
---
# Hyperpane

This directory is the working directory of the **Hyperpane** tab — the always-on tab Avada
keeps open for its own agent. Everything the agent needs to inspect and change the workspace lives
here, under `.claude/skills/avada/`.

The agent drives the app through one command:

```
avada ctl <verb>
```

The app puts its own directory on this pane's `PATH`, so `avada` resolves here whether or not
it is installed system-wide; `$HP_CTL` holds the absolute path to the same binary if you ever need
it. Run `avada ctl help` for the full verb list, or read
`.claude/skills/avada/REFERENCE.md`.

## What it can reach

- **Read** every window, tab and pane, and the contents of any terminal — including panes on tabs
  that are not on screen.
- **Write** to any terminal: type text, submit a line, send named keys.
- **CRUD** panes (create, close, restart, focus, rename, recolor, re-layout), tabs (create, close,
  rename, focus, reorder) and every application preference.
- **Anything else the control API exposes** — projects, work queues, tasks, devices, tokens — via
  the raw `get` / `post` / `patch` passthroughs.

## About this directory

The app refreshes the files it ships (this rule, the README and everything under
`.claude/skills/avada/`) each time it starts, so they track the installed version. It never
deletes anything else — notes, scratch files and scripts you leave here are yours and survive
upgrades. Text outside an `<!-- avada:module=… -->` fence in `AGENTS.md` or `CLAUDE.md` is
never rewritten.

## About the control API

The Hyperpane tab needs the control API on, so Avada enables it the first time it creates this
tab, including the "allow input" permission that lets the agent type into panes. The listener binds
loopback only and every request needs the token in `control.json`; nothing is exposed off this
machine. To turn it back off, use **Preferences → Control API** — the app will not re-enable it,
because it only touches the setting when it creates this tab, and the tab already exists.
