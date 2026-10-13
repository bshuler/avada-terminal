# Operating-model module — design (DRAFT for approval)

Status: **design only. No production code is written until Bert approves this in writing.**
Branch `feat/operating-model-module`. Revision 2: folds in the oracle review and the sourced
harness table; claims below marked *verified* were checked in code at `131ff9d`.

## 1. What this is

The human → manager → orchestrator → builder way of working becomes a first-class Avada
**module** that writes the operating-model rules into the agent-instruction files of every
pane, for every harness Avada can drive, and can be enabled, disabled, upgraded and swapped
like any other module.

Bert's three requirements, and how each is met:

| Requirement | Met by |
|---|---|
| Travels with Avada | In-tree at `rs/modules/operating-model/`, listed in `rs/modules/versions.lock`, seeded on launch by `seed_bundled` like the other first-party modules. |
| Works with all harnesses | One always-on rule with **no `tools:` filter** → shared `AGENTS.md`; the existing adapters project it into `CLAUDE.md` (`@AGENTS.md`), `CONVENTIONS.md`, etc. Coverage and gaps in §4. |
| Is a module | A normal `avada.toml` module (`bshuler/avada-operating-model`) using the existing skills materializer. No new host mechanism for the injection itself. |

## 2. Verified facts this design rests on

Checked against the tree at `131ff9d`, not taken from the scout map:

- Fence primitive: `module-sdk/src/skills.rs` `fence()` / `splice_fenced()`; text outside a
  module's fence is never rewritten. Host fence id `avada/modules` carries the `@AGENTS.md`
  import in `CLAUDE.md`.
- `Materializer::plan`/`apply` (`core/src/skills/materialize.rs`, `plan.rs`) writes one
  fenced block per module into shared `AGENTS.md`; always-on units with no `tools:` filter go
  there. Gated by accepted capability `skills.materialize` and (project scope) enabled.
  Removal on disable removes that module's fence, but see §2.2 item 1: the same sweep also
  removes fences it does not know.
- `hyperpane` (`rs/modules/hyperpane/`) is the template: manifest declares
  `skills.materialize` and `[skills] paths=["skills"]`; its `skills/hyperpane/SKILL.md`
  (`kind: rule`, `activation: always`) is the template for a prose unit. The fence in the live
  orchestrator `AGENTS.md` is `bshuler/avada-orchestrator`, of unknown origin (not produced
  by HEAD).
- A module must ship a runnable binary even with zero contributions (`module-contract.md` §1);
  `module-sdk/examples/hello.rs` is the minimal one.
- Adapters that actually write files today: `claude-code`, `aider`, `cline`, `kiro`,
  `augment`, `continue`, plus the shared `AGENTS.md` layer (`adapters.rs::ADAPTERS`).

### 2.1 Correction: the caller exists, but not on `main`, and it reaches one folder only

Rev 1 and the oracle review both said no caller exists. **That was wrong for the release line,
right for `main`.** `main` (131ff9d, `Cargo.toml` 0.2.33) is 50-61 commits behind the branches
that ship (`feat/scheduled-tasks`, `fix/repaint-on-show`, `b5-closed-persist`; tip 0.2.42; the
installed app is 0.2.41). Commit `01020e8` (0.2.39, B6) is on those branches and not on `main`.
There, `core/src/orchestrator.rs:132` `materialize_modules_into` runs `Materializer::plan` and
`apply`, called from `app/src/module_runtime.rs:305` on `ModuleRuntime::new`/`reconcile`
(*verified* by the spike agent against the 0.2.42 tree and the installed 0.2.41 binary).

Its scope is the point: **one root, the orchestrator folder (`paths::orchestrator_dir()`), with
`home: None`.** So today an enabled module's rule reaches the manager/Orchestrator tab's folder
and nothing else. Builders and orchestrator panes working in project repos read those repos'
instruction files, which no code writes. Bert's requirement (every pane, every harness) therefore
needs the roots widened, which is the large-blast-radius step (D6), not just a module.

**Base branch (needs Bert, D7):** this design branch is off `main`. Building must happen on the
real release line, and I will not pick it. Everything in this doc about `Materializer` APIs is
unchanged between the two; only the caller differs.

### 2.2 Verified hazards (they bite once roots widen beyond the orchestrator folder)

0. **Spike results (rule injection works as designed):** a prose-only, no-`tools:` rule produced
   exactly one fenced block in `AGENTS.md`, the `@AGENTS.md` import in `CLAUDE.md`, an inlined
   copy in `CONVENTIONS.md`; second plan empty; a version bump rewrote only the fence; disable
   removed only the fence. Quirks: a missing fence is appended at end of file, and the first
   apply adds a trailing newline to a file that lacked one (left behind on disable).
1. **The sweep deletes foreign fences.** `assemble` (`materialize.rs:807`) drops every fence in
   a swept file whose id is not in the current request. Run over the orchestrator directory
   without `bshuler/avada-orchestrator` in the request and that fence (the dir's whole
   `AGENTS.md`) is deleted. Gate: pass all installed modules, and only sweep ids in a known set
   (installed now, or recorded in a ledger); test that a foreign fence survives.
2. **`splice_fenced` eats text after an unterminated fence** (`module-sdk/src/skills.rs:245`):
   with an open line and no matching close it skips to end of file. Gate: no close found →
   leave the file untouched, record an error.
3. **User scope ignores enable/disable** (`gated`, `materialize.rs:147`) and would write into
   `~/.claude/CLAUDE.md` (Bert's hand-managed global index) and double-load in Claude panes.
   Gate: this module is **project-scope only** (caller passes `home: None`, plus a test).
4. **`provides` is ignored** by the materializer; two enabled providers of the same shape both
   inject (*verified* by the spike with two ids: both fences written, disabling one leaves the other). "Swappable" therefore needs an
   exclusivity rule in core (D4 is required, not optional).
5. **Writing into repos dirties tracked files.** `AGENTS.md`/`CLAUDE.md` are normally
   committed; every worktree would go dirty and the block could be committed upstream. Which
   roots get written is a policy decision (D6).

## 3. Module shape

```
rs/modules/operating-model/
  avada.toml
  Cargo.toml, src/main.rs        # hello-style no-op JSON-RPC program (no contributions)
  README.md
  skills/
    operating-model/SKILL.md     # ALWAYS-ON rule → AGENTS.md block   (~45 lines, §5.1)
    om-orchestrator/SKILL.md     # activation: model → .agents/skills/ (playbook, §5.2)
    om-builder/SKILL.md          # activation: model → .agents/skills/ (playbook, §5.3)
  tests/                         # fence, idempotence, upgrade, swap (§7)
```

Manifest (essentials):

```toml
capabilities = ["skills.materialize"]

[module]
id = "bshuler/avada-operating-model"
name = "Operating Model"
version = "0.1.0"
description = "The manager -> orchestrator -> builder way of working, in every agent's instructions"
publisher = "Avada"
contract = "^1"

[distribution]
kind = "source"

[[provides]]
shape = "avada.agent.operating_model"
version = "1.0.0"

[skills]
paths = ["skills"]
```

Why three units: every pane pays for the always-on block on every turn. It carries only what
must bind every managed agent; the longer role playbooks are on-demand skills
(`activation: model`). Emitted skill names are `<owner>-<repo>-<name>` (`unit.rs:64`), so
shipped text refers to them by role ("the orchestrator playbook skill"), never by a literal name.

**Role comes from the pane, not the file.** Instruction files belong to a directory and several
panes with different roles share one cwd. The host already tracks a pane's role and parent
(`PaneInfo.meta.role`/`parent`, `manager-architecture-plan.md`, commit `131ff9d`). Design: the
host launches managed panes with `AVADA_ROLE=manager|orchestrator|builder`, and the shared block
begins "If `AVADA_ROLE` is unset, ignore this section" so Bert typing directly into an agent keeps
his own rules (ask via `AskUserQuestion`). **Open item R1**: confirm that pane spawn can set
that env var and fill it from `meta.role`; if not, it is a small host change inside B4.

## 4. Harness coverage (sourced from vendor docs, fetched 2026-10-13)

| Harness | Reads | Delivery | Status |
|---|---|---|---|
| Claude Code | `CLAUDE.md`; `AGENTS.md` only when no `CLAUDE.md` exists above cwd | adapter writes `CLAUDE.md` = `@AGENTS.md` | covered |
| Codex CLI | `AGENTS.md` (32 KiB combined default cap) | shared layer | covered; cap means our block must stay small and not rely on being last |
| Cursor (editor) | `.cursor/rules`, `AGENTS.md` | shared layer | covered; **cursor-agent CLI not documented** |
| Copilot (agent + CLI) | `AGENTS.md`, `.github/copilot-instructions.md` | shared layer | covered |
| Goose, opencode, Amp | `AGENTS.md` | shared layer | covered |
| Aider | only files passed via `--read` / `read:` in `.aider.conf.yml` | adapter writes `CONVENTIONS.md`, but Aider loads it only if configured | **partial**: needs `read:` config or user opt-in |
| Cline, Kiro, Augment | own rule dirs | adapters | covered (block is emitted twice by design: shared + native) |
| Continue | `.continue/rules/` | adapter | covered; `AGENTS.md` reading not documented |
| **Gemini CLI** | `GEMINI.md` (supports `@file` imports); not `AGENTS.md` | none | **GAP**: one adapter row, `ImportShared("@AGENTS.md")` |
| Ollama | not an agent (model server) | n/a | out of scope |
| Cody, vim/emacs/nano/edit | not documented / not agents | n/a | out of scope |

Adding a future harness is one declared change: one row in `ADAPTERS` (id, name, detect, rules
file, `RulesForm`, dialect). This module needs no edit because its rule has no `tools:` filter.
Gemini would be the first such row (D2). Copilot CLI also reads `GEMINI.md` but does not expand
`@` there; harmless.

## 5. Shipped content (proposed wording — for Bert's edits)

### 5.1 Always-on block (fenced in `AGENTS.md`; ~12 lines; byte cap enforced by test)

Deliberately thin. It does not repeat `~/.claude/CLAUDE.md` (oracle escalation, pause rules,
done = verified): those are the user's own and would be paid for twice. It names no person.

```markdown
# Operating model

Applies only when `AVADA_ROLE` is set in your environment (you were launched by an Avada
manager or orchestrator). If it is unset, ignore this section.

1. Your parent is the tier above you (builder → orchestrator → manager → human). A decision
   that only the human can make goes as one line under `## NEEDS HUMAN` at the top of
   `REPORT.md` in your project root. Then keep working on everything that does not depend on it.
   Do not open a question menu and wait for the human to find you.
2. Keep `REPORT.md` current: done and verified, in flight, blocked and on what.
3. Do not deploy, publish, spend, or irreversibly delete without written approval relayed to
   you. Build and verify locally, then report ready.

Playbooks: your role's playbook skill (`AVADA_ROLE`) has the detail.
```

Open wording points for Bert: heading `## NEEDS HUMAN` vs the current fleet convention
`## NEEDS BERT` (the manager reads the latter today; D1 includes this choice). No claim is made
that a manager exists: "your parent" is whoever launched you.

### 5.2 Orchestrator playbook skill (on demand, outline; full text after §5.1 is approved)
Plan → design → get the design approved before production code → decompose into bounded
sub-briefs → dispatch builders → integrate → quality gate → report up. Owns `REPORT.md`; never
stalls on one blocker; batches human decisions.

### 5.3 Builder playbook skill (on demand, outline)
Read the written brief; do exactly that scope; post human decisions and continue; keep
`REPORT.md` current; stop at the approval gate the brief names.

### 5.4 Not shipped
The manager's eight duties and alarm wiring stay in the Orchestrator tab's own instructions;
injecting them into every pane costs tokens and invites builders to act as managers (D3).

## 6. Swap, upgrade, enable/disable

- **Enable/disable** per workspace via `avada ctl marketplace enable|disable`; project scope
  only (§2.2 item 3). Disable removes this module's fence once the §2.2 sweep gate is in.
- **Upgrade** rewrites only this module's fence; capability-adding updates are held
  (`HeldUpdate::check`); this module never adds a capability beyond `skills.materialize`.
- **Swap**: a competing module declares `[[provides]] shape = "avada.agent.operating_model"`
  under another id. Because the materializer ignores `provides` (*verified*), swapping by
  enable/disable works only if the old one is disabled; leaving both enabled injects both.
  Required core change (D4): at most one enabled module per shape that carries skills is
  materialized, chosen by provider default (`marketplace.defaults.set`), the other reported
  as skipped.
- Two workspaces sharing a root must be merged into one request with `enabled` unioned
  (`materialize.rs:79`) or the block flaps.

## 7. Tests

1. Scratch tree, module enabled: one fenced block in `AGENTS.md`; `@AGENTS.md` import in
   `CLAUDE.md`; second plan empty.
2. User bytes before and after the fence unchanged across an upgrade.
3. Disable removes only this fence; re-enable leaves user bytes unchanged (not "identical
   bytes": a missing fence is appended at end of file).
4. Foreign fence (`bshuler/avada-orchestrator`-like, not in the request) survives. (§2.2.1)
5. Unterminated fence: file untouched, error recorded. (§2.2.2)
6. Project-only: with `home: Some`, nothing is written to user scope for this module. (§2.2.3)
7. Swap: two providers of the shape, only the default one is written. (§6)
8. Block size cap (≤ 20 lines and a byte ceiling).
9. Manifest parses/validates; `versions.lock` hash test updated.
10. Golden file per Implemented adapter.

## 8. Work breakdown

| Id | Task | When |
|---|---|---|
| S1 | Spike: prose-only module injects as claimed; two-provider behaviour | now (running) |
| S2 | Harness table | done, folded into §4 |
| S3 | Oracle review | done, folded in |
| B4a | Safety gates §2.2.1–3 in `splice_fenced`/`assemble`/`gated` + tests | after D5 approval |
| B4b | Widen the existing caller (`orchestrator.rs`/`module_runtime.rs`) from the orchestrator folder to the roots D6 chooses; merged roots | after D5/D6 |
| B4c | Exclusive shape rule (D4) | after D4 |
| B1 | Scaffold module: manifest, hello binary, `versions.lock`, seed | after D1 |
| B2 | Final prose (§5) with Bert's edits | after D1 |
| B3 | Tests §7 | after D1 |
| B5 | `AVADA_ROLE` env at pane spawn (R1) | after D1 |
| B6 | Gemini adapter row | only if D2 = yes |

B4a–c touch core and affect every module's output; they are a separate, larger-risk change
than the module itself.

## 9. Decisions requested from Bert

- **D1** Approve module shape (§3), the conditional thin wording (§5.1), and the heading
  (`NEEDS HUMAN` vs `NEEDS BERT`). Edit freely.
- **D2** Add the Gemini adapter row now, or ship it as a documented gap? Recommend: now, it is
  one row.
- **D3** Keep manager duties out of the injected text (recommended)?
- **D4** Approve the core rule making a shape exclusive (required for a real swap).
- **D5** Approve widening where the materializer writes (it exists, but covers only the
  orchestrator folder) together with its three safety fixes (§2.2), as a prerequisite separate
  from the module. If you choose "orchestrator folder only" in D6, D5 shrinks to nothing.
- **D6** Which roots get the block (this decides whether builders in repos get it at all):
  (a) orchestrator folder only: zero risk, but builders in repos are not reached;
  (b) opt-in per workspace project root (recommended): reaches builders, dirties tracked files
  only where the human asked;
  (c) every workspace root: reaches everyone, dirties every repo and worktree.
- **D7** Which branch is the release line to build on (`main` is 50+ commits behind)?

## 10. Decisions recorded (Bert, 2026-10-13, answered in session)

| Topic (doc id) | Ruling |
|---|---|
| Base branch (D7) | Build on `feat/scheduled-tasks`; rebase this design branch onto it. |
| Where to write (D6) | Opt-in per workspace project root. Orchestrator folder stays as is. |
| Safety fixes (D5) | Fix all three first (unknown fences survive, unterminated fence leaves file untouched, module never writes user scope), each with a test, before the module ships. |
| Swap rule (D4) | Add the exclusive-shape rule: one enabled provider per shape is written, chosen by provider default. |
| Injected text (D1) | Short conditional version (§5.1), active only when `AVADA_ROLE` is set; playbooks on demand. |
| Heading (D1) | `## NEEDS HUMAN`; the manager is updated to read both `NEEDS HUMAN` and `NEEDS BERT`. |
| Gemini + duties (D2, D3) | Add the Gemini adapter row (`GEMINI.md` = `@AGENTS.md`); manager duties stay out of injected text. |

Not yet given: Bert's explicit go to start building. Open implementation item R1 (setting
`AVADA_ROLE` at pane spawn) is verified during B5, not assumed.

## 11. As built (2026-10-13)

Shape name is `avada.agent.operating_model` (underscore) throughout; the manifest validator rejects hyphens. See `REPORT-operating-model.md` for commits, limits, and merge notes.
