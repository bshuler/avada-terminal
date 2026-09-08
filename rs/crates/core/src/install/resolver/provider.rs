//! The pubgrub `DependencyProvider` over a [`Source`].
//!
//! Packages are module ids (plus, in the side-by-side retry, the compatibility line
//! the requirement was mapped to); versions are `semver::Version`; version sets are
//! `pubgrub::Ranges<Version>`. A synthetic root package depends on every requested
//! module, so one `resolve` call covers a whole install request.
//!
//! Version preference (`choose_version`): the active installed version, then other
//! installed versions newest first, then published versions newest first. Pinned
//! modules offer only their pin. Pre-releases are offered only when asked for
//! exactly (a root installed by tag, or a pin).

use super::ranges::{is_prerelease, ranges_of, Compat};
use super::source::{Source, SourceError};
use super::{InstalledVersion, Root};
use avada_module_sdk::manifest::{Manifest, ModuleId};
use pubgrub::{
    Dependencies, DependencyConstraints, DependencyProvider, PackageResolutionStatistics, Ranges,
};
use semver::{Version, VersionReq};
use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// A pubgrub package.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) enum Pkg {
    /// The install request itself; depends on every root.
    Root,
    /// A module, optionally narrowed to one compatibility line (side-by-side retry).
    Module { id: ModuleId, line: Option<Compat> },
}

impl fmt::Display for Pkg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Pkg::Root => f.write_str("the install request"),
            Pkg::Module { id, line: None } => write!(f, "{id}"),
            Pkg::Module {
                id,
                line: Some(line),
            } => write!(f, "{id} ({line})"),
        }
    }
}

impl Pkg {
    /// The module, for anything but the root.
    pub(super) fn id(&self) -> Option<&ModuleId> {
        match self {
            Pkg::Root => None,
            Pkg::Module { id, .. } => Some(id),
        }
    }
}

/// One candidate version of a module and where it came from.
#[derive(Debug, Clone)]
struct Candidate {
    version: Version,
    installed: bool,
}

/// The provider. Interior mutability caches source answers and records the
/// dependency edges pubgrub walked, for the plan's install order.
pub(super) struct Provider<'a> {
    source: &'a dyn Source,
    installed: &'a [InstalledVersion],
    roots: &'a [Root],
    pins: &'a BTreeMap<ModuleId, Version>,
    /// Retry mode: each compatibility line of a module is its own package.
    split: bool,
    candidates: RefCell<BTreeMap<ModuleId, Vec<Candidate>>>,
    manifests: RefCell<BTreeMap<(ModuleId, Version), Option<Manifest>>>,
    edges: RefCell<BTreeMap<(Pkg, Version), Vec<Pkg>>>,
    notes: RefCell<BTreeSet<String>>,
}

impl<'a> Provider<'a> {
    pub(super) fn new(
        source: &'a dyn Source,
        installed: &'a [InstalledVersion],
        roots: &'a [Root],
        pins: &'a BTreeMap<ModuleId, Version>,
        split: bool,
    ) -> Self {
        Provider {
            source,
            installed,
            roots,
            pins,
            split,
            candidates: RefCell::new(BTreeMap::new()),
            manifests: RefCell::new(BTreeMap::new()),
            edges: RefCell::new(BTreeMap::new()),
            notes: RefCell::new(BTreeSet::new()),
        }
    }

    /// Human notes gathered while resolving (pins, unknown modules, shapes nobody
    /// provides): context for a conflict report.
    pub(super) fn notes(&self) -> Vec<String> {
        self.notes.borrow().iter().cloned().collect()
    }

    /// The roots this provider solves for.
    pub(super) fn roots(&self) -> &[Root] {
        self.roots
    }

    fn note(&self, text: String) {
        self.notes.borrow_mut().insert(text);
    }

    /// Dependency edges walked, keyed by the depending package version.
    pub(super) fn edges(&self) -> BTreeMap<(Pkg, Version), Vec<Pkg>> {
        self.edges.borrow().clone()
    }

    /// The manifest at a version, cached; `None` when the source has none.
    pub(super) fn manifest(
        &self,
        id: &ModuleId,
        version: &Version,
    ) -> Result<Option<Manifest>, SourceError> {
        let key = (id.clone(), version.clone());
        if let Some(m) = self.manifests.borrow().get(&key) {
            return Ok(m.clone());
        }
        let m = self.source.manifest(id, version)?;
        self.manifests.borrow_mut().insert(key, m.clone());
        Ok(m)
    }

    /// Whether `id` at `version` is installed.
    pub(super) fn is_installed(&self, id: &ModuleId, version: &Version) -> bool {
        self.installed
            .iter()
            .any(|i| &i.id == id && &i.version == version)
    }

    /// Candidate versions in preference order (see the module doc).
    fn candidates(&self, id: &ModuleId) -> Result<Vec<Candidate>, SourceError> {
        if let Some(c) = self.candidates.borrow().get(id) {
            return Ok(c.clone());
        }
        let mut list: Vec<Candidate> = Vec::new();
        if let Some(pin) = self.pins.get(id) {
            self.note(format!("{id} is pinned to {pin} in this workspace"));
            list.push(Candidate {
                version: pin.clone(),
                installed: self.is_installed(id, pin),
            });
        } else {
            let mut active: Option<Version> = None;
            let mut installed: Vec<Version> = Vec::new();
            for i in self.installed.iter().filter(|i| &i.id == id) {
                if i.active {
                    active = Some(i.version.clone());
                } else {
                    installed.push(i.version.clone());
                }
            }
            let mut published: Vec<Version> = self.source.versions(id)?;
            published.retain(|v| Some(v) != active.as_ref() && !installed.contains(v));
            installed.sort();
            published.sort();
            published.dedup();
            list.extend(active.into_iter().map(|version| Candidate {
                version,
                installed: true,
            }));
            list.extend(installed.into_iter().rev().map(|version| Candidate {
                version,
                installed: true,
            }));
            list.extend(published.into_iter().rev().map(|version| Candidate {
                version,
                installed: false,
            }));
            if list.is_empty() {
                self.note(format!(
                    "{id} is not installed and has no vX.Y.Z tag to install"
                ));
            }
        }
        self.candidates
            .borrow_mut()
            .insert(id.clone(), list.clone());
        Ok(list)
    }

    /// Whether a candidate may satisfy `range`: in it, and not a pre-release unless
    /// the range asks for exactly that version.
    fn offered(v: &Version, range: &Ranges<Version>) -> bool {
        range.contains(v) && (!is_prerelease(v) || *range == Ranges::singleton(v.clone()))
    }

    /// The package a requirement on `id` within `range` maps to. In split mode that
    /// is the compatibility line of the preferred candidate in range, and the range
    /// is narrowed to that line.
    fn dep(
        &self,
        id: &ModuleId,
        range: Ranges<Version>,
    ) -> Result<(Pkg, Ranges<Version>), SourceError> {
        if !self.split {
            return Ok((
                Pkg::Module {
                    id: id.clone(),
                    line: None,
                },
                range,
            ));
        }
        let preferred = self
            .candidates(id)?
            .into_iter()
            .find(|c| Self::offered(&c.version, &range))
            .map(|c| Compat::of(&c.version));
        Ok(match preferred {
            Some(line) => (
                Pkg::Module {
                    id: id.clone(),
                    line: Some(line),
                },
                range.intersection(&line.ranges()),
            ),
            None => (
                Pkg::Module {
                    id: id.clone(),
                    line: None,
                },
                range,
            ),
        })
    }

    /// Versions of `provider` that provide `shape` within `req`: every installed
    /// one that does, plus the newest published one that does. Manifests are read
    /// lazily, newest first, so a provider that has always offered the shape costs
    /// one read.
    fn provider_versions(
        &self,
        provider: &ModuleId,
        shape: &str,
        req: &VersionReq,
    ) -> Result<Ranges<Version>, SourceError> {
        let mut set = Ranges::empty();
        let mut found_published = false;
        for c in self.candidates(provider)? {
            if !c.installed && found_published {
                continue;
            }
            if is_prerelease(&c.version) && !c.installed {
                continue;
            }
            let provides = self
                .manifest(provider, &c.version)?
                .map(|m| {
                    m.provides
                        .iter()
                        .any(|p| p.shape == shape && req.matches(&p.version))
                })
                .unwrap_or(false);
            if provides {
                set = set.union(&Ranges::singleton(c.version.clone()));
                if !c.installed {
                    found_published = true;
                }
            }
        }
        Ok(set)
    }
}

impl DependencyProvider for Provider<'_> {
    type P = Pkg;
    type V = Version;
    type VS = Ranges<Version>;
    type Priority = (u32, Reverse<usize>);
    type M = String;
    type Err = SourceError;

    fn prioritize(
        &self,
        package: &Pkg,
        range: &Ranges<Version>,
        stats: &PackageResolutionStatistics,
    ) -> Self::Priority {
        let matching = match package {
            Pkg::Root => 1,
            Pkg::Module { id, .. } => self
                .candidates(id)
                .map(|c| {
                    c.iter()
                        .filter(|c| Self::offered(&c.version, range))
                        .count()
                })
                .unwrap_or(0),
        };
        (stats.conflict_count(), Reverse(matching))
    }

    fn choose_version(
        &self,
        package: &Pkg,
        range: &Ranges<Version>,
    ) -> Result<Option<Version>, SourceError> {
        let Pkg::Module { id, line } = package else {
            return Ok(Some(Version::new(0, 0, 0)));
        };
        Ok(self
            .candidates(id)?
            .into_iter()
            .map(|c| c.version)
            .find(|v| Self::offered(v, range) && line.is_none_or(|l| l == Compat::of(v))))
    }

    fn get_dependencies(
        &self,
        package: &Pkg,
        version: &Version,
    ) -> Result<Dependencies<Pkg, Ranges<Version>, String>, SourceError> {
        let mut deps: Vec<(Pkg, Ranges<Version>)> = Vec::new();
        match package {
            Pkg::Root => {
                for root in self.roots {
                    deps.push(self.dep(&root.id, ranges_of(&root.version))?);
                }
            }
            Pkg::Module { id, .. } => {
                let Some(manifest) = self.manifest(id, version)? else {
                    return Ok(Dependencies::Unavailable(format!(
                        "{id} {version} has no manifest"
                    )));
                };
                for (dep, req) in &manifest.dependencies {
                    deps.push(self.dep(dep, ranges_of(req))?);
                }
                for r in &manifest.requires {
                    let Some(provider) = &r.provider else {
                        continue; // shaped requirements are matched after solving
                    };
                    let set = self.provider_versions(provider, &r.shape, &r.version)?;
                    if set.is_empty() {
                        self.note(format!(
                            "{id} {version} requires {} {} from {provider}, and no version of \
                             {provider} provides it",
                            r.shape, r.version
                        ));
                    }
                    deps.push(self.dep(provider, set)?);
                }
            }
        }
        // The same package twice (an identity dependency and a named requirement on
        // the same module) is one edge with the intersection.
        let mut merged: Vec<(Pkg, Ranges<Version>)> = Vec::new();
        for (p, r) in deps {
            match merged.iter_mut().find(|(q, _)| *q == p) {
                Some((_, existing)) => *existing = existing.intersection(&r),
                None => merged.push((p, r)),
            }
        }
        self.edges.borrow_mut().insert(
            (package.clone(), version.clone()),
            merged.iter().map(|(p, _)| p.clone()).collect(),
        );
        Ok(Dependencies::Available(
            merged.into_iter().collect::<DependencyConstraints<_, _>>(),
        ))
    }
}
