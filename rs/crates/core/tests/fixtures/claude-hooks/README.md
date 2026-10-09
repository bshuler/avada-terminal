# Claude Code hook payloads (subagent panes)

Captured 2026-10-09 from a live headless `claude_auto -p` session (Claude Code, one `Explore`
subagent) with a logging hook on `SubagentStart`, `SubagentStop` and `Stop`. Only the paths are
scrubbed: the home directory became `/home/u` and the project `/home/u/proj` (encoded
`-home-u-proj`). Field names and shapes are exactly as Claude Code sent them.

What the capture established (pinned by `claude_subagents` tests):

- `SubagentStart` carries `session_id`, `transcript_path` (the **parent's** transcript),
  `cwd`, `prompt_id`, `agent_id`, `agent_type`, `hook_event_name` — and **no** path to the
  subagent's own transcript.
- `SubagentStop` adds `agent_transcript_path`, which equals the derivation
  `<dir of transcript_path>/<session_id>/subagents/agent-<agent_id>.jsonl`. The hook script
  uses that derivation for both events.
- `Stop` has no `agent_id`; it marks the orchestrator's turn ending.
- No payload carries a timestamp; the hook stamps its own wall clock.

`projects/` is a hand-trimmed transcript directory for the same session (the real `.jsonl`
was 143 KB; three representative lines are kept), used by the transcript-dir fallback tests.
