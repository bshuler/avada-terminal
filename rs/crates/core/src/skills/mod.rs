//! Skills: materializing modules' `SKILL.md` units into the files AI coding tools
//! read, in the projects a workspace runs in and in the user's home.
//!
//! Pipeline: [`Materializer::plan`] reads the enabled modules' skill sources and
//! the current target files and returns a [`Plan`] (sorted writes, removals,
//! skips, per-unit errors; nothing touched), then [`Materializer::apply`] carries
//! it out. A second plan over an applied tree is empty.
//!
//! Layout written (project scope, per root; `<m>` is `<owner>-<repo>`):
//!
//! ```text
//! AGENTS.md                       one fenced block per module: always-on rules, plus
//!                                 glob rules in prose ("Applies only when working on…")
//! .agents/skills/<m>-<name>/      one dir per on-demand skill (+ scripts/, references/)
//! .agents/skills/avada-modules/   generated index: installed modules + CLI verbs
//! CLAUDE.md                       fenced `@AGENTS.md` import
//! .claude/rules/<m>-<name>.md     Claude Code: glob rules with `paths:` front matter
//! .claude/skills/<m>-<name>/      Claude Code copy of each skill (+ the index);
//!                                 manual units get `disable-model-invocation: true`
//! CONVENTIONS.md                  Aider: rules inlined (Aider cannot import)
//! .clinerules/<m>-<name>.md       Cline rule files (`paths:` for globs)
//! .clinerules/workflows/<m>-<name>.md  Cline workflows for manual units
//! .kiro/steering/<m>-<name>.md    Kiro steering files (`inclusion:` per activation)
//! .augment/rules/<m>-<name>.md    Augment rules (`type:` per activation)
//! .continue/rules/<m>-<name>.md   Continue rules (`globs:` / `alwaysApply:`)
//! ```
//!
//! User scope (`<home>`): the same per-tool trees under the tool's home directory
//! (`.claude/`, `Documents/Cline/{Rules,Workflows}`, `.kiro/steering`,
//! `.augment/rules`, `.continue/rules`), with always-on rules in `.claude/CLAUDE.md`.
//! Every emitted file or section sits inside `<!-- avada:module=<id> -->` …
//! `<!-- /avada:module=<id> -->`; regeneration replaces only fences, fenced rule
//! files and directories whose `SKILL.md` carries one, and disable/uninstall
//! removes exactly those. Text outside a fence, and files we did not write, are
//! never rewritten.
//!
//! Which tools are written is data ([`ADAPTERS`]) filtered by [`Tools`], which
//! detects tools from an injectable home and `PATH` and honours a per-tool
//! disable set. Units larger than a tool's documented byte cap ([`Cap`]) are cut
//! at a paragraph boundary with a fenced notice and reported in
//! [`Plan::truncated`]. See `docs/skills.md`.

pub mod adapters;
pub mod index;
pub mod materialize;
pub mod plan;
pub mod unit;

#[cfg(test)]
mod tests;

pub use adapters::{
    adapter, known_tool_ids, rel, AdapterRow, AdapterStatus, Cap, Detect, Dialect, Layout,
    RulesDir, RulesForm, Scope, Tools, ADAPTERS, SHARED,
};
pub use materialize::{
    gated, is_ours, render_skill, render_skill_with, truncate_at_paragraph, Materializer,
    ModuleInput, Request, CAP_RESERVE, HOST_ID, INDEX_NAME, MARKER, SHARED_RULES, SHARED_SKILLS,
};
pub use plan::{apply, Applied, FileWrite, Overflow, Plan, Removal, Skipped, Truncated};
pub use unit::{
    emitted_name, is_kebab, load_units, validate, Unit, UnitError, UnitRef, MAX_DESCRIPTION,
    MAX_NAME, SIBLING_DIRS,
};
