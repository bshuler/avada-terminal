//! Loading a module's skill units from its version directory and checking them
//! against the Agent Skills spec before anything is written.
//!
//! Source layout (binding, plan §2): `skills/<name>/SKILL.md`, optionally with
//! `scripts/` and `references/` beside it, and per-tool overrides at
//! `skills/<tool>/<name>/SKILL.md`. A failure in one unit is reported for that
//! unit and never stops the others.

use std::fs;
use std::path::{Path, PathBuf};

use avada_module_sdk::manifest::ModuleId;
use avada_module_sdk::skills::Skill;

use super::adapters::known_tool_ids;

/// Longest emitted skill name the Agent Skills spec allows.
pub const MAX_NAME: usize = 64;
/// Longest description the Agent Skills spec allows.
pub const MAX_DESCRIPTION: usize = 1024;

/// Sibling directories copied alongside a `SKILL.md`.
pub const SIBLING_DIRS: &[&str] = &["scripts", "references", "assets"];

/// Identifies one unit in plan entries.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct UnitRef {
    /// Owning module.
    pub module: ModuleId,
    /// The `<name>` segment of the source path.
    pub name: String,
    /// `Some(tool)` when this is a per-tool override from `skills/<tool>/<name>/`.
    pub tool: Option<String>,
    /// The `SKILL.md` it was read from.
    pub source: PathBuf,
}

/// A parsed, validated unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    /// Identity.
    pub reference: UnitRef,
    /// Frontmatter and body.
    pub skill: Skill,
    /// The directory holding `SKILL.md` and its siblings.
    pub dir: PathBuf,
}

/// A unit that could not be used. Recorded in the plan; never fatal.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct UnitError {
    /// Owning module.
    pub module: ModuleId,
    /// What failed (a `SKILL.md`, or a declared skills path that is missing).
    pub path: PathBuf,
    /// Why.
    pub error: String,
}

/// The directory name and frontmatter `name` a unit is emitted under:
/// `<owner>-<repo>-<name>`, kebab-cased. Unique across modules because the module
/// id is part of it, which is what lets regeneration and uninstall find exactly
/// their own directories.
pub fn emitted_name(module: &ModuleId, name: &str) -> String {
    let mut out = String::new();
    let mut dash = true;
    for c in module
        .as_str()
        .chars()
        .chain(std::iter::once('-'))
        .chain(name.chars())
    {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
            dash = false;
        } else if !dash {
            out.push('-');
            dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Kebab-case per the Agent Skills spec: lowercase letters, digits and single
/// hyphens, neither leading nor trailing.
pub fn is_kebab(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('-')
        && !s.ends_with('-')
        && !s.contains("--")
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Check a parsed unit against the spec and the source layout.
pub fn validate(module: &ModuleId, dir_name: &str, skill: &Skill) -> Result<(), String> {
    let fm = &skill.frontmatter;
    if !is_kebab(&fm.name) {
        return Err(format!(
            "name `{}` is not kebab-case (lowercase letters, digits, single hyphens)",
            fm.name
        ));
    }
    if fm.name != dir_name {
        return Err(format!(
            "name `{}` does not match its directory `{}`",
            fm.name, dir_name
        ));
    }
    let emitted = emitted_name(module, &fm.name);
    if emitted.chars().count() > MAX_NAME {
        return Err(format!(
            "emitted name `{emitted}` is {} characters; the limit is {MAX_NAME}",
            emitted.chars().count()
        ));
    }
    if fm.description.chars().count() > MAX_DESCRIPTION {
        return Err(format!(
            "description is {} characters; the limit is {MAX_DESCRIPTION}",
            fm.description.chars().count()
        ));
    }
    if fm.kind == avada_module_sdk::skills::SkillKind::Skill && fm.description.trim().is_empty() {
        return Err("a skill needs a description; the model picks it by that".into());
    }
    for t in &fm.tools {
        if !known_tool_ids().contains(&t.as_str()) {
            return Err(format!(
                "tools: `{t}` is not a known tool ({})",
                known_tool_ids().join(", ")
            ));
        }
    }
    Ok(())
}

/// Read every unit under the module's declared skills paths.
///
/// Returns the good units and the per-unit errors. A declared path that is absolute,
/// escapes the version directory, or does not exist is one error for that path.
#[tracing::instrument(level = "debug", skip(paths))]
pub fn load_units(
    module: &ModuleId,
    version_dir: &Path,
    paths: &[String],
) -> (Vec<Unit>, Vec<UnitError>) {
    let mut units = Vec::new();
    let mut errors = Vec::new();
    let tool_ids = known_tool_ids();
    for p in paths {
        let rel = Path::new(p);
        if rel.is_absolute()
            || rel
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            errors.push(UnitError {
                module: module.clone(),
                path: rel.to_path_buf(),
                error: "skills path must be relative and inside the module".into(),
            });
            continue;
        }
        let root = version_dir.join(rel);
        if !root.is_dir() {
            errors.push(UnitError {
                module: module.clone(),
                path: root,
                error: "declared skills path does not exist".into(),
            });
            continue;
        }
        for dir in sorted_dirs(&root) {
            let name = dir_name(&dir);
            if dir.join("SKILL.md").is_file() {
                load_one(module, &dir, &name, None, &mut units, &mut errors);
            } else if tool_ids.contains(&name.as_str()) {
                for sub in sorted_dirs(&dir) {
                    if sub.join("SKILL.md").is_file() {
                        let sub_name = dir_name(&sub);
                        load_one(
                            module,
                            &sub,
                            &sub_name,
                            Some(name.clone()),
                            &mut units,
                            &mut errors,
                        );
                    }
                }
            }
        }
    }
    (units, errors)
}

fn load_one(
    module: &ModuleId,
    dir: &Path,
    name: &str,
    tool: Option<String>,
    units: &mut Vec<Unit>,
    errors: &mut Vec<UnitError>,
) {
    let source = dir.join("SKILL.md");
    let text = match fs::read_to_string(&source) {
        Ok(t) => t,
        Err(e) => {
            errors.push(UnitError {
                module: module.clone(),
                path: source,
                error: format!("cannot read: {e}"),
            });
            return;
        }
    };
    let skill = match Skill::parse(&text) {
        Ok(s) => s,
        Err(e) => {
            errors.push(UnitError {
                module: module.clone(),
                path: source,
                error: e.to_string(),
            });
            return;
        }
    };
    if let Err(e) = validate(module, name, &skill) {
        errors.push(UnitError {
            module: module.clone(),
            path: source,
            error: e,
        });
        return;
    }
    units.push(Unit {
        reference: UnitRef {
            module: module.clone(),
            name: name.to_string(),
            tool,
            source,
        },
        skill,
        dir: dir.to_path_buf(),
    });
}

fn sorted_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(root)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default();
    dirs.sort();
    dirs
}

fn dir_name(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Every file under `dir/<sibling>/` for the sibling dirs the spec names, as
/// `(relative path, bytes, executable)`, sorted by path.
pub fn sibling_files(dir: &Path) -> Vec<(PathBuf, Vec<u8>, bool)> {
    let mut out = Vec::new();
    for sib in SIBLING_DIRS {
        let base = dir.join(sib);
        if base.is_dir() {
            walk(&base, Path::new(sib), &mut out);
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn walk(abs: &Path, rel: &Path, out: &mut Vec<(PathBuf, Vec<u8>, bool)>) {
    let Ok(rd) = fs::read_dir(abs) else { return };
    for entry in rd.filter_map(Result::ok) {
        let p = entry.path();
        let r = rel.join(entry.file_name());
        if p.is_dir() {
            walk(&p, &r, out);
        } else if let Ok(bytes) = fs::read(&p) {
            out.push((r, bytes, is_executable(&p)));
        }
    }
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(p)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("sh") || e.eq_ignore_ascii_case("py"))
}
