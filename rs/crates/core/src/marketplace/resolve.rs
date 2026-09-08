//! The marketplace's [`Source`] for the dependency resolver.
//!
//! [`install::resolver`](crate::install::resolver) never touches the network or the
//! disk. This is the implementation that does, for the free-tier pipeline:
//!
//! - **versions**: `git ls-remote --tags` on `<git_base>/<owner>/<repo>.git`, keeping
//!   only `vX.Y.Z` tags, plus every version already in the install store.
//! - **manifests**: the install store's signed record when the version is installed,
//!   the manifest already read from the job's own checkout for the module being
//!   installed, and otherwise a shallow clone of that one tag, read and thrown away.
//!
//! Every answer is cached for the life of the source (one install job), so a module
//! is cloned at most once per candidate version even when the solver backtracks over
//! it. Clones live under `<state dir>/scratch/<job>/src` and are removed as soon as
//! the manifest has been read; the job's scratch directory is removed whole when the
//! job ends.

use super::fetch::{read_manifest, Git};
use crate::install::resolver::{Source, SourceError};
use avada_module_sdk::manifest::{Manifest, ModuleId};
use semver::Version;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Candidate versions and manifests, read from git and the install store.
pub struct MarketplaceSource<'a> {
    git: &'a Git,
    git_base: String,
    scratch: PathBuf,
    /// Manifests known without any fetch: installed versions and the job's own
    /// checkout.
    known: BTreeMap<(ModuleId, Version), Manifest>,
    log: &'a dyn Fn(&str),
    tags: RefCell<BTreeMap<ModuleId, BTreeMap<Version, String>>>,
    manifests: RefCell<BTreeMap<(ModuleId, Version), Option<Manifest>>>,
}

impl<'a> MarketplaceSource<'a> {
    /// A source cloning from `git_base` into `scratch`, answering from `known`
    /// first. `log` receives one line per clone.
    pub fn new(
        git: &'a Git,
        git_base: &str,
        scratch: &Path,
        known: BTreeMap<(ModuleId, Version), Manifest>,
        log: &'a dyn Fn(&str),
    ) -> Self {
        MarketplaceSource {
            git,
            git_base: git_base.trim_end_matches('/').to_string(),
            scratch: scratch.to_path_buf(),
            known,
            log,
            tags: RefCell::new(BTreeMap::new()),
            manifests: RefCell::new(BTreeMap::new()),
        }
    }

    /// The clone URL of a module.
    pub fn url(&self, id: &ModuleId) -> String {
        format!("{}/{}.git", self.git_base, id.as_str())
    }

    /// `vX.Y.Z` tags of a module, version → tag name, cached. A module with no
    /// repository (or no tags) answers an empty map rather than failing: the solver
    /// then has no published candidate for it and says so.
    fn tags(&self, id: &ModuleId) -> BTreeMap<Version, String> {
        if let Some(t) = self.tags.borrow().get(id) {
            return t.clone();
        }
        let found = match self.git.ls_remote_tags(&self.url(id)) {
            Ok(tags) => tags
                .into_keys()
                .filter_map(|t| {
                    let v = Version::parse(t.strip_prefix('v')?).ok()?;
                    Some((v, t))
                })
                .collect(),
            Err(e) => {
                (self.log)(&format!("no tags for {}: {e}", id.as_str()));
                BTreeMap::new()
            }
        };
        self.tags.borrow_mut().insert(id.clone(), found.clone());
        found
    }

    /// Clone one tag into the job's scratch, read its manifest and remove the
    /// checkout again.
    fn fetch_manifest(&self, id: &ModuleId, version: &Version) -> Option<Manifest> {
        let tag = self.tags(id).get(version)?.clone();
        let dir = self.scratch.join(format!("{}-{version}", id.dir_name()));
        let _ = std::fs::remove_dir_all(&dir);
        if let Some(parent) = dir.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                (self.log)(&format!("{}: {e}", parent.display()));
                return None;
            }
        }
        (self.log)(&format!("reading the manifest of {} {tag}", id.as_str()));
        let cloned = self
            .git
            .clone_tag(&self.url(id), &tag, &dir, &mut |_| {})
            .map_err(|e| (self.log)(&format!("clone of {} {tag} failed: {e}", id.as_str())))
            .is_ok();
        let manifest = if cloned {
            read_manifest(&dir)
                .map_err(|e| (self.log)(&format!("{} {tag}: {e}", id.as_str())))
                .ok()
        } else {
            None
        };
        let _ = std::fs::remove_dir_all(&dir);
        manifest
    }
}

impl Source for MarketplaceSource<'_> {
    fn versions(&self, id: &ModuleId) -> Result<Vec<Version>, SourceError> {
        let mut out: Vec<Version> = self
            .known
            .keys()
            .filter(|(k, _)| k == id)
            .map(|(_, v)| v.clone())
            .collect();
        for v in self.tags(id).into_keys() {
            if !out.contains(&v) {
                out.push(v);
            }
        }
        Ok(out)
    }

    fn manifest(&self, id: &ModuleId, version: &Version) -> Result<Option<Manifest>, SourceError> {
        let key = (id.clone(), version.clone());
        if let Some(m) = self.known.get(&key) {
            return Ok(Some(m.clone()));
        }
        if let Some(m) = self.manifests.borrow().get(&key) {
            return Ok(m.clone());
        }
        let found = self.fetch_manifest(id, version);
        self.manifests.borrow_mut().insert(key, found.clone());
        Ok(found)
    }
}
