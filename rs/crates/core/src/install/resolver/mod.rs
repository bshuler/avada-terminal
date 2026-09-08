//! Dependency resolution for module installs (docs/marketplace.md §dependencies).
//!
//! Given the modules a request names ([`Root`]s), a [`Source`] of candidate versions
//! and manifests, what is installed, the user's provider [`Defaults`] and a
//! workspace's pins, [`resolve`] produces a [`Plan`]: every module version to have on
//! disk, in dependency order, and which provider answers each shaped requirement.
//! Identity dependencies (`[dependencies]`) and named shape requirements
//! (`[[requires]] provider = "..."`) go through pubgrub; unnamed shape requirements
//! are matched against the plan and the installed set afterwards.
//!
//! Version choice prefers what is installed, then the newest published version.
//! One version per module is the rule; two compatibility lines of a module are
//! allowed side by side only when two different roots need them (see
//! [`resolve`]). A refusal is a [`Conflict`] whose text names the modules and
//! ranges involved, rendered from pubgrub's derivation tree.

pub mod defaults;
mod provider;
pub mod ranges;
pub mod source;

pub use defaults::Defaults;
pub use source::{MemorySource, Source, SourceError};

use avada_module_sdk::manifest::{Manifest, ModuleId};
use provider::{Pkg, Provider};
use pubgrub::{DefaultStringReporter, PubGrubError, Reporter};
use semver::{Version, VersionReq};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// A module the request names, with the versions it accepts (an install by tag is
/// `=x.y.z`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    /// The module.
    pub id: ModuleId,
    /// Acceptable versions.
    pub version: VersionReq,
}

/// A version already in the install store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledVersion {
    /// The module.
    pub id: ModuleId,
    /// The version.
    pub version: Version,
    /// Whether it is the activated version (the one the host loads).
    pub active: bool,
    /// When it was installed, seconds since the epoch (earliest wins ties).
    pub installed_at: u64,
}

/// One module version the plan needs on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// The module.
    pub id: ModuleId,
    /// The version.
    pub version: Version,
    /// Its manifest.
    pub manifest: Manifest,
    /// Already installed: nothing to fetch or build.
    pub installed: bool,
    /// Named by the request (as opposed to pulled in as a dependency).
    pub root: bool,
}

/// Which provider(s) answer one `[[requires]]` of a planned module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderChoice {
    /// The module with the requirement.
    pub requirer: ModuleId,
    /// The shape.
    pub shape: String,
    /// The shape versions accepted.
    pub version: VersionReq,
    /// Whether the requirement named its provider.
    pub named: bool,
    /// The providers chosen, in preference order; more than one only for `multi`.
    pub providers: Vec<(ModuleId, Version)>,
}

/// The outcome of a successful resolution.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plan {
    /// Every module version needed, dependencies before dependents.
    pub steps: Vec<Step>,
    /// Provider choices for every shaped requirement of every step.
    pub providers: Vec<ProviderChoice>,
    /// Context gathered while resolving (pins honoured, and the like).
    pub notes: Vec<String>,
}

impl Plan {
    /// The steps that are not installed yet, in order.
    pub fn to_install(&self) -> impl Iterator<Item = &Step> {
        self.steps.iter().filter(|s| !s.installed)
    }

    /// The step for a module version, if planned.
    pub fn step(&self, id: &ModuleId, version: &Version) -> Option<&Step> {
        self.steps
            .iter()
            .find(|s| &s.id == id && &s.version == version)
    }
}

/// Why resolution was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictKind {
    /// No set of versions satisfies every requirement.
    NoSolution,
    /// An unnamed shape requirement has no installed or planned provider.
    NoProvider,
    /// The source could not answer.
    Source,
}

/// A refusal, with the reasoning spelled out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// The category.
    pub kind: ConflictKind,
    /// One line naming what could not be satisfied.
    pub message: String,
    /// pubgrub's derivation, one sentence per line, for [`ConflictKind::NoSolution`].
    pub derivation: Option<String>,
    /// Context (pins, modules with no versions, shapes nobody provides).
    pub notes: Vec<String>,
}

impl fmt::Display for Conflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)?;
        if let Some(d) = &self.derivation {
            for line in d.lines() {
                write!(f, "\n  {line}")?;
            }
        }
        for n in &self.notes {
            write!(f, "\n  note: {n}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Conflict {}

/// Resolve `roots` into a [`Plan`].
///
/// Pass one solves with one version per module. When that has no solution the
/// solver runs again with every compatibility line (`2.x`, `0.3.x`) of a module
/// treated as its own package, and the result is accepted only if no single root
/// reaches two lines of the same module through its own dependencies: two roots
/// may disagree on a major, one module's dependency graph may not. Otherwise the
/// first pass's derivation is the answer.
pub fn resolve(
    roots: &[Root],
    source: &dyn Source,
    installed: &[InstalledVersion],
    defaults: &Defaults,
    pins: &BTreeMap<ModuleId, Version>,
) -> Result<Plan, Conflict> {
    let one = Provider::new(source, installed, roots, pins, false);
    let first = match solve(&one) {
        Ok(selected) => return finish(&one, roots, installed, defaults, selected),
        Err(c) if c.kind == ConflictKind::NoSolution => c,
        Err(c) => return Err(c),
    };
    let split = Provider::new(source, installed, roots, pins, true);
    let Ok(selected) = solve(&split) else {
        return Err(first);
    };
    if let Some(note) = shared_module_on_two_lines(&split, &selected) {
        let mut first = first;
        first.notes.push(note);
        return Err(first);
    }
    let mut plan = finish(&split, roots, installed, defaults, selected)?;
    plan.notes.push(
        "two requested modules need incompatible versions of a shared dependency; \
         both versions are installed side by side"
            .to_string(),
    );
    Ok(plan)
}

type Selected = Vec<(Pkg, Version)>;

/// Run pubgrub, returning the selected packages sorted for determinism.
fn solve(provider: &Provider<'_>) -> Result<Selected, Conflict> {
    match pubgrub::resolve(provider, Pkg::Root, Version::new(0, 0, 0)) {
        Ok(selected) => {
            let mut list: Selected = selected
                .into_iter()
                .filter(|(p, _)| *p != Pkg::Root)
                .collect();
            list.sort();
            Ok(list)
        }
        Err(PubGrubError::NoSolution(mut tree)) => {
            tree.collapse_no_versions();
            let roots: Vec<String> = provider
                .roots()
                .iter()
                .map(|r| format!("{} {}", r.id, r.version))
                .collect();
            Err(Conflict {
                kind: ConflictKind::NoSolution,
                message: format!(
                    "cannot install {}: no set of versions satisfies every requirement",
                    roots.join(", ")
                ),
                derivation: Some(DefaultStringReporter::report(&tree)),
                notes: provider.notes(),
            })
        }
        Err(e) => Err(Conflict {
            kind: ConflictKind::Source,
            message: e.to_string(),
            derivation: None,
            notes: provider.notes(),
        }),
    }
}

/// In a split solution: the first root whose transitive dependencies contain two
/// compatibility lines of one module, as a note.
fn shared_module_on_two_lines(provider: &Provider<'_>, selected: &Selected) -> Option<String> {
    let edges = provider.edges();
    let version_of = |p: &Pkg| {
        selected
            .iter()
            .find(|(q, _)| q == p)
            .map(|(_, v)| v.clone())
    };
    let root_deps = edges
        .get(&(Pkg::Root, Version::new(0, 0, 0)))
        .cloned()
        .unwrap_or_default();
    for root in root_deps {
        let mut seen: Vec<Pkg> = Vec::new();
        let mut stack = vec![root.clone()];
        while let Some(p) = stack.pop() {
            if seen.contains(&p) {
                continue;
            }
            seen.push(p.clone());
            if let Some(v) = version_of(&p) {
                if let Some(next) = edges.get(&(p, v)) {
                    stack.extend(next.iter().cloned());
                }
            }
        }
        let mut by_id: BTreeMap<&ModuleId, Vec<&Pkg>> = BTreeMap::new();
        for p in &seen {
            if let Some(id) = p.id() {
                by_id.entry(id).or_default().push(p);
            }
        }
        for (id, pkgs) in by_id {
            if pkgs.len() > 1 {
                let lines: Vec<String> = pkgs.iter().map(|p| p.to_string()).collect();
                return Some(format!(
                    "{} alone would need {id} on two lines ({}); one module's dependencies \
                     cannot mix versions",
                    root.id().map(|r| r.to_string()).unwrap_or_default(),
                    lines.join(" and ")
                ));
            }
        }
    }
    None
}

/// Order the selection, then match unnamed shape requirements.
fn finish(
    provider: &Provider<'_>,
    roots: &[Root],
    installed: &[InstalledVersion],
    defaults: &Defaults,
    selected: Selected,
) -> Result<Plan, Conflict> {
    let edges = provider.edges();
    let version_of = |p: &Pkg| {
        selected
            .iter()
            .find(|(q, _)| q == p)
            .map(|(_, v)| v.clone())
    };
    let root_deps = edges
        .get(&(Pkg::Root, Version::new(0, 0, 0)))
        .cloned()
        .unwrap_or_default();

    // Dependencies first: post-order over the edges pubgrub walked.
    let mut order: Vec<Pkg> = Vec::new();
    let mut visiting: Vec<Pkg> = Vec::new();
    fn visit(
        p: &Pkg,
        edges: &BTreeMap<(Pkg, Version), Vec<Pkg>>,
        version_of: &dyn Fn(&Pkg) -> Option<Version>,
        order: &mut Vec<Pkg>,
        visiting: &mut Vec<Pkg>,
    ) {
        if order.contains(p) || visiting.contains(p) {
            return;
        }
        visiting.push(p.clone());
        if let Some(v) = version_of(p) {
            if let Some(next) = edges.get(&(p.clone(), v)) {
                for n in next {
                    visit(n, edges, version_of, order, visiting);
                }
            }
        }
        visiting.pop();
        order.push(p.clone());
    }
    for r in &root_deps {
        visit(r, &edges, &version_of, &mut order, &mut visiting);
    }
    // Anything selected but not reached from a root (should not happen) goes last.
    for (p, _) in &selected {
        if !order.contains(p) {
            order.push(p.clone());
        }
    }

    let mut steps = Vec::new();
    for p in &order {
        let (Some(id), Some(version)) = (p.id(), version_of(p)) else {
            continue;
        };
        let manifest = provider
            .manifest(id, &version)
            .map_err(|e| source_conflict(provider, e))?
            .ok_or_else(|| Conflict {
                kind: ConflictKind::Source,
                message: format!("{id} {version} was selected but has no manifest"),
                derivation: None,
                notes: provider.notes(),
            })?;
        steps.push(Step {
            id: id.clone(),
            version: version.clone(),
            manifest,
            installed: provider.is_installed(id, &version),
            root: root_deps.contains(p) && roots.iter().any(|r| &r.id == id),
        });
    }

    // Shaped requirements: named ones read off the solution, unnamed ones matched
    // against the plan and the active installed set.
    let mut providers = Vec::new();
    for (p, step) in order.iter().zip(steps.iter()) {
        for r in &step.manifest.requires {
            let chosen = match &r.provider {
                Some(named) => {
                    let dep = edges
                        .get(&(p.clone(), step.version.clone()))
                        .and_then(|next| next.iter().find(|q| q.id() == Some(named)))
                        .and_then(|q| version_of(q).map(|v| (named.clone(), v)));
                    dep.into_iter().collect()
                }
                None => choose_unnamed(
                    provider, &steps, installed, defaults, &r.shape, &r.version, r.multi,
                )
                .map_err(|e| source_conflict(provider, e))?,
            };
            if chosen.is_empty() {
                return Err(Conflict {
                    kind: ConflictKind::NoProvider,
                    message: format!(
                        "{} {} requires {} {} but nothing installed provides it and no \
                         provider is named (install a provider first)",
                        step.id, step.version, r.shape, r.version
                    ),
                    derivation: None,
                    notes: provider.notes(),
                });
            }
            providers.push(ProviderChoice {
                requirer: step.id.clone(),
                shape: r.shape.clone(),
                version: r.version.clone(),
                named: r.provider.is_some(),
                providers: chosen,
            });
        }
    }

    Ok(Plan {
        steps,
        providers,
        notes: provider.notes(),
    })
}

fn source_conflict(provider: &Provider<'_>, e: SourceError) -> Conflict {
    Conflict {
        kind: ConflictKind::Source,
        message: e.to_string(),
        derivation: None,
        notes: provider.notes(),
    }
}

/// Providers of `shape` within `req` among the plan and the active installed
/// modules, ordered: the user's default first, then the highest shape version,
/// then the highest module version, then the earliest install, then id. `multi`
/// keeps them all; otherwise only the first.
fn choose_unnamed(
    provider: &Provider<'_>,
    steps: &[Step],
    installed: &[InstalledVersion],
    defaults: &Defaults,
    shape: &str,
    req: &VersionReq,
    multi: bool,
) -> Result<Vec<(ModuleId, Version)>, SourceError> {
    struct Found {
        id: ModuleId,
        version: Version,
        shape_version: Version,
        installed_at: u64,
    }
    let planned: BTreeSet<&ModuleId> = steps.iter().map(|s| &s.id).collect();
    let installed_at = |id: &ModuleId, v: &Version| {
        installed
            .iter()
            .find(|i| &i.id == id && &i.version == v)
            .map(|i| i.installed_at)
            .unwrap_or(u64::MAX)
    };
    let mut found: Vec<Found> = Vec::new();
    let mut consider = |id: &ModuleId, version: &Version, manifest: &Manifest| {
        for p in &manifest.provides {
            if p.shape == shape && req.matches(&p.version) {
                found.push(Found {
                    id: id.clone(),
                    version: version.clone(),
                    shape_version: p.version.clone(),
                    installed_at: installed_at(id, version),
                });
            }
        }
    };
    for s in steps {
        consider(&s.id, &s.version, &s.manifest);
    }
    for i in installed
        .iter()
        .filter(|i| i.active && !planned.contains(&i.id))
    {
        if let Some(m) = provider.manifest(&i.id, &i.version)? {
            consider(&i.id, &i.version, &m);
        }
    }
    found.sort_by(|a, b| {
        let da = defaults.is_default(shape, &a.id);
        let db = defaults.is_default(shape, &b.id);
        db.cmp(&da)
            .then(b.shape_version.cmp(&a.shape_version))
            .then(b.version.cmp(&a.version))
            .then(a.installed_at.cmp(&b.installed_at))
            .then(a.id.cmp(&b.id))
    });
    let mut out: Vec<(ModuleId, Version)> = found.into_iter().map(|f| (f.id, f.version)).collect();
    if !multi {
        out.truncate(1);
    }
    Ok(out)
}

// ---- pins

/// What one installed module needs of another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DemandKind {
    /// An identity dependency on the module.
    Identity(VersionReq),
    /// A shaped requirement naming the module as provider.
    Shape {
        /// The shape.
        shape: String,
        /// Accepted shape versions.
        version: VersionReq,
    },
}

/// A requirement one module places on the module being pinned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Demand {
    /// The module with the requirement.
    pub requirer: ModuleId,
    /// Its version.
    pub requirer_version: Version,
    /// The requirement.
    pub kind: DemandKind,
}

impl Demand {
    fn satisfied_by(&self, manifest: &Manifest) -> bool {
        match &self.kind {
            DemandKind::Identity(req) => req.matches(&manifest.module.version),
            DemandKind::Shape { shape, version } => manifest
                .provides
                .iter()
                .any(|p| &p.shape == shape && version.matches(&p.version)),
        }
    }

    fn describe(&self, pinned: &ModuleId) -> String {
        match &self.kind {
            DemandKind::Identity(req) => format!(
                "{} {} needs {pinned} {req}",
                self.requirer, self.requirer_version
            ),
            DemandKind::Shape { shape, version } => format!(
                "{} {} needs {shape} {version} from {pinned}",
                self.requirer, self.requirer_version
            ),
        }
    }
}

/// A pin that would break an enabled module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinConflict {
    /// Each unmet demand, spelled out.
    pub conflicts: Vec<String>,
    /// The nearest candidate (up or down) meeting every demand, if any.
    pub nearest: Option<Version>,
    /// The whole story in one string.
    pub message: String,
}

impl fmt::Display for PinConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// Check that pinning `id` to `version` keeps every `demand` met. `candidates` are
/// the installed versions of `id` with their manifests (the pinned one among them);
/// on conflict the nearest candidate satisfying every demand is offered, the
/// closer of up and down, the higher on a tie.
pub fn check_pin(
    id: &ModuleId,
    version: &Version,
    candidates: &[(Version, Manifest)],
    demands: &[Demand],
) -> Result<(), PinConflict> {
    let mut sorted: Vec<&(Version, Manifest)> = candidates.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let Some(at) = sorted.iter().position(|(v, _)| v == version) else {
        return Err(PinConflict {
            conflicts: vec![format!("{id} {version} is not installed")],
            nearest: None,
            message: format!("cannot pin {id} to {version}: that version is not installed"),
        });
    };
    let conflicts: Vec<String> = demands
        .iter()
        .filter(|d| !d.satisfied_by(&sorted[at].1))
        .map(|d| d.describe(id))
        .collect();
    if conflicts.is_empty() {
        return Ok(());
    }
    let nearest = sorted
        .iter()
        .enumerate()
        .filter(|(i, (_, m))| *i != at && demands.iter().all(|d| d.satisfied_by(m)))
        .min_by_key(|(i, _)| (i.abs_diff(at), std::cmp::Reverse(*i)))
        .map(|(_, (v, _))| v.clone());
    let offer = match &nearest {
        Some(n) if n > version => {
            format!("; the nearest installed version that keeps them working is {n} (upgrade)")
        }
        Some(n) => {
            format!("; the nearest installed version that keeps them working is {n} (downgrade)")
        }
        None => "; no installed version satisfies every one of them".to_string(),
    };
    Err(PinConflict {
        message: format!(
            "cannot pin {id} to {version}: {}{offer}",
            conflicts.join("; ")
        ),
        conflicts,
        nearest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> ModuleId {
        ModuleId::new(s).unwrap()
    }

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    fn req(s: &str) -> VersionReq {
        VersionReq::parse(s).unwrap()
    }

    /// A manifest for `id` at `version`; `extra` is appended TOML (dependencies,
    /// provides, requires).
    fn m(id: &str, version: &str, extra: &str) -> Manifest {
        let text = format!(
            "[module]\nid = \"{id}\"\nname = \"x\"\nversion = \"{version}\"\ndescription = \"d\"\n\
             publisher = \"p\"\ncontract = \"^1\"\n\n[distribution]\nkind = \"source\"\n\n{extra}"
        );
        Manifest::parse(&text).unwrap_or_else(|e| panic!("{text}: {e}"))
    }

    fn deps(pairs: &[(&str, &str)]) -> String {
        let mut s = String::from("[dependencies]\n");
        for (d, r) in pairs {
            s.push_str(&format!("\"{d}\" = \"{r}\"\n"));
        }
        s
    }

    fn provides(shape: &str, version: &str) -> String {
        format!("[[provides]]\nshape = \"{shape}\"\nversion = \"{version}\"\n")
    }

    fn requires(shape: &str, version: &str, provider: Option<&str>, multi: bool) -> String {
        let mut s = format!("[[requires]]\nshape = \"{shape}\"\nversion = \"{version}\"\n");
        if let Some(p) = provider {
            s.push_str(&format!("provider = \"{p}\"\n"));
        }
        if multi {
            s.push_str("multi = true\n");
        }
        s
    }

    fn inst(i: &str, ver: &str, active: bool, at: u64) -> InstalledVersion {
        InstalledVersion {
            id: id(i),
            version: v(ver),
            active,
            installed_at: at,
        }
    }

    fn root(i: &str, r: &str) -> Root {
        Root {
            id: id(i),
            version: req(r),
        }
    }

    fn plan(roots: &[Root], source: &MemorySource, installed: &[InstalledVersion]) -> Plan {
        resolve(
            roots,
            source,
            installed,
            &Defaults::default(),
            &BTreeMap::new(),
        )
        .unwrap_or_else(|c| panic!("{c}"))
    }

    fn ids(plan: &Plan) -> Vec<String> {
        plan.steps
            .iter()
            .map(|s| format!("{} {}", s.id, s.version))
            .collect()
    }

    #[test]
    fn identity_dependencies_come_first_and_prefer_installed_versions() {
        let src = MemorySource::new();
        src.add(m(
            "acme/app",
            "1.0.0",
            &deps(&[("acme/lib", "^1"), ("acme/util", "^2")]),
        ))
        .add(m("acme/lib", "1.0.0", ""))
        .add(m("acme/lib", "1.5.0", ""))
        .add(m("acme/lib", "2.0.0", ""))
        .add(m("acme/util", "2.1.0", &deps(&[("acme/lib", ">=1.2")])))
        .add(m("acme/util", "2.0.0", ""));
        // Nothing installed: newest in range everywhere.
        let p = plan(&[root("acme/app", "=1.0.0")], &src, &[]);
        assert_eq!(
            ids(&p),
            ["acme/lib 1.5.0", "acme/util 2.1.0", "acme/app 1.0.0"]
        );
        assert!(p.steps[2].root && !p.steps[0].root);
        assert!(p.to_install().count() == 3);
        // lib 1.0.0 installed: util ^2 needs lib >=1.2 so the installed one cannot
        // stand; but with util 2.0.0 (no such need) installed, it is kept.
        let p = plan(
            &[root("acme/app", "=1.0.0")],
            &src,
            &[
                inst("acme/lib", "1.0.0", true, 1),
                inst("acme/util", "2.0.0", true, 2),
            ],
        );
        assert_eq!(
            ids(&p),
            ["acme/lib 1.0.0", "acme/util 2.0.0", "acme/app 1.0.0"]
        );
        let to_install: Vec<_> = p.to_install().map(|s| s.id.as_str()).collect();
        assert_eq!(to_install, ["acme/app"]);
        assert!(p.step(&id("acme/lib"), &v("1.0.0")).unwrap().installed);
    }

    #[test]
    fn prereleases_are_offered_only_when_named_exactly() {
        let src = MemorySource::new();
        src.add(m("acme/app", "1.0.0", &deps(&[("acme/lib", "^1")])))
            .add(m("acme/lib", "1.0.0", ""))
            .add(m("acme/lib", "1.1.0-rc.1", ""));
        let p = plan(&[root("acme/app", "=1.0.0")], &src, &[]);
        assert_eq!(ids(&p), ["acme/lib 1.0.0", "acme/app 1.0.0"]);
        let p = plan(&[root("acme/lib", "=1.1.0-rc.1")], &src, &[]);
        assert_eq!(ids(&p), ["acme/lib 1.1.0-rc.1"]);
    }

    #[test]
    fn named_provider_is_chosen_by_the_shape_version_it_provides() {
        let src = MemorySource::new();
        src.add(m(
            "acme/git",
            "1.0.0",
            &requires("avada.files.tree", "^2", Some("acme/files"), false),
        ))
        .add(m(
            "acme/files",
            "1.0.0",
            &provides("avada.files.tree", "1.0.0"),
        ))
        .add(m(
            "acme/files",
            "1.4.0",
            &provides("avada.files.tree", "2.0.0"),
        ))
        .add(m(
            "acme/files",
            "1.6.0",
            &provides("avada.files.tree", "2.1.0"),
        ))
        // The newest tag dropped the shape version we need.
        .add(m(
            "acme/files",
            "1.9.0",
            &provides("avada.files.tree", "3.0.0"),
        ));
        let p = plan(&[root("acme/git", "=1.0.0")], &src, &[]);
        assert_eq!(ids(&p), ["acme/files 1.6.0", "acme/git 1.0.0"]);
        assert_eq!(
            p.providers,
            vec![ProviderChoice {
                requirer: id("acme/git"),
                shape: "avada.files.tree".into(),
                version: req("^2"),
                named: true,
                providers: vec![(id("acme/files"), v("1.6.0"))],
            }]
        );
        // An installed version that provides it is kept over the newer tag.
        let p = plan(
            &[root("acme/git", "=1.0.0")],
            &src,
            &[inst("acme/files", "1.4.0", true, 1)],
        );
        assert_eq!(ids(&p), ["acme/files 1.4.0", "acme/git 1.0.0"]);
        // Nobody provides the shape at that version: a conflict naming both.
        let src2 = MemorySource::new();
        src2.add(m(
            "acme/git",
            "1.0.0",
            &requires("avada.files.tree", "^9", Some("acme/files"), false),
        ))
        .add(m(
            "acme/files",
            "1.0.0",
            &provides("avada.files.tree", "1.0.0"),
        ));
        let c = resolve(
            &[root("acme/git", "=1.0.0")],
            &src2,
            &[],
            &Defaults::default(),
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert_eq!(c.kind, ConflictKind::NoSolution);
        let text = c.to_string();
        assert!(text.contains("acme/git"), "{text}");
        assert!(text.contains("acme/files"), "{text}");
        assert!(
            text.contains("no version of acme/files provides it"),
            "{text}"
        );
    }

    #[test]
    fn unnamed_provider_order_is_default_then_shape_version_then_earliest_install() {
        let src = MemorySource::new();
        src.add(m(
            "acme/git",
            "1.0.0",
            &requires("avada.files.tree", "^1", None, false),
        ))
        .add(m("acme/a", "1.0.0", &provides("avada.files.tree", "1.0.0")))
        .add(m("acme/b", "1.0.0", &provides("avada.files.tree", "1.2.0")))
        .add(m("acme/c", "1.0.0", &provides("avada.files.tree", "1.2.0")))
        .add(m("acme/d", "1.0.0", &provides("avada.files.tree", "2.0.0")));
        let installed = [
            inst("acme/a", "1.0.0", true, 10),
            inst("acme/b", "1.0.0", true, 30),
            inst("acme/c", "1.0.0", true, 20),
            inst("acme/d", "1.0.0", true, 5),
        ];
        let chosen = |defaults: &Defaults| {
            resolve(
                &[root("acme/git", "=1.0.0")],
                &src,
                &installed,
                defaults,
                &BTreeMap::new(),
            )
            .unwrap()
            .providers
            .remove(0)
            .providers
        };
        // Highest shape version (b, c at 1.2.0), then earliest install: c.
        assert_eq!(
            chosen(&Defaults::default()),
            vec![(id("acme/c"), v("1.0.0"))]
        );
        // The user's default beats everything.
        let mut d = Defaults::default();
        d.set_default("avada.files.tree", &id("acme/a"));
        assert_eq!(chosen(&d), vec![(id("acme/a"), v("1.0.0"))]);
        // A default that does not provide the shape in range is ignored.
        d.set_default("avada.files.tree", &id("acme/d"));
        assert_eq!(chosen(&d), vec![(id("acme/c"), v("1.0.0"))]);
        // Nobody installed: refused, naming the shape.
        let c = resolve(
            &[root("acme/git", "=1.0.0")],
            &src,
            &[],
            &Defaults::default(),
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert_eq!(c.kind, ConflictKind::NoProvider);
        assert!(
            c.message.contains("requires avada.files.tree ^1 but nothing installed provides it and no provider is named"),
            "{c}"
        );
    }

    #[test]
    fn multi_keeps_every_provider_and_planned_modules_count() {
        let src = MemorySource::new();
        src.add(m(
            "acme/git",
            "1.0.0",
            &format!(
                "{}{}",
                deps(&[("acme/b", "^1")]),
                requires("avada.files.tree", "^1", None, true)
            ),
        ))
        .add(m("acme/a", "1.0.0", &provides("avada.files.tree", "1.0.0")))
        .add(m("acme/b", "1.0.0", &provides("avada.files.tree", "1.1.0")))
        .add(m(
            "acme/old",
            "1.0.0",
            &provides("avada.files.tree", "0.9.0"),
        ));
        let installed = [
            inst("acme/a", "1.0.0", true, 10),
            inst("acme/old", "1.0.0", true, 1),
        ];
        let p = plan(&[root("acme/git", "=1.0.0")], &src, &installed);
        assert_eq!(ids(&p), ["acme/b 1.0.0", "acme/git 1.0.0"]);
        assert_eq!(
            p.providers[0].providers,
            vec![(id("acme/b"), v("1.0.0")), (id("acme/a"), v("1.0.0"))]
        );
    }

    #[test]
    fn conflicts_render_the_derivation_with_modules_and_ranges() {
        let src = MemorySource::new();
        src.add(m(
            "acme/app",
            "1.0.0",
            &deps(&[("acme/lib", "^1"), ("acme/util", "^1")]),
        ))
        .add(m("acme/util", "1.0.0", &deps(&[("acme/lib", "^2")])))
        .add(m("acme/lib", "1.0.0", ""))
        .add(m("acme/lib", "2.0.0", ""));
        let c = resolve(
            &[root("acme/app", "=1.0.0")],
            &src,
            &[],
            &Defaults::default(),
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert_eq!(c.kind, ConflictKind::NoSolution);
        let text = c.to_string();
        assert!(text.starts_with("cannot install acme/app =1.0.0"), "{text}");
        let d = c.derivation.clone().unwrap();
        assert!(d.contains("acme/util"), "{d}");
        assert!(d.contains("acme/lib >=2.0.0, <3.0.0"), "{d}");
        assert!(d.contains("acme/lib >=1.0.0, <2.0.0"), "{d}");
        // One root: the shared dependency may not be split, and the note says so.
        assert!(
            c.notes.iter().any(|n| n.contains("acme/lib on two lines")),
            "{c}"
        );
        // Unknown module: named as such.
        let c = resolve(
            &[root("acme/none", "^1")],
            &src,
            &[],
            &Defaults::default(),
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert!(c.to_string().contains("acme/none"), "{c}");
        assert!(
            c.notes.iter().any(|n| n.contains("has no vX.Y.Z tag")),
            "{c}"
        );
    }

    #[test]
    fn two_roots_may_take_incompatible_majors_side_by_side() {
        let src = MemorySource::new();
        src.add(m("acme/one", "1.0.0", &deps(&[("acme/lib", "^1")])))
            .add(m("acme/two", "1.0.0", &deps(&[("acme/lib", "^2")])))
            .add(m("acme/lib", "1.3.0", ""))
            .add(m("acme/lib", "2.0.0", ""));
        let p = plan(&[root("acme/one", "^1"), root("acme/two", "^1")], &src, &[]);
        assert_eq!(
            ids(&p),
            [
                "acme/lib 1.3.0",
                "acme/one 1.0.0",
                "acme/lib 2.0.0",
                "acme/two 1.0.0"
            ]
        );
        assert!(p.notes.iter().any(|n| n.contains("side by side")), "{p:?}");
        // Add a shared helper both roots use: still fine, one version of it.
        src.add(m(
            "acme/one",
            "1.1.0",
            &deps(&[("acme/lib", "^1"), ("acme/h", "^1")]),
        ))
        .add(m(
            "acme/two",
            "1.1.0",
            &deps(&[("acme/lib", "^2"), ("acme/h", "^1")]),
        ))
        .add(m("acme/h", "1.0.0", ""));
        let p = plan(&[root("acme/one", "^1"), root("acme/two", "^1")], &src, &[]);
        assert_eq!(
            p.steps.iter().filter(|s| s.id.as_str() == "acme/h").count(),
            1
        );
    }

    #[test]
    fn pins_restrict_a_module_to_the_pinned_version() {
        let src = MemorySource::new();
        src.add(m("acme/app", "1.0.0", &deps(&[("acme/lib", "^1")])))
            .add(m("acme/lib", "1.0.0", ""))
            .add(m("acme/lib", "1.5.0", ""));
        let installed = [
            inst("acme/lib", "1.0.0", false, 1),
            inst("acme/lib", "1.5.0", true, 2),
        ];
        let pins = BTreeMap::from([(id("acme/lib"), v("1.0.0"))]);
        let p = resolve(
            &[root("acme/app", "=1.0.0")],
            &src,
            &installed,
            &Defaults::default(),
            &pins,
        )
        .unwrap();
        assert_eq!(ids(&p), ["acme/lib 1.0.0", "acme/app 1.0.0"]);
        assert!(
            p.notes.iter().any(|n| n.contains("pinned to 1.0.0")),
            "{p:?}"
        );
        // A pin outside the range is a conflict that mentions the pin.
        src.add(m("acme/app", "2.0.0", &deps(&[("acme/lib", "^1.2")])));
        let c = resolve(
            &[root("acme/app", "=2.0.0")],
            &src,
            &installed,
            &Defaults::default(),
            &pins,
        )
        .unwrap_err();
        assert!(c.to_string().contains("pinned to 1.0.0"), "{c}");
        assert!(c.to_string().contains("acme/lib >=1.2.0, <2.0.0"), "{c}");
    }

    #[test]
    fn manifests_are_fetched_lazily() {
        let src = MemorySource::new();
        src.add(m(
            "acme/git",
            "1.0.0",
            &requires("avada.files.tree", "^1", Some("acme/files"), false),
        ));
        for ver in ["1.0.0", "1.1.0", "1.2.0", "1.3.0"] {
            src.add(m("acme/files", ver, &provides("avada.files.tree", "1.0.0")));
        }
        plan(&[root("acme/git", "=1.0.0")], &src, &[]);
        let files_fetches = src
            .fetches()
            .iter()
            .filter(|(i, _)| i.as_str() == "acme/files")
            .count();
        assert_eq!(files_fetches, 1, "{:?}", src.fetches());
    }

    #[test]
    fn pin_check_names_the_conflict_and_the_nearest_version() {
        let lib = |ver: &str, shape_ver: &str| {
            (
                v(ver),
                m("acme/lib", ver, &provides("avada.files.tree", shape_ver)),
            )
        };
        let candidates = [
            lib("1.0.0", "1.0.0"),
            lib("1.2.0", "1.0.0"),
            lib("1.4.0", "2.0.0"),
            lib("2.0.0", "2.0.0"),
            lib("2.1.0", "2.1.0"),
        ];
        let demands = [
            Demand {
                requirer: id("acme/app"),
                requirer_version: v("3.0.0"),
                kind: DemandKind::Identity(req("^1.2")),
            },
            Demand {
                requirer: id("acme/git"),
                requirer_version: v("1.0.0"),
                kind: DemandKind::Shape {
                    shape: "avada.files.tree".into(),
                    version: req("^2"),
                },
            },
        ];
        assert!(check_pin(&id("acme/lib"), &v("1.4.0"), &candidates, &demands).is_ok());
        let e = check_pin(&id("acme/lib"), &v("1.0.0"), &candidates, &demands).unwrap_err();
        assert_eq!(e.nearest, Some(v("1.4.0")));
        assert_eq!(
            e.message,
            "cannot pin acme/lib to 1.0.0: acme/app 3.0.0 needs acme/lib ^1.2; \
             acme/git 1.0.0 needs avada.files.tree ^2 from acme/lib; the nearest installed \
             version that keeps them working is 1.4.0 (upgrade)"
        );
        let e = check_pin(&id("acme/lib"), &v("2.1.0"), &candidates, &demands).unwrap_err();
        assert_eq!(e.nearest, Some(v("1.4.0")));
        assert!(e.message.ends_with("1.4.0 (downgrade)"), "{}", e.message);
        assert_eq!(e.conflicts.len(), 1);
        // Nothing fits: say so.
        let only_old = [lib("1.0.0", "1.0.0"), lib("1.1.0", "1.0.0")];
        let e = check_pin(&id("acme/lib"), &v("1.0.0"), &only_old, &demands).unwrap_err();
        assert_eq!(e.nearest, None);
        assert!(
            e.message.contains("no installed version satisfies"),
            "{}",
            e.message
        );
        // A version that is not installed cannot be pinned.
        let e = check_pin(&id("acme/lib"), &v("9.0.0"), &candidates, &demands).unwrap_err();
        assert!(e.message.contains("not installed"));
    }
}
