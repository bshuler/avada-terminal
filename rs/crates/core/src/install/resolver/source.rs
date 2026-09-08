//! Where candidate versions and their manifests come from.
//!
//! The resolver never touches the network or the disk itself: it asks a [`Source`]
//! for the versions a module has and the manifest at one of them. Tests use
//! [`MemorySource`]; the marketplace supplies an implementation that lists git tags
//! and reads manifests from the install store (already installed versions) or a
//! shallow clone (everything else).

use avada_module_sdk::manifest::{Manifest, ModuleId};
use semver::Version;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Mutex;

/// A source could not answer (network, git, a manifest that does not parse).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceError(pub String);

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for SourceError {}

/// Candidate versions and manifests for the resolver.
pub trait Source {
    /// Every version of `id` that could be selected: published tags plus whatever is
    /// installed locally. Unknown modules answer an empty list. Order is irrelevant.
    fn versions(&self, id: &ModuleId) -> Result<Vec<Version>, SourceError>;

    /// The manifest of `id` at `version`, `None` when that version does not exist
    /// or carries no manifest.
    fn manifest(&self, id: &ModuleId, version: &Version) -> Result<Option<Manifest>, SourceError>;
}

/// An in-memory source for tests: manifests keyed by module and version.
#[derive(Default)]
pub struct MemorySource {
    manifests: Mutex<BTreeMap<ModuleId, BTreeMap<Version, Manifest>>>,
    /// Manifest fetches, for tests asserting laziness.
    fetches: Mutex<Vec<(ModuleId, Version)>>,
}

impl MemorySource {
    /// Empty.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a manifest (its own id and version say where it goes).
    pub fn add(&self, manifest: Manifest) -> &Self {
        self.manifests
            .lock()
            .unwrap()
            .entry(manifest.id().clone())
            .or_default()
            .insert(manifest.module.version.clone(), manifest);
        self
    }

    /// Every `(module, version)` whose manifest was asked for, in order.
    pub fn fetches(&self) -> Vec<(ModuleId, Version)> {
        self.fetches.lock().unwrap().clone()
    }
}

impl Source for MemorySource {
    fn versions(&self, id: &ModuleId) -> Result<Vec<Version>, SourceError> {
        Ok(self
            .manifests
            .lock()
            .unwrap()
            .get(id)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default())
    }

    fn manifest(&self, id: &ModuleId, version: &Version) -> Result<Option<Manifest>, SourceError> {
        self.fetches
            .lock()
            .unwrap()
            .push((id.clone(), version.clone()));
        Ok(self
            .manifests
            .lock()
            .unwrap()
            .get(id)
            .and_then(|m| m.get(version))
            .cloned())
    }
}
