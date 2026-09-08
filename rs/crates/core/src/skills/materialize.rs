//! Turn enabled modules' skill units into a [`Plan`] for the tools present on
//! this machine, then apply it.
//!
//! The materializer never edits a file it did not fence. Every unit takes one of
//! five forms per tool, chosen from the tool's [`Layout`] by [`resolve`]:
//!
//! * a fenced block in a shared rules file (`AGENTS.md`, `CLAUDE.md`,
//!   `CONVENTIONS.md`) for always-on rules, or for glob rules where the tool
//!   has no native glob form (the block then opens with the glob list in prose);
//! * one native rule file per unit in the tool's rules directory
//!   (`.claude/rules/`, `.clinerules/`, `.kiro/steering/`, `.augment/rules/`,
//!   `.continue/rules/`), with the tool's own front matter for the activation;
//! * a skill directory `<owner>-<repo>-<name>/SKILL.md` (+ `scripts/`,
//!   `references/`, `assets/`), with `disable-model-invocation: true` for a
//!   manual unit where the tool honours it;
//! * a workflow file (Cline) for a manual unit;
//! * a recorded skip, with a reason the user can read.
//!
//! Every emitted file or section sits inside the module's fence, which is how a
//! later run tells its own output from the user's.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use avada_module_sdk::caps::Capability;
use avada_module_sdk::descriptor::SchemaDocument;
use avada_module_sdk::manifest::{ModuleId, SkillsSection};
use avada_module_sdk::skills::{fence, splice_fenced, Activation, SkillKind};

use super::adapters::{
    rel, AdapterStatus, Cap, Dialect, Layout, RulesDir, RulesForm, Scope, Tools, ADAPTERS, SHARED,
};
use super::index;
use super::plan::{apply, Applied, FileWrite, Overflow, Plan, Removal, Skipped, Truncated};
use super::unit::{emitted_name, load_units, sibling_files, Unit, UnitRef};

/// Fence id of everything the host itself generates (the `@AGENTS.md` import line
/// and the index skill), as opposed to a module's units.
pub const HOST_ID: &str = "avada/modules";
/// Directory and frontmatter name of the generated index skill.
pub const INDEX_NAME: &str = "avada-modules";
/// Prefix of an opening fence line; a `SKILL.md` containing one is ours.
pub const MARKER: &str = "<!-- avada:module=";
/// The shared rules file every tool following the Agent Skills spec reads.
pub const SHARED_RULES: &[&str] = &["AGENTS.md"];
/// The shared skills directory.
pub const SHARED_SKILLS: &[&str] = &[".agents", "skills"];
/// Bytes held back from a tool's cap for front matter, fences and the notice.
pub const CAP_RESERVE: usize = 2048;

/// The shared layer as a layout, so it flows through the same emitter as a tool.
/// It has no manual form (the Agent Skills spec has no manual-only flag) and no
/// documented cap.
const SHARED_LAYOUT: Layout = Layout {
    skills_dir: Some(SHARED_SKILLS),
    rules_file: Some(SHARED_RULES),
    ..Layout::EMPTY
};

/// One installed module as the materializer sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleInput {
    /// Module id; also the fence id.
    pub id: ModuleId,
    /// Display name, for the index skill.
    pub name: String,
    /// Installed version, for the index skill.
    pub version: String,
    /// The directory `manifest.skills.paths` are relative to.
    pub version_dir: PathBuf,
    /// `[skills]` from the manifest.
    pub skills: SkillsSection,
    /// Capabilities the user accepted at install. Nothing is written unless it
    /// holds [`Capability::SkillsMaterialize`].
    pub accepted: BTreeSet<Capability>,
    /// Enabled in the workspace(s) whose roots are being materialized. Gates the
    /// project scope only; the user scope follows installation, not enablement.
    /// Two workspaces sharing a root pass the union: enabled in either is enabled.
    pub enabled: bool,
}

/// Everything one planning run needs.
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    /// Installed modules.
    pub modules: &'a [ModuleInput],
    /// Project roots (project scope). Duplicates are folded.
    pub roots: &'a [PathBuf],
    /// The home directory for the user scope, or `None` to leave it alone.
    /// Tests pass a scratch directory; the real one is only ever touched by the app.
    pub home: Option<&'a Path>,
    /// Which tools to write for (detected minus disabled; see [`Tools::has`]).
    pub tools: &'a Tools,
    /// The host's `GET /schema`, for the index skill. `None` emits no index.
    pub schema: Option<&'a SchemaDocument>,
    /// Workspace name shown in the index skill.
    pub workspace: Option<&'a str>,
}

/// Plans and applies. Stateless; exists so track H4 has one handle to hold.
#[derive(Debug, Clone, Copy, Default)]
pub struct Materializer;

impl Materializer {
    /// A materializer.
    pub fn new() -> Self {
        Materializer
    }

    /// Compute what should change on disk. Reads the modules' skill sources and
    /// the current state of the target files; writes nothing.
    #[tracing::instrument(level = "debug", skip_all, fields(modules = req.modules.len(), roots = req.roots.len()))]
    pub fn plan(&self, req: &Request<'_>) -> Plan {
        let mut plan = Plan::default();
        let loaded: Vec<Loaded<'_>> = req
            .modules
            .iter()
            .filter(|m| gated(m, Scope::Project) || gated(m, Scope::User))
            .map(|m| {
                let (units, errors) = load_units(&m.id, &m.version_dir, &m.skills.paths);
                plan.errors.extend(errors);
                Loaded { input: m, units }
            })
            .collect();

        let mut desired = Desired::default();
        let roots: BTreeSet<&PathBuf> = req.roots.iter().collect();
        for root in roots {
            plan_project(&mut desired, &mut plan, root, &loaded, req);
        }
        if let Some(home) = req.home {
            plan_user(&mut desired, &mut plan, home, &loaded, req);
        }
        diff(&desired, &mut plan);
        plan.normalize();
        plan
    }

    /// Carry a plan out.
    pub fn apply(&self, plan: &Plan) -> Applied {
        apply(plan)
    }
}

/// The accepted-capability gate. Project scope also needs the module enabled.
pub fn gated(m: &ModuleInput, scope: Scope) -> bool {
    m.accepted.contains(&Capability::SkillsMaterialize) && (scope == Scope::User || m.enabled)
}

/// The text of an emitted `SKILL.md`: minimal frontmatter, then the body inside
/// the module's fence.
pub fn render_skill(module_id: &str, name: &str, description: &str, body: &str) -> String {
    render_skill_with(module_id, name, description, &[], body)
}

/// [`render_skill`] with extra `key: value` front matter lines after the
/// description (Claude Code's `disable-model-invocation: true`, for one).
pub fn render_skill_with(
    module_id: &str,
    name: &str,
    description: &str,
    extra: &[(&str, &str)],
    body: &str,
) -> String {
    let (open, close) = fence(module_id);
    let mut fm = format!("name: {name}\ndescription: {}\n", yaml_str(description));
    for (k, v) in extra {
        fm.push_str(&format!("{k}: {v}\n"));
    }
    format!(
        "---\n{fm}---\n{open}\n{}\n{close}\n",
        body.trim_end_matches(['\r', '\n'])
    )
}

/// Does this text carry one of our fences?
pub fn is_ours(text: &str) -> bool {
    text.lines().any(|l| l.trim_start().starts_with(MARKER))
}

/// Cut `body` down to at most `max` bytes at the last blank line before the
/// limit (failing that the last line break, failing that a char boundary).
/// `None` when it already fits.
pub fn truncate_at_paragraph(body: &str, max: usize) -> Option<String> {
    if body.len() <= max {
        return None;
    }
    let mut cut = max;
    while !body.is_char_boundary(cut) {
        cut -= 1;
    }
    let head = &body[..cut];
    let at = head
        .rfind("\n\n")
        .or_else(|| head.rfind('\n'))
        .unwrap_or(cut);
    Some(body[..at].trim_end().to_string())
}

/// A YAML double-quoted scalar.
fn yaml_str(s: &str) -> String {
    let inner = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace(['\r', '\n'], " ");
    format!("\"{inner}\"")
}

fn yaml_list(key: &str, items: &[String]) -> String {
    let mut out = format!("{key}:\n");
    for i in items {
        out.push_str(&format!("  - {}\n", yaml_str(i)));
    }
    out
}

struct Loaded<'a> {
    input: &'a ModuleInput,
    units: Vec<Unit>,
}

struct DesiredDir {
    files: BTreeMap<PathBuf, (Vec<u8>, bool)>,
    unit: Option<UnitRef>,
    tool: String,
}

struct DesiredFile {
    text: String,
    unit: UnitRef,
    tool: String,
}

#[derive(Default)]
struct Desired {
    /// Fenced file → ordered (fence id, block).
    fenced: BTreeMap<PathBuf, Vec<(String, String)>>,
    /// Skill directory → its files.
    dirs: BTreeMap<PathBuf, DesiredDir>,
    /// Native rule or workflow file → its whole text.
    files: BTreeMap<PathBuf, DesiredFile>,
    /// Skills directories whose `<module>-<name>/` children we own.
    sweep_dirs: BTreeSet<PathBuf>,
    /// Fenced files whose stale fences we remove.
    sweep_files: BTreeSet<PathBuf>,
    /// Fenced rules file → the cap the tool applies to the whole file, and the
    /// tool's id. Only the accumulating rules files appear here; a per-unit file
    /// is bounded by [`body_for`] on its way in.
    caps: BTreeMap<PathBuf, (Cap, String)>,
    /// Rules and workflow directories whose fenced `*.md` files we own.
    sweep_file_dirs: BTreeSet<PathBuf>,
}

/// What one unit becomes for one tool.
enum Form {
    /// Fenced into the rules file.
    Block,
    /// Fenced into the rules file with the glob list in prose.
    GlobBlock,
    /// One native file in the rules directory with this activation's front matter.
    RuleFile(Activation),
    /// A skill directory; `manual` adds `disable-model-invocation: true`.
    SkillDir { manual: bool },
    /// A workflow file in the manual directory.
    Workflow,
    /// Not written, and why.
    Skip(String),
}

/// Can this rules directory's front matter express the activation?
fn supports(r: RulesDir, a: Activation) -> bool {
    if r.always_only {
        return a == Activation::Always;
    }
    match r.dialect {
        Dialect::ClaudeCode | Dialect::Cline => {
            matches!(a, Activation::Always | Activation::Glob)
        }
        Dialect::Kiro | Dialect::Augment | Dialect::Continue => true,
    }
}

/// Pick the form a unit takes in a layout. Pure: the same unit and layout
/// always resolve the same way, which is what the fixture tests pin down.
fn resolve(u: &Unit, layout: &Layout, tool: &str) -> Form {
    let fm = &u.skill.frontmatter;
    let rules_dir_for = |a: Activation| layout.rules_dir.filter(|r| supports(*r, a));
    let always_only = layout.rules_dir.is_some_and(|r| r.always_only)
        && layout.rules_file.is_none()
        && layout.skills_dir.is_none();
    match (fm.kind, fm.activation) {
        (SkillKind::Rule, Activation::Model) => {
            Form::Skip("a rule the model activates is a skill; set kind: skill".into())
        }
        (SkillKind::Skill, Activation::Always) => {
            Form::Skip("a skill that is always on is a rule; set kind: rule".into())
        }
        (SkillKind::Rule, Activation::Always) => {
            if layout.rules_file.is_some() {
                Form::Block
            } else if rules_dir_for(Activation::Always).is_some() {
                Form::RuleFile(Activation::Always)
            } else {
                Form::Skip("this tool has no rules file".into())
            }
        }
        (_, Activation::Glob) => {
            if rules_dir_for(Activation::Glob).is_some() {
                Form::RuleFile(Activation::Glob)
            } else if layout.rules_file.is_some() {
                Form::GlobBlock
            } else if always_only {
                Form::Skip(
                    "user-level rules are always-on only here; front matter is ignored".into(),
                )
            } else {
                Form::Skip("this tool has no glob form".into())
            }
        }
        (_, Activation::Manual) => {
            if layout.manual_dir.is_some() {
                Form::Workflow
            } else if layout.skills_dir.is_some() && layout.manual_skills {
                Form::SkillDir { manual: true }
            } else if rules_dir_for(Activation::Manual).is_some() {
                Form::RuleFile(Activation::Manual)
            } else if tool == SHARED {
                Form::Skip(
                    "the shared Agent Skills layer has no manual-only flag; manual units are \
                     written per tool"
                        .into(),
                )
            } else if always_only {
                Form::Skip(
                    "user-level rules are always-on only here; front matter is ignored".into(),
                )
            } else {
                Form::Skip("this tool has no manual form".into())
            }
        }
        (SkillKind::Skill, Activation::Model) => {
            if layout.skills_dir.is_some() {
                Form::SkillDir { manual: false }
            } else if rules_dir_for(Activation::Model).is_some() {
                Form::RuleFile(Activation::Model)
            } else if always_only {
                Form::Skip(
                    "user-level rules are always-on only here; front matter is ignored".into(),
                )
            } else {
                Form::Skip("this tool has no on-demand skills; only rules are written".into())
            }
        }
    }
}

fn allows(u: &Unit, tool: &str) -> bool {
    let t = &u.skill.frontmatter.tools;
    t.is_empty() || t.iter().any(|x| x == tool)
}

/// The units in force for one tool: base units whose allow-list admits it, with
/// `skills/<tool>/<name>/` overrides winning by name. Sorted by name.
fn effective<'u>(units: &'u [Unit], tool: &str) -> Vec<&'u Unit> {
    let mut by_name: BTreeMap<&str, &Unit> = BTreeMap::new();
    for u in units
        .iter()
        .filter(|u| u.reference.tool.is_none() && allows(u, tool))
    {
        by_name.insert(&u.reference.name, u);
    }
    for u in units
        .iter()
        .filter(|u| u.reference.tool.as_deref() == Some(tool))
    {
        by_name.insert(&u.reference.name, u);
    }
    by_name.into_values().collect()
}

fn skip(plan: &mut Plan, u: &Unit, tool: &str, reason: impl Into<String>) {
    plan.skipped.push(Skipped {
        unit: u.reference.clone(),
        tool: tool.to_string(),
        reason: reason.into(),
    });
}

/// The unit's body as written for a layout: trimmed, and cut to the tool's cap
/// with a notice when it does not fit. Truncation is recorded in the plan.
fn body_for(plan: &mut Plan, u: &Unit, tool: &str, layout: &Layout) -> String {
    let body = u.skill.body.trim();
    let Some(cap) = layout.cap else {
        return body.to_string();
    };
    let max = cap.bytes.saturating_sub(CAP_RESERVE);
    let Some(head) = truncate_at_paragraph(body, max) else {
        return body.to_string();
    };
    plan.truncated.push(Truncated {
        unit: u.reference.clone(),
        tool: tool.to_string(),
        bytes: body.len(),
        cap: cap.bytes,
    });
    format!(
        "{head}\n\n> Avada truncated this unit at a paragraph boundary: it is {} bytes and {tool} \
         reads at most {} bytes ({}). The full text is in {}.",
        body.len(),
        cap.bytes,
        cap.source,
        u.reference.source.display()
    )
}

fn glob_prose(globs: &[String]) -> String {
    globs
        .iter()
        .map(|g| format!("`{g}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The front matter of a native rule file in a dialect, for an activation.
fn rule_frontmatter(dialect: Dialect, a: Activation, u: &Unit, name: &str) -> String {
    let fm = &u.skill.frontmatter;
    let desc = fm.description.as_str();
    let lines = match (dialect, a) {
        (Dialect::ClaudeCode | Dialect::Cline, Activation::Glob) => yaml_list("paths", &fm.globs),
        (Dialect::ClaudeCode | Dialect::Cline, _) => String::new(),
        (Dialect::Kiro, Activation::Always) => "inclusion: always\n".into(),
        (Dialect::Kiro, Activation::Glob) => {
            format!(
                "inclusion: fileMatch\n{}",
                yaml_list("fileMatchPattern", &fm.globs)
            )
        }
        (Dialect::Kiro, Activation::Manual) => "inclusion: manual\n".into(),
        (Dialect::Kiro, Activation::Model) => format!(
            "inclusion: auto\nname: {}\ndescription: {}\n",
            yaml_str(name),
            yaml_str(desc)
        ),
        (Dialect::Augment, Activation::Always) => "type: always_apply\n".into(),
        (Dialect::Augment, Activation::Glob) => format!(
            "type: agent_requested\ndescription: {}\n",
            yaml_str(&format!(
                "{desc} Applies to files matching {}.",
                fm.globs.join(", ")
            ))
        ),
        (Dialect::Augment, Activation::Manual) => "type: manual\n".into(),
        (Dialect::Augment, Activation::Model) => {
            format!("type: agent_requested\ndescription: {}\n", yaml_str(desc))
        }
        (Dialect::Continue, Activation::Always) => {
            format!("name: {}\nalwaysApply: true\n", yaml_str(name))
        }
        (Dialect::Continue, Activation::Glob) => {
            format!(
                "name: {}\n{}",
                yaml_str(name),
                yaml_list("globs", &fm.globs)
            )
        }
        (Dialect::Continue, Activation::Manual) => format!(
            "name: {}\nalwaysApply: false\ndescription: {}\n",
            yaml_str(name),
            yaml_str(&format!("Only when the user asks for it by name: {desc}"))
        ),
        (Dialect::Continue, Activation::Model) => format!(
            "name: {}\nalwaysApply: false\ndescription: {}\n",
            yaml_str(name),
            yaml_str(desc)
        ),
    };
    if lines.is_empty() {
        String::new()
    } else {
        format!("---\n{lines}---\n")
    }
}

fn fenced(module: &ModuleId, body: &str) -> String {
    let (open, close) = fence(module.as_str());
    format!("{open}\n{body}\n{close}\n")
}

fn skill_files(
    u: &Unit,
    module: &ModuleId,
    manual: bool,
    body: &str,
) -> BTreeMap<PathBuf, (Vec<u8>, bool)> {
    let name = emitted_name(module, &u.reference.name);
    let extra: &[(&str, &str)] = if manual {
        &[("disable-model-invocation", "true")]
    } else {
        &[]
    };
    let mut files = BTreeMap::new();
    files.insert(
        PathBuf::from("SKILL.md"),
        (
            render_skill_with(
                module.as_str(),
                &name,
                &u.skill.frontmatter.description,
                extra,
                body,
            )
            .into_bytes(),
            false,
        ),
    );
    for (rel_path, bytes, exec) in sibling_files(&u.dir) {
        files.insert(rel_path, (bytes, exec));
    }
    files
}

/// Emit one module's effective units for one tool under `base` with `layout`.
/// Returns the blocks for the rules file. `covered` are the units already carried
/// by the shared layer, which an importing tool must not repeat.
fn emit(
    d: &mut Desired,
    plan: &mut Plan,
    m: &Loaded<'_>,
    tool: &str,
    base: &Path,
    layout: &Layout,
    covered: Option<&BTreeSet<UnitRef>>,
) -> Vec<String> {
    let mut rules = Vec::new();
    let module = &m.input.id;
    for u in effective(&m.units, tool) {
        let form = resolve(u, layout, tool);
        let is_covered = covered.is_some_and(|c| c.contains(&u.reference));
        match form {
            Form::Block | Form::GlobBlock if is_covered => {}
            Form::Block => rules.push(body_for(plan, u, tool, layout)),
            Form::GlobBlock => {
                let body = body_for(plan, u, tool, layout);
                rules.push(format!(
                    "Applies only when working on files matching {}:\n\n{body}",
                    glob_prose(&u.skill.frontmatter.globs)
                ));
            }
            Form::RuleFile(a) => {
                let Some(r) = layout.rules_dir else { continue };
                let name = emitted_name(module, &u.reference.name);
                let body = body_for(plan, u, tool, layout);
                let text = format!(
                    "{}{}",
                    rule_frontmatter(r.dialect, a, u, &name),
                    fenced(module, &body)
                );
                d.files.insert(
                    rel(base, r.path).join(format!("{name}.md")),
                    DesiredFile {
                        text,
                        unit: u.reference.clone(),
                        tool: tool.to_string(),
                    },
                );
            }
            Form::SkillDir { manual } => {
                let Some(dir) = layout.skills_dir else {
                    continue;
                };
                let name = emitted_name(module, &u.reference.name);
                let body = body_for(plan, u, tool, layout);
                d.dirs.insert(
                    rel(base, dir).join(&name),
                    DesiredDir {
                        files: skill_files(u, module, manual, &body),
                        unit: Some(u.reference.clone()),
                        tool: tool.to_string(),
                    },
                );
            }
            Form::Workflow => {
                let Some(dir) = layout.manual_dir else {
                    continue;
                };
                let name = emitted_name(module, &u.reference.name);
                let body = body_for(plan, u, tool, layout);
                d.files.insert(
                    rel(base, dir).join(format!("{name}.md")),
                    DesiredFile {
                        text: fenced(module, &body),
                        unit: u.reference.clone(),
                        tool: tool.to_string(),
                    },
                );
            }
            Form::Skip(reason) => skip(plan, u, tool, reason),
        }
    }
    rules
}

fn push_block(d: &mut Desired, file: PathBuf, id: &str, block: String) {
    d.fenced
        .entry(file)
        .or_default()
        .push((id.to_string(), block));
}

/// One tool at one scope: sweep, then write for every active module.
#[allow(clippy::too_many_arguments)]
fn plan_tool(
    d: &mut Desired,
    plan: &mut Plan,
    base: &Path,
    scope: Scope,
    active: &[&Loaded<'_>],
    req: &Request<'_>,
    covered: &BTreeMap<&ModuleId, BTreeSet<UnitRef>>,
    shared_has_rules: bool,
) {
    for row in ADAPTERS {
        let Some(layout) = row.layout(scope) else {
            continue;
        };
        let layout = layout.resolve(base);
        if row.status == AdapterStatus::Implemented {
            sweep(d, base, &layout);
        }
        if !req.tools.has(row.id) {
            continue;
        }
        if row.status == AdapterStatus::Unimplemented {
            for m in active {
                for u in effective(&m.units, row.id) {
                    skip(
                        plan,
                        u,
                        row.id,
                        format!("{} adapter is not implemented yet", row.name),
                    );
                }
            }
            continue;
        }
        if let (Some(file), Some(cap)) = (layout.rules_file, layout.cap) {
            d.caps.insert(rel(base, file), (cap, row.id.to_string()));
        }
        if let (RulesForm::ImportShared(line), Some(rules_file), true) =
            (layout.rules_form, layout.rules_file, shared_has_rules)
        {
            push_block(d, rel(base, rules_file), HOST_ID, line.to_string());
        }
        for m in active {
            let cov = match layout.rules_form {
                RulesForm::ImportShared(_) => covered.get(&m.input.id),
                RulesForm::Inline => None,
            };
            let rules = emit(d, plan, m, row.id, base, &layout, cov);
            if let (false, Some(file)) = (rules.is_empty(), layout.rules_file) {
                push_block(d, rel(base, file), m.input.id.as_str(), rules.join("\n\n"));
            }
        }
    }
}

fn plan_project(
    d: &mut Desired,
    plan: &mut Plan,
    root: &Path,
    loaded: &[Loaded<'_>],
    req: &Request<'_>,
) {
    let shared_rules = rel(root, SHARED_RULES);
    let shared_skills = rel(root, SHARED_SKILLS);
    sweep(d, root, &SHARED_LAYOUT);

    let active: Vec<&Loaded<'_>> = loaded
        .iter()
        .filter(|m| gated(m.input, Scope::Project))
        .collect();

    // Shared layer first.
    let mut shared_has_rules = false;
    let mut covered: BTreeMap<&ModuleId, BTreeSet<UnitRef>> = BTreeMap::new();
    for m in &active {
        covered.insert(
            &m.input.id,
            effective(&m.units, SHARED)
                .into_iter()
                .filter(|u| {
                    matches!(
                        resolve(u, &SHARED_LAYOUT, SHARED),
                        Form::Block | Form::GlobBlock
                    )
                })
                .map(|u| u.reference.clone())
                .collect(),
        );
        let rules = emit(d, plan, m, SHARED, root, &SHARED_LAYOUT, None);
        if !rules.is_empty() {
            shared_has_rules = true;
            push_block(
                d,
                shared_rules.clone(),
                m.input.id.as_str(),
                rules.join("\n\n"),
            );
        }
    }

    plan_tool(
        d,
        plan,
        root,
        Scope::Project,
        &active,
        req,
        &covered,
        shared_has_rules,
    );

    // The index skill.
    if let Some(schema) = req.schema {
        let text = index::render(schema, req.modules, req.workspace);
        let mut targets = vec![shared_skills.join(INDEX_NAME)];
        for row in ADAPTERS
            .iter()
            .filter(|r| r.status == AdapterStatus::Implemented && req.tools.has(r.id))
        {
            if let Some(dir) = row.layout(Scope::Project).and_then(|l| l.skills_dir) {
                targets.push(rel(root, dir).join(INDEX_NAME));
            }
        }
        for t in targets {
            let mut files = BTreeMap::new();
            files.insert(
                PathBuf::from("SKILL.md"),
                (text.clone().into_bytes(), false),
            );
            d.dirs.insert(
                t,
                DesiredDir {
                    files,
                    unit: None,
                    tool: HOST_ID.to_string(),
                },
            );
        }
    }
}

fn plan_user(
    d: &mut Desired,
    plan: &mut Plan,
    home: &Path,
    loaded: &[Loaded<'_>],
    req: &Request<'_>,
) {
    let active: Vec<&Loaded<'_>> = loaded
        .iter()
        .filter(|m| gated(m.input, Scope::User))
        .collect();
    // No shared layer at user scope: every rule is inlined, nothing is covered.
    plan_tool(
        d,
        plan,
        home,
        Scope::User,
        &active,
        req,
        &BTreeMap::new(),
        false,
    );
}

fn sweep(d: &mut Desired, base: &Path, layout: &Layout) {
    if let Some(s) = layout.skills_dir {
        d.sweep_dirs.insert(rel(base, s));
    }
    if let Some(f) = layout.rules_file {
        d.sweep_files.insert(rel(base, f));
    }
    if let Some(r) = layout.rules_dir {
        d.sweep_file_dirs.insert(rel(base, r.path));
    }
    if let Some(m) = layout.manual_dir {
        d.sweep_file_dirs.insert(rel(base, m));
    }
}

/// Fence ids opened in a file, in order.
fn fence_ids(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim_end_matches('\r');
            l.strip_prefix(MARKER)
                .and_then(|r| r.strip_suffix(" -->"))
                .map(str::to_string)
        })
        .collect()
}

/// Splice `keep` into `current`, dropping every fence of ours that `keep` does
/// not carry. The result is the whole final file, the user's own bytes included.
fn assemble(current: &str, keep: &[(String, String)]) -> String {
    let ids: BTreeSet<&str> = keep.iter().map(|(id, _)| id.as_str()).collect();
    let mut next = current.to_string();
    for id in fence_ids(current) {
        if !ids.contains(id.as_str()) {
            next = splice_fenced(&next, &id, None);
        }
    }
    for (id, block) in keep {
        next = splice_fenced(&next, id, Some(block));
    }
    next
}

/// Assemble `keep` into `current`, dropping whole module blocks from the end
/// until the result fits `cap` bytes. Returns the final text and the ids
/// dropped, in the order they stood in the file.
///
/// The host's own fence (the `@AGENTS.md` import line) is never dropped: it is a
/// single line, and dropping it would take the whole shared layer with it. Once
/// every module block is gone the loop stops even if the file is still over cap,
/// because what is left is the user's own bytes and they are not ours to cut.
pub(crate) fn fit_cap(
    current: &str,
    keep: &mut Vec<(String, String)>,
    cap: usize,
) -> (String, Vec<String>) {
    let mut next = assemble(current, keep);
    let mut dropped: Vec<String> = Vec::new();
    while next.len() > cap {
        let Some(at) = keep.iter().rposition(|(id, _)| id != HOST_ID) else {
            break;
        };
        dropped.push(keep.remove(at).0);
        next = assemble(current, keep);
    }
    dropped.reverse();
    (next, dropped)
}

/// Compare the desired state with the disk and fill the plan's writes and removals.
fn diff(d: &Desired, plan: &mut Plan) {
    // Fenced files: splice against the current text, write only when it changes,
    // remove the file when nothing but whitespace would remain.
    let files: BTreeSet<&PathBuf> = d.sweep_files.iter().chain(d.fenced.keys()).collect();
    for file in files {
        let exists = file.is_file();
        let current = if exists {
            fs::read_to_string(file).unwrap_or_default()
        } else {
            String::new()
        };
        let wanted: &[(String, String)] = d.fenced.get(file).map(Vec::as_slice).unwrap_or(&[]);
        let mut keep = wanted.to_vec();
        let mut next = assemble(&current, &keep);
        // The cap is the tool's limit on the *file*, and an over-cap file is
        // skipped whole rather than read in part, so a file that grew past it
        // would take every module's rules down with it. Drop whole blocks from
        // the end until it fits, newest module first, keeping the host's own
        // import line and never touching the user's own bytes.
        if let Some((cap, tool)) = d.caps.get(file) {
            let assembled = next.len();
            let dropped;
            (next, dropped) = fit_cap(&current, &mut keep, cap.bytes);
            if !dropped.is_empty() {
                plan.overflowed.push(Overflow {
                    path: file.clone(),
                    tool: tool.clone(),
                    bytes: assembled,
                    cap: cap.bytes,
                    dropped,
                });
            }
        }
        if next == current {
            continue;
        }
        if next.trim().is_empty() {
            if exists {
                plan.removals.push(Removal {
                    path: file.clone(),
                    dir: false,
                });
            }
        } else {
            plan.writes.push(FileWrite {
                path: file.clone(),
                bytes: next.into_bytes(),
                executable: false,
            });
        }
    }

    // Native rule and workflow files: a fenced `*.md` we no longer want goes; a
    // wanted path holding a file without our fence is left alone and reported.
    let mut file_dirs: BTreeSet<PathBuf> = d.sweep_file_dirs.clone();
    file_dirs.extend(
        d.files
            .keys()
            .filter_map(|p| p.parent().map(Path::to_path_buf)),
    );
    for dir in &file_dirs {
        let Ok(rd) = fs::read_dir(dir) else {
            continue;
        };
        for entry in rd.filter_map(Result::ok) {
            let p = entry.path();
            if !p.is_file() || p.extension().is_none_or(|e| e != "md") || d.files.contains_key(&p) {
                continue;
            }
            if fs::read_to_string(&p).is_ok_and(|t| is_ours(&t)) {
                plan.removals.push(Removal {
                    path: p,
                    dir: false,
                });
            }
        }
    }
    for (path, want) in &d.files {
        match fs::read_to_string(path) {
            Ok(existing) if existing == want.text => continue,
            Ok(existing) if !is_ours(&existing) => {
                plan.skipped.push(Skipped {
                    unit: want.unit.clone(),
                    tool: want.tool.clone(),
                    reason: format!(
                        "{} exists and was not written by Avada; left alone",
                        path.display()
                    ),
                });
                continue;
            }
            _ => {}
        }
        plan.writes.push(FileWrite {
            path: path.clone(),
            bytes: want.text.clone().into_bytes(),
            executable: false,
        });
    }

    // Skill directories: ours and unwanted go; wanted ones are brought to exactly
    // the desired file set; a directory whose SKILL.md we did not write is left alone.
    let mut parents: BTreeSet<PathBuf> = d.sweep_dirs.clone();
    parents.extend(
        d.dirs
            .keys()
            .filter_map(|p| p.parent().map(Path::to_path_buf)),
    );
    for parent in &parents {
        let Ok(rd) = fs::read_dir(parent) else {
            continue;
        };
        for entry in rd.filter_map(Result::ok) {
            let sub = entry.path();
            if !sub.is_dir() || d.dirs.contains_key(&sub) {
                continue;
            }
            let ours = fs::read_to_string(sub.join("SKILL.md")).is_ok_and(|t| is_ours(&t));
            if ours {
                plan.removals.push(Removal {
                    path: sub,
                    dir: true,
                });
            }
        }
    }
    for (dir, want) in &d.dirs {
        let skill_md = dir.join("SKILL.md");
        if let Ok(existing) = fs::read_to_string(&skill_md) {
            if !is_ours(&existing) {
                if let Some(unit) = &want.unit {
                    plan.skipped.push(Skipped {
                        unit: unit.clone(),
                        tool: want.tool.clone(),
                        reason: format!(
                            "{} exists and was not written by Avada; left alone",
                            dir.display()
                        ),
                    });
                }
                continue;
            }
        }
        for (rel_path, (bytes, exec)) in &want.files {
            let path = dir.join(rel_path);
            if fs::read(&path).ok().as_deref() != Some(bytes.as_slice()) {
                plan.writes.push(FileWrite {
                    path,
                    bytes: bytes.clone(),
                    executable: *exec,
                });
            }
        }
        let mut on_disk = Vec::new();
        collect_files(dir, Path::new(""), &mut on_disk);
        for rel_path in on_disk {
            if !want.files.contains_key(&rel_path) {
                plan.removals.push(Removal {
                    path: dir.join(rel_path),
                    dir: false,
                });
            }
        }
    }
}

fn collect_files(abs: &Path, rel_path: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(abs) else { return };
    for entry in rd.filter_map(Result::ok) {
        let p = entry.path();
        let r = rel_path.join(entry.file_name());
        if p.is_dir() {
            collect_files(&p, &r, out);
        } else {
            out.push(r);
        }
    }
}
