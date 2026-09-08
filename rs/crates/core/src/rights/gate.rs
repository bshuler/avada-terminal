//! [`RightsGate`]: the rights service behind the module host's `CapabilityGate`.
//!
//! The host checks every `host.*` call through `CapabilityGate::check` from the module's
//! reader thread, so the gate has to be `Send + Sync`, while [`RightsService`] is plain
//! single-owner state the preferences page also mutates. The gate is therefore a shared
//! handle (`Arc<Mutex<RightsService>>`) the app keeps alongside its own copy, plus the
//! one thing the host does not know about: which workspace is active, which decides
//! the workspace column of the resolve table.
//!
//! A `Decision::Ask` is turned into a parked [`PendingAsk`](super::PendingAsk) on the
//! service's queue right here, so the toast appears without the host having to learn
//! about asks; the host treats `Ask` as `Deny` for that one call, and the user's answer
//! (`always` / `workspace`) makes the next call `Allow`.

use std::sync::{Arc, Mutex, MutexGuard};

use avada_module_sdk::caps::{Capability, Decision};
use avada_module_sdk::manifest::ModuleId;

use super::RightsService;
use crate::module::CapabilityGate;

/// Shared, thread-safe handle over a [`RightsService`], usable as the host's gate.
#[derive(Clone)]
pub struct RightsGate {
    service: Arc<Mutex<RightsService>>,
    workspace: Arc<Mutex<Option<String>>>,
}

impl RightsGate {
    /// Wrap a service. The app keeps the returned handle for its own reads and writes
    /// (`lock`) and gives a clone to `Host::new`.
    pub fn new(service: RightsService) -> Self {
        RightsGate {
            service: Arc::new(Mutex::new(service)),
            workspace: Arc::new(Mutex::new(None)),
        }
    }

    /// Wrap an already shared service.
    pub fn shared(service: Arc<Mutex<RightsService>>) -> Self {
        RightsGate {
            service,
            workspace: Arc::new(Mutex::new(None)),
        }
    }

    /// The service, for the preferences page and the ask toast. A poisoned lock is
    /// recovered: the rights state is plain data and a panic elsewhere must not turn
    /// every capability call into a hang.
    pub fn lock(&self) -> MutexGuard<'_, RightsService> {
        self.service.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The shared service handle itself.
    pub fn service(&self) -> Arc<Mutex<RightsService>> {
        Arc::clone(&self.service)
    }

    /// Set (or with `None`, clear) the active workspace key. Loads the workspace's
    /// override column into the service so `check` sees it.
    pub fn set_workspace(&self, key: Option<&str>) {
        if let Some(k) = key {
            self.lock().load_workspace(k);
        }
        *self.workspace.lock().unwrap_or_else(|e| e.into_inner()) = key.map(str::to_owned);
    }

    /// The active workspace key, if any.
    pub fn workspace(&self) -> Option<String> {
        self.workspace
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl CapabilityGate for RightsGate {
    fn check(&self, module: &ModuleId, cap: Capability) -> Decision {
        let workspace = self.workspace();
        let mut service = self.lock();
        let decision = service.decide(module, cap, workspace.as_deref());
        if decision == Decision::Ask {
            // Park the question for the toast; the host answers this call with a denial
            // and the user's choice decides the next one.
            service.ask(module, cap, workspace.as_deref());
        }
        decision
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rights::{AskAnswer, InstallKind, InstallRecord, RightValue};
    use avada_module_sdk::manifest::{DistributionKind, Manifest};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const FIXTURE: &str = include_str!("../../../module-sdk/tests/fixtures/avada.toml");

    fn record() -> InstallRecord {
        let m = Manifest::parse(FIXTURE).expect("fixture manifest parses");
        InstallRecord {
            module_id: m.module.id.clone(),
            repo: "https://github.com/acme/avada-files".into(),
            tag: m.tag(),
            commit: "0123456789abcdef0123456789abcdef01234567".into(),
            version: m.module.version.clone(),
            artifact_sha256: "00".repeat(32),
            source: DistributionKind::Source,
            accepted: m.capabilities.iter().copied().collect(),
            manifest: m,
            installed_at: 1_700_000_000,
            kind: InstallKind::Manual,
        }
    }

    struct TempRoot(PathBuf);
    impl TempRoot {
        fn new() -> Self {
            static N: AtomicUsize = AtomicUsize::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("avada-rights-gate-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            TempRoot(dir)
        }
    }
    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn gate(root: &TempRoot) -> (RightsGate, ModuleId) {
        let mut s = RightsService::with_root(&root.0);
        let r = record();
        let id = r.module_id.clone();
        s.register(r);
        (RightsGate::new(s), id)
    }

    #[test]
    fn gate_is_a_capability_gate_the_host_can_hold() {
        let root = TempRoot::new();
        let (g, _) = gate(&root);
        let _host_side: Arc<dyn CapabilityGate> = Arc::new(g);
    }

    #[test]
    fn always_allows_never_denies_and_a_stranger_is_denied() {
        let root = TempRoot::new();
        let (g, id) = gate(&root);
        g.lock()
            .set_user(&id, Capability::FsRead, RightValue::Always)
            .unwrap();
        g.lock()
            .set_user(&id, Capability::UiToast, RightValue::Never)
            .unwrap();
        assert_eq!(g.check(&id, Capability::FsRead), Decision::Allow);
        assert_eq!(g.check(&id, Capability::UiToast), Decision::Deny);
        let stranger = ModuleId::new("nobody/nothing").unwrap();
        assert_eq!(g.check(&stranger, Capability::FsRead), Decision::Deny);
        assert!(
            g.lock().asks.is_empty(),
            "no ask was parked for plain answers"
        );
    }

    #[test]
    fn an_ask_parks_one_pending_ask_and_the_answer_settles_the_next_call() {
        let root = TempRoot::new();
        let (g, id) = gate(&root);
        // The fixture's default for an accepted capability without a user value is Ask.
        assert_eq!(g.check(&id, Capability::FsRead), Decision::Ask);
        assert_eq!(g.check(&id, Capability::FsRead), Decision::Ask);
        assert_eq!(g.lock().asks.len(), 1, "the same question is coalesced");
        let pending = g.lock().asks.front().cloned().unwrap();
        assert_eq!(pending.module, id);
        assert_eq!(pending.cap, Capability::FsRead);
        assert_eq!(pending.workspace, None);

        g.lock()
            .answer(pending.id, AskAnswer::Always)
            .unwrap()
            .expect("the ask was pending");
        assert_eq!(g.check(&id, Capability::FsRead), Decision::Allow);
        assert!(g.lock().asks.is_empty());
    }

    #[test]
    fn the_active_workspace_column_is_consulted_and_carried_on_the_ask() {
        let root = TempRoot::new();
        let (g, id) = gate(&root);
        g.set_workspace(Some("/tmp/ws-a"));
        assert_eq!(g.workspace().as_deref(), Some("/tmp/ws-a"));
        g.lock()
            .set_workspace(
                &id,
                "/tmp/ws-a",
                Capability::FsRead,
                Some(RightValue::Always),
            )
            .unwrap();
        assert_eq!(g.check(&id, Capability::FsRead), Decision::Allow);

        g.set_workspace(Some("/tmp/ws-b"));
        assert_eq!(g.check(&id, Capability::FsRead), Decision::Ask);
        let pending = g.lock().asks.front().cloned().unwrap();
        assert_eq!(pending.workspace.as_deref(), Some("/tmp/ws-b"));

        g.set_workspace(None);
        assert_eq!(g.workspace(), None);
    }

    #[test]
    fn a_clone_shares_the_service_and_the_workspace() {
        let root = TempRoot::new();
        let (g, id) = gate(&root);
        let twin = g.clone();
        twin.set_workspace(Some("/tmp/ws"));
        assert_eq!(g.workspace().as_deref(), Some("/tmp/ws"));
        twin.lock()
            .set_user(&id, Capability::FsRead, RightValue::Never)
            .unwrap();
        assert_eq!(g.check(&id, Capability::FsRead), Decision::Deny);
        assert!(Arc::ptr_eq(&g.service(), &twin.service()));
    }
}
