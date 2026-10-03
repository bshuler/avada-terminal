//! The Marketplace *module*, end to end: the real `avada-marketplace` binary, spawned by
//! the module host, talking to a real control server whose `/marketplace/...` routes are
//! answered by a [`Marketplace`] over the fakes in [`super::testing`].
//!
//! Every action is driven the way the app drives it — a palette command
//! (`module.command.invoke`) or a click on a rail row (`module.row.activate` with the
//! row's own `data`) — and every outcome is read back twice: from the store the server
//! owns, and from the rows the module re-renders. That covers the leg no other test
//! does: the module's token being honoured by the control server (`HostTokens`), the
//! control URL reaching the module, and each row's `data` naming an action the server
//! accepts.
//!
//! The walk: search → click the result (install) → the Installed row says
//! "open to disable" → click it (disable) → it says "open to enable" → click it (enable)
//! → click its Uninstall row twice → the row is gone and the store is empty.
//!
//! Unix only: the pipeline rig runs POSIX-shell fakes for cargo and git.

use super::job::Phase;
use super::testing::{files_state, manifest_for, rig, scratch, wait, FakeCargo, FILES};
use crate::install::{InstallPaths, InstallStore, MemoryKeyStore};
use crate::module::{DeclaredOnly, Host, HostConfig, ModuleStatus};
use avada_module_sdk::contract::WorkspaceInfo;
use avada_module_sdk::manifest::DistributionKind;
use avada_module_sdk::rail::{Gesture, Row, RowActivate};
use avada_module_sdk::rights::{InstallKind, InstallRecord};
use avada_module_sdk::{Manifest, ModuleId};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(60);
const WS: &str = "ws-e2e";
const ENTRY: &str = "marketplace";
const MASTER: &str = "marketplace-e2e-master";

/// Build `avada-marketplace` with the cargo running this test; cargo's own artifact
/// report names the binary, so a custom target dir is no problem.
fn build_marketplace() -> PathBuf {
    let out = Command::new(env!("CARGO"))
        .args([
            "build",
            "-q",
            "-p",
            "avada-marketplace",
            "--message-format=json",
        ])
        .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .output()
        .expect("run cargo build for avada-marketplace");
    assert!(
        out.status.success(),
        "cargo build -p avada-marketplace failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|m| m["target"]["name"] == "avada-marketplace")
        .filter_map(|m| m["executable"].as_str().map(PathBuf::from))
        .last()
        .expect("cargo reported the avada-marketplace executable")
}

/// The record a first-party seed gets: everything the manifest asks for, accepted.
fn record(binary: &Path, home: &Path) -> InstallRecord {
    let out = Command::new(binary)
        .arg("--manifest")
        .env("HOME", home)
        .output()
        .expect("run avada-marketplace --manifest");
    assert!(out.status.success(), "avada-marketplace --manifest failed");
    let manifest = Manifest::parse(&String::from_utf8(out.stdout).unwrap()).unwrap();
    InstallRecord {
        module_id: manifest.id().clone(),
        repo: format!("https://github.com/{}", manifest.id().as_str()),
        tag: manifest.tag(),
        commit: "0123456789abcdef0123456789abcdef01234567".into(),
        version: manifest.module.version.clone(),
        artifact_sha256: crate::install::hash_file(binary).unwrap(),
        skills_sha256: String::new(),
        source: DistributionKind::Source,
        accepted: manifest.capabilities.iter().copied().collect(),
        manifest,
        installed_at: 1_700_000_000,
        kind: InstallKind::Manual,
    }
}

/// Poll the module's rendered rows until `pick` finds what it wants.
fn rows_until<R>(host: &Host, id: &ModuleId, what: &str, pick: impl Fn(&[Row]) -> Option<R>) -> R {
    let deadline = Instant::now() + WAIT;
    loop {
        let rows = host
            .rail_state(id)
            .rows
            .get(ENTRY)
            .cloned()
            .unwrap_or_default();
        if let Some(r) = pick(&rows) {
            return r;
        }
        assert!(
            Instant::now() < deadline,
            "waiting for {what}; rows: {rows:#?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The row whose `data` carries `action` for `module`, if one is showing.
fn row_for(rows: &[Row], action: &str, module: &str) -> Option<Row> {
    rows.iter()
        .find(|r| r.data["action"] == action && r.data["module"] == module)
        .cloned()
}

/// A click on `row`, exactly as the rail sends it.
fn click(host: &Host, id: &ModuleId, row: &Row) -> Value {
    host.activate_row(
        id,
        &RowActivate {
            entry: ENTRY.into(),
            target: Default::default(),
            row: row.id.clone(),
            data: row.data.clone(),
            gesture: Gesture::Open,
        },
    )
    .unwrap_or_else(|e| panic!("clicking {:?} failed: {e}", row.label))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_marketplace_module_installs_disables_enables_and_uninstalls() {
    // ---- the server side: fake GitHub, a tagged fixture repo, fake cargo -------------
    let r = rig(
        "module-e2e",
        files_state(),
        FakeCargo::Builds,
        Duration::from_secs(300),
    )
    .await;
    r.fixtures.repo(
        FILES,
        "v1.0.0",
        &manifest_for(FILES, "1.0.0", "kind = \"source\"", ""),
    );
    let mp = Arc::clone(&r.mp);
    let dir = scratch("module-e2e-host");
    let (shared, port) =
        crate::control::server::serve_for_test(dir.join("control.json"), true, MASTER).unwrap();
    shared.install_marketplace(Arc::clone(&mp));

    // Everything below blocks on module round trips; keep it off the async workers.
    tokio::task::spawn_blocking(move || {
        // ---- the module side: the real binary, installed and spawned as the app does
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let built = build_marketplace();
        let rec = record(&built, &home);
        let id = rec.module_id.clone();
        assert!(
            rec.accepted
                .contains(&avada_module_sdk::Capability::MarketplaceManage),
            "the seed accepts marketplace.manage"
        );
        let store = InstallStore::open(
            InstallPaths::under(dir.join("modules")),
            Arc::new(MemoryKeyStore::new()),
        )
        .unwrap();
        let installed = store.install(rec, &built, None).unwrap();
        let rights = installed.rights().clone();

        let mut config = HostConfig::new(dir.join("data"));
        config.workspace = Some(WorkspaceInfo {
            id: WS.into(),
            name: "E2E".into(),
            root: None,
        });
        let host = Host::new(config, Arc::new(DeclaredOnly::from_record(&rights)));
        let _bridge = crate::control::modules::attach_host(shared.clone(), &host);
        host.set_control_url(Some(format!("http://127.0.0.1:{port}")));
        host.spawn_with(
            &rights,
            &installed.binary,
            &[("HOME".to_string(), home.display().to_string())],
            &[],
        )
        .unwrap();
        let deadline = Instant::now() + WAIT;
        while host.status(&id) != ModuleStatus::Running {
            assert!(Instant::now() < deadline, "status {:?}", host.status(&id));
            std::thread::sleep(Duration::from_millis(10));
        }
        // Activation is the first call that needs the control server; a 401 here was
        // the bug that left the live rail blank.
        host.activate(&id, None)
            .expect("module.activate reaches the server");
        rows_until(&host, &id, "the Installed header", |rows| {
            rows.iter().any(|r| r.label == "Installed").then_some(())
        });
        assert!(mp.installed().unwrap().is_empty(), "fresh store");

        // ---- install: palette search, then click the result ----------------------
        host.invoke_command(&id, "search", json!({ "q": "files" }))
            .expect("Marketplace: Search modules");
        let result = rows_until(&host, &id, "a search result for acme/avada-files", |rows| {
            row_for(rows, "install", FILES)
        });
        assert!(result.detail.contains("open to install"), "{result:?}");
        click(&host, &id, &result);

        let job = {
            let deadline = Instant::now() + WAIT;
            loop {
                if let Some(j) = mp.jobs().into_iter().find(|j| j.module == FILES) {
                    break j;
                }
                assert!(Instant::now() < deadline, "the click never started a job");
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        let done = tokio::runtime::Handle::current().block_on(wait(&mp, &job.id));
        assert_eq!(done.phase, Phase::Done, "{done:?}");
        let list = mp.installed().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].module.as_deref(), Some(FILES));
        assert_eq!(
            list[0].enabled.get(WS),
            Some(&true),
            "installing from a workspace enables it there"
        );

        // ---- disable: the Installed row offers it; click ------------------------
        host.invoke_command(&id, "refresh", Value::Null)
            .expect("refresh");
        let on = rows_until(&host, &id, "the installed row offering disable", |rows| {
            row_for(rows, "disable", FILES)
        });
        assert!(on.detail.contains("enabled"), "{on:?}");
        click(&host, &id, &on);
        assert_eq!(mp.installed().unwrap()[0].enabled.get(WS), Some(&false));

        // ---- enable: the same row now offers the opposite ---------------------------
        let off = rows_until(&host, &id, "the installed row offering enable", |rows| {
            row_for(rows, "enable", FILES)
        });
        assert!(off.detail.contains("disabled"), "{off:?}");
        click(&host, &id, &off);
        assert_eq!(mp.installed().unwrap()[0].enabled.get(WS), Some(&true));
        rows_until(&host, &id, "the row flipping back to disable", |rows| {
            row_for(rows, "disable", FILES)
        });

        // ---- uninstall: the row's Uninstall, clicked twice; the row and record both go
        let remove = rows_until(&host, &id, "the Uninstall row", |rows| {
            row_for(rows, "uninstall", FILES)
        });
        assert!(remove.detail.starts_with("open to remove"), "{remove:?}");
        click(&host, &id, &remove);
        assert_eq!(mp.installed().unwrap().len(), 1, "one click only arms");
        let armed = rows_until(&host, &id, "the Uninstall row armed", |rows| {
            row_for(rows, "uninstall", FILES).filter(|r| r.detail.contains("again"))
        });
        click(&host, &id, &armed);
        assert!(mp.installed().unwrap().is_empty(), "the record is gone");
        rows_until(&host, &id, "the installed row gone", |rows| {
            let gone = row_for(rows, "enable", FILES).is_none()
                && row_for(rows, "disable", FILES).is_none();
            gone.then_some(())
        });

        host.shutdown_all();
    })
    .await
    .unwrap();
}
