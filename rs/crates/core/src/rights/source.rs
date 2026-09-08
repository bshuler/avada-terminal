//! [`ModuleTokens`]: module bearer tokens as a control-server [`CapabilitySource`].
//!
//! The control server's router asks its [`CapabilityResolver`](crate::control::dispatch::CapabilityResolver)
//! what a presented bearer may do; the master token and every device or scoped token
//! answer "everything" through `LegacyTokens`. A module's per-run token must answer with
//! *that module's* rights instead, and the only two things that know are the host (which
//! token belongs to which running module) and the rights service (what the user has
//! allowed it). This adapter joins the two and is installed first in the chain, so a
//! module token never falls through to the legacy "all".
//!
//! Only an outright `Allow` puts a capability in the set. `Ask` is not a denial the user
//! sees here: the toast belongs to the pipe (`RightsGate`), where the module makes its
//! `host.*` calls; an HTTP route is refused with the contract's 403 until the answer has
//! been given on that side. A token the host does not recognise yields `None`, so the
//! next source (ultimately `LegacyTokens`) decides.

use std::collections::BTreeSet;

use avada_module_sdk::caps::{Capability, Decision};

use super::RightsGate;
use crate::control::dispatch::CapabilitySource;
use crate::module::Host;

/// Module tokens → the capabilities the rights service currently allows the module.
#[derive(Clone)]
pub struct ModuleTokens {
    host: Host,
    gate: RightsGate,
}

impl ModuleTokens {
    /// Join a host (which knows the tokens) with the rights gate (which knows the
    /// answers). Both are cheap handles; the app installs the result with
    /// `shared.caps.install(Arc::new(source))`.
    pub fn new(host: Host, gate: RightsGate) -> Self {
        ModuleTokens { host, gate }
    }
}

impl CapabilitySource for ModuleTokens {
    fn caps_for(&self, token: &str) -> Option<BTreeSet<Capability>> {
        let module = self.host.module_for_token(token)?;
        let workspace = self.gate.workspace();
        let service = self.gate.lock();
        Some(
            Capability::ALL
                .iter()
                .copied()
                .filter(|cap| {
                    service.decide(&module, *cap, workspace.as_deref()) == Decision::Allow
                })
                .collect(),
        )
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use crate::module::testkit::{self, child_args, MODE_ENV};
    use crate::module::{HostConfig, ModuleStatus};
    use crate::rights::{RightValue, RightsService};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    struct Dir(PathBuf);
    impl Dir {
        fn new() -> Dir {
            let p = std::env::temp_dir().join(format!(
                "avada-rights-source-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&p).unwrap();
            Dir(p)
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A rights gate over the fixture record plus a host running the fake module through
    /// that gate, so the token the source sees is a real minted one.
    fn rig(dir: &Dir) -> (Host, RightsGate) {
        let record = testkit::record(&[Capability::UiRail, Capability::UiCommands]);
        let mut service = RightsService::with_root(dir.0.join("rights"));
        service.register(record.clone());
        service
            .set_user(&record.module_id, Capability::UiRail, RightValue::Always)
            .unwrap();
        service
            .set_user(
                &record.module_id,
                Capability::UiCommands,
                RightValue::Always,
            )
            .unwrap();
        let gate = RightsGate::new(service);
        let host = Host::new(HostConfig::new(dir.0.join("data")), Arc::new(gate.clone()));
        host.spawn_with(
            &record,
            &std::env::current_exe().unwrap(),
            &[(MODE_ENV.to_string(), "normal".to_string())],
            &child_args(),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while host.status(&record.module_id) != ModuleStatus::Running {
            assert!(Instant::now() < deadline, "module never came up");
            std::thread::sleep(Duration::from_millis(20));
        }
        (host, gate)
    }

    #[test]
    fn an_unknown_token_is_left_to_the_next_source() {
        let dir = Dir::new();
        let (host, gate) = rig(&dir);
        let source = ModuleTokens::new(host.clone(), gate);
        assert_eq!(source.caps_for(""), None);
        assert_eq!(source.caps_for(&"0".repeat(64)), None);
        host.shutdown_all();
    }

    #[test]
    fn a_module_token_holds_exactly_what_the_user_allowed() {
        let dir = Dir::new();
        let (host, gate) = rig(&dir);
        let id = testkit::module_id();
        let token = host.test_token(&id).expect("running module has a token");
        assert_eq!(host.module_for_token(&token), Some(id.clone()));
        let source = ModuleTokens::new(host.clone(), gate.clone());

        let caps = source.caps_for(&token).expect("the host knows this token");
        assert_eq!(
            caps,
            [Capability::UiRail, Capability::UiCommands]
                .into_iter()
                .collect()
        );

        // Turning one off on the rights page is visible on the very next request.
        gate.lock()
            .set_user(&id, Capability::UiCommands, RightValue::Never)
            .unwrap();
        let caps = source.caps_for(&token).unwrap();
        assert_eq!(caps, [Capability::UiRail].into_iter().collect());

        // Something the record never accepted is not granted by `always`.
        assert!(!caps.contains(&Capability::FsWrite));

        // Once the module is gone the token means nothing.
        host.shutdown(&id).unwrap();
        assert_eq!(host.module_for_token(&token), None);
        assert_eq!(source.caps_for(&token), None);
    }

    #[test]
    fn installed_first_it_outranks_the_legacy_all() {
        use crate::control::dispatch::CapabilityResolver;
        let dir = Dir::new();
        let (host, gate) = rig(&dir);
        let id = testkit::module_id();
        let token = host.test_token(&id).unwrap();
        let resolver = CapabilityResolver::new();
        resolver.install(Arc::new(ModuleTokens::new(host.clone(), gate)));
        assert!(resolver.check(&token, Capability::UiRail).is_ok());
        assert_eq!(
            resolver.check(&token, Capability::FsWrite),
            Err(Capability::FsWrite)
        );
        // A token the host does not know keeps its legacy authority.
        assert!(resolver
            .check("not-a-module-token", Capability::FsWrite)
            .is_ok());
        host.shutdown_all();
    }
}
