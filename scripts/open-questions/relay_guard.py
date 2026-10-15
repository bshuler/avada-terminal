#!/usr/bin/env python3
"""relay_guard.py — PreToolUse hook for the Open Questions relay pane.

The relay carries messages between Bert and the Manager and does nothing else
(docs/manager-architecture-spec.md §1a, MUST 10, in avada-terminal). The brief in AGENTS.md
says so; this hook makes it mechanical, because a brief alone already failed once
(backlog B13, 2026-10-15: the pane spawned a worktree builder itself).

Refused:
  * any tool that spawns or messages an agent or forks work: Agent, Task, SendMessage,
    Workflow, EnterWorktree, ExitWorktree;
  * Write / Edit / MultiEdit / NotebookEdit outside the relay's own directory and the
    Manager's inbox;
  * Bash commands that launch sessions or agents (claude, claude_auto), make worktrees or
    branches, commit, push, build or test, deploy, or drive panes beyond the relay's own
    procedure (anything but ctl tabs / panes / read / info / submit / send and
    keys backspace|escape).

Exit 0 lets the call through; exit 2 refuses it with the reason on stderr. Stdlib only.
Test: echo '{"tool_name":"Agent","tool_input":{}}' | python3 -I relay_guard.py; echo $?
"""
import json
import os
import re
import sys

HOME = os.path.expanduser("~")
RELAY_DIR = os.path.join(HOME, "Library", "Application Support", "avada", "open-questions")
MANAGER_INBOX = os.path.join(HOME, "Library", "Application Support", "avada", "orchestrator", "inbox")
ALLOWED_WRITE_ROOTS = (RELAY_DIR, MANAGER_INBOX)

FORBIDDEN_TOOLS = {"Agent", "Task", "SendMessage", "Workflow", "EnterWorktree", "ExitWorktree"}
WRITE_TOOLS = {"Write", "Edit", "MultiEdit", "NotebookEdit"}

CTL_ALLOWED_VERBS = {"tabs", "panes", "read", "info", "submit", "send", "keys", "--help", "-h", "help"}
CTL_VERB = re.compile(r"(?:^|[\s;&|(])(?:\S*/)?avada\s+ctl\s+(\S+)(.*)", re.M)
HP_CTL_VERB = re.compile(r"\$\{?HP_CTL\}?\s+(?:ctl\s+)?(\S+)(.*)", re.M)
KEYS_ALLOWED = re.compile(r"^\s+\S+\s+(backspace|escape)\s*(?:$|[;&|])", re.I)

BASH_FORBIDDEN = [
    (re.compile(r"(?:^|[\s;&|(/])claude(?:_auto|_or)?(?:\s|$)", re.M), "launching a Claude session"),
    (re.compile(r"\bgit\s+(?:worktree|commit|push|checkout|switch|merge|rebase|reset|cherry-pick|stash|branch\s+-)"), "changing a git tree"),
    (re.compile(r"\b(?:cargo|npm|pnpm|yarn|make|xcodebuild|swift\s+build|pytest|go\s+(?:build|test|run))\b"), "building or testing"),
    (re.compile(r"\b(?:launchctl|systemctl|docker|kubectl|ansible|terraform)\b"), "deploying or changing a service"),
    (re.compile(r"\brm\s+-[a-zA-Z]*[rf]"), "deleting"),
    (re.compile(r"\b(?:osascript|open\s+-a)\b"), "taking the keyboard"),
]


def under(path, roots):
    try:
        real = os.path.realpath(os.path.expanduser(path))
    except (TypeError, ValueError):
        return False
    return any(real == r or real.startswith(r + os.sep) for r in roots)


def refuse_bash(command):
    for pattern, what in BASH_FORBIDDEN:
        if pattern.search(command):
            return f"relay-guard: refused {what}. The Open Questions relay only relays (spec §1a MUST 10). Hand this to the Manager instead."
    for regex in (CTL_VERB, HP_CTL_VERB):
        for m in regex.finditer(command):
            verb, rest = m.group(1), m.group(2)
            if verb not in CTL_ALLOWED_VERBS:
                return (f"relay-guard: refused `avada ctl {verb}`. The relay may only tabs / panes / read / info / submit / "
                        "send and keys backspace|escape, and only into the Manager pane. Opening, closing, moving, focusing "
                        "or restarting panes is the Manager's job.")
            if verb == "keys" and not KEYS_ALLOWED.match(rest):
                return "relay-guard: refused `avada ctl keys` with anything but backspace or escape. Never drive a live dialog."
    return None


def refuse_write(tool_input):
    path = (tool_input or {}).get("file_path") or (tool_input or {}).get("notebook_path") or ""
    if path and under(path, ALLOWED_WRITE_ROOTS):
        return None
    return (f"relay-guard: refused writing to `{path or '?'}`. The relay writes only inside its own directory "
            f"({RELAY_DIR}) and the Manager inbox ({MANAGER_INBOX}).")


def main():
    try:
        event = json.load(sys.stdin)
    except (ValueError, OSError):
        return 0
    tool = event.get("tool_name", "")
    tool_input = event.get("tool_input") or {}
    reason = None
    if tool in FORBIDDEN_TOOLS:
        reason = (f"relay-guard: refused `{tool}`. The Open Questions relay never spawns or messages agents "
                  "(spec §1a MUST 10). Write the instruction to the Manager inbox and submit the pointer to the Manager pane.")
    elif tool in WRITE_TOOLS:
        reason = refuse_write(tool_input)
    elif tool == "Bash":
        command = tool_input.get("command", "")
        if isinstance(command, str):
            reason = refuse_bash(command)
    if reason is None:
        return 0
    sys.stderr.write(reason + "\n")
    return 2


if __name__ == "__main__":
    sys.exit(main())
