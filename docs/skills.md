# Skills

How a module's `SKILL.md` files reach the AI tools a user runs. Code:
`rs/crates/core/src/skills/` (`avada_core::skills`); SDK types in
`rs/crates/module-sdk/src/skills.rs`; the shipped Hyperpane skill in
`resources/claude/hyperpane/skills/hyperpane/`.

## Source format

A module declares `[skills] paths = ["skills"]`. Each `skills/<name>/SKILL.md`
opens with a flat `key: value` frontmatter (no YAML crate, no nesting):

```
---
name: deploy                # kebab-case, equal to the directory name
description: "One paragraph the model picks the skill by"   # <= 1024 chars
kind: skill                 # skill (on demand, default) | rule (always in context)
activation: model           # always | model (default) | glob | manual
globs: ["*.rs"]             # for activation: glob
tools: [claude-code, agents]  # allow-list; empty = every tool + the shared layer
---
Body in Markdown.
```

`scripts/`, `references/` and `assets/` beside a `SKILL.md` are copied with it.
`skills/<tool>/<name>/SKILL.md` is a per-tool override and wins for that tool.
Tool ids: `claude-code`, `aider`, `cline`, `kiro`, `augment`, `continue`, and
`agents` for the shared layer.

Every `kind` x `activation` pair has a form per tool (table below). Two pairs
are contradictions and are `Skipped` everywhere with a reason: a `rule` that the
model activates ("set kind: skill") and a `skill` that is always on ("set kind:
rule"). `activation: glob` without a `globs:` entry is a per-unit error.

The emitted name is `<owner>-<repo>-<name>` (kebab-cased, <= 64 chars): unique
across modules, and the token regeneration and uninstall use to find their own
directories.

## Where each tool reads

Project scope, per project root of the workspace (`<m>` is `<owner>-<repo>`):

| Target | Path | Content |
|---|---|---|
| Shared rules | `AGENTS.md` | one fenced block per module: always-on rules, then glob rules as prose ("Applies only when working on files matching `*.rs`:") |
| Shared skills | `.agents/skills/<m>-<name>/` | `SKILL.md` (name, description, fenced body) + siblings |
| Index | `.agents/skills/avada-modules/` | generated: installed modules, enabled state, CLI verbs from `GET /schema` |
| Claude Code | `CLAUDE.md` | fenced `@AGENTS.md` import (id `avada/modules`) |
| Claude Code | `.claude/rules/<m>-<name>.md` | glob rules with `paths:` front matter |
| Claude Code | `.claude/skills/<m>-<name>/` | copy of each skill + the index; manual units carry `disable-model-invocation: true` |
| Aider | `CONVENTIONS.md` | rules inlined per module, glob rules in prose (Aider cannot import; it has no skills) |
| Cline | `.clinerules/<m>-<name>.md` | one file per rule; glob rules with `paths:` front matter |
| Cline | `.clinerules/workflows/<m>-<name>.md` | manual units as workflows (`/<m>-<name>.md` in chat) |
| Kiro | `.kiro/steering/<m>-<name>.md` | one file per unit with `inclusion: always` / `fileMatch` + `fileMatchPattern` / `manual` / `auto` + `name` + `description` |
| Augment | `.augment/rules/<m>-<name>.md` | one file per unit with `type: always_apply` / `agent_requested` + `description` / `manual` |
| Continue | `.continue/rules/<m>-<name>.md` | one file per unit with `name` and `alwaysApply: true` / `globs` / `alwaysApply: false` + `description` |

User scope, under the home directory, with the same fences, regeneration and
uninstall semantics as project scope:

| Tool | Rules | Glob | Manual | Skills |
|---|---|---|---|---|
| Claude Code | fenced block in `.claude/CLAUDE.md` | `.claude/rules/<m>-<name>.md` | `.claude/skills/...` with `disable-model-invocation` | `.claude/skills/<m>-<name>/` |
| Cline | `Documents/Cline/Rules/<m>-<name>.md` | same, with `paths:` | `Documents/Cline/Workflows/<m>-<name>.md` | skipped |
| Kiro | `.kiro/steering/<m>-<name>.md` | same, `fileMatch` | same, `manual` | same, `auto` |
| Augment | `.augment/rules/<m>-<name>.md` | skipped | skipped | skipped |
| Continue | `.continue/rules/<m>-<name>.md` | same, `globs` | same | same |
| Aider | none | | | |

`~/.claude/settings.json` is never touched. The shared layer (`AGENTS.md`,
`.agents/skills`) exists at project scope only.

### Activation per tool

How each `activation` is written, and what is `Skipped` with which reason:

| Tool | `always` (rule) | `glob` | `manual` | `model` (skill) |
|---|---|---|---|---|
| shared `agents` | `AGENTS.md` block | prose in the `AGENTS.md` block | skipped: "the shared Agent Skills layer has no manual-only flag; manual units are written per tool" | skill dir |
| Claude Code | `AGENTS.md` via import (project); `.claude/CLAUDE.md` (user) | `.claude/rules/*.md` with `paths:` | skill dir with `disable-model-invocation: true` | skill dir |
| Aider | `CONVENTIONS.md` | prose in `CONVENTIONS.md` | skipped: "this tool has no manual form" | skipped: "this tool has no on-demand skills; only rules are written" |
| Cline | `.clinerules/*.md` | `.clinerules/*.md` with `paths:` | `.clinerules/workflows/*.md` | skipped: no on-demand skills |
| Kiro | `inclusion: always` | `inclusion: fileMatch` + `fileMatchPattern` | `inclusion: manual` (user types `#name`) | `inclusion: auto` + `name` + `description` |
| Augment (project) | `type: always_apply` | `type: agent_requested`, description ends "Applies to files matching `*.rs`." | `type: manual` | `type: agent_requested` + `description` |
| Augment (user) | `type: always_apply` | skipped: "user-level rules are always-on only here; front matter is ignored" | same skip | same skip |
| Continue | `alwaysApply: true` | `globs:` list | `alwaysApply: false`, description prefixed "Only when the user asks for it by name:" | `alwaysApply: false` + `description` |

Notes on the table:

- Augment has no glob field, so a glob rule becomes an agent-requested rule
  whose description names the globs; the model, not the file path, decides.
- A glob rule appears twice for Claude Code at project scope: as prose inside
  `AGENTS.md` (which Claude Code imports) and as the native `.claude/rules` file.
  The "covered by the shared layer" elision applies only to plain rule blocks;
  the native file is worth the duplication because it is path-scoped and the
  prose is not.
- When a project already has a single-file `.clinerules` (Cline's legacy form),
  the adapter writes a fenced block into that file instead of creating the
  directory; workflows are then skipped ("this tool has no manual form").
  `Layout::resolve(base)` is the hook that makes this decision; every other
  layout is static.
- A rule file at our path that we did not write (no fence) is left alone and
  the unit is `Skipped` ("exists and was not written by Avada"). Fenced files
  of ours that no unit wants any more are removed on the next plan.

### Detection and the per-tool toggle

`Tools::detect(home, path_dirs)` probes every adapter row's `Detect`: a path
under `home` exists, or a binary (with or without `.exe`) is in one of the
`path_dirs`. Nothing else is read.

| Tool | Home paths | Binaries |
|---|---|---|
| claude-code | `~/.claude` | `claude` |
| aider | `~/.aider.conf.yml`, `~/.aider` | `aider` |
| cline | `~/Documents/Cline` | `cline` |
| kiro | `~/.kiro` | `kiro`, `kiro-cli` |
| augment | `~/.augment` | `auggie` |
| continue | `~/.continue` | `cn` |

`Tools::detect_here()` uses the real home and `PATH`; every other constructor
takes injected values so tests never touch the machine. The per-tool toggle is a
set of disabled ids that detection honours: `Tools::detect_with(home, dirs,
disabled)`, `Tools::detect_here_with(disabled)`, and `disable(id)` /
`enable(id)` on any value. `has(id)` is "present and not disabled" and is what
the materializer consults; `detected(id)` ignores the toggle so a UI can show a
tool as found-but-off. `ids()` lists the effective set, `detected_ids()` and
`disabled_ids()` the other two. A disabled tool's files are still swept, so
turning a tool off removes what was written for it and turning it back on
restores them. The shared layer is written whatever is detected.

The disabled set persists in `skills-settings.json` under the config dir
(`persistence::skills_settings`, `{ "disabledTools": ["cline", ...] }`), and the
intended read is
`Tools::detect_here_with(skills_settings::load().disabled_tools).with("claude-code")`.
Loading is forgiving — a missing or corrupt file, a non-array value, a
non-string element, or an id no adapter in this build claims all coerce to
"nothing disabled", which is the safe direction for a toggle that gates writes.

That call currently has **no caller**. `hyperpane::materialize()` used to be it,
back when the Hyperpane skill was built in; the skill left with the
`bshuler/avada-hyperpane` module and the host copy-out no longer materializes
anything. The next caller is whatever drives materialization from the module
registry at install time, which is where the force-add of Claude Code belongs
too: it makes a tool *present*, not *enabled* — if the user switched it off,
`Tools::has` still says no and the sweep removes what an earlier run wrote.
Nothing writes the settings file yet either; the per-tool toggles are still owed
a surface in the app's settings UI.

### Size caps

Each `Layout` may declare `cap: Some(Cap { bytes, source })`. Only Claude Code
declares one: 4 MiB, from code.claude.com/docs/en/memory: "Claude Code loads a
CLAUDE.md file of up to 4 MiB in full and skips a larger file" (re-verified
2026-09-08 against that page, quoted verbatim). The cap sits on the whole
layout, not on one file, so it also bounds a `.claude/rules/*.md` and a
`SKILL.md` — code.claude.com/docs/en/skills documents no cap of its own (only
"keep `SKILL.md` under 500 lines" as advice), and an unbounded write is the
worse default. No cap is documented for Aider, Cline, Kiro, Augment or
Continue, so their layouts carry `None` and nothing is cut.

A unit whose body exceeds `cap.bytes - CAP_RESERVE` (2 KiB for front matter,
fences and the notice) is truncated at the last blank line before the limit
(else the last line break, else the byte limit on a char boundary), and a
fenced block-quote notice follows it: "Avada truncated this unit at a paragraph
boundary: it is N bytes and claude-code reads at most M bytes (source). The
full text is in <SKILL.md path>." The plan reports it in
`Plan::truncated: Vec<Truncated { unit, tool, bytes, cap }>`. Nothing is ever
dropped silently.

That bounds one unit. The file the units accumulate in is bounded separately,
because the failure modes differ: a unit cut short loses its tail, but a
`CLAUDE.md` that grows past 4 MiB is skipped *whole*, taking every other
module's rules with it. So after the blocks are spliced into the file --- in
`diff`, the one place the final text is known, the user's own bytes included ---
`fit_cap` drops whole module blocks from the end until the result fits. Whole
blocks, never partial ones: a fence's contract is that regeneration replaces the
text between its markers, and half a block is not that. The host's own
`@AGENTS.md` import line is never dropped (one line, and losing it would take
the entire shared layer), and once every module block is gone the loop stops
even if the file is still too big --- what is left is the user's own writing,
which is not ours to cut. The plan reports it in
`Plan::overflowed: Vec<Overflow { path, tool, bytes, cap, dropped }>`, where
`dropped` lists the module ids in the order they stood in the file. The uncapped
shared `AGENTS.md` keeps the full text of everything.

Which module loses is positional, not a judgement: blocks are dropped from the
end, and the order is the order the modules arrive in the request. A ranking
worth defending --- oldest install first, or smallest-first so the most modules
survive --- needs a signal the materializer is not given today.

Gates: a module contributes to the project scope only when its accepted
capability set holds `skills.materialize` and it is enabled in a workspace
using that root; the user scope needs the capability only. Two workspaces
sharing a root pass the union of their enabled modules.

### Sources, and the one row still resting on an inference

Checked 2026-09-08 against each tool's public documentation (URLs in the
`ADAPTERS` doc comment in `rs/crates/core/src/skills/adapters.rs`), by fetching
every page and quoting it:

- Claude Code: `.claude/rules/*.md` with `paths:` front matter, `~/.claude/rules/`,
  skills with `disable-model-invocation: true`, the 4 MiB cap.
- Cline: `.clinerules/` directory of `.md`/`.txt`, `paths:` front matter,
  `~/Documents/Cline/Rules`, workflows in `.clinerules/workflows/` and
  `~/Documents/Cline/Workflows`. It also reads `AGENTS.md` and
  `~/.agents/AGENTS.md`.
- Kiro: `.kiro/steering/` and `~/.kiro/steering/`, `inclusion:` values. It reads
  `AGENTS.md` too, always and without inclusion modes. Its steering docs
  describe no skill form at all, so no skills are written for Kiro.
- Augment: `.augment/rules/`, `type:` values; `type: manual` is IDE-only and
  "skipped by the CLI"; user rules under `~/.augment/rules/` are always-on
  regardless of front matter. It reads `AGENTS.md`, and `CLAUDE.md` above it.
- Continue: `.continue/rules/` with `name`, `globs`, `description`,
  `alwaysApply`. A `regex` condition also exists and is not emitted.
- No file size limit is documented for Cline, Kiro, Augment or Continue, so none
  is enforced for them.

One row is not quoted from anywhere: Continue's global `~/.continue/rules/`
appears on neither of Continue's two rules pages. It is inferred from Continue's
data directory, and the user-scope layout is written on that inference — safe in
the sense that a tool which does not read the directory just ignores it, but it
is the only path in the table that could simply be wrong.

Because Cline, Kiro and Augment all read `AGENTS.md`, an always-on rule reaches
them twice at project scope: once in the shared file and once in their native
rules directory. That is deliberate. Augment publishes an order of precedence
over rules files without saying whether it loads all of them or stops at the
first, so suppressing the native copy risks dropping the rule entirely, which is
worse than repeating it. The suppression path exists for importing tools
(`RulesForm::ImportShared`, which is how Claude Code avoids the repeat) and can
be extended to native rule files once one of the three documents that its lists
accumulate.

## Fence contract

Every emitted section and every emitted `SKILL.md` body sits inside

```
<!-- avada:module=<owner>/<repo> -->
...
<!-- /avada:module=<owner>/<repo> -->
```

- Regeneration replaces the text between a module's fences and the files under
  its own `<m>-<name>/` directories. Bytes outside a fence are never rewritten;
  a fence that does not exist yet is appended after the user's last line.
- Disable and uninstall remove exactly those fences and directories. A fenced
  file left with only whitespace is deleted.
- A directory that carries our name but whose `SKILL.md` has no fence is the
  user's: it is left alone and the unit is `Skipped` with that reason.
- Host-generated pieces (the `@AGENTS.md` import, the index skill) use the id
  `avada/modules`.

## API

```rust
let mat = Materializer::new();
let plan: Plan = mat.plan(&Request { modules, roots, home, tools, schema, workspace });
// plan.writes / removals / skipped / truncated / errors are sorted and inspectable; nothing written yet.
let applied: Applied = mat.apply(&plan);   // removals then writes; failures listed, not fatal
assert!(mat.plan(&request).is_empty());     // idempotent
```

`ModuleInput { id, name, version, version_dir, skills, accepted, enabled }` is
what the registry knows per installed module — the only shape this materializer
accepts. There is no built-in module any more: `bshuler/avada-hyperpane` carries
the Hyperpane rule and arrives through the marketplace like any other, so its
skills reach the tab's directory as an installed module's `[skills] paths`, not
as a special case in `hyperpane::materialize()`.

## Extending

An adapter is one `AdapterRow` in `adapters::ADAPTERS`:

```rust
AdapterRow {
    id, name, status: Implemented,
    detect: Detect { home_paths: &[&[".kiro"]], binaries: &["kiro", "kiro-cli"] },
    project: Some(Layout { rules_dir: Some(RulesDir { path: &[".kiro", "steering"],
                                                       dialect: Dialect::Kiro, always_only: false }),
                           ..Layout::EMPTY }),
    user: Some(Layout { /* same, rooted at home */ ..Layout::EMPTY }),
}
```

A `Layout` names where a tool reads: `skills_dir` (Agent Skills directories;
`manual_skills` when the tool has a manual-only flag), `rules_file` with
`rules_form` (`ImportShared("<line>")` for a tool that imports `AGENTS.md`, else
`Inline`), `rules_dir` (one fenced file per unit in the named `Dialect`'s front
matter), `manual_dir` (one fenced file per manual unit, no front matter) and
`cap`. Paths are segment lists joined with `Path::join`. Adding a tool is one
row, a `Dialect` arm in `materialize::rule_frontmatter` if its front matter is
new, a `Detect` line in the table above, and a fixture test in
`skills/tests.rs` asserting the exact files written at both scopes. The
materializer needs no other change.
