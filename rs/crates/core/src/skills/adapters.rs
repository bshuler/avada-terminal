//! The adapter table: where each AI tool reads rules and skills from, how the host
//! tells the tool is installed, and whether this wave writes to it.
//!
//! Every row is data. Track G9 fills the `Unimplemented` rows by giving them a
//! [`Layout`] that is right for the tool and flipping `status`; the materializer
//! needs no other change. Paths are segment lists joined with [`Path::join`], never
//! strings with `/`, so a row means the same thing on every OS.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The shared layer's id as it appears in a `tools:` allow-list or an override
/// directory (`skills/agents/<name>/`). It is not an adapter: it is `AGENTS.md`
/// and `.agents/skills/`, which every tool that follows the Agent Skills spec reads.
pub const SHARED: &str = "agents";

/// Whether the materializer writes to a tool yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterStatus {
    /// Files are emitted.
    Implemented,
    /// The row exists so detection and the `tools:` vocabulary are stable, but
    /// nothing is written; every unit that would go here is recorded as skipped.
    Unimplemented,
}

/// How a tool's rules file relates to the shared `AGENTS.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RulesForm {
    /// The tool reads an import line: one shared block carries it (`@AGENTS.md`),
    /// and only rules whose allow-list excludes the shared layer are inlined in
    /// the module's own block.
    ImportShared(&'static str),
    /// The tool cannot import; every always-on rule is inlined in the module's block.
    Inline,
}

/// Where one scope (project root or home) of one tool is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    /// Directory holding `<module>-<name>/SKILL.md`, or `None` if the tool has no
    /// notion of on-demand skills.
    pub skills_dir: Option<&'static [&'static str]>,
    /// The file always-on rules are fenced into.
    pub rules_file: Option<&'static [&'static str]>,
    /// How the rules file is filled.
    pub rules_form: RulesForm,
}

/// How to tell a tool is installed without asking it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Detect {
    /// Paths under the user's home whose existence means the tool is installed.
    pub home_paths: &'static [&'static [&'static str]],
    /// Binary names looked up on `PATH` (`.exe`/`.cmd` suffixes are tried too).
    pub binaries: &'static [&'static str],
}

/// One tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdapterRow {
    /// Id used in `tools:` allow-lists and override directories.
    pub id: &'static str,
    /// Display name.
    pub name: &'static str,
    /// Written this wave or not.
    pub status: AdapterStatus,
    /// Installed-ness probe.
    pub detect: Detect,
    /// Project-scope layout, relative to a project root.
    pub project: Option<Layout>,
    /// User-scope layout, relative to the home directory.
    pub user: Option<Layout>,
}

impl AdapterRow {
    /// The layout for a scope.
    pub fn layout(&self, scope: Scope) -> Option<Layout> {
        match scope {
            Scope::Project => self.project,
            Scope::User => self.user,
        }
    }
}

/// Project root or user home.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    /// Relative to a project root a workspace's panes run in.
    Project,
    /// Relative to the user's home directory.
    User,
}

/// The table. Order is the order fences are appended in a shared file.
pub const ADAPTERS: &[AdapterRow] = &[
    AdapterRow {
        id: "claude-code",
        name: "Claude Code",
        status: AdapterStatus::Implemented,
        detect: Detect {
            home_paths: &[&[".claude"]],
            binaries: &["claude"],
        },
        project: Some(Layout {
            skills_dir: Some(&[".claude", "skills"]),
            rules_file: Some(&["CLAUDE.md"]),
            rules_form: RulesForm::ImportShared("@AGENTS.md"),
        }),
        user: Some(Layout {
            skills_dir: Some(&[".claude", "skills"]),
            rules_file: Some(&[".claude", "CLAUDE.md"]),
            rules_form: RulesForm::Inline,
        }),
    },
    AdapterRow {
        id: "aider",
        name: "Aider",
        status: AdapterStatus::Implemented,
        detect: Detect {
            home_paths: &[&[".aider.conf.yml"], &[".aider"]],
            binaries: &["aider"],
        },
        project: Some(Layout {
            skills_dir: None,
            rules_file: Some(&["CONVENTIONS.md"]),
            rules_form: RulesForm::Inline,
        }),
        user: None,
    },
    // ---- G9 fills the rows below: give each a Layout and flip the status. ----
    AdapterRow {
        id: "cline",
        name: "Cline",
        status: AdapterStatus::Unimplemented,
        detect: Detect {
            home_paths: &[&["Documents", "Cline"]],
            binaries: &[],
        },
        project: Some(Layout {
            skills_dir: None,
            rules_file: Some(&[".clinerules"]),
            rules_form: RulesForm::Inline,
        }),
        user: None,
    },
    AdapterRow {
        id: "kiro",
        name: "Kiro",
        status: AdapterStatus::Unimplemented,
        detect: Detect {
            home_paths: &[&[".kiro"]],
            binaries: &["kiro"],
        },
        project: Some(Layout {
            skills_dir: None,
            rules_file: Some(&[".kiro", "steering", "avada.md"]),
            rules_form: RulesForm::Inline,
        }),
        user: None,
    },
    AdapterRow {
        id: "augment",
        name: "Augment",
        status: AdapterStatus::Unimplemented,
        detect: Detect {
            home_paths: &[&[".augment"]],
            binaries: &[],
        },
        project: Some(Layout {
            skills_dir: None,
            rules_file: Some(&[".augment", "rules", "avada.md"]),
            rules_form: RulesForm::Inline,
        }),
        user: None,
    },
    AdapterRow {
        id: "continue",
        name: "Continue",
        status: AdapterStatus::Unimplemented,
        detect: Detect {
            home_paths: &[&[".continue"]],
            binaries: &[],
        },
        project: Some(Layout {
            skills_dir: None,
            rules_file: Some(&[".continue", "rules", "avada.md"]),
            rules_form: RulesForm::Inline,
        }),
        user: None,
    },
];

/// Look a row up by id.
pub fn adapter(id: &str) -> Option<&'static AdapterRow> {
    ADAPTERS.iter().find(|a| a.id == id)
}

/// Every id the `tools:` allow-list and the override directories understand.
pub fn known_tool_ids() -> Vec<&'static str> {
    let mut ids: Vec<&'static str> = ADAPTERS.iter().map(|a| a.id).collect();
    ids.push(SHARED);
    ids
}

/// Join a segment list onto a base with [`Path::join`] only.
pub fn rel(base: &Path, segments: &[&str]) -> PathBuf {
    segments.iter().fold(base.to_path_buf(), |p, s| p.join(s))
}

/// Which tools are present, i.e. which adapters get written. Built from detection
/// on the real machine ([`Tools::detect_here`]) or handed in by a caller or a test.
/// A tool the user switched off in preferences is simply left out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tools {
    present: BTreeSet<String>,
}

impl Tools {
    /// No tool at all: only the shared layer is written.
    pub fn none() -> Self {
        Self::default()
    }

    /// Exactly these ids.
    pub fn only<I, S>(ids: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Tools {
            present: ids.into_iter().map(Into::into).collect(),
        }
    }

    /// Add one.
    pub fn with(mut self, id: &str) -> Self {
        self.present.insert(id.to_string());
        self
    }

    /// Drop one (the per-tool toggle).
    pub fn without(mut self, id: &str) -> Self {
        self.present.remove(id);
        self
    }

    /// Present?
    pub fn has(&self, id: &str) -> bool {
        self.present.contains(id)
    }

    /// The ids, sorted.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.present.iter().map(String::as_str)
    }

    /// Data-driven detection against a given home and a given `PATH` split into
    /// directories. Tests pass scratch directories; nothing here reads the
    /// environment.
    pub fn detect(home: &Path, path_dirs: &[PathBuf]) -> Self {
        let mut present = BTreeSet::new();
        for row in ADAPTERS {
            let in_home = row
                .detect
                .home_paths
                .iter()
                .any(|segs| rel(home, segs).exists());
            let on_path = row.detect.binaries.iter().any(|bin| {
                path_dirs.iter().any(|d| {
                    d.join(bin).is_file()
                        || d.join(format!("{bin}.exe")).is_file()
                        || d.join(format!("{bin}.cmd")).is_file()
                })
            });
            if in_home || on_path {
                present.insert(row.id.to_string());
            }
        }
        Tools { present }
    }

    /// Detection on this machine: the real home and the real `PATH`.
    #[tracing::instrument(level = "debug", ret)]
    pub fn detect_here() -> Self {
        let Some(home) = directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf()) else {
            return Tools::none();
        };
        let path_dirs: Vec<PathBuf> = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect())
            .unwrap_or_default();
        Tools::detect(&home, &path_dirs)
    }
}
