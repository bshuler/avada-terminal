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
//! AGENTS.md                       one fenced block per module: its always-on rules
//! .agents/skills/<m>-<name>/      one dir per on-demand skill (+ scripts/, references/)
//! .agents/skills/avada-modules/   generated index: installed modules + CLI verbs
//! CLAUDE.md                       fenced `@AGENTS.md` import (+ Claude-only rules)
//! .claude/skills/<m>-<name>/      Claude Code copy of each skill (+ the index)
//! CONVENTIONS.md                  Aider: rules inlined (Aider cannot import)
//! ```
//!
//! User scope (`<home>`): `.claude/skills/<m>-<name>/` and a fenced block in
//! `.claude/CLAUDE.md`. Every emitted file or section sits inside
//! `<!-- avada:module=<id> -->` … `<!-- /avada:module=<id> -->`; regeneration
//! replaces only fences and directories whose `SKILL.md` carries one, and
//! disable/uninstall removes exactly those. Text outside a fence is never rewritten.
//!
//! Which tools are written is data ([`ADAPTERS`]) filtered by [`Tools`]; the rows
//! marked unimplemented are track G9's to fill. See `docs/skills.md`.

pub mod adapters;
pub mod index;
pub mod materialize;
pub mod plan;
pub mod unit;

#[cfg(test)]
mod tests;

pub use adapters::{
    adapter, known_tool_ids, rel, AdapterRow, AdapterStatus, Detect, Layout, RulesForm, Scope,
    Tools, ADAPTERS, SHARED,
};
pub use materialize::{
    gated, is_ours, render_skill, Materializer, ModuleInput, Request, HOST_ID, INDEX_NAME, MARKER,
    SHARED_RULES, SHARED_SKILLS,
};
pub use plan::{apply, Applied, FileWrite, Plan, Removal, Skipped};
pub use unit::{
    emitted_name, is_kebab, load_units, validate, Unit, UnitError, UnitRef, MAX_DESCRIPTION,
    MAX_NAME, SIBLING_DIRS,
};
