//! `avada.toml` — the one file a module repository carries.
//!
//! ```toml
//! [module]
//! id = "acme/avada-files"          # GitHub owner/repo; also the dependency key
//! name = "Files"
//! version = "1.2.0"                # must equal the git tag `v1.2.0` it is built from
//! description = "File browser for the left panel"
//! publisher = "Acme"
//! contract = ">=1, <2"             # contract versions the module speaks
//!
//! [distribution]
//! kind = "source"                  # or "binary" (commercial edition only)
//! commercial = false               # true: requires a license for product id = module id
//!
//! [dependencies]
//! "acme/avada-git" = "^1.0"
//!
//! [[provides]]                     # extension points this module implements
//! shape = "avada.files.tree"
//! version = "1.0.0"
//!
//! [[requires]]                     # extension points this module needs from someone
//! shape = "avada.git.status"
//! version = "^1"
//!
//! [[contributions]]
//! kind = "rail"                    # rail | pane | command | prefs | route
//! id = "files"
//! tier = 1
//! label = "Files"
//! icon = "icons/files.svg"
//!
//! capabilities = ["fs.read", "workspace.read", "ui.rail"]
//!
//! [[profiles]]
//! name = "High security"
//! description = "Never writes; asks before opening panes"
//! [profiles.values]
//! "fs.read" = "always"
//! "panes.spawn" = "ask"
//!
//! [skills]
//! paths = ["skills"]
//! ```
//!
//! Everything the host trusts at runtime comes from the **install record** (see
//! [`crate::rights`]), which is minted from this manifest at install time and HMAC'd.
//! The manifest sent in the handshake must match that record; the file on disk is
//! never consulted after install.

use crate::caps::Capability;
use crate::rights::PermissionProfile;
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

/// A module id: the GitHub `owner/repo` the module is discovered and installed from.
///
/// Lowercase ASCII letters, digits, `-`, `_` and `.` per segment, exactly one `/`.
/// GitHub is case-insensitive for these; the id is stored lowercased so the same
/// repo can never be installed twice under two spellings.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ModuleId(String);

impl ModuleId {
    /// Parse and normalise.
    pub fn new(s: &str) -> Result<Self, ManifestError> {
        let s = s.trim().to_ascii_lowercase();
        let mut parts = s.split('/');
        let (Some(owner), Some(repo), None) = (parts.next(), parts.next(), parts.next()) else {
            return Err(ManifestError::BadId(s));
        };
        let ok = |seg: &str| {
            !seg.is_empty()
                && seg != "."
                && seg != ".."
                && seg
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "-_.".contains(c))
        };
        if !ok(owner) || !ok(repo) {
            return Err(ManifestError::BadId(s));
        }
        Ok(ModuleId(s))
    }
    /// `owner/repo`.
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// The GitHub owner.
    pub fn owner(&self) -> &str {
        self.0.split('/').next().unwrap_or("")
    }
    /// The repository name.
    pub fn repo(&self) -> &str {
        self.0.split('/').nth(1).unwrap_or("")
    }
    /// A filesystem-safe directory name: `owner__repo`.
    pub fn dir_name(&self) -> String {
        format!("{}__{}", self.owner(), self.repo())
    }
}

impl fmt::Debug for ModuleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ModuleId({})", self.0)
    }
}
impl fmt::Display for ModuleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl FromStr for ModuleId {
    type Err = ManifestError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        ModuleId::new(s)
    }
}
impl TryFrom<String> for ModuleId {
    type Error = ManifestError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        ModuleId::new(&s)
    }
}
impl From<ModuleId> for String {
    fn from(id: ModuleId) -> String {
        id.0
    }
}

/// The `[module]` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModuleSection {
    /// `owner/repo`.
    pub id: ModuleId,
    /// Display name.
    pub name: String,
    /// Semver; must equal the tag the artifact was built from (`v{version}`).
    pub version: Version,
    /// One line for the marketplace.
    #[serde(default)]
    pub description: String,
    /// Publisher display name (also the name shown on a license badge).
    #[serde(default)]
    pub publisher: String,
    /// Contract versions the module speaks, e.g. `">=1, <2"`. The host negotiates the
    /// highest version inside this range that it also supports.
    pub contract: VersionReq,
}

/// How the module is shipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DistributionKind {
    /// Built from source by the installing machine (`cargo build --release`).
    #[default]
    Source,
    /// A precompiled, notarized release asset. Commercial edition only.
    Binary,
}

/// The `[distribution]` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct DistributionSection {
    /// Source or binary.
    #[serde(default)]
    pub kind: DistributionKind,
    /// Requires a license whose `product` claim equals this module's id.
    #[serde(default)]
    pub commercial: bool,
    /// Issuer URL for the license (OAuth-profile discovery at
    /// `/.well-known/oauth-authorization-server`). Required when `commercial`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
    /// Cargo binary name when the repo builds more than one; defaults to the repo name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bin: Option<String>,
}

/// An extension point this module implements.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provision {
    /// Dotted shape name, e.g. `avada.files.tree`.
    pub shape: String,
    /// The version of the shape implemented.
    pub version: Version,
}

/// An extension point this module needs someone to provide.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Requirement {
    /// Dotted shape name.
    pub shape: String,
    /// Acceptable versions of the shape.
    pub version: VersionReq,
    /// A named module that must be the provider. Named beats shaped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ModuleId>,
    /// Every installed provider is active (rail entries, commands), not just one.
    #[serde(default)]
    pub multi: bool,
}

/// UI tiers a contribution may declare. Core enforces the tier at handshake: a
/// contribution that asks for more than its declared tier is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub enum UiTier {
    /// Rows, models, typed prefs, string-id commands, path-string icons.
    Data = 1,
    /// A `slint-interpreter` component in a fixed host slot.
    Slot = 2,
    /// A pixel surface over shared memory.
    Pixels = 3,
    /// Route descriptor table → `GET /schema` → CLI.
    Route = 4,
    /// A cell grid surface (editor).
    Grid = 5,
}

impl TryFrom<u8> for UiTier {
    type Error = ManifestError;
    fn try_from(v: u8) -> Result<Self, Self::Error> {
        Ok(match v {
            1 => UiTier::Data,
            2 => UiTier::Slot,
            3 => UiTier::Pixels,
            4 => UiTier::Route,
            5 => UiTier::Grid,
            other => return Err(ManifestError::BadTier(other)),
        })
    }
}
impl From<UiTier> for u8 {
    fn from(t: UiTier) -> u8 {
        t as u8
    }
}

/// What kind of thing a contribution is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContributionKind {
    /// A left-panel rail entry.
    Rail,
    /// A pane surface the module owns.
    Pane,
    /// A string-id command.
    Command,
    /// A typed preferences page.
    Prefs,
    /// A control-plane route (declared in full in the handshake descriptor table).
    Route,
}

/// One `[[contributions]]` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Contribution {
    /// Rail, pane, command, prefs, route.
    pub kind: ContributionKind,
    /// Stable id, unique within the module; the host namespaces it as `<module>/<id>`.
    pub id: String,
    /// The UI tier this contribution uses.
    pub tier: UiTier,
    /// Display label.
    #[serde(default)]
    pub label: String,
    /// Path to an SVG in the module repo, relative to its root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

/// The `[skills]` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SkillsSection {
    /// Directories (relative to the repo root) holding `<name>/SKILL.md`.
    #[serde(default)]
    pub paths: Vec<String>,
}

/// The whole manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// `[module]`.
    pub module: ModuleSection,
    /// `[distribution]`.
    #[serde(default)]
    pub distribution: DistributionSection,
    /// `[dependencies]`: module id → version requirement.
    #[serde(default)]
    pub dependencies: BTreeMap<ModuleId, VersionReq>,
    /// `[[provides]]`.
    #[serde(default)]
    pub provides: Vec<Provision>,
    /// `[[requires]]`.
    #[serde(default)]
    pub requires: Vec<Requirement>,
    /// `[[contributions]]`.
    #[serde(default)]
    pub contributions: Vec<Contribution>,
    /// Capabilities requested. The user accepts a subset at install.
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    /// `[[profiles]]`.
    #[serde(default)]
    pub profiles: Vec<PermissionProfile>,
    /// `[skills]`.
    #[serde(default)]
    pub skills: SkillsSection,
}

/// Why a manifest was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    /// The TOML did not parse or did not match the schema.
    Parse(String),
    /// A module id was not `owner/repo`.
    BadId(String),
    /// A tier outside 1..=5.
    BadTier(u8),
    /// A semantic rule failed; the string names it.
    Invalid(String),
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ManifestError::Parse(e) => write!(f, "manifest does not parse: {e}"),
            ManifestError::BadId(s) => write!(f, "`{s}` is not a module id (owner/repo)"),
            ManifestError::BadTier(t) => write!(f, "ui tier {t} is not 1..=5"),
            ManifestError::Invalid(s) => write!(f, "manifest invalid: {s}"),
        }
    }
}
impl std::error::Error for ManifestError {}

impl Manifest {
    /// Parse `avada.toml` text and validate it.
    pub fn parse(text: &str) -> Result<Manifest, ManifestError> {
        let m: Manifest = toml::from_str(text).map_err(|e| ManifestError::Parse(e.to_string()))?;
        m.validate()?;
        Ok(m)
    }

    /// Serialize back to TOML (used by the install record and by tests).
    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).expect("manifest is always serializable")
    }

    /// The semantic rules that TOML typing cannot express.
    pub fn validate(&self) -> Result<(), ManifestError> {
        let inv = |s: String| Err(ManifestError::Invalid(s));
        if self.module.name.trim().is_empty() {
            return inv("module.name is empty".into());
        }
        if self.dependencies.contains_key(&self.module.id) {
            return inv("a module cannot depend on itself".into());
        }
        if self.distribution.commercial && self.distribution.issuer.is_none() {
            return inv("distribution.commercial requires distribution.issuer".into());
        }
        let mut ids = std::collections::BTreeSet::new();
        for c in &self.contributions {
            if c.id.is_empty()
                || !c
                    .id
                    .chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
            {
                return inv(format!(
                    "contribution id `{}` must be lowercase-kebab",
                    c.id
                ));
            }
            if !ids.insert((c.kind, c.id.as_str())) {
                return inv(format!("duplicate contribution `{}`", c.id));
            }
            let needs = match c.kind {
                ContributionKind::Rail => Capability::UiRail,
                ContributionKind::Pane => Capability::UiPane,
                ContributionKind::Command => Capability::UiCommands,
                ContributionKind::Prefs => Capability::UiPrefs,
                ContributionKind::Route => Capability::ControlRoute,
            };
            if !self.capabilities.contains(&needs) {
                return inv(format!(
                    "contribution `{}` ({:?}) needs capability `{}`",
                    c.id, c.kind, needs
                ));
            }
            let tier_ok = match c.kind {
                ContributionKind::Rail => matches!(c.tier, UiTier::Data | UiTier::Slot),
                ContributionKind::Pane => !matches!(c.tier, UiTier::Route),
                ContributionKind::Command | ContributionKind::Prefs => c.tier == UiTier::Data,
                ContributionKind::Route => c.tier == UiTier::Route,
            };
            if !tier_ok {
                return inv(format!(
                    "contribution `{}` ({:?}) cannot use tier {}",
                    c.id,
                    c.kind,
                    u8::from(c.tier)
                ));
            }
        }
        for p in &self.provides {
            if !is_shape(&p.shape) {
                return inv(format!(
                    "provides.shape `{}` is not dotted-lowercase",
                    p.shape
                ));
            }
        }
        for r in &self.requires {
            if !is_shape(&r.shape) {
                return inv(format!(
                    "requires.shape `{}` is not dotted-lowercase",
                    r.shape
                ));
            }
        }
        for pr in &self.profiles {
            for cap in pr.values.keys() {
                if !self.capabilities.contains(cap) {
                    return inv(format!(
                        "profile `{}` sets `{}` which the module does not request",
                        pr.name, cap
                    ));
                }
            }
        }
        if !self.skills.paths.is_empty()
            && !self.capabilities.contains(&Capability::SkillsMaterialize)
        {
            return inv("skills.paths requires capability `skills.materialize`".into());
        }
        Ok(())
    }

    /// The git tag this version is built from.
    pub fn tag(&self) -> String {
        format!("v{}", self.module.version)
    }

    /// `owner/repo` — the identity used in every record.
    pub fn id(&self) -> &ModuleId {
        &self.module.id
    }
}

/// `avada.files.tree` style: at least two dotted segments, lowercase, digits, `_`.
pub fn is_shape(s: &str) -> bool {
    let segs: Vec<&str> = s.split('.').collect();
    segs.len() >= 2
        && segs.iter().all(|seg| {
            !seg.is_empty()
                && seg
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const EXAMPLE: &str = r#"
# Top-level keys must come before the first table header; TOML attaches anything
# after a header to that table.
capabilities = ["fs.read", "workspace.read", "ui.rail", "ui.commands", "skills.materialize"]

[module]
id = "Acme/Avada-Files"
name = "Files"
version = "1.2.0"
description = "File browser"
publisher = "Acme"
contract = ">=1, <2"

[distribution]
kind = "source"

[dependencies]
"acme/avada-git" = "^1.0"

[[provides]]
shape = "avada.files.tree"
version = "1.0.0"

[[requires]]
shape = "avada.git.status"
version = "^1"

[[contributions]]
kind = "rail"
id = "files"
tier = 1
label = "Files"
icon = "icons/files.svg"

[[contributions]]
kind = "command"
id = "reveal"
tier = 1
label = "Reveal in Files"

[[profiles]]
name = "High security"
description = "Read only"
[profiles.values]
"fs.read" = "always"
"workspace.read" = "ask"

[skills]
paths = ["skills"]
"#;

    #[test]
    fn example_parses_and_round_trips() {
        let m = Manifest::parse(EXAMPLE).unwrap();
        assert_eq!(
            m.module.id.as_str(),
            "acme/avada-files",
            "ids are lowercased"
        );
        assert_eq!(m.tag(), "v1.2.0");
        assert_eq!(m.module.id.dir_name(), "acme__avada-files");
        assert_eq!(m.contributions.len(), 2);
        assert_eq!(m.contributions[0].tier, UiTier::Data);
        assert!(m.module.contract.matches(&Version::new(1, 0, 0)));
        assert!(!m.module.contract.matches(&Version::new(2, 0, 0)));
        let again = Manifest::parse(&m.to_toml()).unwrap();
        assert_eq!(again, m);
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(serde_json::from_str::<Manifest>(&json).unwrap(), m);
    }

    #[test]
    fn ids_are_owner_slash_repo() {
        for bad in ["acme", "a/b/c", "", "/x", "x/", "a b/c", "../x", "a/.."] {
            assert!(ModuleId::new(bad).is_err(), "{bad:?} should be rejected");
        }
        assert_eq!(
            ModuleId::new(" Acme/X.y_z-1 ").unwrap().as_str(),
            "acme/x.y_z-1"
        );
    }

    fn with(edit: impl FnOnce(&mut Manifest)) -> Result<(), ManifestError> {
        let mut m = Manifest::parse(EXAMPLE).unwrap();
        edit(&mut m);
        m.validate()
    }

    #[test]
    fn contributions_need_their_capability_and_a_legal_tier() {
        assert!(with(|m| m.capabilities.retain(|c| *c != Capability::UiRail)).is_err());
        assert!(with(|m| m.contributions[0].tier = UiTier::Pixels).is_err());
        assert!(with(|m| m.contributions[1].tier = UiTier::Slot).is_err());
        assert!(with(|m| m.contributions[1].id = "Reveal".into()).is_err());
        assert!(with(|m| m.contributions.push(m.contributions[0].clone())).is_err());
    }

    #[test]
    fn commercial_needs_an_issuer_and_self_dependency_is_refused() {
        assert!(with(|m| m.distribution.commercial = true).is_err());
        assert!(with(|m| {
            m.distribution.commercial = true;
            m.distribution.issuer = Some("https://avada.to".into());
        })
        .is_ok());
        assert!(with(|m| {
            m.dependencies
                .insert(m.module.id.clone(), VersionReq::parse("*").unwrap());
        })
        .is_err());
    }

    #[test]
    fn profiles_may_only_name_requested_capabilities() {
        assert!(with(|m| {
            m.profiles[0]
                .values
                .insert(Capability::ProcessSpawn, crate::caps::RightValue::Always);
        })
        .is_err());
    }

    #[test]
    fn shapes_are_dotted_lowercase() {
        assert!(is_shape("avada.files.tree"));
        assert!(is_shape("a.b"));
        assert!(!is_shape("files"));
        assert!(!is_shape("Avada.Files"));
        assert!(!is_shape("a..b"));
    }

    #[test]
    fn tier_outside_range_is_a_parse_error() {
        let bad = EXAMPLE.replace("tier = 1\nlabel = \"Files\"", "tier = 9\nlabel = \"Files\"");
        assert!(matches!(
            Manifest::parse(&bad),
            Err(ManifestError::Parse(_))
        ));
    }
}
