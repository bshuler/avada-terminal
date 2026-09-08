//! Module host (docs/modules-fanout-plan.md, track H1): spawn, transport, handshake,
//! per-module tokens, supervision, registry.
//!
//! # Spawning one module
//!
//! ```no_run
//! use avada_core::module::{DeclaredOnly, Host, HostConfig, ModuleStatus, RailEvent};
//! use std::sync::Arc;
//! # fn record() -> avada_module_sdk::rights::InstallRecord { unimplemented!() }
//!
//! // The install record (track H5 stores these) fixes the binary hash, the manifest and
//! // the capabilities the user accepted. The gate decides every capability check; the
//! // bundled `DeclaredOnly` allows exactly the record's accepted set (track H2 plugs a
//! // richer policy in through the same trait).
//! let record = record();
//! let gate = Arc::new(DeclaredOnly::from_record(&record));
//! let host = Host::new(HostConfig::new("/path/to/modules-data"), gate);
//!
//! // Subscribe before spawning so no event is missed.
//! let rail = host.rail_events();
//!
//! // Hash check → spawn → hello exchange. Returns once the module is `Running`.
//! host.spawn(&record, std::path::Path::new("/path/to/module-binary"))?;
//! assert_eq!(host.status(&record.module_id), ModuleStatus::Running);
//!
//! // What the module put on the rail arrives here (`std::sync::mpsc`).
//! if let Ok(RailEvent::Registered { entries, .. }) = rail.recv() {
//!     println!("{} rail entries", entries.len());
//! }
//!
//! // Host → module calls are synchronous with a timeout.
//! host.activate(&record.module_id, None)?;
//!
//! // `module.shutdown`, up to five seconds of grace, then a kill.
//! host.shutdown(&record.module_id)?;
//! # Ok::<(), avada_core::module::HostError>(())
//! ```
//!
//! # What the host guarantees
//!
//! * The binary is re-hashed (`sha2`) and compared to `InstallRecord::artifact_sha256`
//!   before **every** spawn, restarts included. A mismatch runs nothing, returns
//!   [`HostError::HashMismatch`] and leaves the module `Broken`.
//! * The manifest in the module's hello must equal the record's
//!   (`InstallRecord::matches_hello`) or the pipe is closed and the module is `Broken`.
//! * Every `host.*` method is checked against the [`CapabilityGate`] with the capability
//!   `contract::methods::required_capability` names; a denial answers with the contract's
//!   `CapabilityDenied` (-32001).
//! * A per-run token (32 random bytes, hex) travels to the module inside the host hello
//!   (`HostHello::token`); the host keeps it only for [`Host::token_matches`] and never
//!   logs it (`Token`'s `Debug` is redacted).
//! * A crash restarts the module at most `RestartPolicy::max_restarts` times per rolling
//!   window (default 3 / 60 s, backing off 250 ms, 1 s, 4 s), then disables it. Each
//!   restart and the disable produce a [`HostEvent::Toast`]; every status change a
//!   [`HostEvent::Status`].
//!
//! # Seams
//!
//! * **Rail (H4):** [`Host::rail_events`] — a `std::sync::mpsc::Receiver<RailEvent>`
//!   per call, all fed the same events; `RailEvent::{Registered, Rows, Gone}`.
//! * **Rights (H2):** the [`CapabilityGate`] trait; [`DeclaredOnly`] is the reference
//!   implementation. `Decision::Ask` is treated as a denial by the host.
//! * **App (placeholder pane):** [`Host::status`] → `ModuleStatus`, which
//!   `app::module_ui::placeholder` renders.
//! * **Licensing (G8):** [`Licensing`] is asked before a module whose manifest says
//!   `distribution.commercial` starts; a host with no gate installed runs everything.
//!   `crate::license::CachedGate` is the implementation core provides.
//! * **Transport:** Unix `socketpair` today; Windows named pipes land in track H7
//!   (`transport::pair` returns an `Unsupported` error there until then).

pub mod gate;
pub mod host;
pub mod rail;
pub mod rpc;
pub mod spawn;
pub mod supervisor;
pub mod token;
pub mod transport;

/// The wire vocabulary, straight from the SDK. Re-exported so the app — which does not and
/// should not depend on the module SDK — spells `files.reveal` and friends exactly once,
/// in the crate that defines them.
pub use avada_module_sdk::contract::methods;

pub use gate::{CapabilityGate, Decision, DeclaredOnly};
pub use host::{Host, HostConfig, HostError, HostEvent, Licensing};
pub use rail::{Gesture, RailEntry, RailEvent, RailState, Row, RowActivate};
pub use rpc::CommandSpec;
pub use spawn::{HandshakeError, SpawnError};
pub use supervisor::{ModuleStatus, RestartPolicy};
pub use token::Token;

/// Fixtures and the fake module every cross-process test in this directory uses.
///
/// The fake module is this very test binary, re-executed with `AVADA_H1_FAKE_MODULE`
/// set (see [`testkit::fake_module_child`]); it talks to the host through
/// `avada_module_sdk::client::from_env` exactly as a real module would.
#[cfg(test)]
pub(crate) mod testkit {
    use super::spawn::sha256_hex;
    use avada_module_sdk::caps::Capability;
    use avada_module_sdk::contract::{HelloKind, ModuleHello, CONTRACT_VERSION};
    use avada_module_sdk::manifest::DistributionKind;
    use avada_module_sdk::manifest::Manifest;
    use avada_module_sdk::rights::{InstallKind, InstallRecord};
    use avada_module_sdk::ModuleId;
    use std::sync::OnceLock;

    /// The SDK's reference manifest (`acme/avada-files` 1.2.0).
    pub(crate) const MANIFEST: &str = include_str!("../../../module-sdk/tests/fixtures/avada.toml");

    pub(crate) fn manifest() -> Manifest {
        Manifest::parse(MANIFEST).expect("fixture manifest parses")
    }

    pub(crate) fn module_id() -> ModuleId {
        ModuleId::new("acme/avada-files").unwrap()
    }

    /// Hash of the running test binary, computed once.
    pub(crate) fn self_hash() -> String {
        static HASH: OnceLock<String> = OnceLock::new();
        HASH.get_or_init(|| sha256_hex(&std::env::current_exe().unwrap()).unwrap())
            .clone()
    }

    /// An install record for the fixture manifest whose binary is this test executable
    /// and whose accepted set is exactly `caps`.
    pub(crate) fn record(caps: &[Capability]) -> InstallRecord {
        let manifest = manifest();
        InstallRecord {
            module_id: manifest.module.id.clone(),
            repo: "https://github.com/acme/avada-files".into(),
            tag: "v1.2.0".into(),
            commit: "0".repeat(40),
            version: manifest.module.version.clone(),
            artifact_sha256: self_hash(),
            source: DistributionKind::Source,
            accepted: caps.iter().copied().collect(),
            manifest,
            installed_at: 0,
            kind: InstallKind::Manual,
        }
    }

    /// Make `manifest` describe a module that is sold. Both sides of the licence tests
    /// go through here, because the host compares the record's manifest with the one the
    /// module sends at handshake and a difference is a `ManifestMismatch`.
    /// `Manifest::validate` only accepts `commercial` alongside an issuer.
    pub(crate) fn sell(manifest: &mut Manifest) {
        manifest.distribution.commercial = true;
        manifest.distribution.issuer = Some("https://issuer.test".into());
    }

    pub(crate) fn module_hello(manifest: &Manifest) -> ModuleHello {
        ModuleHello {
            kind: HelloKind::Module,
            manifest: manifest.clone(),
            contract_min: CONTRACT_VERSION,
            contract_max: CONTRACT_VERSION,
            methods: vec![],
            sdk_version: "test".into(),
        }
    }

    /// Env var that turns this test binary into a module. Values: `normal`, `crash`,
    /// `bad-manifest`, `commercial`, `no-cap`, `hang`, `silent`, `events`.
    #[cfg(unix)]
    pub(crate) const MODE_ENV: &str = "AVADA_H1_FAKE_MODULE";

    /// Arguments that make `current_exe` run [`fake_module_child`] and nothing else.
    #[cfg(unix)]
    pub(crate) fn child_args() -> Vec<String> {
        [
            "--exact",
            "module::testkit::fake_module_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    /// The fake module. Ignored so the normal run skips it; the host tests re-execute the
    /// binary with `--ignored --exact` and `MODE_ENV` set. Without the env it returns.
    #[test]
    #[ignore]
    #[cfg(unix)]
    fn fake_module_child() {
        use avada_module_sdk::client::{self, ENV_FD};
        use avada_module_sdk::contract::methods;
        use avada_module_sdk::contract::{Message, Request};
        use avada_module_sdk::rail::{RailEntry, RegisterRail, Row, SetRows};
        use serde_json::json;
        use std::os::unix::io::FromRawFd;

        let Ok(mode) = std::env::var(MODE_ENV) else {
            return;
        };
        if mode == "silent" {
            std::thread::sleep(std::time::Duration::from_secs(30));
            return;
        }
        // A raw writer on the same socket, for the one test that must bypass the SDK's
        // client-side capability check.
        let fd: i32 = std::env::var(ENV_FD).unwrap().parse().unwrap();
        #[allow(unsafe_code)]
        // SAFETY: the host created the descriptor for this process and `from_env` below
        // only clones it; both handles point at one socket that outlives the test.
        let raw = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };
        let mut raw_writer = raw.try_clone().unwrap();
        std::mem::forget(raw);
        let mut conn = client::from_env().expect("launched by the host");

        let mut manifest = manifest();
        if mode == "bad-manifest" {
            manifest.module.name = "Not Files".into();
        }
        if mode == "commercial" {
            sell(&mut manifest);
        }
        let hello = conn
            .handshake(
                manifest,
                methods::MODULE_REQUIRED_V1
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            )
            .expect("handshake");
        assert_eq!(hello.data_dir, std::env::var(client::ENV_DATA_DIR).unwrap());

        let entry = |label: &str| RailEntry {
            id: "files".into(),
            label: label.into(),
            icon: None,
            tier: avada_module_sdk::manifest::UiTier::Data,
            module: None,
            order: 0,
            component: None,
        };

        if mode == "no-cap" {
            // `host.toast` without `ui.toast`: the host must refuse it with -32001.
            let req = Request::new(900, methods::HOST_TOAST, json!({ "text": "hi" }));
            client::write_line(&mut raw_writer, &Message::Request(req).to_line()).unwrap();
            let label = match conn.recv().unwrap() {
                Some(Message::Response(r)) => match r.error {
                    Some(e) => format!("denied {}", e.code),
                    None => "allowed".to_string(),
                },
                other => format!("unexpected {other:?}"),
            };
            conn.call(
                methods::HOST_RAIL_REGISTER,
                serde_json::to_value(RegisterRail {
                    entries: vec![entry(&label)],
                })
                .unwrap(),
            )
            .unwrap();
            return;
        }

        if mode == "events" {
            // Ask for one kind only. The host must deliver `rail.query` and stay quiet
            // about `files.reveal`, which proves the subscription set is honoured rather
            // than every notification being broadcast.
            conn.call(
                methods::HOST_EVENTS_SUBSCRIBE,
                json!({ "kinds": [methods::events::RAIL_QUERY] }),
            )
            .expect("events.subscribe");
        }

        conn.call(
            methods::HOST_RAIL_REGISTER,
            serde_json::to_value(RegisterRail {
                entries: vec![entry("Files")],
            })
            .unwrap(),
        )
        .expect("rail.register");
        conn.call(
            methods::HOST_ROWS_SET,
            serde_json::to_value(SetRows {
                entry: "files".into(),
                rows: vec![Row {
                    id: "readme".into(),
                    label: "README.md".into(),
                    detail: String::new(),
                    depth: 0,
                    expandable: false,
                    expanded: false,
                    icon: None,
                    marks: vec![],
                    data: json!({ "path": "README.md" }),
                }],
            })
            .unwrap(),
        )
        .expect("rows.set");

        if mode == "crash" {
            std::process::exit(3);
        }

        while let Some(msg) = conn.recv().unwrap() {
            match msg {
                Message::Request(req) => {
                    let resp = match req.method.as_str() {
                        methods::MODULE_ACTIVATE => req.ok(json!({ "activated": true })),
                        methods::MODULE_DEACTIVATE => req.ok(json!({})),
                        methods::MODULE_COMMAND_INVOKE => {
                            req.ok(json!({ "ran": req.params["id"] }))
                        }
                        methods::MODULE_ROW_ACTIVATE => req.ok(json!({ "row": req.params["row"] })),
                        _ => req.err(avada_module_sdk::contract::RpcError::new(
                            avada_module_sdk::contract::ErrorCode::MethodNotFound,
                            "no",
                        )),
                    };
                    conn.respond(resp).unwrap();
                }
                Message::Notification(n) if n.method == methods::MODULE_SHUTDOWN => {
                    if mode == "hang" {
                        std::thread::sleep(std::time::Duration::from_secs(30));
                    }
                    return;
                }
                Message::Notification(n) if n.method == methods::MODULE_EVENT => {
                    // Echo through the rail so the host test can see what arrived.
                    let label = format!("event {} {}", n.params["kind"], n.params["payload"]);
                    conn.call(
                        methods::HOST_RAIL_REGISTER,
                        serde_json::to_value(RegisterRail {
                            entries: vec![entry(&label)],
                        })
                        .unwrap(),
                    )
                    .unwrap();
                }
                Message::Notification(n) if n.method == methods::MODULE_PREFS_CHANGED => {
                    // Echo the new values back through the rail so the test can see them.
                    let label = format!("prefs {}", n.params["values"]["theme"]);
                    conn.call(
                        methods::HOST_RAIL_REGISTER,
                        serde_json::to_value(RegisterRail {
                            entries: vec![entry(&label)],
                        })
                        .unwrap(),
                    )
                    .unwrap();
                }
                _ => {}
            }
        }
    }
}

#[cfg(all(test, unix))]
mod host_tests {
    use super::gate::DeclaredOnly;
    use super::rpc::tests::tempdir::Dir;
    use super::testkit::{self, child_args, MODE_ENV};
    use super::*;
    use avada_module_sdk::caps::Capability;
    use avada_module_sdk::contract::methods::events as contract_events;
    use avada_module_sdk::contract::ErrorCode;
    use avada_module_sdk::rail::{Gesture, RowActivate};
    use avada_module_sdk::rights::InstallRecord;
    use avada_module_sdk::ModuleId;
    use serde_json::{json, Map, Value};
    use std::sync::mpsc::Receiver;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    // Loose on purpose: every spawn re-hashes this (large, debug) test binary, and a
    // crash test lives three times. Under a parallel cargo load 10 s was not enough;
    // a passing test never comes near the bound.
    const WAIT: Duration = Duration::from_secs(60);

    struct Rig {
        host: Host,
        record: InstallRecord,
        rail: Receiver<RailEvent>,
        events: Receiver<HostEvent>,
        _dir: Dir,
    }

    fn rig(caps: &[Capability], tweak: impl FnOnce(&mut HostConfig)) -> Rig {
        let dir = Dir::new("host");
        let record = testkit::record(caps);
        let mut config = HostConfig::new(dir.0.join("data"));
        config.workspace = Some(avada_module_sdk::contract::WorkspaceInfo {
            id: "ws1".into(),
            name: "Test".into(),
            root: None,
        });
        tweak(&mut config);
        let host = Host::new(config, Arc::new(DeclaredOnly::from_record(&record)));
        let rail = host.rail_events();
        let events = host.events();
        Rig {
            host,
            record,
            rail,
            events,
            _dir: dir,
        }
    }

    fn spawn(rig: &Rig, mode: &str) -> Result<(), HostError> {
        rig.host.spawn_with(
            &rig.record,
            &std::env::current_exe().unwrap(),
            &[(MODE_ENV.to_string(), mode.to_string())],
            &child_args(),
        )
    }

    fn all_ui() -> Vec<Capability> {
        vec![Capability::UiRail, Capability::UiCommands]
    }

    fn wait_status(
        host: &Host,
        id: &ModuleId,
        pred: impl Fn(&ModuleStatus) -> bool,
    ) -> ModuleStatus {
        let deadline = Instant::now() + WAIT;
        loop {
            let s = host.status(id);
            if pred(&s) || Instant::now() > deadline {
                return s;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn handshake_succeeds_and_the_rail_sees_entries_then_rows() {
        let r = rig(&all_ui(), |_| {});
        spawn(&r, "normal").unwrap();
        let id = r.record.module_id.clone();
        assert_eq!(r.host.status(&id), ModuleStatus::Running);
        match r.rail.recv_timeout(WAIT).unwrap() {
            RailEvent::Registered { module, entries } => {
                assert_eq!(module, id);
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].id, "files");
                assert_eq!(entries[0].module.as_ref(), Some(&id));
            }
            other => panic!("{other:?}"),
        }
        match r.rail.recv_timeout(WAIT).unwrap() {
            RailEvent::Rows { entry, rows, .. } => {
                assert_eq!(entry, "files");
                assert_eq!(rows[0].label, "README.md");
            }
            other => panic!("{other:?}"),
        }
        let state = r.host.rail_state(&id);
        assert_eq!(state.entries.len(), 1);
        assert_eq!(state.rows["files"].len(), 1);
        // Spawning again while live is a no-op.
        spawn(&r, "normal").unwrap();
        assert_eq!(r.host.statuses().len(), 1);
        r.host.shutdown(&id).unwrap();
    }

    #[test]
    fn host_to_module_calls_round_trip() {
        let r = rig(&all_ui(), |_| {});
        spawn(&r, "normal").unwrap();
        let id = r.record.module_id.clone();
        assert_eq!(
            r.host.activate(&id, None).unwrap(),
            json!({ "activated": true })
        );
        assert_eq!(
            r.host.invoke_command(&id, "reveal", Value::Null).unwrap(),
            json!({ "ran": "reveal" })
        );
        let row = RowActivate {
            entry: "files".into(),
            row: "readme".into(),
            data: json!({ "path": "README.md" }),
            gesture: Gesture::Open,
        };
        assert_eq!(
            r.host.activate_row(&id, &row).unwrap(),
            json!({ "row": "readme" })
        );
        assert_eq!(r.host.deactivate(&id).unwrap(), json!({}));
        match r.host.call(&id, "module.nope", Value::Null) {
            Err(HostError::Rpc(e)) => assert_eq!(e.kind(), ErrorCode::MethodNotFound),
            other => panic!("{other:?}"),
        }
        // A token was minted and only the exact value matches.
        assert!(!r.host.token_matches(&id, ""));
        assert!(!r.host.token_matches(&id, &"0".repeat(64)));
        r.host.shutdown(&id).unwrap();
        assert!(!r.host.token_matches(&id, ""));
    }

    #[test]
    fn set_prefs_persists_and_pushes_prefs_changed() {
        let r = rig(&all_ui(), |_| {});
        spawn(&r, "normal").unwrap();
        let id = r.record.module_id.clone();
        // Drain the two registration events.
        r.rail.recv_timeout(WAIT).unwrap();
        r.rail.recv_timeout(WAIT).unwrap();
        let mut values = Map::new();
        values.insert("theme".into(), json!("dark"));
        r.host.set_prefs(&id, values).unwrap();
        match r.rail.recv_timeout(WAIT).unwrap() {
            RailEvent::Registered { entries, .. } => assert_eq!(entries[0].label, "prefs \"dark\""),
            other => panic!("{other:?}"),
        }
        assert_eq!(r.host.prefs(&id)["theme"], "dark");
        let on_disk =
            std::fs::read_to_string(r.host.data_dir(&id).unwrap().join("prefs.json")).unwrap();
        assert!(on_disk.contains("dark"));
        r.host.shutdown(&id).unwrap();
    }

    #[test]
    fn manifest_mismatch_is_refused_and_marks_broken() {
        let r = rig(&all_ui(), |_| {});
        let id = r.record.module_id.clone();
        match spawn(&r, "bad-manifest") {
            Err(HostError::Handshake(HandshakeError::ManifestMismatch)) => {}
            other => panic!("{other:?}"),
        }
        assert!(matches!(r.host.status(&id), ModuleStatus::Broken { .. }));
        assert!(r.rail.try_recv().is_err());
    }

    #[test]
    fn hash_mismatch_is_refused_before_spawn() {
        let mut r = rig(&all_ui(), |_| {});
        r.record.artifact_sha256 = "f".repeat(64);
        let id = r.record.module_id.clone();
        match spawn(&r, "normal") {
            Err(HostError::HashMismatch { expected, actual }) => {
                assert_eq!(expected, "f".repeat(64));
                assert_eq!(actual, testkit::self_hash());
            }
            other => panic!("{other:?}"),
        }
        match r.host.status(&id) {
            ModuleStatus::Broken { reason } => assert!(reason.contains("changed")),
            other => panic!("{other:?}"),
        }
        // Nothing ran: no Starting status was ever announced.
        while let Ok(ev) = r.events.try_recv() {
            if let HostEvent::Status { status, .. } = ev {
                assert!(!status.is_live(), "{status:?}");
            }
        }
        // Reopen re-hashes and refuses again.
        assert!(matches!(
            r.host.reopen(&id),
            Err(HostError::HashMismatch { .. })
        ));
    }

    #[test]
    fn a_method_without_its_capability_is_refused_with_the_contract_code() {
        // Only ui.rail: the module's raw host.toast must come back as -32001.
        let r = rig(&[Capability::UiRail], |_| {});
        spawn(&r, "no-cap").unwrap();
        match r.rail.recv_timeout(WAIT).unwrap() {
            RailEvent::Registered { entries, .. } => {
                assert_eq!(entries[0].label, "denied -32001");
            }
            other => panic!("{other:?}"),
        }
        r.host.shutdown(&r.record.module_id).unwrap();
    }

    #[test]
    fn crash_restarts_with_toasts_then_disables_after_the_cap() {
        let r = rig(&all_ui(), |c| {
            c.policy = RestartPolicy {
                max_restarts: 2,
                window: Duration::from_secs(60),
                backoff: vec![Duration::from_millis(20)],
            };
        });
        spawn(&r, "crash").unwrap();
        let id = r.record.module_id.clone();
        let status = wait_status(&r.host, &id, |s| matches!(s, ModuleStatus::Disabled { .. }));
        match status {
            ModuleStatus::Disabled { reason } => assert!(reason.contains("3 times")),
            other => panic!("{other:?}"),
        }
        let mut toasts = 0;
        let mut crashed = Vec::new();
        while let Ok(ev) = r.events.try_recv() {
            match ev {
                HostEvent::Toast { level, .. } => {
                    assert!(level == "warn" || level == "error");
                    toasts += 1;
                }
                HostEvent::Status {
                    status: ModuleStatus::Crashed { restarts },
                    ..
                } => crashed.push(restarts),
                _ => {}
            }
        }
        assert_eq!(crashed, vec![1, 2]);
        assert_eq!(toasts, 3, "one per restart plus one for the disable");
        // Every life registered the rail and every death took it down again.
        let mut gone = 0;
        while let Ok(ev) = r.rail.try_recv() {
            if matches!(ev, RailEvent::Gone { .. }) {
                gone += 1;
            }
        }
        assert_eq!(gone, 3);
        assert!(r.host.rail_state(&id).entries.is_empty());
    }

    #[test]
    fn shutdown_kills_a_module_that_ignores_the_request_within_the_grace() {
        let r = rig(&all_ui(), |c| c.shutdown_grace = Duration::from_millis(300));
        spawn(&r, "hang").unwrap();
        let id = r.record.module_id.clone();
        let t0 = Instant::now();
        r.host.shutdown(&id).unwrap();
        assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
        assert!(matches!(r.host.status(&id), ModuleStatus::Disabled { .. }));
        let gone = std::iter::from_fn(|| r.rail.try_recv().ok())
            .any(|e| matches!(e, RailEvent::Gone { .. }));
        assert!(gone);
        // A shutdown module is not restarted, and can be reopened on purpose.
        std::thread::sleep(Duration::from_millis(100));
        assert!(matches!(r.host.status(&id), ModuleStatus::Disabled { .. }));
        r.host.reopen(&id).unwrap();
        assert_eq!(r.host.status(&id), ModuleStatus::Running);
        r.host.shutdown(&id).unwrap();
    }

    #[test]
    fn a_module_that_never_says_hello_is_cut_off_at_the_timeout() {
        let r = rig(&all_ui(), |c| {
            c.handshake_timeout = Duration::from_millis(300)
        });
        let id = r.record.module_id.clone();
        let t0 = Instant::now();
        let outcome = spawn(&r, "silent");
        assert!(
            matches!(outcome, Err(HostError::HandshakeTimeout)),
            "{outcome:?}"
        );
        // The silent child sleeps 30 s; anything well under that proves the cut-off.
        // The bound is loose because every spawn re-hashes this (large, debug) test
        // binary, and a parallel test run makes that slow.
        assert!(t0.elapsed() < Duration::from_secs(25), "{:?}", t0.elapsed());
        assert!(matches!(r.host.status(&id), ModuleStatus::Disabled { .. }));
    }

    #[test]
    fn unknown_ids_are_not_installed_and_dropping_the_host_stops_modules() {
        let r = rig(&all_ui(), |_| {});
        let other = ModuleId::new("nobody/nothing").unwrap();
        assert_eq!(r.host.status(&other), ModuleStatus::NotInstalled);
        assert!(matches!(
            r.host.shutdown(&other),
            Err(HostError::NotInstalled(_))
        ));
        spawn(&r, "normal").unwrap();
        let id = r.record.module_id.clone();
        let rail = r.rail;
        drop(r.host);
        let gone = std::iter::from_fn(|| rail.recv_timeout(WAIT).ok())
            .any(|e| matches!(e, RailEvent::Gone { module } if module == id));
        assert!(gone);
    }
    /// A licence gate that always answers the same thing and remembers what it was
    /// asked, so a test can prove a free module is never a licence question.
    struct Fixed {
        answer: crate::license::Gate,
        asked: std::sync::Mutex<Vec<(ModuleId, u64)>>,
    }

    impl Fixed {
        fn new(answer: crate::license::Gate) -> Arc<Fixed> {
            Arc::new(Fixed {
                answer,
                asked: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn asked(&self) -> Vec<(ModuleId, u64)> {
            self.asked.lock().unwrap().clone()
        }
    }

    impl Licensing for Fixed {
        fn gate(&self, product: &ModuleId, major: u64) -> crate::license::Gate {
            self.asked.lock().unwrap().push((product.clone(), major));
            self.answer.clone()
        }
    }

    /// Sell the fixture module. `Manifest::validate` only accepts `commercial` with an
    /// issuer, so a record that skipped the issuer would not describe a real install.
    fn commercial(r: &mut Rig) {
        testkit::sell(&mut r.record.manifest);
    }

    #[test]
    fn a_free_module_is_never_a_licence_question() {
        let r = rig(&all_ui(), |_| {});
        let gate = Fixed::new(crate::license::Gate::Refuse("refuses everything".into()));
        r.host.set_licensing(gate.clone());
        spawn(&r, "normal").unwrap();
        let id = r.record.module_id.clone();
        assert_eq!(r.host.status(&id), ModuleStatus::Running);
        assert!(
            gate.asked().is_empty(),
            "a manifest without `commercial` must not reach the gate"
        );
        r.host.shutdown(&id).unwrap();
    }

    #[test]
    fn a_commercial_module_runs_when_the_host_has_no_licence_gate() {
        // The free edition ships no LicenseService, and already refuses a binary
        // distribution at install time; failing open here protects nothing.
        let mut r = rig(&all_ui(), |_| {});
        commercial(&mut r);
        assert!(r.host.licensing().is_none());
        spawn(&r, "commercial").unwrap();
        let id = r.record.module_id.clone();
        assert_eq!(r.host.status(&id), ModuleStatus::Running);
        r.host.shutdown(&id).unwrap();
    }

    #[test]
    fn a_refused_licence_stops_the_spawn_and_leaves_the_module_broken() {
        let mut r = rig(&all_ui(), |_| {});
        commercial(&mut r);
        let gate = Fixed::new(crate::license::Gate::Refuse(
            "license for acme/avada-files expired 3 days ago".into(),
        ));
        r.host.set_licensing(gate.clone());
        let id = r.record.module_id.clone();
        match spawn(&r, "commercial") {
            Err(HostError::Unlicensed(reason)) => assert!(reason.contains("expired"), "{reason}"),
            other => panic!("{other:?}"),
        }
        match r.host.status(&id) {
            ModuleStatus::Broken { reason } => assert!(reason.contains("expired"), "{reason}"),
            other => panic!("{other:?}"),
        }
        // Asked once, about this module at the major it is installed at.
        assert_eq!(gate.asked(), vec![(id, r.record.version.major)]);
        // Nothing ran: no live status was announced and the rail stayed empty.
        while let Ok(ev) = r.events.try_recv() {
            if let HostEvent::Status { status, .. } = ev {
                assert!(!status.is_live(), "{status:?}");
            }
        }
        assert!(r.rail.try_recv().is_err());
    }

    #[test]
    fn a_licence_inside_its_grace_window_runs_and_says_so_on_a_toast() {
        let mut r = rig(&all_ui(), |_| {});
        commercial(&mut r);
        r.host
            .set_licensing(Fixed::new(crate::license::Gate::RunWithBanner(
                "license expires in 3 days".into(),
            )));
        let id = r.record.module_id.clone();
        spawn(&r, "commercial").unwrap();
        assert_eq!(r.host.status(&id), ModuleStatus::Running);
        // The banner is the first thing the app hears, ahead of Starting: the user
        // learns a licence is running out before the module is even up.
        match r.events.recv_timeout(WAIT).unwrap() {
            HostEvent::Toast {
                module,
                text,
                level,
            } => {
                assert_eq!(module, id);
                assert_eq!(text, "license expires in 3 days");
                assert_eq!(level, "warn");
            }
            other => panic!("{other:?}"),
        }
        r.host.shutdown(&id).unwrap();
    }

    #[test]
    fn set_licensing_reaches_a_module_that_was_installed_before_it() {
        let mut r = rig(&all_ui(), |_| {});
        commercial(&mut r);
        let id = r.record.module_id.clone();
        spawn(&r, "commercial").unwrap();
        assert_eq!(r.host.status(&id), ModuleStatus::Running);
        r.host.shutdown(&id).unwrap();

        // The gate arrives after the slot did. Because the slot shares the host's cell
        // rather than a copy of it, the next start consults the new gate.
        r.host
            .set_licensing(Fixed::new(crate::license::Gate::Refuse(
                "no seat left".into(),
            )));
        match r.host.reopen(&id) {
            Err(HostError::Unlicensed(reason)) => assert_eq!(reason, "no seat left"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn emit_reaches_the_subscribed_module_and_nobody_else() {
        let r = rig(
            &[
                Capability::UiRail,
                Capability::UiCommands,
                Capability::EventsSubscribe,
            ],
            |_| {},
        );
        spawn(&r, "events").unwrap();
        let id = r.record.module_id.clone();
        // Drain registration + rows.
        r.rail.recv_timeout(WAIT).unwrap();
        r.rail.recv_timeout(WAIT).unwrap();

        // A kind the module never named: nobody is told, and no rail traffic follows.
        assert_eq!(
            r.host.emit(
                contract_events::FILES_REVEAL,
                json!({ "path": "/w/README.md" })
            ),
            0
        );

        // The kind it did name arrives, payload intact.
        assert_eq!(
            r.host.emit(
                contract_events::RAIL_QUERY,
                json!({"entry":"files","query":"rea"})
            ),
            1
        );
        match r.rail.recv_timeout(WAIT).unwrap() {
            RailEvent::Registered { entries, .. } => assert_eq!(
                entries[0].label,
                r#"event "rail.query" {"entry":"files","query":"rea"}"#
            ),
            other => panic!("{other:?}"),
        }
        r.host.shutdown(&id).unwrap();

        // Dead modules are skipped rather than counted.
        assert_eq!(r.host.emit(contract_events::RAIL_QUERY, json!({})), 0);
    }

    #[test]
    fn a_module_that_never_subscribed_hears_nothing() {
        let r = rig(&all_ui(), |_| {});
        spawn(&r, "normal").unwrap();
        let id = r.record.module_id.clone();
        r.rail.recv_timeout(WAIT).unwrap();
        r.rail.recv_timeout(WAIT).unwrap();
        assert_eq!(r.host.emit(contract_events::RAIL_QUERY, json!({})), 0);
        assert_eq!(r.host.emit(contract_events::FILES_REVEAL, json!({})), 0);
        assert!(r.rail.try_recv().is_err());
        r.host.shutdown(&id).unwrap();
    }

    #[test]
    fn the_workspace_root_seeds_the_fs_scope_and_a_switch_retargets_it() {
        let inside = Dir::new("ws-root");
        let root = inside.0.join("w");
        std::fs::create_dir_all(&root).unwrap();
        let r = rig(&all_ui(), |c| {
            c.workspace = Some(avada_module_sdk::contract::WorkspaceInfo {
                id: "ws1".into(),
                name: "One".into(),
                root: Some(root.to_string_lossy().into_owned()),
            });
        });
        assert_eq!(r.host.workspace_root(), Some(root.clone()));

        // A workspace switch moves every module's filesystem scope with it, even for
        // modules that were already running when the human switched.
        let other = inside.0.join("other");
        std::fs::create_dir_all(&other).unwrap();
        spawn(&r, "normal").unwrap();
        let id = r.record.module_id.clone();
        r.host
            .activate(
                &id,
                Some(avada_module_sdk::contract::WorkspaceInfo {
                    id: "ws2".into(),
                    name: "Two".into(),
                    root: Some(other.to_string_lossy().into_owned()),
                }),
            )
            .unwrap();
        assert_eq!(r.host.workspace_root(), Some(other));
        r.host.shutdown(&id).unwrap();
    }
}
