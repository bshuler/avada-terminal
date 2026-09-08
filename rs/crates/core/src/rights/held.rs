//! Held updates: an update whose manifest asks for a capability the installed record
//! does not carry is parked until the user has seen the diff and accepted it
//! (docs/module-contract.md §6). The SDK computes the diff ([`HeldUpdate::check`]);
//! this store keeps one held update per module and turns "accept" into the accepted
//! set the install store re-signs.
//!
//! In-memory only: an update the user never answered is re-detected on the next
//! update check, which is cheaper and safer than persisting a stale diff.

use std::collections::{BTreeMap, BTreeSet};

use super::{Capability, HeldUpdate, ModuleId};

/// One held update per module (a newer candidate replaces an older held one).
#[derive(Debug, Default)]
pub struct HeldStore {
    held: BTreeMap<ModuleId, HeldUpdate>,
}

impl HeldStore {
    /// Park (or replace) the held update for its module.
    pub fn hold(&mut self, update: HeldUpdate) {
        self.held.insert(update.module_id.clone(), update);
    }

    /// The held update for a module, if any.
    pub fn get(&self, module: &ModuleId) -> Option<&HeldUpdate> {
        self.held.get(module)
    }

    /// Every held update, sorted by module id.
    pub fn all(&self) -> Vec<&HeldUpdate> {
        self.held.values().collect()
    }

    /// Number of modules with a held update.
    pub fn len(&self) -> usize {
        self.held.len()
    }

    /// True when nothing is held.
    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    /// Accept: remove the held update and return the accepted set for the next
    /// record — `installed_accepted` (what the user agreed to at install; `None` when
    /// the record is unknown, treated as empty) minus `removed`, plus `added`. Every
    /// capability the user had declined stays declined; only the diff they just saw is
    /// granted, which is what makes the held-update toast not a hidden grant.
    pub fn accept(
        &mut self,
        module: &ModuleId,
        installed_accepted: Option<&BTreeSet<Capability>>,
    ) -> Option<BTreeSet<Capability>> {
        let held = self.held.remove(module)?;
        let mut next: BTreeSet<Capability> = installed_accepted.cloned().unwrap_or_default();
        for cap in &held.removed {
            next.remove(cap);
        }
        next.extend(held.added.iter().copied());
        Some(next)
    }

    /// Reject: drop the held update; the installed version keeps running.
    pub fn reject(&mut self, module: &ModuleId) -> Option<HeldUpdate> {
        self.held.remove(module)
    }
}
