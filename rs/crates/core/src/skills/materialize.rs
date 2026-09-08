//! Turn enabled modules' skill units into a [`Plan`] for the tools present on
//! this machine, then apply it.
//!
//! The materializer never edits a file it did not fence. Rules go into fenced
//! blocks of shared files (`AGENTS.md`, `CLAUDE.md`, `CONVENTIONS.md`); skills go
//! into directories named `<owner>-<repo>-<name>` whose `SKILL.md` carries the same
//! fence, which is how a later run tells its own directories from the user's.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use avada_module_sdk::caps::Capability;
use avada_module_sdk::descriptor::SchemaDocument;
use avada_module_sdk::manifest::{ModuleId, SkillsSection};
use avada_module_sdk::skills::{fence, splice_fenced, Activation, SkillKind};

use super::adapters::{rel, AdapterStatus, Layout, RulesForm, Scope, Tools, ADAPTERS, SHARED};
use super::index;
use super::plan::{apply, Applied, FileWrite, Plan, Removal, Skipped};
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
    /// Which tools to write for.
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
    let (open, close) = fence(module_id);
    let desc = description
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace(['\r', '\n'], " ");
    format!(
        "---\nname: {name}\ndescription: \"{desc}\"\n---\n{open}\n{}\n{close}\n",
        body.trim_end_matches(['\r', '\n'])
    )
}

/// Does this `SKILL.md` text carry one of our fences?
pub fn is_ours(text: &str) -> bool {
    text.lines().any(|l| l.trim_start().starts_with(MARKER))
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

#[derive(Default)]
struct Desired {
    /// Fenced file → ordered (fence id, block).
    fenced: BTreeMap<PathBuf, Vec<(String, String)>>,
    /// Skill directory → its files.
    dirs: BTreeMap<PathBuf, DesiredDir>,
    /// Skills directories whose `<module>-<name>/` children we own.
    sweep_dirs: BTreeSet<PathBuf>,
    /// Fenced files whose stale fences we remove.
    sweep_files: BTreeSet<PathBuf>,
}

enum Class {
    Rule,
    Skill,
    Skip(&'static str),
}

fn classify(u: &Unit) -> Class {
    let fm = &u.skill.frontmatter;
    match (fm.kind, fm.activation) {
        (SkillKind::Rule, Activation::Always) => Class::Rule,
        (SkillKind::Rule, Activation::Glob) => {
            Class::Skip("glob-activated rules are not emitted yet (track G9)")
        }
        (SkillKind::Rule, Activation::Manual) => {
            Class::Skip("manual rules have no home in any tool yet")
        }
        (SkillKind::Rule, Activation::Model) => {
            Class::Skip("a rule the model activates is a skill; set kind: skill")
        }
        (SkillKind::Skill, Activation::Model) => Class::Skill,
        (SkillKind::Skill, Activation::Always) => {
            Class::Skip("a skill that is always on is a rule; set kind: rule")
        }
        (SkillKind::Skill, Activation::Glob) => {
            Class::Skip("glob-activated skills are not emitted yet (track G9)")
        }
        (SkillKind::Skill, Activation::Manual) => {
            Class::Skip("manual skills are not emitted yet (track G9)")
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

fn skill_files(u: &Unit, module: &ModuleId) -> BTreeMap<PathBuf, (Vec<u8>, bool)> {
    let name = emitted_name(module, &u.reference.name);
    let mut files = BTreeMap::new();
    files.insert(
        PathBuf::from("SKILL.md"),
        (
            render_skill(
                module.as_str(),
                &name,
                &u.skill.frontmatter.description,
                &u.skill.body,
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

/// Emit one module's effective units for one tool into a skills dir and/or a
/// rules block. `covered` are the units already carried by the shared layer,
/// which an importing tool must not repeat.
fn emit(
    d: &mut Desired,
    plan: &mut Plan,
    m: &Loaded<'_>,
    tool: &str,
    skills_dir: Option<&Path>,
    covered: Option<&BTreeSet<UnitRef>>,
) -> Vec<String> {
    let mut rules = Vec::new();
    for u in effective(&m.units, tool) {
        match classify(u) {
            Class::Rule => {
                if covered.is_some_and(|c| c.contains(&u.reference)) {
                    continue;
                }
                rules.push(u.skill.body.trim().to_string());
            }
            Class::Skill => match skills_dir {
                Some(dir) => {
                    let name = emitted_name(&m.input.id, &u.reference.name);
                    d.dirs.insert(
                        dir.join(&name),
                        DesiredDir {
                            files: skill_files(u, &m.input.id),
                            unit: Some(u.reference.clone()),
                            tool: tool.to_string(),
                        },
                    );
                }
                None => skip(
                    plan,
                    u,
                    tool,
                    "this tool has no on-demand skills; only rules are written",
                ),
            },
            Class::Skip(reason) => skip(plan, u, tool, reason),
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

fn plan_project(
    d: &mut Desired,
    plan: &mut Plan,
    root: &Path,
    loaded: &[Loaded<'_>],
    req: &Request<'_>,
) {
    let shared_rules = rel(root, SHARED_RULES);
    let shared_skills = rel(root, SHARED_SKILLS);
    d.sweep_files.insert(shared_rules.clone());
    d.sweep_dirs.insert(shared_skills.clone());

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
                .filter(|u| matches!(classify(u), Class::Rule))
                .map(|u| u.reference.clone())
                .collect(),
        );
        let rules = emit(d, plan, m, SHARED, Some(&shared_skills), None);
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

    // Then each tool.
    for row in ADAPTERS {
        let Some(layout) = row.layout(Scope::Project) else {
            continue;
        };
        if row.status == AdapterStatus::Implemented {
            sweep(d, root, &layout);
        }
        if !req.tools.has(row.id) {
            continue;
        }
        if row.status == AdapterStatus::Unimplemented {
            for m in &active {
                for u in effective(&m.units, row.id) {
                    skip(
                        plan,
                        u,
                        row.id,
                        format!("{} adapter is not implemented yet (track G9)", row.name),
                    );
                }
            }
            continue;
        }
        let skills_dir = layout.skills_dir.map(|s| rel(root, s));
        if let (RulesForm::ImportShared(line), Some(rules_file), true) =
            (layout.rules_form, layout.rules_file, shared_has_rules)
        {
            push_block(d, rel(root, rules_file), HOST_ID, line.to_string());
        }
        for m in &active {
            let cov = match layout.rules_form {
                RulesForm::ImportShared(_) => covered.get(&m.input.id),
                RulesForm::Inline => None,
            };
            let rules = emit(d, plan, m, row.id, skills_dir.as_deref(), cov);
            match (rules.is_empty(), layout.rules_file) {
                (false, Some(file)) => {
                    push_block(d, rel(root, file), m.input.id.as_str(), rules.join("\n\n"))
                }
                (false, None) => {
                    for u in effective(&m.units, row.id) {
                        if matches!(classify(u), Class::Rule) {
                            skip(plan, u, row.id, "this tool has no rules file");
                        }
                    }
                }
                (true, _) => {}
            }
        }
    }

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
    for row in ADAPTERS {
        let Some(layout) = row.layout(Scope::User) else {
            continue;
        };
        if row.status == AdapterStatus::Implemented {
            sweep(d, home, &layout);
        }
        if !req.tools.has(row.id) {
            continue;
        }
        if row.status == AdapterStatus::Unimplemented {
            for m in &active {
                for u in effective(&m.units, row.id) {
                    skip(
                        plan,
                        u,
                        row.id,
                        format!("{} adapter is not implemented yet (track G9)", row.name),
                    );
                }
            }
            continue;
        }
        let skills_dir = layout.skills_dir.map(|s| rel(home, s));
        for m in &active {
            // No shared layer at user scope: every rule is inlined.
            let rules = emit(d, plan, m, row.id, skills_dir.as_deref(), None);
            if !rules.is_empty() {
                match layout.rules_file {
                    Some(file) => {
                        push_block(d, rel(home, file), m.input.id.as_str(), rules.join("\n\n"))
                    }
                    None => {
                        for u in effective(&m.units, row.id) {
                            if matches!(classify(u), Class::Rule) {
                                skip(plan, u, row.id, "this tool has no rules file");
                            }
                        }
                    }
                }
            }
        }
    }
}

fn sweep(d: &mut Desired, base: &Path, layout: &Layout) {
    if let Some(s) = layout.skills_dir {
        d.sweep_dirs.insert(rel(base, s));
    }
    if let Some(f) = layout.rules_file {
        d.sweep_files.insert(rel(base, f));
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
        let wanted_ids: BTreeSet<&str> = wanted.iter().map(|(id, _)| id.as_str()).collect();
        let mut next = current.clone();
        for id in fence_ids(&current) {
            if !wanted_ids.contains(id.as_str()) {
                next = splice_fenced(&next, &id, None);
            }
        }
        for (id, block) in wanted {
            next = splice_fenced(&next, id, Some(block));
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
