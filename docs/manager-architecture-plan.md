# Manager / Orchestrator / Agent — implementation plan

**Status:** living plan. Authoritative *what*: `manager-architecture-spec.md`. This
file is the *how*, reconciled with existing prior art (spec §3). Produced by the
architect/build pass 2026-10-12.

---

## 0. Reconciliation summary (per requirement: extends vs new, and where it lands)

The single most important finding: **Avada has two IPC planes, and all
orchestration/messaging lives on the second one, not the first.**

- **Session plane (`avadad`, `core::session`)** — a PTY-owning daemon reached over
  a Unix-domain socket / Windows named pipe, length-prefixed JSON frames
  (`session/proto.rs`, `PROTO_VER = 3`). It streams terminal bytes and handles
  spawn/write/resize/kill/claim. **It knows nothing about messages, topics, or
  inboxes.** `session-daemon-plan.md` + `mux-backend-plan.md` ("avadad *is* the
  multiplexer").
- **Control plane (`core::control`, axum HTTP + `/events` WebSocket on loopback,
  bearer-token-gated)** — runs in the GUI-host process, owns `control::server::Shared`
  which holds the read-model, token store, message **inbox**, **nudge** ledger,
  WS **EventHub**, and the SQLite **WorkQueue**. This is where the MQ, hierarchy,
  Q/A panel, and Tasks panel belong.

**Decision (spec §4 default honored): the MQ lives on the control plane, not
`session::daemon`.** The task brief says "on the session daemon"; spec §4 qualifies
that with *"unless the daemon design already dictates otherwise — match
`session-daemon-plan.md`."* It does dictate otherwise: `avadad` is a pty sink with
no message surface, while `Shared` already owns the inbox/scope/readmodel the MQ must
cooperate with. Placing the broker in `control::Shared` is the low-risk, in-process
choice the spec's §4 default actually points to. (A consequence for durability is
flagged For Bert — `Shared` is in the GUI-host process, so "survives restart" means
SQLite or moving the broker into `avadad`.)

| # | Requirement | Extends / New | Lands in |
|---|---|---|---|
| 1 | Manager→Orchestrator→Agent hierarchy | **Extends** ("hierarchy is data, not API": `meta.role`/`parent`; `goals-orch → spec → impl` is a live 3-level org). New: first-class built-in + the labels. | `control::readmodel` (`PaneInfo.meta`), new `control::org` model |
| 2 | Harness + model per level | Model-per-level extends (persona tiers `opus/fable/sonnet[1m]`); **harness-per-level new** (only `claude` is wired; `agentType` meta is free-form). | `meta.agentType`, spawn `command`+`args`, new harness adapter contract |
| 3 | Per-project override (manager exempt) | **Extends** the default→workspace-pin resolution (`WorkspaceModuleState`, `is_enabled`). New: applying it to harness/model + the manager carve-out. | `persistence/projects.rs` `Project`, new per-project config store |
| 4 | Manager = singleton leftmost window, non-closable | **Mostly new.** Precedents only: root/CEO + master token; `hyperpane.rs` "always-on tab". | `hyperpane.rs`, app window/layout (`ui/app.slint`), `control::tokens` |
| 5 | Right Q/A panel aggregating questions | **New panel.** Reuses `awaitingInput`/`detectAwaitingInput`, liveness `awaiting-input`, inbox transport, rail `Row`+`marks`. | new `control` aggregator + `ui/` right-rail panel |
| 6 | Built-in MQ, no cross-talk, event-driven | **Strongly extends** the per-pane inbox + scope-filtered `EventHub` (no-leak already proven). New: the `manager`/`orchestrator:<id>`/`agent:<pane-id>` topic addressing. | **new `control::mq`** (this phase) |
| 7 | Tasks panel (scheduled, editable) | **Extends** — `queue-gui-roadmap.md` *is* this panel (planned, not built) over the `WorkQueue`. New: recurring/cron tasks (`available_at` re-enqueue), a first-class cancel verb for a still-`Queued` task. | `control::work`, new `ui/` Tasks panel |
| 8 | Nudge per-project | **Extends** `control::nudge` / `arm_inbox_nudge` (`hp.nudge`, `AVADA_MSG_NUDGE`, 60s rate-limit). New: per-project config surface. | `control::nudge`, per-project config store |
| 9–11 | Open Questions relay (spec §1a, 2026-10-15): one human-facing relay, relay-only both ways, Bert ⇄ Open Questions ⇄ Manager → Orchestrator → Builder | **Extends** req 5 — the right Q/A panel is the native relay; adds a third flow (Bert's instructions down to the Manager). Today: a plain pane the Orchestrator tab opened + a Manager-written brief (see backlog B13). | phase 3 Q/A panel; until then the pane + brief |

**Two "nudges" must not be conflated.** (a) Today's `control::nudge` is
**inbox-arrival-triggered** — it types one line into a parked TUI agent when mail
lands and the pane goes quiet. It stays; its 2s poll-waiter becomes an MQ-push
consumer in phase 3. (b) Bert's "regular nudge" (req 7/8) is the **recurring
status-check** he runs by hand from hyperpane today — that becomes a **recurring
WorkQueue task** (`available_at` re-enqueue), the deferred
`goals-system-plan.md` build-order item 5. Req 8's per-project knobs apply to (a);
req 7's "nudge as a managed task" is (b).

---

## 1. Component list

- `control::mq` (**new, phase 1**) — in-process topic broker: `TopicAddr`
  (`manager` / `orchestrator:<id>` / `agent:<pane-id>`), publish/subscribe over
  `tokio::sync::broadcast`, exact-topic keying for no-cross-talk, plus a pure
  `may_subscribe` authority check built on `control::scope`.
- `control::org` (phase 2) — Manager/Orchestrator/Agent entities projected from
  the read-model `meta`; config resolution.
- Harness adapter contract (phase 2) — a trait fronting `claude` (first-class
  today) + new `codex`/`copilot`/`aider` drivers; keyed by `meta.agentType`.
- Per-project config store (phase 2/4) — harness+model per level + nudge knobs,
  keyed by `Project.id`, resolved default→project-override (manager exempt).
- Manager singleton window + right Q/A panel (phase 3) — `hyperpane.rs` + `ui/`.
- Tasks panel + scheduler (phase 4) — over `control::work`; recurring-task support.

## 2. Data model

### Entities (projected from existing read-model `meta`, not a parallel tree)
Reuse "hierarchy is data, not API". No new pane-id type — pane id stays `String`,
canonicalized via `readmodel::resolve_pane_id` (`pane-<uuid>` ≡ `<uuid>`).

- **Manager** — singleton. Identity = root/CEO holding the unscoped **master
  token** (`control.json`). Not per-project, not overridable. One per app.
- **Orchestrator** — one per project by default (spec §2 "per project / per
  workstream"). **Identity today does not exist as a first-class id** — expressed
  loosely as `meta.role`, the free-form inbox `from` label, and `Task.goal_id`.
  **Default: `orchestrator:<id>` where `<id>` = `Project.id`** (`persistence/projects.rs`).
  (Per-workstream orchestrators → For Bert.)
- **Agent** — a pane (today's worker). `agent:<pane-id>` = canonical pane id.

### Per-level config + resolution
```
level default (Manager | Orchestrator | Agent)  ->  project override (orch & agent only)
```
- Shape per level: `{ harness: agentType, model: tier }` (tiers reuse
  `opus`/`fable`/`sonnet[1m]`). Manager is fixed/Avada-controlled — no config surface.
- Resolution mirrors the existing module default→workspace-pin pattern
  (`WorkspaceModuleState::is_enabled`): a user-level default flows to projects; a
  project may pin an override; **the Manager level is never consulted for an
  override** (hard carve-out).

### MQ message (phase 1, `control::mq::MqMessage`)
`{ topic: String, from: String, body: String, seq: u64, ts: i64 }`, camelCase on
the wire for parity with `inbox::PaneMessage`. `from` stays free-form `String` in
phase 1 for inbox compat; **phase 3 Q/A reply-routing needs `from` to parse as a
`TopicAddr`** — tightening flagged there, not forced now.

## 3. MQ transport + no-cross-talk topic scheme (phase 1)

- **Transport:** in-process `tokio::sync::broadcast` channel per topic, owned by
  `Shared.mq: Arc<mq::Broker>`. No external broker, no network, no new crate
  (tokio `broadcast`/`mpsc` already in the graph — Cargo.toml untouched). This is
  the push layer; **the durable `inbox` remains the source of truth** (matches
  "the push on `/events` is only a nudge; the read is the source of truth").
- **Topic scheme:** `TopicAddr ∈ { Manager, Orchestrator(String), Agent(String) }`
  ⇄ `"manager"` / `"orchestrator:<id>"` / `"agent:<pane-id>"`.
- **No-cross-talk, two halves:**
  1. *Topic isolation* — the broker keys senders by the exact `TopicAddr`
     (`Eq`+`Hash`); a publish to `agent:A` reaches only `agent:A` subscribers,
     never `agent:B`, `orchestrator:X`, or `manager`.
  2. *Authority* — `may_subscribe(topic, scope, coords)` reuses `scope::pane_in_scope`:
     `Manager` only for an unscoped (master) caller; `Agent(id)` only if the
     caller's `Scope` contains that pane; `Orchestrator(_)` false for any scoped
     caller until orchestrator identity exists (phase 2) — documented. This is the
     same mechanism behind the existing proven "ZERO sibling frames" guarantee.
- **Gating:** additive and off by default. `Shared.mq_enabled: AtomicBool`, set
  from `AVADA_MQ` once in `run_server`; the broker is cheap and always constructed,
  but the one wired producer (below) only publishes when enabled, so the live fleet
  is undisturbed until Bert restarts with the flag.

## 4. Migration of the polling / nudge path

Polling to retire (from `control::server` tickers + `arm_inbox_nudge`):
- `run_activity_ticker` (500ms), `run_reaper_ticker` (5s), the **nudge waiter**
  (3s settle + 2s poll loop in `arm_inbox_nudge`), `notify_state` coalescer.
- **Phase order:** keep every poller running; in phase 3 replace the nudge
  waiter's 2s poll with an MQ-push consumer (publish a wake on `agent:<pane-id>`,
  the pane's liveness-flip pushes back) — the most directly replaceable loop.
  Activity/reaper tickers migrate later or stay (low cost). **No poller is removed
  outright in phase 1** (migration-behind-MQ; final removal → For Bert).

## 5. Phased build order

0. **Recon + this plan.** (done)
1. **MQ spine** — `control::mq` broker + `TopicAddr` + `may_subscribe`, unit tests
   incl. no-cross-talk; wire `messages_post` to also publish on `agent:<pane-id>`
   behind `AVADA_MQ`. (this commit)
2. **Hierarchy + config model** — `control::org`; per-level harness+model defaults;
   per-project overrides (manager exempt); harness adapter trait + codex/copilot/aider.
3. **Manager singleton window** (leftmost, non-closable) + **right Q/A panel** as
   MQ consumer; answers routed back to the origin `agent:` topic. The panel is the
   native Open Questions relay (spec §1a): questions up, answers down, **and Bert's
   instructions down to the Manager** — never executed by the panel itself.
4. **Tasks panel + scheduler** — `WorkQueue` UI; recurring tasks; migrate the
   recurring nudge in as the first managed task; per-project nudge config.

---

## 6. For Bert (decisions only Bert should make — defaults taken meanwhile)

1. **MQ durability.** The broker is in the GUI-host process and in-memory, so
   messages are lost on Avada restart and a pane that was down misses them. The
   durable `inbox` (500/pane ring) already survives as the read-of-record, so the
   taken default is *ephemeral push + durable inbox*. True replay-after-restart
   needs either SQLite (the `WorkQueue` already does this) or moving the broker
   into `avadad`. **Keep ephemeral push, or invest in persisted replay?**
2. **Manager window: real OS window vs reserved leftmost pane column.** Affects the
   multi-monitor story and how hard the "always leftmost" guarantee is. Default
   pending your call: reserved leftmost column via `hyperpane` (cheapest, matches
   the existing "always-on tab" precedent).
3. **Removing the old polling path.** Default is migrate-behind-MQ (keep pollers
   during transition). Remove the activity/reaper tickers outright later, or keep
   them as a belt-and-braces fallback?
4. **Orchestrator granularity.** Default `orchestrator:<id> = Project.id` (one
   orchestrator per project). Do you want **per-workstream** orchestrators within a
   project (multiple orchestrators sharing one project)?
