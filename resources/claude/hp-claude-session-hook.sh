#!/bin/sh
# Claude Code session + subagent hook -> avada pane->conversation map and subagent records.
#
# Avada registers this (core/src/claude_hook.rs) in every Claude settings.json under
# SessionStart, SessionEnd, SubagentStart, SubagentStop and Stop:
#   "hooks": {
#     "SessionStart": [ { "hooks": [ { "type": "command", "command": "<path to this script>" } ] } ],
#     ... the same entry for each of the other four events ...
#   }
#
# Claude pipes hook JSON (session_id, cwd, hook_event_name, ...) on stdin. When the
# claude runs inside an Avada pane (AVADA_PANE_ID in the pane env), this dispatches on
# hook_event_name:
#   SessionStart  -> <state dir>/claude-sessions/<pane-id>.json
#                    = { "sessionId":..., "cwd":..., "configDir":... }
#   SessionEnd    -> removes that marker, so a marker exists exactly while a conversation
#                    is live in that pane. The GUI's relaunch snapshot embeds the id, letting
#                    a restored pane `claude --resume` the same conversation.
#   SubagentStart / SubagentStop
#                 -> <state dir>/claude-subagents/<pane-id>/<agent_id>.json
#                    = { "agentId", "agentType", "transcript", "event", "startedAt", "stoppedAt" }
#                    (merged over the previous record, so Stop keeps startedAt; times are
#                    epoch ms from this hook's clock — no payload carries one).
#   Stop          -> <state dir>/claude-subagents/<pane-id>/_turn.json = { "endedAt" }
#   anything else -> nothing.
# Only SessionStart may write the session marker: rewriting it bumps its mtime, which resets
# the GUI's resume ready-age check.
# (Paths must mirror avada-core persistence::paths::claude_sessions_dir / claude_subagents_dir.)
#
# SubagentStart carries only the PARENT's transcript_path; the subagent's own transcript is
# <dir of transcript_path>/<session_id>/subagents/agent-<agent_id>.jsonl — exactly the
# agent_transcript_path SubagentStop reports (pinned by core/tests/fixtures/claude-hooks).
#
# This MUST print nothing on stdout: a Stop/SubagentStop hook that emits JSON can block
# Claude from ending its turn. Outside a pane, or on any error, it exits 0 silently — a hook
# must never break claude.
[ -n "$AVADA_PANE_ID" ] || { cat >/dev/null 2>&1; exit 0; }

case "$(uname 2>/dev/null)" in
  Darwin) base="$HOME/Library/Application Support/avada" ;;
  *)      base="${XDG_STATE_HOME:-$HOME/.local/state}/avada" ;;
esac

HP_SESS_DIR="$base/claude-sessions" HP_SUB_DIR="$base/claude-subagents" \
HP_PANE="$AVADA_PANE_ID" python3 -c '
import json, os, re, sys, time

try:
    data = json.load(sys.stdin)
except Exception:
    sys.exit(0)
if not isinstance(data, dict):
    sys.exit(0)
pane = os.environ["HP_PANE"]
event = data.get("hook_event_name")


def replace_json(path, obj):
    tmp = "%s.%d.tmp" % (path, os.getpid())
    with open(tmp, "w") as f:
        json.dump(obj, f)
    os.replace(tmp, path)


def now_ms():
    return int(time.time() * 1000)


try:
    if event == "SessionStart":
        d = os.environ["HP_SESS_DIR"]
        os.makedirs(d, exist_ok=True)
        # configDir records the account this conversation was saved under: `claude` stores
        # transcripts in $CLAUDE_CONFIG_DIR/projects, so a relaunch must set the SAME
        # CLAUDE_CONFIG_DIR for `claude --resume <id>` to find the session (multi-account).
        replace_json(os.path.join(d, pane + ".json"), {
            "sessionId": data.get("session_id", ""),
            "cwd": data.get("cwd", ""),
            "configDir": os.environ.get("CLAUDE_CONFIG_DIR", ""),
        })
    elif event == "SessionEnd":
        try:
            os.remove(os.path.join(os.environ["HP_SESS_DIR"], pane + ".json"))
        except FileNotFoundError:
            pass
    elif event in ("SubagentStart", "SubagentStop"):
        agent = str(data.get("agent_id") or "")
        # The id becomes a file name: refuse anything that is not a plain token.
        if not re.fullmatch(r"[A-Za-z0-9_-]{1,128}", agent):
            sys.exit(0)
        transcript = data.get("agent_transcript_path") or ""
        parent = data.get("transcript_path") or ""
        session = str(data.get("session_id") or "")
        if not transcript and parent and session:
            transcript = os.path.join(os.path.dirname(parent), session, "subagents",
                                      "agent-%s.jsonl" % agent)
        d = os.path.join(os.environ["HP_SUB_DIR"], pane)
        os.makedirs(d, exist_ok=True)
        path = os.path.join(d, agent + ".json")
        try:
            with open(path) as f:
                rec = json.load(f)
            if not isinstance(rec, dict):
                rec = {}
        except Exception:
            rec = {}
        rec["agentId"] = agent
        rec["agentType"] = data.get("agent_type") or rec.get("agentType", "")
        rec["transcript"] = transcript or rec.get("transcript", "")
        rec["event"] = event
        if event == "SubagentStart":
            rec["startedAt"] = now_ms()
            rec["stoppedAt"] = None
        else:
            rec.setdefault("startedAt", None)
            rec["stoppedAt"] = now_ms()
        replace_json(path, rec)
    elif event == "Stop":
        d = os.path.join(os.environ["HP_SUB_DIR"], pane)
        os.makedirs(d, exist_ok=True)
        replace_json(os.path.join(d, "_turn.json"), {"endedAt": now_ms()})
except (OSError, ValueError):
    pass
' >/dev/null 2>&1
exit 0
