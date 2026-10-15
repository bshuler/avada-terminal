# Design brief — B13 step 2: the `relay` operating-model role

**Repo:** `/Users/bshuler/code/avada/avada-terminal`
**Branch:** `main`
**HEAD at research time:** `38a09d3 docs: name the Open Questions pane Riddle's Diary`
(later than the required `e22b27b`; all four spec sources present and current)

**Scope of this brief:** B13 **step 2 only** — teach the role system a new `relay` role and
ship the relay's operating rule as a module file so it travels with Avada. Step 1 (the brief in
the repo + hand-placed guard hook) is **done**. Step 3 (the native right-side Q/A panel) is out
of scope. One builder, one worktree+branch.

> Note on authority: the spec sources below are the authority. Where the backlog wording ("whose
> fenced rule is **the brief**", i.e. all 154 lines) and the module-design doctrine (`§5.1`/`D1`:
> the always-on fence is "deliberately thin", detail goes to on-demand playbooks) pull in
> different directions, this brief flags it as a decision for Bert rather than silently choosing.

---

## 1. What B13 step 2 is

From `docs/backlog.md` — item **B13**, "Fix direction, in order", step (2)
(`docs/backlog.md:44-47`):

> "(2) the operating-model module grows a relay role (`AVADA_ROLE=relay`, accepted by
> `rs/crates/core/src/pane_role.rs`) whose fenced rule is the brief, so it travels with Avada
> and the hook becomes a module file"

The spec restates the same gap and its cause. `docs/manager-architecture-spec.md:110-116`:

> "the operating-model module's fences only bind a pane that carries an accepted `AVADA_ROLE`
> (`manager|orchestrator|builder`), which the relay has none of until B13 step 2 adds `relay`.
> So the brief carries its own rules, and a PreToolUse hook in that directory
> (`.claude/hooks/relay_guard.py`) refuses the calls MUST 10 forbids…"

And the rule the relay must embody — MUST 10, `docs/manager-architecture-spec.md:65-72`:

> "**The relay only relays — in both directions.** … It never builds, fixes, investigates on
> its own account, spawns subagents or worktrees, drives or closes other panes, messages agents
> other than the Manager, or decides which orchestrator a task belongs to."

The gap that caused the incident — the relay brief originally never told the relay to *forward
Bert's instructions down* — is recorded at `docs/manager-architecture-spec.md:97-102`. The
forward-down duty is therefore a MUST-level line of the shipped rule, not optional.

Where step 2 sits in the plan: `docs/manager-architecture-plan.md:46` (phases 2–4 context) lists
the relay under requirements 9–11 and marks "**step 2 relay role pending**"; the native panel
is phase 3 (`plan:148-152`). Step 2 is the stopgap that makes the relay pane bind an *accepted*
role so a module-shipped rule governs it, until the phase-3 panel replaces the pane.

---

## 2. Current state (what exists today)

### 2.1 The role system — one centralized definition

`rs/crates/core/src/pane_role.rs` is the single source of truth. Roles are a 3-element const and
every other site derives from it:

- `pane_role.rs:16` — `pub const ROLES: &[&str] = &["manager", "orchestrator", "builder"];`
- `pane_role.rs:19-28` — `validate(role)` → `Ok` iff in `ROLES`, else an error built from
  `ROLES.join("|")` (auto-updates when `ROLES` grows).
- `pane_role.rs:32-36` — `canonical(meta)` → the stored `meta.role` only if it is in `ROLES`.
- `pane_role.rs:44-60` — `env_with_role(...)` strips any caller-supplied `AVADA_ROLE`, then
  re-exports it only from a canonical stored role (respawn path).
- `pane_role.rs:67-89` — `apply_to_spec(spec)` mirrors a canonical `meta.role` into
  `env.AVADA_ROLE`; refuses a hand-set `env.AVADA_ROLE`.

Call sites that already route through this (adding to `ROLES` propagates to all of them):
- `rs/crates/app/src/ctl_cli.rs:257-260` — `new-pane --role` validates via
  `pane_role::validate` (fast client-side usage error).
- `rs/crates/core/src/control/dispatch.rs:454` and `:1262` — server validates via
  `apply_to_spec` (authoritative).
- `rs/crates/core/src/pane_origin.rs:51-53` — a canonical role rides the origin bag so it
  survives restart/re-host/relaunch and re-exports `AVADA_ROLE`.
- `rs/crates/app/src/state.rs:3984` — reads `canonical(...)` from the origin bag.

Collision check: `rg -n '"relay"' rs/` returns **nothing**. No existing pane carries a free-form
`meta.role="relay"`, so adding `"relay"` to `ROLES` cannot retroactively start exporting
`AVADA_ROLE` for an existing pane. Clean.

**Two display strings are hardcoded (not derived) and will go stale:**
- `pane_role.rs:78` — the `apply_to_spec` error literal `"(manager|orchestrator|builder)"`.
- `ctl_cli.rs:58` — the usage line `--role manager|orchestrator|builder`.

### 2.2 The operating-model module — how the fence ships

`rs/modules/operating-model/` is a files-only module (no behaviour; `src/main.rs:1-8`). Its
manifest (`avada.toml`) asks for exactly one capability, `skills.materialize` (`avada.toml:9`),
and ships `skills/` (`avada.toml:27`). It `provides` the exclusive shape
`avada.agent.operating_model` (`avada.toml:22-24`), meaning only one module may own this rule
(`materialize.rs:87 EXCLUSIVE_SHAPES`).

Three units ship today (`tests/manifest.rs:14-16`):
- `skills/operating-model/SKILL.md` — `kind: rule`, `activation: always`. The ~12-line always-on
  block. Gated **textually** by its first line: "Applies only when `AVADA_ROLE` is set … If it
  is unset, ignore this section." (`skills/operating-model/SKILL.md:9-10`).
- `skills/om-orchestrator/SKILL.md` — `kind: skill`, `activation: model`. On-demand playbook,
  self-gated "Use this when `AVADA_ROLE` is `orchestrator`" (`om-orchestrator/SKILL.md:5,9`).
- `skills/om-builder/SKILL.md` — same shape for `builder` (`om-builder/SKILL.md:5,9`).
  (There is no `om-manager` unit.)

How a unit reaches an agent (`rs/crates/core/src/skills/materialize.rs`):
- A `kind: rule, activation: always` unit becomes a **fenced block** in the shared rules file
  `AGENTS.md` (`materialize.rs:46 SHARED_RULES`, `:361-369 resolve→Form::Block`,
  `:724/:770 push_block`). Tools that read `CLAUDE.md`/`GEMINI.md` get a one-line
  `@AGENTS.md` import instead (`operating_model_e2e.rs:124-137`).
- A `kind: skill, activation: model` unit becomes a **skill directory**
  `<owner>-<repo>-<name>/SKILL.md`, available to every pane, invoked by the model on demand
  (`materialize.rs:404-416`).
- **Materialization is NOT role-aware.** A fence lands in *every* materialized root's `AGENTS.md`
  regardless of the pane's role; `AVADA_ROLE` only switches the text on/off at read time
  (doctrine stated in `docs/operating-model-module.md:131-137`). All on-demand playbooks are
  likewise available to all panes and gate themselves in prose.
- Roots are the launch workspace's **project roots** plus user scope (home), not arbitrary pane
  cwds: `rs/crates/app/src/module_runtime.rs:315,322-341`
  (`roots: avada_core::orchestrator::project_roots(&self.project_roots)`), `plan`/`apply` via
  `Materializer`. (This is the acceptance-test caveat in §4.)

Design doctrine to respect — `docs/operating-model-module.md:127-130` ("every pane pays for the
always-on block on every turn … the longer role playbooks are on-demand skills") and `§5.1`
(`operating-model-module.md:152-155`: the always-on block is "**Deliberately thin** … ~12 lines;
byte cap enforced by test"). Codex CLI's 32 KiB combined `AGENTS.md` cap is cited as the reason
(`operating-model-module.md:139`).

**The cap is a real, hard test (decisive constraint).** `operating_model_e2e.rs:287-294`,
`the_agents_md_block_stays_small`, materializes the module alone and asserts the **whole**
rendered `AGENTS.md` fence is `t.len() < 2048` bytes **and** `t.lines().count() <= 20` lines.
Today the shipped block body is ~820 bytes / ~13 lines, so the fence is already most of the way
to the 20-line ceiling: **there is room for roughly 5–6 more lines, not a never-list.** This
makes putting the relay never-list into the shared always-on block (§3.2 shapes B/C) effectively
impossible without changing the test's bound — which the builder must not do silently. It is the
single biggest force on the rule-shape decision (Q1), and it is why §3.2 recommends shape (A).

### 2.3 The relay rule content and the guard hook today

- The rule text already exists as prose: `docs/open-questions-brief.md` (154 lines) — the
  repo source — copied verbatim to
  `~/Library/Application Support/avada/open-questions/AGENTS.md` (the live copy the relay pane
  reads). `open-questions-brief.md:10-12` asserts the two are the same text, repo copy wins.
- The PreToolUse guard hook exists **only in the live dir**, not the repo:
  `~/Library/Application Support/avada/open-questions/.claude/hooks/relay_guard.py` (5002 bytes,
  executable). `rg -l relay_guard` over the repo finds it named only in `docs/` (spec + backlog),
  never as a committed script.

---

## 3. What must change — file-by-file

### 3.1 Accept the `relay` role (required, small, MUST-level)

`rs/crates/core/src/pane_role.rs`:
- `:16` — add `"relay"` to `ROLES`: `&["manager", "orchestrator", "builder", "relay"]`.
- `:78` — replace the hardcoded `"(manager|orchestrator|builder)"` in the `apply_to_spec` error
  with a value derived from `ROLES.join("|")` (so it never goes stale again). `validate`'s error
  at `:25` already derives, so leave it.
- `:1` — module doc comment ("A pane's operating-model role: `manager`, `orchestrator` or
  `builder`.") — add `relay`.

Display/comment touch-ups (no logic):
- `ctl_cli.rs:58` — usage string `--role manager|orchestrator|builder|relay`.
- `dispatch.rs:452` — comment lists the roles; add `relay`.

No other code change is needed for acceptance/export: `validate`, `canonical`, `env_with_role`,
`apply_to_spec`, `pane_origin` carry, and `state.rs:3984` all derive from `ROLES`.

### 3.2 Ship the relay rule as a module unit (the main decision — see §7 Q1)

The cap test (§2.2) rules out the obvious shapes and sharpens the choice. Three candidates:

- **(A) Relay detail in an on-demand `om-relay` playbook; always-on block barely touched
  (recommended, passes every existing test as-is).** Add
  `rs/modules/operating-model/skills/om-relay/SKILL.md` (`kind: skill`, `activation: model`,
  front matter self-gating "Use this when `AVADA_ROLE` is `relay`", mirroring
  `om-builder/SKILL.md:1-10`) carrying the full relay procedure: the never-list
  (`open-questions-brief.md:32-52`), the forward-down duty (`:27-30,64-67`), the
  `avada ctl tabs`→find-Manager→inbox-file→`--screen`→submit Down procedure (`:64-102`), the
  ghost-suggestion probe (`:85-89`), the Up flow + `AskUserQuestion` presentation (`:104-134`),
  secrets (`:136-141`). The always-on block gains at most a one-line relay pointer (fits the
  ~5-line headroom) plus the gate fix (§3.3). **Trade-off:** a model-activated skill is not
  guaranteed to be loaded on every turn, so the never-list is not always-on; the PreToolUse
  guard hook (§3.4) is then the real backstop, matching `open-questions-brief.md:54-55` ("The
  hook is a backstop. The rule is this file.").

- **(B) A separate always-on relay unit in the same module, and relax the cap test per role.**
  Honours "the never-list must be always-on" but a second always-on unit makes `emit` join both
  bodies into one fence (`materialize.rs:724`), so the combined `AGENTS.md` blows both bounds of
  `the_agents_md_block_stays_small` and `the exact-equality assertion` — the builder must
  re-target that test (e.g. per-role or per-unit size budgets), changing its intent. Only choose
  this if Bert decides the relay's safety rules must bind on every turn regardless of context
  cost.

- **(C) Full 154-line brief as one always-on fence (literal backlog wording).** Grossly over the
  2048-byte / 20-line cap and injects the brief into every materialized `AGENTS.md`. Not viable
  without gutting the cap test; recorded only for completeness.

Recommend **(A)**. It is the only shape that passes the existing tests unchanged, matches how
builder/orchestrator already split (short always-on + on-demand playbook), and leans on the hook
as the backstop the brief itself names. (B) is the fallback if Bert wants always-on safety
binding. Q1 is this choice.

**Keep product-generic.** The brief's standing facts and local paths (gluster, hp3 drive,
1Password SSH agent, hard-coded labels/paths — `open-questions-brief.md:143-154` and the
`Riddle's Diary` label) are Bert's fleet, not the product that "travels with Avada". The shipped
unit must be generic MUST 9–11 wording. The local facts stay in
`~/Library/Application Support/avada/open-questions/AGENTS.md` **outside** the module fence,
where the materializer preserves them byte-for-byte (proven by
`operating_model_e2e.rs:*user_text_outside_the_fence_is_byte_identical*`). This changes what is
canonical (Q3).

### 3.3 Fix the gate so `relay` is not bound by the worker rule (required, MUST-level)

Today the always-on block gates on "`AVADA_ROLE` is **set**"
(`operating-model/SKILL.md:9`), so once `AVADA_ROLE=relay` exists the relay pane would inherit
rules written for workers that directly contradict the relay's job:
- `SKILL.md:12` "Your parent is the tier above you" vs `spec:86-87` "the relay … is not a tier
  of its own — it is a peer of the agents".
- `SKILL.md:15` "Do not open a question menu and wait" vs `open-questions-brief.md:131` "Every
  question goes through `AskUserQuestion`".

So the gate must become role-conditional: either change the generic block's gate to "Applies
only when `AVADA_ROLE` is `manager`, `orchestrator` or `builder`", with a separate relay branch,
or have the relay clause explicitly override. Put this near the top of the change list — it is a
correctness item, not cosmetic.

### 3.4 The guard hook — "the hook becomes a module file" (blocked as literally stated; see Q2)

The backlog says the hook "becomes a module file". The module system has **no capability to
register a PreToolUse hook** — `docs/module-contract.md:106` lists only `SkillsMaterialize` for
writing files; there is no hook/settings capability. The materializer *can* ship a `scripts/`
sibling next to a `SKILL.md` (`materialize.rs:13-14,:579 sibling_files`) but it never writes
`.claude/settings.json`, so a shipped `relay_guard.py` would **not** be wired as a hook.

Therefore step 2's achievable form is: bring `relay_guard.py` (and its sample-call tests,
per `backlog.md:46`) into the repo under the module (e.g. as a sibling script of the relay unit)
and document the one-line `.claude/settings.json` PreToolUse wiring as a manual step — OR defer
hook-shipping to a separate brief (2b). Do **not** invent a new materializer hook form inside
this brief; that is a larger, separate change (Q2). The fenced rule is the primary safeguard
("The hook is a backstop. The rule is this file." — `open-questions-brief.md:54-55`), so the
rule landing correctly is what makes step 2 meaningful even if the hook wiring stays manual.

### 3.5 Tests to extend (required — do not delete)

- `rs/crates/core/src/pane_role.rs:95-103` — `accepts_exactly_the_three_roles` asserts over
  `["manager","orchestrator","builder"]` and rejects others; add `relay` to the accepted list
  (and rename, since "three" is now four). Keep the rejection cases (`"Relay"`, `"worker"`, etc.).
- `rs/modules/operating-model/tests/manifest.rs:13-22` — `the_three_units_ship` iterates
  `["operating-model","om-orchestrator","om-builder"]`; add `om-relay` (and rename). The
  `kind: rule`/`activation: always` assertions on `operating-model/SKILL.md` still hold.
- `rs/crates/core/tests/operating_model_e2e.rs:99-120` — **exact-equality** assertion
  `assert_eq!(agents, fence_of(ID, &rule_body()))` where `rule_body()` reads only
  `operating-model/SKILL.md`, plus the needle at `:111` "Applies only when `AVADA_ROLE` is set".
  Under shape (A) the always-on unit still equals its own body, so exact-equality holds; the
  `:111` needle **breaks** when §3.3 changes the gate text — update that one needle. Under shape
  (B) `emit` joins both units into one fence (`materialize.rs:724`) and exact-equality breaks
  entirely — the builder must re-target it. Name these lines so the builder extends, not deletes.
- `rs/crates/core/tests/operating_model_e2e.rs:287-294` — `the_agents_md_block_stays_small`:
  `t.len() < 2048` **and** `t.lines().count() <= 20` on the whole `AGENTS.md`. Shape (A) must
  keep the always-on block (plus any one-line relay pointer) under both bounds — current ~820
  bytes / ~13 lines, ~5-line headroom. Shape (B) requires re-targeting this test.

No other test enumerates the module's shipped unit set: `rg "om-builder|om-orchestrator|om-relay"
rs/crates/core/tests/ rs/crates/core/src/skills/` returns nothing beyond `manifest.rs`. So §3.5
is complete — there is no golden/fixture skill-dir test to also update.

### 3.6 Docs to refresh (stale after the change)

`docs/manager-architecture-spec.md:112` ("which the relay has none of until B13 step 2 adds
`relay`" → now added); `docs/operating-model-module.md:135` (role list); `docs/backlog.md` B13
step 2 status line; `docs/open-questions-brief.md:9-12` (the "same text / repo copy wins"
identity claim, once the module unit becomes canonical — Q3).

---

## 4. Acceptance criteria (human-visible — "done" = Bert can see it work)

Respect the no-focus-steal rule: run every check on a **hidden** tab, drive by pane id, never
focus, and only when Bert is away (HID idle > 180s) or `AVS_PERMIT=1` — `new-pane` creates a pane
and must not fire while he is at the keyboard. `ROLE=relay` appearing on screen is the
human-visible proof.

**Done-doctrine gate (names a human decision — see §7 Q5).** Acceptance item 1 requires the
*server* to accept `--role relay`, and the server is authoritative (`dispatch.rs:454/:1262`). The
running server is the live 0.2.49, which rejects `relay`. Building and `cargo test`-ing in the
worktree needs no relaunch, but *proving it human-visibly* needs the new build installed and one
window restart — exactly what the standing no-rebuild/no-relaunch-of-live-Avada rule forbids
(0.2.49 consumed the one prior exception). So step 2 cannot reach "done = Bert sees `ROLE=relay`"
without a fresh install+restart approval. This is Q5.

1. **Role accepted end-to-end.** On a hidden tab, in a **materialized project root** — e.g. the
   orchestrator data dir `~/Library/Application Support/avada/orchestrator/`, which is already a
   root (its `AGENTS.md` carries the operating-model fence today):
   `avada ctl new-pane --why "verify relay role" --role relay
   --cwd "$HOME/Library/Application Support/avada/orchestrator"
   --cmd 'sh -c "echo ROLE=$AVADA_ROLE; grep -c relay AGENTS.md; sleep 60"'`
   then `avada ctl read <pane> --screen` shows `ROLE=relay` and a non-zero `grep` count, and
   `avada ctl info <pane>` shows `role: relay`.
2. **Rule is present and governs.** Under shape (A): the always-on fence in that root's
   `AGENTS.md` carries the one-line relay pointer and the fixed gate text, and the
   `<owner>-<repo>-om-relay/SKILL.md` directory exists carrying the full generic MUST 9–11
   procedure (never-list + forward-down duty). Under shape (B): the never-list and forward-down
   line appear in the always-on fence itself. Either way the forward-down duty must be reachable
   by the relay pane — that is the incident fix, so it is a hard acceptance point.
3. **Survives restart.** `avada ctl restart-pane <pane>` (on the hidden tab) and re-reading the
   screen still shows `ROLE=relay` (`pane_origin.rs:51-53` re-export).
4. **Negatives still fail.** `avada ctl new-pane --role Relay …` and `--role worker …` still
   produce the usage error (capitalization and unknown values rejected).
5. `cargo test` green including the extended tests in §3.5.

**Caveat that is not the builder's code:** acceptance item 1's `grep` only passes if the pane's
cwd is a materialized root. Materialization targets roots derived from *running panes' cwds*:
`project_roots(pane_dirs)` keeps each dir that `find_project_root` resolves, and
`find_project_root` recognizes a dir only if it has an Avada project marker (`has_project_dir`)
or a `.git` (`orchestrator.rs:290-304`, `workspace/project.rs:107-127`). The orchestrator data
dir qualifies (hence its fence); the relay's own dir
`~/Library/Application Support/avada/open-questions/` does **not** today. So demo against a dir
that already qualifies (above). Whether `open-questions/` should *become* a root is Q4 — flag it
in the demo rather than letting a missing fence there read as a code failure.

---

## 5. Test / quality gate

Crate names verified against the manifests: `avada-core` (`crates/core/Cargo.toml:2`), `avada`
(`crates/app/Cargo.toml:2`), `avada-operating-model` (`modules/operating-model/Cargo.toml:2`,
a workspace member — `rs/Cargo.toml:15`). From `rs/`:
```
cargo build -p avada-core -p avada
cargo test  -p avada-core             # pane_role unit tests + tests/operating_model_e2e
cargo test  -p avada-operating-model  # tests/manifest.rs
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```
The `om-orchestrator` playbook states its gate generically — "Nothing is done until it has been
built and tested, and you have read the output yourself" (`om-orchestrator/SKILL.md:24-25`) — not
as fixed commands. So the gate here is: the commands above green **and** the human-visible demo
in §4 recorded, not just a passing `cargo test`.

---

## 6. Estimated size

- **Code:** `pane_role.rs` one const element + one derived error string + doc line; `ctl_cli.rs`
  + `dispatch.rs` comment/usage strings. ~10–15 LOC.
- **Tests:** three edits (§3.5), ~15–30 LOC.
- **Module content (shape A):** a one-line always-on relay pointer + the gate-text fix (within
  the ~5-line headroom under the 20-line cap) + `om-relay/SKILL.md` (~60–100 lines of generic
  procedure). Plus docs refresh. (Shape B instead adds a full always-on relay clause and forces
  the cap test to be re-targeted.)
- **Files touched:** ~6 Rust/test files, 1–2 new module files, ~4 doc files.

**Verdict:** one-builder-one-brief sized **iff** Q2 resolves as "ship script + document manual
wiring, or defer". If Bert wants the materializer to gain a hook-registration form, split that
into a separate brief (2b) — it touches the module contract, a new capability, and the
materializer, and is larger than the role+rule change.

---

## 7. Open design questions for the human (decisions only Bert can make)

1. **Rule shape (forced by the cap test, §2.2/§3.2).** (A, recommended) relay procedure in an
   on-demand `om-relay` playbook with only a one-line always-on pointer, relying on the guard
   hook as the never-list backstop — passes every existing test unchanged; (B) a separate
   always-on relay unit so the never-list binds every turn, which requires re-targeting
   `the_agents_md_block_stays_small` and the exact-equality e2e test; or (C) the full 154-line
   brief always-on (over cap, not viable). Pick A unless the relay's safety rules must bind on
   every turn regardless of context cost (then B).
2. **The guard hook.** The module cannot auto-register a PreToolUse hook (no capability). Do you
   want: (a) bring `relay_guard.py` + its tests into the repo and document the manual
   `.claude/settings.json` wiring; (b) defer the hook to a separate brief (2b); or (c) add a new
   materializer hook-shipping form (larger, separate brief)?
3. **Canonical source after step 2.** Largely settled by Q1: if (A) is picked, the module
   `om-relay` unit becomes canonical by construction and `docs/open-questions-brief.md` becomes a
   local-facts companion/pointer. The only residual choice is where the standing facts
   (`open-questions-brief.md:143-154`, Bert's fleet specifics) live — recommended: outside the
   fence in the live `AGENTS.md`, where the materializer preserves them byte-for-byte. Confirm or
   redirect.
4. **Does `open-questions/` become a materialized root? (recommended: yes — add the marker.)**
   Step 2's whole point (§1) is that a *module-shipped* rule governs the relay. If
   `open-questions/` stays a hand-copied non-root, the module fence never lands there and that
   purpose is not met — the relay would still be running on a hand-maintained copy, which is the
   status quo. So the recommended path is to give `open-questions/` an Avada project marker, so
   the running relay pane's cwd makes it a materialized root and the fence lands automatically
   (exactly like the orchestrator data dir does today). The fallback — keep a hand-copied
   `AGENTS.md` with the module unit as canonical source — is only if Bert does not want that dir
   to become a project root. Recommend the marker.
5. **Install + window restart to prove it (done-doctrine, §4).** The server-side acceptance of
   `--role relay` can only be shown on screen after the new build is installed and the window
   restarted, which the standing no-relaunch rule forbids by default. (a, recommended) grant that
   install+restart in advance, conditional on the full quality gate (§5) being green first, so it
   is one round-trip; (b) I build + gate in the worktree, report ready, and ask you again at that
   point for the restart; (c) land the code but leave it unproven until the next time Avada is
   updated for another reason. Recommend (a) — the only path that reaches "done = you see
   `ROLE=relay`" without a second interruption.
