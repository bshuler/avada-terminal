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

What is emitted this wave: `kind: rule` + `activation: always` becomes a fenced
rules block; `kind: skill` + `activation: model` becomes a skill directory. Every
other combination is recorded in the plan as `Skipped { unit, tool, reason }`
(glob and manual activation are track G9's).

The emitted name is `<owner>-<repo>-<name>` (kebab-cased, <= 64 chars): unique
across modules, and the token regeneration and uninstall use to find their own
directories.

## Where each tool reads

Project scope, per project root of the workspace:

| Target | Path | Content |
|---|---|---|
| Shared rules | `AGENTS.md` | one fenced block per module: always-on rules joined by blank lines |
| Shared skills | `.agents/skills/<m>-<name>/` | `SKILL.md` (name, description, fenced body) + siblings |
| Index | `.agents/skills/avada-modules/` | generated: installed modules, enabled state, CLI verbs from `GET /schema` |
| Claude Code | `CLAUDE.md` | fenced `@AGENTS.md` import (id `avada/modules`) + Claude-only rules |
| Claude Code | `.claude/skills/<m>-<name>/` | copy of each skill + the index |
| Aider | `CONVENTIONS.md` | rules inlined per module (Aider cannot import; it has no skills) |

User scope, under the home directory (Claude Code only this wave):
`.claude/skills/<m>-<name>/` and a fenced block in `.claude/CLAUDE.md` with the
rules inlined. `~/.claude/settings.json` is never touched.

Only tools present in `Tools` are written: `Tools::detect(home, path_dirs)` is a
data-driven probe (a path under home exists, or a binary is on `PATH`), and the
per-tool toggle is `Tools::without(id)`. The shared layer is written whatever is
detected.

Gates: a module contributes to the project scope only when its accepted
capability set holds `skills.materialize` and it is enabled in a workspace
using that root; the user scope needs the capability only. Two workspaces
sharing a root pass the union of their enabled modules.

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

## API (for track H4)

```rust
let mat = Materializer::new();
let plan: Plan = mat.plan(&Request { modules, roots, home, tools, schema, workspace });
// plan.writes / removals / skipped / errors are sorted and inspectable; nothing written yet.
let applied: Applied = mat.apply(&plan);   // removals then writes; failures listed, not fatal
assert!(mat.plan(&request).is_empty());     // idempotent
```

`ModuleInput { id, name, version, version_dir, skills, accepted, enabled }` is
what the registry knows per installed module. `hyperpane::materialize()` is the
first caller: the built-in `avada/hyperpane` module materialized into the
Hyperpane tab's directory with Claude Code always on.

## Extending (track G9)

An adapter is one `AdapterRow` in `adapters::ADAPTERS`:

```rust
AdapterRow { id, name, status: Implemented | Unimplemented,
             detect: Detect { home_paths: &[&[".kiro"]], binaries: &["kiro"] },
             project: Some(Layout { skills_dir, rules_file, rules_form }), user: Option<Layout> }
```

Paths are segment lists joined with `Path::join`. `rules_form` is
`ImportShared("<line>")` for a tool that reads an import (it gets one shared
block plus only the rules not already in `AGENTS.md`) or `Inline`. Filling a
stub is: give it a `Layout`, flip `status`, add a fixture test in
`skills/tests.rs`. The materializer needs no other change.
