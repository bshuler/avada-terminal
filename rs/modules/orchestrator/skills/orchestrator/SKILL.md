---
name: orchestrator
description: "You are running in the Orchestrator tab, Avada Terminal's always-on agent tab. Drive the app with `avada ctl <verb>`: read any window, tab or pane, type into terminals, manage panes and tabs, change preferences."
kind: rule
activation: always
---
# Orchestrator

This directory is the working directory of the **Orchestrator** tab — the always-on tab Avada
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

The Orchestrator tab needs the control API on, so Avada enables it the first time it creates this
tab, including the "allow input" permission that lets the agent type into panes. The listener binds
loopback only and every request needs the token in `control.json`; nothing is exposed off this
machine. To turn it back off, use **Preferences → Control API** — the app will not re-enable it,
because it only touches the setting when it creates this tab, and the tab already exists.

## "Update installed · Restart to update"

That line, in a Claude pane's footer (yours included), is **Claude Code's** notice, not Avada's.
The `claude` CLI has updated itself on disk, and the running process keeps the old version until
it restarts. It has nothing to do with the Avada app, so it is never a reason to hold off on, or
ask about, an Avada update or relaunch. Do not log it as "benign, ignore" either: the pane really
is running a stale Claude Code.

Avada handles it on its own. About every 30 seconds it restarts such a pane into the same
conversation (`claude --resume`), once it is safe. Safe means:

- the pane has a live session marker;
- Claude is idle: no "esc to interrupt", no option form, no half-typed human line;
- no background task or shell is running;
- Bert is not using that tab right now.

Each conversation is restarted at most once per 30 minutes. Queued prompts survive the restart.
You should rarely need to act. When you do:

- `recoverPane {"action":"inspect"}` reports `"updatePending": true` while the notice is on the
  pane's screen.
- If a worker has sat with the notice for a long time, find out why the restart is held. It is
  usually busy, or has a background shell. Once it is idle, restart it yourself with
  `restartPane {"resume": true}`. Never do that while it is working or while Bert is typing in it.
- If the notice is still there after a restart, the update did not take (e.g. the launcher points
  at an old binary). Tell Bert; do not restart it again in a loop.
