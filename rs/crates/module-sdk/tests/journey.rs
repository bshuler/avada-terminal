//! End-to-end journey through the public API only: parse the reference manifest,
//! record an install, sign it, hand-shake a module over an in-memory pipe, make a
//! guarded call, then hold an update that widens its capabilities.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Cursor;

use avada_module_sdk::caps::{Capability, Decision, RightValue};
use avada_module_sdk::client::{ClientError, Connection};
use avada_module_sdk::contract::{
    methods, ErrorCode, HelloKind, HostHello, Message, CONTRACT_VERSION,
};
use avada_module_sdk::descriptor::{
    validate_table, Param, ParamLocation, RouteDescriptor, Scope, Verb,
};
use avada_module_sdk::install::{LockedModule, Lockfile};
use avada_module_sdk::manifest::{ContributionKind, DistributionKind, Manifest, UiTier};
use avada_module_sdk::rights::{
    HeldUpdate, InstallKind, InstallRecord, ModuleRights, SignedInstallRecord,
};
use avada_module_sdk::{ModuleId, MANIFEST_FILE, PRODUCT_NAME};
use semver::Version;
use serde_json::json;

const FIXTURE: &str = include_str!("fixtures/avada.toml");
const KEY: &[u8] = b"integration-test-key-not-a-secret";

fn manifest() -> Manifest {
    Manifest::parse(FIXTURE).expect("reference manifest parses")
}

fn record(m: &Manifest, accepted: &[Capability]) -> InstallRecord {
    InstallRecord {
        module_id: m.id().clone(),
        repo: format!("https://github.com/{}.git", m.id()),
        tag: m.tag(),
        commit: "0123456789abcdef0123456789abcdef01234567".into(),
        version: m.module.version.clone(),
        artifact_sha256: "ab".repeat(32),
        source: DistributionKind::Source,
        accepted: accepted.iter().copied().collect(),
        manifest: m.clone(),
        installed_at: 1_757_000_000,
        kind: InstallKind::Manual,
    }
}

#[test]
fn reference_manifest_is_the_documented_shape() {
    assert_eq!(MANIFEST_FILE, "avada.toml");
    let m = manifest();
    assert_eq!(m.id().as_str(), "acme/avada-files");
    assert_eq!(m.id().dir_name(), "acme__avada-files");
    assert_eq!(m.tag(), "v1.2.0");
    assert_eq!(m.contributions.len(), 3);
    let kinds: Vec<_> = m.contributions.iter().map(|c| c.kind).collect();
    assert_eq!(
        kinds,
        [
            ContributionKind::Rail,
            ContributionKind::Pane,
            ContributionKind::Command
        ]
    );
    assert_eq!(m.contributions[1].tier, UiTier::Pixels);
    assert!(m.capabilities.contains(&Capability::UiPane));
    assert_eq!(
        m.requires[0].provider.as_ref().unwrap().as_str(),
        "acme/avada-git"
    );
    // Round trip through the serializer keeps every field.
    let again = Manifest::parse(&m.to_toml()).unwrap();
    assert_eq!(again, m);
}

#[test]
fn manifest_refuses_a_typo_loudly() {
    let bad = FIXTURE.replace("publisher = ", "publsher = ");
    let err = Manifest::parse(&bad).unwrap_err();
    assert!(format!("{err}").contains("publsher"), "{err}");
}

#[test]
fn install_sign_verify_and_hold_update() {
    let m = manifest();
    // The user declined the pane and skills capabilities at install time.
    let accepted = [
        Capability::FsRead,
        Capability::WorkspaceRead,
        Capability::UiRail,
        Capability::UiCommands,
    ];
    let rec = record(&m, &accepted);
    assert_eq!(
        rec.declined(),
        BTreeSet::from([Capability::UiPane, Capability::SkillsMaterialize])
    );

    let signed = SignedInstallRecord::sign(rec.clone(), KEY, "k1").unwrap();
    let json = serde_json::to_string(&signed).unwrap();
    let back: SignedInstallRecord = serde_json::from_str(&json).unwrap();
    let verified = back.verify(KEY).unwrap();
    assert!(verified.matches_hello(&m));
    assert!(back.verify(b"other-key").is_err());

    // A lockfile entry for the same install.
    let mut lock = Lockfile::default();
    lock.upsert(LockedModule {
        id: rec.module_id.clone(),
        version: rec.version.clone(),
        tag: rec.tag.clone(),
        commit: rec.commit.clone(),
        sha256: rec.artifact_sha256.clone(),
        source: rec.source,
        installed_at: rec.installed_at,
        kind: rec.kind,
    });
    let lock2 = Lockfile::parse(&lock.to_json()).unwrap();
    assert_eq!(lock2, lock);
    assert!(lock2.get(&rec.module_id).is_some());

    // An update that asks for one more capability is held, not applied.
    let mut next = m.clone();
    next.module.version = Version::new(1, 3, 0);
    next.capabilities.push(Capability::NetFetch);
    let held: HeldUpdate = HeldUpdate::check(&rec, &next).expect("held");
    assert_eq!(held.from, Version::new(1, 2, 0));
    assert_eq!(held.to, Version::new(1, 3, 0));
    assert_eq!(held.added, BTreeSet::from([Capability::NetFetch]));
    assert!(held.removed.is_empty());

    // An update that asks for nothing new flows through.
    let mut quiet = m.clone();
    quiet.module.version = Version::new(1, 2, 1);
    assert!(HeldUpdate::check(&rec, &quiet).is_none());
}

#[test]
fn rights_resolution_through_profile_and_override() {
    let m = manifest();
    let rights = ModuleRights {
        profile: Some("High security".into()),
        overrides: BTreeMap::from([(Capability::WorkspaceRead, RightValue::Never)]),
    };
    assert_eq!(
        rights.value(Capability::FsRead, &m.profiles),
        RightValue::Always
    );
    assert_eq!(
        rights.value(Capability::WorkspaceRead, &m.profiles),
        RightValue::Never
    );
    assert_eq!(
        rights.value(Capability::UiRail, &m.profiles),
        RightValue::Ask
    );
    assert_eq!(
        avada_module_sdk::caps::resolve(true, RightValue::Always, Some(RightValue::Never)),
        Decision::Allow
    );
    assert_eq!(
        avada_module_sdk::caps::resolve(true, RightValue::Never, Some(RightValue::Always)),
        Decision::Deny
    );
}

fn host_hello(granted: Vec<Capability>) -> String {
    serde_json::to_string(&HostHello {
        kind: HelloKind::Host,
        contract_version: CONTRACT_VERSION,
        host_version: "0.1.0".into(),
        product: PRODUCT_NAME.into(),
        granted,
        methods: methods::HOST_REQUIRED_V1
            .iter()
            .map(|s| s.to_string())
            .collect(),
        data_dir: "/tmp/avada/modules/acme__avada-files".into(),
        workspace: None,
    })
    .unwrap()
}

#[test]
fn module_handshakes_calls_and_receives_over_a_pipe() {
    let m = manifest();
    // What the host will say, in order: hello, then the reply to the first call,
    // then an activation request interleaved before the reply to the second call.
    let reply1 = json!({"jsonrpc":"2.0","id":1,"result":{"entries":1}});
    let activate = json!({"jsonrpc":"2.0","id":"h-1","method":methods::MODULE_ACTIVATE,"params":{"workspace":{"id":"w1","name":"repo","root":"/r"}}});
    let reply2 = json!({"jsonrpc":"2.0","id":2,"error":{"code":ErrorCode::UserDenied.code(),"message":"no"}});
    let input = format!(
        "{}\n{reply1}\n{activate}\n{reply2}\n",
        host_hello(vec![Capability::UiRail])
    );
    let mut c = Connection::new(Cursor::new(input.into_bytes()), Vec::new());
    let hello = c
        .handshake(m, vec![methods::MODULE_ACTIVATE.into()])
        .unwrap();
    assert_eq!(hello.product, PRODUCT_NAME);
    assert_eq!(c.contract_version(), CONTRACT_VERSION);
    assert!(c.has(Capability::UiRail));
    assert!(!c.has(Capability::FsRead));

    let r = c
        .call(methods::HOST_RAIL_REGISTER, json!({"entries": []}))
        .unwrap();
    assert_eq!(r["entries"], 1);

    // A method the module has no grant for never reaches the wire.
    match c.call(methods::HOST_FS_READ, json!({"path": "/etc/hosts"})) {
        Err(ClientError::Rpc(e)) => assert_eq!(e.kind(), ErrorCode::CapabilityDenied),
        other => panic!("{other:?}"),
    }

    // The second call's reply arrives after an interleaved request, which is queued.
    match c.call(methods::HOST_RAIL_REGISTER, json!({})) {
        Err(ClientError::Rpc(e)) => assert_eq!(e.kind(), ErrorCode::UserDenied),
        other => panic!("{other:?}"),
    }
    match c.recv().unwrap() {
        Some(Message::Request(req)) => assert_eq!(req.method, methods::MODULE_ACTIVATE),
        other => panic!("{other:?}"),
    }
    assert!(c.recv().unwrap().is_none());
}

#[test]
fn descriptor_table_for_a_module_mounts_under_its_id() {
    let id: ModuleId = "acme/avada-files".parse().unwrap();
    let route = RouteDescriptor {
        method: "files.tree".into(),
        path: "/tree/{root}".into(),
        verb: Verb::Get,
        capability: Some(Capability::FsRead),
        summary: "List a directory".into(),
        params: vec![Param {
            name: "root".into(),
            location: ParamLocation::Path,
            kind: "string".into(),
            required: true,
            summary: "Directory".into(),
        }],
        scope: Scope::Token,
        module: Some(id),
        response: None,
    };
    route.validate().unwrap();
    assert_eq!(route.mounted_path(), "/m/acme/avada-files/tree/{root}");
    let dup = route.clone();
    assert!(validate_table(&[route.clone(), dup]).is_err());
    validate_table(&[route]).unwrap();
}
