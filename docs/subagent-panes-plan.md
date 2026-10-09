# Subagent panes and the breathing "hide subagents" toggle — plan

Status (2026-10-09): **requested, not started.** Tracked as B2 in
[`backlog.md`](backlog.md). Written at the end
of the 0.2.31 session so the next session starts from a spec instead of a chat message.

## 1. The request (Bert, 2026-10-09)

> I want when an agent creates subagents, I want those to appear as panels in the tab with
> the orchestrator, and I want a toggle button on the window with the maximize buttons that
> says "hide subagents" and it should pulse with activity when the subagent windows are
> pulsing, even if they are hidden, so we know it is working, should resemble breathing.

Follow-up in the same session: this is probably best built with specialised subagents and
the advisor tool. Agreed; §5 lays out the lanes.

## 2. What exists today

- **Subagents are not terminals.** A Claude Code session spawns in-process subagents (the
  Agent tool). Their only external trace is the transcript directory
  `~/.claude/projects/<project>/<session-id>/subagents/agent-<id>.jsonl` plus a
  `agent-<id>.meta.json` with `agentType`, `description`, `toolUseId`, `spawnDepth`,
  `requestShape` (`background` or not). Claude Code also fires `SubagentStart` /
  `SubagentStop` hook events; Avada's session hook (`rs/crates/core/src/tools/session_hook.rs`)
  does not subscribe to them yet.
- **Real worker panes** (lanes opened with `ctl new-pane`) already appear in the tab; the
  orchestration plan (`agent-orchestration-plan.md`) covers parent/child scoping for those.
- **Breathing already exists.** `rs/crates/app/src/glow.rs` implements the idle glow with a
  "calm, regular breathing" style driven per tick into `PaneItem.glow`; the toggle should
  reuse that curve, not add a second animation.
- **Window chrome.** macOS runs a hidden titlebar with the native traffic lights overlaid
  top-left (`rs/crates/app/src/window/macos.rs`). The toggle is a Slint control placed in
  the top strip just right of the traffic lights; Windows and Linux put it in the same
  position in their own strips.
- **Activity** per pane comes from `activity_for` in the control server; for a subagent the
  equivalent is "its transcript grew in the last N seconds" or, better, the
  `SubagentStart`..`SubagentStop` window from the hook.

## 3. Behaviour

1. **Discovery.** When a pane's Claude session gains a subagent (hook event, with the
   transcript directory as the fallback for sessions started before the hook was installed),
   Avada opens a **subagent pane** in the same tab as the orchestrator pane, after it in
   layout order, labelled `<orchestrator label> › <description>`, carrying `spawnDepth`.
2. **Content.** The subagent pane shows a live tail of the subagent transcript (assistant
   text, tool names, results collapsed), rendered with the existing viewer-pane machinery.
   It is read-only. (See §6 for the alternative where subagents become real terminal lanes.)
3. **Lifecycle.** On `SubagentStop` the pane stays, marked finished; it is closed when the
   orchestrator's turn ends or when the user closes it. Subagent panes are never persisted
   into the workspace snapshot as terminals; on restart they are rebuilt from the transcript
   directory if the orchestrator session is still live.
4. **Toggle.** A button in the window chrome, label "hide subagents" / "show subagents",
   scoped to the current tab. Hidden subagent panes release their layout space; the
   orchestrator pane grows back. State is per tab and persisted.
5. **Breathing.** While any subagent of the current tab is active (started and not stopped,
   or transcript grew within the idle threshold) the button runs the glow breathing curve.
   It breathes whether the subagent panes are shown or hidden. No active subagent: static.
6. **Control API.** `ctl state` lists subagent panes with `kind: "subagent"`, parent pane id,
   and activity; `ctl tabs` shows them indented under the parent. A `toggleSubagents`
   command mirrors the button.

## 4. Acceptance (the test is the definition of done)

- Unit: hook payload → subagent record; transcript-directory fallback finds the same set.
- Unit: breathing predicate (active subagents ∧ current tab) true/false table, including the
  hidden case.
- UI test (existing `uitest` harness): spawn a fake session with two subagents, assert two
  panes appear under the orchestrator, toggle hides them, button still animates, toggle shows.
- Control API: `ctl state` carries the `subagent` kind and the parent link.
- Feature-test-matrix row added.

## 5. How to build it (lanes)

Coordinator in one pane of a new "Subagents" tab; one lane per worktree, each launched with
`claude_auto`; advisor at the design gate and before "done".

| Lane | Scope | Touches |
|---|---|---|
| discovery | hook events + transcript fallback → subagent model in core | core/tools/session_hook.rs, core/claude_panes.rs, control server state |
| pane | subagent viewer pane, layout placement, lifecycle | app/viewpane.rs, app/state.rs, gridpane.rs |
| chrome | toggle button, per-tab hide state, breathing reuse of glow.rs | app/app.rs top strip, window/*, glow.rs, prefs |
| proof | uitest + matrix row + docs | app/src/uitest, docs/feature-test-matrix.md |

The discovery lane goes first (the other two depend on its model); pane and chrome run in
parallel; proof runs last.

## 6. Open question for Bert

In-process subagents are not terminals, so "appear as panels" needs one of:

1. **Transcript-tail viewer panes** (recommended, §3.2): no change to how orchestrators spawn
   subagents; works for every Claude session automatically; read-only.
2. **Real terminal lanes**: orchestrators are told (via the Avada skill) to spawn workers as
   `ctl new-pane` lanes instead of in-process subagents. Interactive, but only orchestrators
   that follow the skill get panes, and in-process subagents stay invisible.
3. **Both**: viewer panes for in-process subagents, and lanes are tagged as children so the
   toggle and breathing cover them too.
4. **Chrome only first**: ship the toggle and breathing over existing lanes now, viewer panes
   in a second step.
