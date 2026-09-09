//! End to end: the SDK's `hello` example module, installed through the install store
//! and run by the module host, exactly as a real module would be.
//!
//! The test builds `crates/module-sdk/examples/hello.rs` with the same `cargo` that is
//! running the tests, installs the binary into an [`InstallStore`] in a scratch
//! directory (the record carries the accepted capabilities and the artifact hash),
//! verifies it, spawns it through the [`Host`], watches the rail fill, round-trips
//! `module.activate` and `module.command.invoke`, makes the module crash and sees the
//! supervisor bring it back, shuts it down cleanly, and finally proves a binary whose
//! bytes changed since the user accepted it is refused before anything runs.
//!
//! Unix only: the SDK's `client::from_env` (the example's transport) is `#[cfg(unix)]`,
//! and the host hands the module an inherited socketpair. The Windows named-pipe
//! transport has its own tests.
//!
//! Every wait has a bounded deadline ([`WAIT`]); nothing sleeps for a fixed time.
#![cfg(unix)]

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};

use avada_core::install::{hash_file, InstallError, InstallPaths, InstallStore, MemoryKeyStore};
use avada_core::module::{
    DeclaredOnly, Host, HostConfig, HostError, HostEvent, ModuleStatus, RailEvent, RestartPolicy,
};
use avada_module_sdk::contract::WorkspaceInfo;
use avada_module_sdk::manifest::DistributionKind;
use avada_module_sdk::rights::{InstallKind, InstallRecord};
use avada_module_sdk::{Capability, Manifest, ModuleId};
use serde_json::{json, Value};

/// The deadline for every wait in this file.
const WAIT: Duration = Duration::from_secs(60);

/// A private scratch directory, removed on drop.
/// One raw HTTP/1.1 exchange against the control server (same helper as
/// `control_parity.rs`, so the `/m/...` leg uses no HTTP client crate).
fn request(port: u16, method: &str, path: &str, token: Option<&str>) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n");
    if let Some(t) = token {
        req.push_str(&format!("Authorization: Bearer {t}\r\n"));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).expect("write");
    let mut resp = String::new();
    stream.read_to_string(&mut resp).expect("read");
    let status: u16 = resp
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .expect("status code");
    let body = resp
        .split_once("\r\n\r\n")
        .map(|x| x.1)
        .unwrap_or("")
        .to_string();
    (status, body)
}

/// The schema bridge applies host events on its own thread, so a route appears (and
/// vanishes) a moment after the test sees the event: poll until the status settles.
fn request_until(port: u16, path: &str, token: Option<&str>, want: u16) -> (u16, String) {
    let deadline = Instant::now() + WAIT;
    loop {
        let (status, body) = request(port, "GET", path, token);
        if status == want || Instant::now() > deadline {
            return (status, body);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

const MASTER: &str = "e2e-master-token";

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("avada-hello-e2e-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        Scratch(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The workspace root: `crates/core` is two levels below it.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/core sits two levels under the workspace")
        .to_path_buf()
}

/// Build the `hello` example with the cargo that runs this test and return the
/// executable's path. `CARGO_TARGET_DIR` is inherited, so the build lands in the same
/// target directory as the test itself; the path comes from cargo's own artifact
/// message rather than a guess.
fn build_hello() -> PathBuf {
    let cargo = env!("CARGO");
    let out = Command::new(cargo)
        .args([
            "build",
            "--offline",
            "--example",
            "hello",
            "-p",
            "avada-module-sdk",
            "--message-format=json",
        ])
        .current_dir(workspace_root())
        .output()
        .expect("run cargo build for the hello example");
    assert!(
        out.status.success(),
        "cargo build --example hello failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let from_cargo = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|m| m["reason"] == "compiler-artifact")
        .filter(|m| m["target"]["name"] == "hello")
        .filter(|m| {
            m["target"]["kind"]
                .as_array()
                .is_some_and(|kinds| kinds.iter().any(|k| k == "example"))
        })
        .find_map(|m| m["executable"].as_str().map(PathBuf::from));
    let binary = from_cargo.unwrap_or_else(|| {
        let target = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| workspace_root().join("target"));
        target.join("debug").join("examples").join("hello")
    });
    assert!(
        binary.is_file(),
        "hello example not found at {}",
        binary.display()
    );
    binary
}

/// Ask the built module for the manifest it will present, so the record matches the
/// hello byte for byte (`InstallRecord::matches_hello`).
fn hello_manifest(binary: &Path, home: &Path) -> Manifest {
    let out = Command::new(binary)
        .arg("--manifest")
        .env("HOME", home)
        .output()
        .expect("run hello --manifest");
    assert!(out.status.success(), "hello --manifest failed");
    let text = String::from_utf8(out.stdout).expect("manifest is UTF-8");
    Manifest::parse(&text).expect("hello's manifest validates")
}

fn record(manifest: Manifest, sha256: String) -> InstallRecord {
    InstallRecord {
        module_id: manifest.id().clone(),
        repo: format!("https://github.com/{}", manifest.id().as_str()),
        tag: manifest.tag(),
        commit: "0123456789abcdef0123456789abcdef01234567".into(),
        version: manifest.module.version.clone(),
        artifact_sha256: sha256,
        source: DistributionKind::Source,
        accepted: BTreeSet::from([
            Capability::UiRail,
            Capability::UiCommands,
            Capability::ControlRoute,
        ]),
        manifest,
        installed_at: 1_700_000_000,
        kind: InstallKind::Manual,
    }
}

fn new_host(data_root: PathBuf, record: &InstallRecord) -> Host {
    let mut config = HostConfig::new(data_root);
    config.workspace = Some(WorkspaceInfo {
        id: "ws1".into(),
        name: "E2E".into(),
        root: None,
    });
    // Restart quickly so the crash leg of the test is not dominated by backoff.
    config.policy = RestartPolicy {
        max_restarts: 3,
        window: Duration::from_secs(60),
        backoff: vec![Duration::from_millis(20)],
    };
    Host::new(config, Arc::new(DeclaredOnly::from_record(record)))
}

/// Poll the status until `pred` holds or the deadline passes; returns the last status.
fn wait_status(host: &Host, id: &ModuleId, pred: impl Fn(&ModuleStatus) -> bool) -> ModuleStatus {
    let deadline = Instant::now() + WAIT;
    loop {
        let s = host.status(id);
        if pred(&s) || Instant::now() > deadline {
            return s;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The next event for which `pick` returns `Some`, skipping the others; panics at the
/// deadline.
fn next<T, R>(rx: &Receiver<T>, what: &str, pick: impl Fn(T) -> Option<R>) -> R
where
    T: std::fmt::Debug,
{
    let deadline = Instant::now() + WAIT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let ev = rx
            .recv_timeout(left)
            .unwrap_or_else(|e| panic!("waiting for {what}: {e:?}"));
        if let Some(r) = pick(ev) {
            return r;
        }
    }
}

#[test]
fn hello_module_installs_runs_restarts_and_refuses_a_tampered_binary() {
    let scratch = Scratch::new();
    let home = scratch.path("home");
    std::fs::create_dir_all(&home).unwrap();

    // ---- build and hash ---------------------------------------------------------
    let built = build_hello();
    let sha256 = avada_core::module::spawn::sha256_hex(&built).expect("hash the example");
    assert_eq!(sha256.len(), 64);
    assert_eq!(hash_file(&built).unwrap(), sha256, "both hashers agree");
    let manifest = hello_manifest(&built, &home);
    let id = manifest.id().clone();
    let version = manifest.module.version.clone();
    assert_eq!(id.as_str(), "avada/hello");

    // ---- install ----------------------------------------------------------------
    let store = InstallStore::open(
        InstallPaths::under(scratch.path("modules")),
        Arc::new(MemoryKeyStore::new()),
    )
    .expect("open the store");
    let installed = store
        .install(record(manifest.clone(), sha256.clone()), &built, None)
        .expect("install the hello example");
    assert!(installed.active, "the lockfile pins the only version");
    assert_eq!(installed.rights().artifact_sha256, sha256);
    assert_eq!(installed.rights().module_id, id);
    assert!(installed.binary.is_file());
    assert_eq!(store.binary_path(&id, &version).unwrap(), installed.binary);

    // The per-spawn check: record MAC ok, binary re-hashed against it.
    let verified = store.verify_binary(&id, &version).expect("verify_binary");
    assert_eq!(verified.binary, installed.binary);
    assert_eq!(hash_file(&verified.binary).unwrap(), sha256);
    let rights = verified.rights().clone();

    // ---- spawn and handshake ----------------------------------------------------
    let host = new_host(scratch.path("data"), &rights);
    let rail = host.rail_events();
    let events = host.events();
    // The control server, wired to this host: `/m/avada/hello/...` forwards to the
    // module and the schema follows its routes. Attached before the spawn so the first
    // `host.routes.register` is seen.
    let (shared, port) =
        avada_core::control::server::serve_for_test(scratch.path("control.json"), true, MASTER)
            .expect("boot the control server");
    let _bridge = avada_core::control::modules::attach_host(shared.clone(), &host);
    host.spawn_with(
        &rights,
        &verified.binary,
        &[("HOME".to_string(), home.display().to_string())],
        &[],
    )
    .expect("spawn hello");
    let status = wait_status(&host, &id, |s| *s == ModuleStatus::Running);
    assert_eq!(status, ModuleStatus::Running);

    // ---- rail: one entry, then its rows -----------------------------------------
    let entries = next(&rail, "RailEvent::Registered", |ev| match ev {
        RailEvent::Registered { module, entries } if module == id => Some(entries),
        _ => None,
    });
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, "hello");
    assert_eq!(entries[0].label, "Hello");
    assert_eq!(
        entries[0].module.as_ref(),
        Some(&id),
        "the host fills in the owner"
    );
    let (entry, rows) = next(&rail, "RailEvent::Rows", |ev| match ev {
        RailEvent::Rows {
            module,
            entry,
            rows,
            ..
        } if module == id => Some((entry, rows)),
        _ => None,
    });
    assert_eq!(entry, "hello");
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].id, "greeting");
    assert_eq!(rows[0].label, "Hello, world");
    assert_eq!(rows[2].detail, "E2E", "the module saw the host's workspace");
    assert_eq!(host.rail_state(&id).entries.len(), 1);

    // The command palette entry came through the `ui.commands` capability.
    let commands = next(&events, "HostEvent::Commands", |ev| match ev {
        HostEvent::Commands { module, commands } if module == id => Some(commands),
        _ => None,
    });
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].id, "greet");

    // ---- the control-plane route: registered, in the schema, served over HTTP -----
    let routes = next(&events, "HostEvent::Routes", |ev| match ev {
        HostEvent::Routes { module, routes } if module == id => Some(routes),
        _ => None,
    });
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].method, "hello.greet");
    assert_eq!(
        routes[0].module.as_ref(),
        Some(&id),
        "the host fills in the owner"
    );
    assert_eq!(routes[0].mounted_path(), "/m/avada/hello/greet/{name}");
    assert_eq!(host.routes(&id), routes);
    let (status, body) = request_until(port, "/m/avada/hello/greet/Avada", Some(MASTER), 200);
    assert_eq!(status, 200, "{body}");
    let answer: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(answer["greeting"], json!("Hello, Avada"));
    assert_eq!(
        answer["count"],
        json!(1),
        "the route shares the process's counter"
    );
    let schema: Value =
        serde_json::from_str(&request(port, "GET", "/schema", Some(MASTER)).1).unwrap();
    let listed = schema["routes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["method"] == json!("hello.greet"))
        .expect("the module's route is in GET /schema");
    assert_eq!(listed["module"], json!("avada/hello"));
    assert_eq!(listed["path"], json!("/greet/{name}"));
    // The usual order of refusals in front of the module: no token, no such route,
    // wrong verb.
    assert_eq!(request(port, "GET", "/m/avada/hello/greet/x", None).0, 401);
    assert_eq!(
        request(port, "GET", "/m/avada/hello/nope", Some(MASTER)).0,
        404
    );
    assert_eq!(
        request(port, "POST", "/m/avada/hello/greet/x", Some(MASTER)).0,
        405
    );

    // ---- activate and command round-trips ---------------------------------------
    let activated = host.activate(&id, None).expect("module.activate");
    assert_eq!(activated["activated"], json!(true));
    assert_eq!(activated["workspace"], json!("ws1"));
    let greeted = host
        .invoke_command(&id, "greet", json!({ "name": "Avada" }))
        .expect("module.command.invoke");
    assert_eq!(greeted["greeting"], json!("Hello, Avada"));
    assert_eq!(greeted["count"], json!(2), "the HTTP greeting counted too");
    let again = host.invoke_command(&id, "greet", Value::Null).unwrap();
    assert_eq!(again["greeting"], json!("Hello, world"));
    assert_eq!(again["count"], json!(3), "same process, third greeting");

    // ---- crash and restart ------------------------------------------------------
    // `crash` makes the example exit 3 without answering, so the call itself fails.
    let crashed = host.invoke_command(&id, "crash", Value::Null);
    assert!(crashed.is_err(), "{crashed:?}");
    let restarts = next(&events, "Crashed status", |ev| match ev {
        HostEvent::Status {
            module,
            status: ModuleStatus::Crashed { restarts },
        } if module == id => Some(restarts),
        _ => None,
    });
    assert_eq!(restarts, 1);
    // The supervisor announces every restart with a toast.
    let level = next(&events, "restart toast", |ev| match ev {
        HostEvent::Toast { module, level, .. } if module == id => Some(level),
        _ => None,
    });
    assert!(level == "warn" || level == "error", "{level}");
    let status = wait_status(&host, &id, |s| *s == ModuleStatus::Running);
    assert_eq!(
        status,
        ModuleStatus::Running,
        "running again after the restart"
    );
    // The rail went away with the old process and came back with the new one.
    next(&rail, "RailEvent::Gone", |ev| match ev {
        RailEvent::Gone { module } if module == id => Some(()),
        _ => None,
    });
    let entries = next(
        &rail,
        "RailEvent::Registered after restart",
        |ev| match ev {
            RailEvent::Registered { module, entries } if module == id => Some(entries),
            _ => None,
        },
    );
    assert_eq!(entries[0].id, "hello");
    next(&rail, "RailEvent::Rows after restart", |ev| match ev {
        RailEvent::Rows { module, rows, .. } if module == id => Some(rows),
        _ => None,
    });
    // The new process registered its route again, and the bridge put it back.
    let routes = next(&events, "HostEvent::Routes after restart", |ev| match ev {
        HostEvent::Routes { module, routes } if module == id => Some(routes),
        _ => None,
    });
    assert_eq!(routes[0].method, "hello.greet");
    let (status, body) = request_until(port, "/m/avada/hello/greet/again", Some(MASTER), 200);
    assert_eq!(status, 200, "{body}");
    let answer: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        answer["count"],
        json!(1),
        "a new process starts counting again"
    );
    let fresh = host.invoke_command(&id, "greet", Value::Null).unwrap();
    assert_eq!(fresh["count"], json!(2));

    // ---- clean shutdown ---------------------------------------------------------
    host.shutdown(&id).expect("module.shutdown");
    match host.status(&id) {
        ModuleStatus::Disabled { reason } => assert!(reason.contains("shut down"), "{reason}"),
        other => panic!("after shutdown: {other:?}"),
    }
    next(&rail, "RailEvent::Gone after shutdown", |ev| match ev {
        RailEvent::Gone { module } if module == id => Some(()),
        _ => None,
    });
    next(&events, "Disabled status", |ev| match ev {
        HostEvent::Status {
            module,
            status: ModuleStatus::Disabled { .. },
        } if module == id => Some(()),
        _ => None,
    });
    // A module that exits on request is not a crash: no toast, no Crashed status, and
    // no restart follow the Disabled status. The window is bounded, not a fixed sleep:
    // it ends early on the first (unexpected) event.
    if let Ok(ev) = events.recv_timeout(Duration::from_millis(300)) {
        match ev {
            HostEvent::Toast { .. } => panic!("a clean shutdown produced a toast: {ev:?}"),
            HostEvent::Status { status, .. } => {
                assert!(!status.is_live(), "restarted after shutdown: {status:?}");
                assert!(
                    !matches!(status, ModuleStatus::Crashed { .. }),
                    "a clean exit was reported as a crash"
                );
            }
            _ => {}
        }
    }
    assert!(matches!(host.status(&id), ModuleStatus::Disabled { .. }));
    assert!(host.rail_state(&id).entries.is_empty());
    // The Disabled status took the route out of the schema: 404 now, never 200. (Until
    // the bridge caught up the answer was 503 — listed but nobody home — which is also
    // not a greeting.)
    let (status, body) = request_until(port, "/m/avada/hello/greet/x", Some(MASTER), 404);
    assert_eq!(status, 404, "{body}");
    let schema: Value =
        serde_json::from_str(&request(port, "GET", "/schema", Some(MASTER)).1).unwrap();
    assert!(
        !schema["routes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["method"] == json!("hello.greet")),
        "the route left GET /schema with the module"
    );
    drop(shared);

    // ---- a binary that changed since it was accepted is refused ----------------
    let tampered = scratch.path("hello-tampered");
    let mut bytes = std::fs::read(&built).unwrap();
    let at = bytes.len() / 2;
    bytes[at] ^= 0xFF;
    std::fs::write(&tampered, &bytes).unwrap();
    let tampered_sha = hash_file(&tampered).unwrap();
    assert_ne!(tampered_sha, sha256);
    let second = record(manifest.clone(), sha256.clone());

    // The store refuses to install it against the original hash…
    let store2 = InstallStore::open(
        InstallPaths::under(scratch.path("modules2")),
        Arc::new(MemoryKeyStore::new()),
    )
    .unwrap();
    match store2.install(second.clone(), &tampered, None) {
        Err(InstallError::HashMismatch {
            expected, actual, ..
        }) => {
            assert_eq!(expected, sha256);
            assert_eq!(actual, tampered_sha);
        }
        other => panic!("install of a tampered artifact: {other:?}"),
    }
    // …and `verify_binary` catches a binary swapped after a good install.
    let good = store2.install(second.clone(), &built, None).unwrap();
    std::fs::write(&good.binary, &bytes).unwrap();
    match store2.verify_binary(&id, &version) {
        Err(InstallError::HashMismatch { expected, .. }) => assert_eq!(expected, sha256),
        other => panic!("verify_binary on a swapped binary: {other:?}"),
    }

    // The host re-hashes before every spawn and runs nothing on a mismatch.
    let host2 = new_host(scratch.path("data2"), &second);
    let events2 = host2.events();
    match host2.spawn(&second, &tampered) {
        Err(HostError::HashMismatch { expected, actual }) => {
            assert_eq!(expected, sha256);
            assert_eq!(actual, tampered_sha);
        }
        other => panic!("spawn of a tampered binary: {other:?}"),
    }
    match host2.status(&id) {
        ModuleStatus::Broken { reason } => assert!(reason.contains("changed"), "{reason}"),
        other => panic!("after a refused spawn: {other:?}"),
    }
    while let Ok(ev) = events2.try_recv() {
        if let HostEvent::Status { status, .. } = ev {
            assert!(!status.is_live(), "something ran: {status:?}");
        }
    }
    assert!(matches!(
        host2.reopen(&id),
        Err(HostError::HashMismatch { .. })
    ));
}
