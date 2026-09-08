//! The capability gate: the seam between this host and the rights service (track H2).
//!
//! Every `host.*` method the module calls is checked here with the capability the
//! contract names for it (`contract::methods::required_capability`) before it does
//! anything. Wave 1 ships [`DeclaredOnly`], which answers from the install record alone;
//! H2's rights service implements the same trait to add the user's `never|always|
//! workspace|ask` values and the workspace override column.

use avada_module_sdk::caps::Capability;
pub use avada_module_sdk::caps::Decision;
use avada_module_sdk::rights::InstallRecord;
use avada_module_sdk::ModuleId;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::RwLock;

/// Decides whether `module` may exercise `cap` right now.
pub trait CapabilityGate: Send + Sync {
    /// `Allow` proceeds, `Deny` refuses with the contract's `CapabilityDenied` code, and
    /// `Ask` is treated as `Deny` by this host until the toast prompt lands (the answer
    /// must come from the user, never from the module's own request).
    fn check(&self, module: &ModuleId, cap: Capability) -> Decision;
}

/// Allows exactly the `accepted` set of each module's install record and nothing else.
/// A module the gate has never heard of is denied everything.
#[derive(Debug, Default)]
pub struct DeclaredOnly {
    accepted: RwLock<BTreeMap<ModuleId, BTreeSet<Capability>>>,
}

impl DeclaredOnly {
    /// An empty gate; add records with [`DeclaredOnly::insert`].
    pub fn new() -> Self {
        Self::default()
    }

    /// A gate for one record.
    pub fn from_record(record: &InstallRecord) -> Self {
        let gate = Self::new();
        gate.insert(record);
        gate
    }

    /// Remember (or replace) a module's accepted set.
    pub fn insert(&self, record: &InstallRecord) {
        self.accepted
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(record.module_id.clone(), record.accepted.clone());
    }

    /// Forget a module.
    pub fn remove(&self, module: &ModuleId) {
        self.accepted
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(module);
    }
}

impl CapabilityGate for DeclaredOnly {
    fn check(&self, module: &ModuleId, cap: Capability) -> Decision {
        let map = self.accepted.read().unwrap_or_else(|e| e.into_inner());
        match map.get(module) {
            Some(set) if set.contains(&cap) => Decision::Allow,
            _ => Decision::Deny,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module::testkit;

    #[test]
    fn declared_only_allows_exactly_the_accepted_set() {
        let record = testkit::record(&[Capability::UiRail, Capability::UiToast]);
        let gate = DeclaredOnly::from_record(&record);
        assert_eq!(
            gate.check(&record.module_id, Capability::UiRail),
            Decision::Allow
        );
        assert_eq!(
            gate.check(&record.module_id, Capability::UiToast),
            Decision::Allow
        );
        assert_eq!(
            gate.check(&record.module_id, Capability::FsRead),
            Decision::Deny
        );
        assert_eq!(
            gate.check(&record.module_id, Capability::UiCommands),
            Decision::Deny
        );
    }

    #[test]
    fn unknown_module_is_denied_everything() {
        let gate = DeclaredOnly::new();
        let stranger = ModuleId::new("nobody/nothing").unwrap();
        assert_eq!(gate.check(&stranger, Capability::UiRail), Decision::Deny);
    }

    #[test]
    fn remove_forgets_the_module() {
        let record = testkit::record(&[Capability::UiRail]);
        let gate = DeclaredOnly::from_record(&record);
        gate.remove(&record.module_id);
        assert_eq!(
            gate.check(&record.module_id, Capability::UiRail),
            Decision::Deny
        );
    }
}
