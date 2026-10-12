# Avada built-in Manager / Orchestrator / Agent architecture — direction capture

**Source:** Bert, direct direction 2026-10-12. This file is the authoritative
capture of *what Bert wants*. The *how* (implementation plan) is produced by the
architect/build pass and must **reconcile with the existing docs** listed under
"Prior art" below rather than re-inventing them.

---

## 1. MUST (Bert's requirements — verbatim intent)

1. **Avada ships with a built-in Manager.** Three-level control hierarchy:
   **Manager → Orchestrator → Agent.** Managers control/direct orchestrators;
   orchestrators manage/direct agents. The manager layer is part of Avada itself,
   not something the user wires up per project.

2. **Configurable AI harness per level.** The "brain" harness is configurable.
   For **each level** you can set:
   - the default **tool/harness** (claude, codex, copilot, aider, … — pluggable list), and
   - the default **model**.

3. **Per-project overrides — except the Manager.** A project can override the
   defaults (its orchestrator tool/model, its agents' tool/model). **The Manager
   is exempt:** it stays an **Avada singleton** — one fixed brain for the whole
   app, not per-project, not user-swappable the way orchestrators/agents are.

4. **Manager = singleton controller window.** Exactly one Manager. It lives in a
   **dedicated window, always left-most** — the controller. There is never more
   than one; it cannot be closed/duplicated like a normal pane.

5. **Right-panel Q/A panel.** On the right, in the panel: a **Q&A panel** where
   **all questions, options, and recommendations from every other agent bubble
   up** for Bert to answer in one place. (This natively replaces the manual
   "status check → AskUserQuestion" nudge loop that a human-driven session does
   today.) Each item carries: originating pane/agent, the question, its options,
   and the agent's recommendation; Bert's answer routes back to that agent.

6. **Built-in message queue (MQ) — kill polling.** Bert considers the current
   polling mechanism a **flaw**. Avada should have a **built-in MQ** so **all
   tabs communicate instantly** with managers and orchestrators, **with no
   cross-talk** (strict addressing/topics — a message for one orchestrator's
   agents never leaks to another's). Event-driven, not interval-polled.

7. **Tasks panel for scheduled tasks.** A **Tasks panel** where **scheduled
   tasks run and are managed** — visible and **editable** in the UI. The
   **regular nudge** is the canonical example: it becomes a managed scheduled
   task shown here, not an invisible external cron/loop.

8. **Nudge configurable per project.** The nudge (cadence, prompt, enabled/
   disabled, scope) is **per-project** configurable.

---

## 2. Model (how the pieces relate — for the architect to refine)

- **Manager (singleton, Avada's brain).** Owns the Q/A aggregation, the Tasks
  panel/scheduler, and global routing. Fixed harness/model (Avada-controlled).
  Leftmost dedicated window. Directs orchestrators.
- **Orchestrator (per project / per workstream).** Directs a set of agents.
  Harness + model default set at the orchestrator level, overridable per project.
- **Agent (the workers = today's panes).** Harness + model default set at the
  agent level, overridable per project.
- **Config resolution order:** level default → project override (orchestrator &
  agent only; manager never overridden).
- **MQ as the spine.** Manager ↔ orchestrators ↔ agents all communicate over the
  built-in MQ with topic/address isolation. The Q/A panel is an MQ consumer
  (questions published "up"); answers are published back "down" to the exact
  originating agent. The scheduler (Tasks panel) publishes nudge/scheduled
  prompts onto the MQ instead of a loop polling each pane.

---

## 3. Prior art to reconcile with (do NOT duplicate or contradict)

These existing docs already cover large parts of this; the build must extend them:
- `agent-orchestration-plan.md` — existing orchestration model.
- `queue-gui-roadmap.md` + `worker-pool.md` + `worker-worktree-base.md` — the
  queue/worker-pool work; likely the seed of the MQ + Tasks panel.
- `goals-system-plan.md` — goals; relates to how work is assigned down the hierarchy.
- `session-daemon-plan.md` + `mux-backend-plan.md` — the daemon/mux backend that
  the MQ and singleton manager window must live on top of.
- `subagent-panes-plan.md` + `modules-fanout-plan.md` — how sub-agents/modules
  spawn as panes (the "Agent" level).
- `interactive-pane-driving-plan.md` + `control-discovery-guard.md` — how panes
  are driven today (the thing the MQ replaces/augments; the `ctl submit` path).
The architect's first job is to read these and state, per requirement above,
whether it extends an existing design or introduces a new one.

## 4. Defaults (use these; raise to Bert via AskUserQuestion only if he'd notice)

- **Manager brain:** the harness/model Avada already runs as its control brain;
  do not add a config surface to change it (requirement 3 says singleton/fixed).
- **MQ transport:** prefer an **in-process / local-IPC broker** owned by the
  session daemon (no external broker service, no network dependency) unless the
  daemon design already dictates otherwise — match `session-daemon-plan.md`.
  Topics addressed by `manager` / `orchestrator:<id>` / `agent:<pane-id>` to
  guarantee no cross-talk.
- **Harness plugin shape:** reuse the existing module/harness abstraction if one
  exists (see `module-contract.md`); add `codex/copilot/aider` as adapters
  behind the same contract rather than special-casing claude.
- **Scheduler store:** scheduled tasks (incl. the nudge) persist in the Avada
  config/state store, keyed per project, editable from the Tasks panel.
- **Nudge default:** keep today's behavior as the shipped default (periodic
  status/needs check), now per-project toggle + cadence + prompt.

## 5. Open decisions for Bert (surface only after the plan exists)

Likely candidates the architect should flag rather than silently decide:
- MQ durability: in-memory only vs persisted (survives Avada restart / replays
  missed messages to a pane that was down).
- Whether the Manager singleton is a real OS window vs a reserved leftmost pane
  column (affects multi-monitor + the "always leftmost" guarantee).
- Migration: does the existing polling path get removed outright, or run behind
  the MQ during a transition.

## 6. Phase order (proposed — architect confirms against prior art)

0. **Recon (no code):** read the Prior-art docs; produce a reconciliation note
   (per requirement, extend-vs-new) + the implementation plan. Report first.
1. **MQ spine** on the session daemon: topic/address model, publish/subscribe,
   no-cross-talk guarantees; wire one existing pane→pane message onto it.
2. **Hierarchy + config model:** Manager/Orchestrator/Agent entities; per-level
   harness+model defaults; per-project overrides (manager exempt). Harness
   adapters behind the module contract.
3. **Manager singleton window** (leftmost, non-closable) + **right Q/A panel**
   as MQ consumer; route answers back to origin agent.
4. **Tasks panel + scheduler**: managed scheduled tasks; migrate the nudge into
   it as the first task; per-project nudge config.

## 7. Handoff

Captured by the hyperpane control session. Build owner: the **Avada core**
effort (daemon + Slint UI), not repaint (repaint is look-and-feel only; it styles
the new Manager window / Q&A / Tasks panels once they exist, per
`arcane-lookfeel-spec.md`). Architect/build pass to produce the reconciliation
note + plan before writing feature code. Do NOT rebuild/relaunch the live Avada
to apply changes — it is picked up on Bert's next restart.
