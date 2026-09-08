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

No preference stores the disabled set yet; `hyperpane::materialize()` calls
`Tools::detect_here().with("claude-code")`. Wiring a setting is one call:
`Tools::detect_here_with(prefs.disabled_tools)`.

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

The cap is per unit, not per file: a `CLAUDE.md` holding many modules' blocks
can still exceed 4 MiB in total, and the `@AGENTS.md` import is not measured.
The uncapped shared `AGENTS.md` keeps the full text.

Gates: a module contributes to the project scope only when its accepted
capability set holds `skills.materialize` and it is enabled in a workspace
using that root; the user scope needs the capability only. Two workspaces
sharing a root pass the union of their enabled modules.

### Sources and what was not verified

Checked 2026-09-08 against each tool's public documentation (URLs in the
`ADAPTERS` doc comment in `rs/crates/core/src/skills/adapters.rs`):

- Claude Code: `.claude/rules/*.md` with `paths:` front matter, `~/.claude/rules/`,
  skills with `disable-model-invocation: true`, the 4 MiB cap.
- Cline: `.clinerules/` directory of `.md`/`.txt`, `paths:` front matter,
  `~/Documents/Cline/Rules`, workflows in `.clinerules/workflows/` and
  `~/Documents/Cline/Workflows`.
- Kiro: `.kiro/steering/` and `~/.kiro/steering/`, `inclusion:` values.
- Augment: `.augment/rules/`, `type:` values; user rules under
  `~/.augment/rules/` are always-on regardless of front matter.
- Continue: `.continue/rules/` with `name`, `globs`, `description`, `alwaysApply`.

Not verified, and how the code treats it:

- Continue's global rules directory (`~/.continue/rules/`) is inferred from its
  config layout, not from a documented statement; the user-scope layout is
  written on that inference.
- Whether Cline reads `AGENTS.md`: not claimed; Cline gets native rule files.
- Augment's `type: manual` is documented for the IDE extension; the CLI's
  handling is unstated. It is written as documented.
- Whether Kiro, Augment or Continue impose a file size limit: none found, so
  none is enforced.

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
what the registry knows per installed module. `hyperpane::materialize()` is the
first caller: the built-in `avada/hyperpane` module materialized into the
Hyperpane tab's directory with Claude Code always on.

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
