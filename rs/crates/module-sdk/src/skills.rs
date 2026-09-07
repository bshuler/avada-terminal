//! Skills a module ships and how the host materializes them into agent tools.
//!
//! A module declares `[skills] paths = ["skills"]`; each `skills/<name>/SKILL.md`
//! starts with a small YAML-shaped frontmatter block. The host, holding
//! `skills.materialize`, writes those files into the locations each agent tool
//! reads, wrapped in a fence so it can find and remove them again on uninstall
//! without touching anything the user wrote.
//!
//! The frontmatter parser here is deliberately tiny: `key: value` lines and
//! `key: [a, b]` lists, no nesting, no quoting beyond stripping matching quotes.
//! That is all a SKILL.md needs and it keeps a YAML crate out of the SDK.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Rule (always in context) or skill (loaded on demand).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SkillKind {
    /// Loaded on demand.
    #[default]
    Skill,
    /// Always in context.
    Rule,
}

/// When a skill is activated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Activation {
    /// Always on.
    Always,
    /// The model decides from the description.
    #[default]
    Model,
    /// On when a matching file is in play.
    Glob,
    /// Only when the user invokes it.
    Manual,
}

/// Parsed frontmatter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillFrontmatter {
    /// Skill name (slug).
    pub name: String,
    /// One paragraph the model uses to decide relevance.
    #[serde(default)]
    pub description: String,
    /// Rule or skill.
    #[serde(default)]
    pub kind: SkillKind,
    /// Activation.
    #[serde(default)]
    pub activation: Activation,
    /// Globs for [`Activation::Glob`].
    #[serde(default)]
    pub globs: Vec<String>,
    /// Tools this skill is written for; empty means all.
    #[serde(default)]
    pub tools: Vec<String>,
}

/// A SKILL.md split into frontmatter and body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// Parsed header.
    pub frontmatter: SkillFrontmatter,
    /// Markdown after the closing `---`.
    pub body: String,
}

/// Parse failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillError {
    /// No `---` block at the top.
    NoFrontmatter,
    /// A line inside the block is not `key: value`.
    BadLine(String),
    /// `name` missing.
    MissingName,
    /// A field has a value outside its enum.
    BadValue(String, String),
}

impl std::fmt::Display for SkillError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkillError::NoFrontmatter => f.write_str("SKILL.md has no frontmatter block"),
            SkillError::BadLine(l) => write!(f, "frontmatter line is not `key: value`: {l}"),
            SkillError::MissingName => f.write_str("frontmatter has no `name`"),
            SkillError::BadValue(k, v) => write!(f, "frontmatter `{k}` cannot be `{v}`"),
        }
    }
}
impl std::error::Error for SkillError {}

/// Split a `---`-fenced header into key/value pairs.
pub fn parse_frontmatter(text: &str) -> Result<(BTreeMap<String, String>, &str), SkillError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let rest = text.strip_prefix("---").ok_or(SkillError::NoFrontmatter)?;
    let rest = rest
        .strip_prefix("\r\n")
        .or_else(|| rest.strip_prefix('\n'))
        .ok_or(SkillError::NoFrontmatter)?;
    let mut map = BTreeMap::new();
    let mut consumed = 0;
    let mut closed = false;
    for line in rest.split_inclusive('\n') {
        consumed += line.len();
        let l = line.trim_end_matches(['\r', '\n']);
        if l.trim() == "---" {
            closed = true;
            break;
        }
        if l.trim().is_empty() || l.trim_start().starts_with('#') {
            continue;
        }
        let (k, v) = l
            .split_once(':')
            .ok_or_else(|| SkillError::BadLine(l.to_string()))?;
        let k = k.trim();
        if k.is_empty() || k.contains(char::is_whitespace) {
            return Err(SkillError::BadLine(l.to_string()));
        }
        map.insert(k.to_string(), unquote(v.trim()).to_string());
    }
    if !closed {
        return Err(SkillError::NoFrontmatter);
    }
    Ok((map, &rest[consumed..]))
}

fn unquote(v: &str) -> &str {
    for q in ['"', '\''] {
        if v.len() >= 2 && v.starts_with(q) && v.ends_with(q) {
            return &v[1..v.len() - 1];
        }
    }
    v
}

fn list(v: &str) -> Vec<String> {
    let inner = v
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(v);
    inner
        .split(',')
        .map(|s| unquote(s.trim()).to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

impl Skill {
    /// Parse a SKILL.md.
    pub fn parse(text: &str) -> Result<Skill, SkillError> {
        let (map, body) = parse_frontmatter(text)?;
        let get = |k: &str| map.get(k).map(String::as_str).unwrap_or("");
        let name = get("name").to_string();
        if name.is_empty() {
            return Err(SkillError::MissingName);
        }
        let kind = match get("kind") {
            "" | "skill" => SkillKind::Skill,
            "rule" => SkillKind::Rule,
            other => return Err(SkillError::BadValue("kind".into(), other.into())),
        };
        let activation = match get("activation") {
            "" | "model" => Activation::Model,
            "always" => Activation::Always,
            "glob" => Activation::Glob,
            "manual" => Activation::Manual,
            other => return Err(SkillError::BadValue("activation".into(), other.into())),
        };
        Ok(Skill {
            frontmatter: SkillFrontmatter {
                name,
                description: get("description").to_string(),
                kind,
                activation,
                globs: list(get("globs")),
                tools: list(get("tools")),
            },
            body: body.to_string(),
        })
    }
}

/// Where one agent tool reads rules and skills from, relative to a project root
/// (`project`) or the user's home (`user`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolAdapter {
    /// Tool id (`claude-code`, `cursor`, `codex`, `copilot`).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Directory for on-demand skills, or `None` if the tool has no concept.
    pub skills_dir: Option<String>,
    /// File that always-on rules are appended to, fenced.
    pub rules_file: Option<String>,
    /// Whether `skills_dir`/`rules_file` are under the project root (true) or `~` (false).
    pub project_scoped: bool,
}

/// The adapters the host knows at contract version 1.
pub fn adapters() -> Vec<ToolAdapter> {
    let a =
        |id: &str, name: &str, skills: Option<&str>, rules: Option<&str>, proj: bool| ToolAdapter {
            id: id.into(),
            name: name.into(),
            skills_dir: skills.map(str::to_string),
            rules_file: rules.map(str::to_string),
            project_scoped: proj,
        };
    vec![
        a(
            "claude-code",
            "Claude Code",
            Some(".claude/skills"),
            Some("CLAUDE.md"),
            true,
        ),
        a("cursor", "Cursor", Some(".cursor/rules"), None, true),
        a("codex", "Codex", None, Some("AGENTS.md"), true),
        a(
            "copilot",
            "GitHub Copilot",
            None,
            Some(".github/copilot-instructions.md"),
            true,
        ),
    ]
}

/// The fence markers around materialized rules so the host can remove them again.
pub fn fence(module_id: &str) -> (String, String) {
    (
        format!("<!-- avada:module={module_id} -->"),
        format!("<!-- /avada:module={module_id} -->"),
    )
}

/// Replace (or append) the fenced block for `module_id` in `file`. `None` removes it.
pub fn splice_fenced(file: &str, module_id: &str, block: Option<&str>) -> String {
    let (open, close) = fence(module_id);
    let mut out = String::new();
    let mut skipping = false;
    let mut found = false;
    for line in file.split_inclusive('\n') {
        let l = line.trim_end_matches(['\r', '\n']);
        if l == open {
            skipping = true;
            found = true;
            if let Some(b) = block {
                push_block(&mut out, &open, b, &close);
            }
            continue;
        }
        if skipping {
            if l == close {
                skipping = false;
            }
            continue;
        }
        out.push_str(line);
    }
    if !found {
        if let Some(b) = block {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            push_block(&mut out, &open, b, &close);
        }
    }
    out
}

fn push_block(out: &mut String, open: &str, block: &str, close: &str) {
    out.push_str(open);
    out.push('\n');
    out.push_str(block.trim_end_matches('\n'));
    out.push('\n');
    out.push_str(close);
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;

    const SKILL: &str = "---\nname: git-status\ndescription: \"Show what changed\"\nkind: rule\nactivation: glob\nglobs: [\"*.rs\", '*.toml']\ntools: [claude-code, codex]\n---\n# Body\n\nHello.\n";

    #[test]
    fn parses_frontmatter_and_body() {
        let s = Skill::parse(SKILL).unwrap();
        assert_eq!(s.frontmatter.name, "git-status");
        assert_eq!(s.frontmatter.description, "Show what changed");
        assert_eq!(s.frontmatter.kind, SkillKind::Rule);
        assert_eq!(s.frontmatter.activation, Activation::Glob);
        assert_eq!(s.frontmatter.globs, vec!["*.rs", "*.toml"]);
        assert_eq!(s.frontmatter.tools, vec!["claude-code", "codex"]);
        assert_eq!(s.body, "# Body\n\nHello.\n");
    }

    #[test]
    fn defaults_bom_and_crlf() {
        let s = Skill::parse("\u{feff}---\r\nname: x\r\n---\r\nbody").unwrap();
        assert_eq!(s.frontmatter.kind, SkillKind::Skill);
        assert_eq!(s.frontmatter.activation, Activation::Model);
        assert!(s.frontmatter.globs.is_empty());
        assert_eq!(s.body, "body");
    }

    #[test]
    fn errors() {
        assert_eq!(
            Skill::parse("no header").unwrap_err(),
            SkillError::NoFrontmatter
        );
        assert_eq!(
            Skill::parse("---\nname: x\n").unwrap_err(),
            SkillError::NoFrontmatter
        );
        assert_eq!(
            Skill::parse("---\ndescription: x\n---\n").unwrap_err(),
            SkillError::MissingName
        );
        assert!(matches!(
            Skill::parse("---\nname: x\njunk\n---\n").unwrap_err(),
            SkillError::BadLine(_)
        ));
        assert!(matches!(
            Skill::parse("---\nname: x\nkind: nope\n---\n").unwrap_err(),
            SkillError::BadValue(..)
        ));
        assert!(matches!(
            Skill::parse("---\nname: x\nactivation: nope\n---\n").unwrap_err(),
            SkillError::BadValue(..)
        ));
    }

    #[test]
    fn comments_and_blank_lines_are_skipped() {
        let s = Skill::parse("---\n# a comment\n\nname: x\n---\n").unwrap();
        assert_eq!(s.frontmatter.name, "x");
    }

    #[test]
    fn fence_splice_appends_replaces_and_removes() {
        let base = "# My rules\n\nkeep me\n";
        let with = splice_fenced(base, "acme/x", Some("rule one\n"));
        assert!(with.starts_with(base));
        assert!(with
            .ends_with("<!-- avada:module=acme/x -->\nrule one\n<!-- /avada:module=acme/x -->\n"));
        let replaced = splice_fenced(&with, "acme/x", Some("rule two"));
        assert!(!replaced.contains("rule one"));
        assert!(replaced.contains("rule two\n<!-- /avada"));
        assert_eq!(replaced.matches("avada:module=acme/x").count(), 2);
        let removed = splice_fenced(&replaced, "acme/x", None);
        assert_eq!(removed, base);
        let other = splice_fenced(&with, "acme/y", Some("y"));
        assert!(other.contains("rule one") && other.contains("acme/y"));
        assert_eq!(
            splice_fenced("no newline", "a/b", Some("z")),
            "no newline\n<!-- avada:module=a/b -->\nz\n<!-- /avada:module=a/b -->\n"
        );
        assert_eq!(splice_fenced(base, "a/b", None), base);
    }

    #[test]
    fn adapters_have_unique_ids_and_a_place_to_write() {
        let ads = adapters();
        let ids: std::collections::BTreeSet<_> = ads.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids.len(), ads.len());
        for a in &ads {
            assert!(
                a.skills_dir.is_some() || a.rules_file.is_some(),
                "{} has nowhere to write",
                a.id
            );
        }
    }
}
