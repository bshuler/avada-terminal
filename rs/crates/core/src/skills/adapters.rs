//! The adapter table: where each AI tool reads rules and skills from, how the host
//! tells the tool is installed, and whether the materializer writes to it.
//!
//! Every row is data. Paths are segment lists joined with [`Path::join`], never
//! strings with `/`, so a row means the same thing on every OS. The materializer
//! has no per-tool code: the [`Layout`] says which of the four forms (a fenced
//! block in a rules file, one native rule file per unit, a skill directory, a
//! workflow file) a tool has, and the [`Dialect`] says how a native rule file's
//! front matter spells `always`, `glob`, `manual` and `model` activation.
//!
//! # Detection table
//!
//! [`Tools::detect`] walks every row's [`Detect`] against one home directory and
//! one `PATH`. A tool is present when any home path exists or any binary is on
//! the path (`.exe` and `.cmd` suffixes are tried). A tool the user switched off
//! is kept in the set as *disabled*: [`Tools::has`] then answers `false`, so its
//! adapter is swept but not written, while [`Tools::disabled_ids`] still lists
//! it for the UI. Nothing in this module reads the environment except
//! [`Tools::detect_here`], which the app calls.
//!
//! | id | home paths | binaries |
//! |---|---|---|
//! | `claude-code` | `~/.claude` | `claude` |
//! | `aider` | `~/.aider.conf.yml`, `~/.aider` | `aider` |
//! | `cline` | `~/Documents/Cline` | `cline` |
//! | `kiro` | `~/.kiro` | `kiro`, `kiro-cli` |
//! | `augment` | `~/.augment` | `auggie` |
//! | `continue` | `~/.continue` | `cn` |

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

/// How a tool spells activation in the front matter of a native rule file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// `.claude/rules/*.md`: no front matter for always-on, `paths:` for globs.
    /// Manual and model activation have no rule form (they are skills).
    ClaudeCode,
    /// `.clinerules/*.md`: no front matter for always-on, `paths:` for globs.
    /// Manual activation is a workflow file; model activation has no form.
    Cline,
    /// `.kiro/steering/*.md`: `inclusion: always | fileMatch | manual | auto`
    /// with `fileMatchPattern:` for globs and `name:`/`description:` for auto.
    Kiro,
    /// `.augment/rules/*.md`: `type: always_apply | agent_requested | manual`.
    /// No glob form: a glob unit becomes `agent_requested` whose description
    /// names the globs.
    Augment,
    /// `.continue/rules/*.md`: `alwaysApply:` and `globs:`; model and manual
    /// activation are `alwaysApply: false` with a description the agent reads.
    Continue,
}

/// A directory of native one-file-per-unit rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RulesDir {
    /// Segments relative to the scope's base.
    pub path: &'static [&'static str],
    /// Front matter vocabulary.
    pub dialect: Dialect,
    /// The tool ignores front matter here and applies every file always (Augment's
    /// `~/.augment/rules/`): only always-on rules are written, the rest are skipped.
    pub always_only: bool,
}

/// A byte cap the tool documents for what it will load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cap {
    /// Largest file the tool reads.
    pub bytes: usize,
    /// Where the number comes from, for the notice and the docs.
    pub source: &'static str,
}

/// Where one scope (project root or home) of one tool is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    /// Directory holding `<module>-<name>/SKILL.md`, or `None` if the tool has no
    /// notion of on-demand skills.
    pub skills_dir: Option<&'static [&'static str]>,
    /// Whether `skills_dir` honours `disable-model-invocation: true`, which is
    /// how a manual unit becomes a skill only the user invokes.
    pub manual_skills: bool,
    /// The file always-on rules are fenced into.
    pub rules_file: Option<&'static [&'static str]>,
    /// How the rules file is filled.
    pub rules_form: RulesForm,
    /// Directory of native rule files, one per unit, for the activations the
    /// rules file cannot express (and for always-on rules when there is no
    /// rules file). If the path exists as a regular file instead (a legacy
    /// single-file `.clinerules`), it is used as an inline rules file.
    pub rules_dir: Option<RulesDir>,
    /// Directory of manual workflow files, one per manual unit (Cline).
    pub manual_dir: Option<&'static [&'static str]>,
    /// A documented byte cap; a unit over it is truncated at a paragraph
    /// boundary with a notice and reported in the plan.
    pub cap: Option<Cap>,
}

impl Layout {
    /// A layout with nothing set; rows fill in what the tool has.
    pub const EMPTY: Layout = Layout {
        skills_dir: None,
        manual_skills: false,
        rules_file: None,
        rules_form: RulesForm::Inline,
        rules_dir: None,
        manual_dir: None,
        cap: None,
    };

    /// Every directory or file segment list the layout names.
    pub fn paths(&self) -> Vec<&'static [&'static str]> {
        self.skills_dir
            .into_iter()
            .chain(self.rules_file)
            .chain(self.rules_dir.map(|r| r.path))
            .chain(self.manual_dir)
            .collect()
    }

    /// The layout as it applies under `base`: a `rules_dir` that exists as a
    /// regular file (a single-file `.clinerules`) is treated as an inline rules
    /// file instead of a directory, and a `manual_dir` that would have to live
    /// inside that file is dropped.
    pub fn resolve(mut self, base: &Path) -> Layout {
        if let Some(r) = self.rules_dir {
            let file = rel(base, r.path);
            if file.is_file() {
                self.rules_dir = None;
                if self.rules_file.is_none() {
                    self.rules_file = Some(r.path);
                    self.rules_form = RulesForm::Inline;
                }
                if self
                    .manual_dir
                    .is_some_and(|m| rel(base, m).starts_with(&file))
                {
                    self.manual_dir = None;
                }
            }
        }
        self
    }
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
    /// Whether the materializer writes to it; every shipped row is `Implemented`.
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

/// Claude Code's documented file cap, applied to everything written for it.
const CLAUDE_CODE_CAP: Cap = Cap {
    bytes: 4 << 20,
    source: "code.claude.com/docs/en/memory: \"Claude Code loads a CLAUDE.md file of up to \
             4 MiB in full and skips a larger file\"",
};

/// The table. Order is the order fences are appended in a shared file.
///
/// Sources of truth, checked 2026-09-08 against each vendor's docs; what could
/// not be verified is marked:
///
/// * **Claude Code** — `code.claude.com/docs/en/memory`: `./CLAUDE.md` with
///   `@AGENTS.md` imports, `.claude/rules/*.md` with `paths:` front matter,
///   `~/.claude/CLAUDE.md`, `~/.claude/rules/`, a 4 MiB file cap.
///   `code.claude.com/docs/en/skills`: `.claude/skills/<name>/SKILL.md`,
///   `~/.claude/skills/`, `disable-model-invocation: true` for manual skills.
///   That page documents no size cap for a skill (only "keep `SKILL.md` under
///   500 lines" as advice), so the 4 MiB memory-file cap is applied to skill
///   files too rather than leaving them uncapped.
/// * **Aider** — `CONVENTIONS.md` is the documented convention (read via
///   `--read`); no user-level file, no on-demand form.
/// * **Cline** — `docs.cline.bot/features/cline-rules`: `.clinerules/` holds
///   `.md`/`.txt` rule files, `paths:` front matter scopes a rule to globs, global
///   rules in `~/Documents/Cline/Rules`; workflows in `.clinerules/workflows/`
///   and `~/Documents/Cline/Workflows`, invoked as `/<file>.md`. No documented
///   size cap ("keep rules concise"). Whether Cline reads `AGENTS.md` on its own
///   was not verified, so always-on rules are written natively too.
/// * **Kiro** — `kiro.dev/docs/steering`: `.kiro/steering/*.md` and
///   `~/.kiro/steering/*.md`; front matter `inclusion: always | fileMatch |
///   manual | auto`, `fileMatchPattern` as string or list, `#name` or `/name`
///   invokes a manual file. No documented cap. Whether Kiro reads
///   `.agents/skills/` was not verified; skills are not written for it.
/// * **Augment** — `docs.augmentcode.com/cli/rules`: `.augment/rules/*.md` with
///   `type: always_apply | agent_requested | manual` (`manual` is IDE-only; the
///   CLI skips it), `description` required for `agent_requested`;
///   `~/.augment/rules/` is read as always-on regardless of front matter. No
///   glob form, no documented cap.
/// * **Continue** — `docs.continue.dev/customize/deep-dives/rules`:
///   `.continue/rules/*.md` with `name`, `description`, `globs`, `alwaysApply`.
///   The global `~/.continue/rules/` directory is from Continue's config
///   layout and was **not** re-verified against the current page. No
///   documented cap.
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
            manual_skills: true,
            rules_file: Some(&["CLAUDE.md"]),
            rules_form: RulesForm::ImportShared("@AGENTS.md"),
            rules_dir: Some(RulesDir {
                path: &[".claude", "rules"],
                dialect: Dialect::ClaudeCode,
                always_only: false,
            }),
            manual_dir: None,
            cap: Some(CLAUDE_CODE_CAP),
        }),
        user: Some(Layout {
            skills_dir: Some(&[".claude", "skills"]),
            manual_skills: true,
            rules_file: Some(&[".claude", "CLAUDE.md"]),
            rules_form: RulesForm::Inline,
            rules_dir: Some(RulesDir {
                path: &[".claude", "rules"],
                dialect: Dialect::ClaudeCode,
                always_only: false,
            }),
            manual_dir: None,
            cap: Some(CLAUDE_CODE_CAP),
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
            rules_file: Some(&["CONVENTIONS.md"]),
            ..Layout::EMPTY
        }),
        user: None,
    },
    AdapterRow {
        id: "cline",
        name: "Cline",
        status: AdapterStatus::Implemented,
        detect: Detect {
            home_paths: &[&["Documents", "Cline"]],
            binaries: &["cline"],
        },
        project: Some(Layout {
            rules_dir: Some(RulesDir {
                path: &[".clinerules"],
                dialect: Dialect::Cline,
                always_only: false,
            }),
            manual_dir: Some(&[".clinerules", "workflows"]),
            ..Layout::EMPTY
        }),
        user: Some(Layout {
            rules_dir: Some(RulesDir {
                path: &["Documents", "Cline", "Rules"],
                dialect: Dialect::Cline,
                always_only: false,
            }),
            manual_dir: Some(&["Documents", "Cline", "Workflows"]),
            ..Layout::EMPTY
        }),
    },
    AdapterRow {
        id: "kiro",
        name: "Kiro",
        status: AdapterStatus::Implemented,
        detect: Detect {
            home_paths: &[&[".kiro"]],
            binaries: &["kiro", "kiro-cli"],
        },
        project: Some(Layout {
            rules_dir: Some(RulesDir {
                path: &[".kiro", "steering"],
                dialect: Dialect::Kiro,
                always_only: false,
            }),
            ..Layout::EMPTY
        }),
        user: Some(Layout {
            rules_dir: Some(RulesDir {
                path: &[".kiro", "steering"],
                dialect: Dialect::Kiro,
                always_only: false,
            }),
            ..Layout::EMPTY
        }),
    },
    AdapterRow {
        id: "augment",
        name: "Augment",
        status: AdapterStatus::Implemented,
        detect: Detect {
            home_paths: &[&[".augment"]],
            binaries: &["auggie"],
        },
        project: Some(Layout {
            rules_dir: Some(RulesDir {
                path: &[".augment", "rules"],
                dialect: Dialect::Augment,
                always_only: false,
            }),
            ..Layout::EMPTY
        }),
        user: Some(Layout {
            rules_dir: Some(RulesDir {
                path: &[".augment", "rules"],
                dialect: Dialect::Augment,
                always_only: true,
            }),
            ..Layout::EMPTY
        }),
    },
    AdapterRow {
        id: "continue",
        name: "Continue",
        status: AdapterStatus::Implemented,
        detect: Detect {
            home_paths: &[&[".continue"]],
            binaries: &["cn"],
        },
        project: Some(Layout {
            rules_dir: Some(RulesDir {
                path: &[".continue", "rules"],
                dialect: Dialect::Continue,
                always_only: false,
            }),
            ..Layout::EMPTY
        }),
        user: Some(Layout {
            rules_dir: Some(RulesDir {
                path: &[".continue", "rules"],
                dialect: Dialect::Continue,
                always_only: false,
            }),
            ..Layout::EMPTY
        }),
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
///
/// The per-tool toggle lives here rather than on the request: a tool the user
/// switched off stays *present* (so the UI can still list it) but *disabled*,
/// and [`Tools::has`] is what the materializer consults, so a disabled tool is
/// swept clean and never written.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tools {
    present: BTreeSet<String>,
    disabled: BTreeSet<String>,
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
            disabled: BTreeSet::new(),
        }
    }

    /// Add one.
    pub fn with(mut self, id: &str) -> Self {
        self.present.insert(id.to_string());
        self
    }

    /// Drop one entirely, as if it had not been detected.
    pub fn without(mut self, id: &str) -> Self {
        self.present.remove(id);
        self
    }

    /// Switch one off: it stays detected but is not written for.
    pub fn disable(mut self, id: &str) -> Self {
        self.disabled.insert(id.to_string());
        self
    }

    /// Undo [`Tools::disable`].
    pub fn enable(mut self, id: &str) -> Self {
        self.disabled.remove(id);
        self
    }

    /// Present and not disabled: written for.
    pub fn has(&self, id: &str) -> bool {
        self.present.contains(id) && !self.disabled.contains(id)
    }

    /// Detected, whether or not disabled.
    pub fn detected(&self, id: &str) -> bool {
        self.present.contains(id)
    }

    /// The ids written for, sorted.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.present
            .iter()
            .filter(|id| !self.disabled.contains(*id))
            .map(String::as_str)
    }

    /// Every detected id, sorted, disabled ones included.
    pub fn detected_ids(&self) -> impl Iterator<Item = &str> {
        self.present.iter().map(String::as_str)
    }

    /// The ids switched off, sorted (whether or not they were detected).
    pub fn disabled_ids(&self) -> impl Iterator<Item = &str> {
        self.disabled.iter().map(String::as_str)
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
        Tools {
            present,
            disabled: BTreeSet::new(),
        }
    }

    /// [`Tools::detect`] with the user's per-tool toggles applied.
    pub fn detect_with<I, S>(home: &Path, path_dirs: &[PathBuf], disabled: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut t = Tools::detect(home, path_dirs);
        t.disabled = disabled.into_iter().map(Into::into).collect();
        t
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

    /// [`Tools::detect_here`] with the user's per-tool toggles applied.
    pub fn detect_here_with<I, S>(disabled: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut t = Tools::detect_here();
        t.disabled = disabled.into_iter().map(Into::into).collect();
        t
    }
}
