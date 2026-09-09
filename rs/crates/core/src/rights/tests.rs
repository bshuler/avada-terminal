//! Rights service tests. Every test that touches disk gets its own temp dir under
//! `std::env::temp_dir()` — never the real app-support dir — and no test writes a key
//! anywhere: the one `SignedInstallRecord` test uses a fixed in-memory byte string.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

/// The SDK's reference manifest: `acme/avada-files` 1.2.0 with six capabilities and one
/// profile ("High security": fs.read=always, workspace.read=ask).
const FIXTURE: &str = include_str!("../../../module-sdk/tests/fixtures/avada.toml");

fn manifest() -> Manifest {
    Manifest::parse(FIXTURE).expect("fixture manifest parses")
}

fn id() -> ModuleId {
    ModuleId::new("acme/avada-files").unwrap()
}

/// A record accepting every declared capability except `skills.materialize`, so the
/// "declined at install" path is exercised by default.
fn record() -> InstallRecord {
    let m = manifest();
    let accepted = m
        .capabilities
        .iter()
        .copied()
        .filter(|c| *c != Capability::SkillsMaterialize)
        .collect();
    InstallRecord {
        module_id: m.module.id.clone(),
        repo: "https://github.com/acme/avada-files".into(),
        tag: m.tag(),
        commit: "0123456789abcdef0123456789abcdef01234567".into(),
        version: m.module.version.clone(),
        artifact_sha256: "00".repeat(32),
        skills_sha256: String::new(),
        source: DistributionKind::Source,
        accepted,
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
            std::env::temp_dir().join(format!("avada-rights-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        TempRoot(dir)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn service(root: &TempRoot) -> RightsService {
    let mut s = RightsService::with_root(&root.0);
    s.register(record());
    s
}

// ---- the resolve matrix -------------------------------------------------------------

/// The whole `accepted × user × workspace` table, through the service rather than the
/// SDK function, so the columns are wired to the right inputs.
#[test]
fn resolve_matrix_through_the_service() {
    let root = TempRoot::new();
    let mut s = service(&root);
    let m = id();
    const WS: &str = "proj-a";
    // (cap accepted?, user, workspace override, expected)
    let ws_cases: [(Option<RightValue>, Decision, Decision); 5] = [
        // workspace value → (expected when user=Workspace, expected when user=Ask)
        (None, Decision::Ask, Decision::Ask),
        (Some(RightValue::Never), Decision::Deny, Decision::Deny),
        (Some(RightValue::Always), Decision::Allow, Decision::Allow),
        (Some(RightValue::Workspace), Decision::Ask, Decision::Ask),
        (Some(RightValue::Ask), Decision::Ask, Decision::Ask),
    ];
    for &(cap, accepted) in &[
        (Capability::FsRead, true),
        (Capability::SkillsMaterialize, false),
    ] {
        for user in RightValue::ALL {
            for (ws, if_workspace, if_ask) in &ws_cases {
                s.set_user(&m, cap, *user).unwrap();
                s.set_workspace(&m, WS, cap, *ws).unwrap();
                let expected = match (accepted, user) {
                    (false, _) => Decision::Deny,
                    (true, RightValue::Never) => Decision::Deny,
                    (true, RightValue::Always) => Decision::Allow,
                    (true, RightValue::Workspace) => *if_workspace,
                    (true, RightValue::Ask) => *if_ask,
                };
                assert_eq!(
                    s.decide(&m, cap, Some(WS)),
                    expected,
                    "accepted={accepted} user={user} ws={ws:?}"
                );
                // The service must agree with the SDK rule verbatim.
                assert_eq!(expected, resolve(accepted, *user, *ws));
            }
        }
    }
}

#[test]
fn a_declined_capability_is_denied_whatever_the_columns_say() {
    let root = TempRoot::new();
    let mut s = service(&root);
    let m = id();
    s.set_user(&m, Capability::SkillsMaterialize, RightValue::Always)
        .unwrap();
    s.set_workspace(
        &m,
        "w",
        Capability::SkillsMaterialize,
        Some(RightValue::Always),
    )
    .unwrap();
    assert_eq!(
        s.decide(&m, Capability::SkillsMaterialize, Some("w")),
        Decision::Deny
    );
    // ...and one the manifest never declared, too.
    assert_eq!(s.decide(&m, Capability::Keychain, None), Decision::Deny);
}

#[test]
fn an_unregistered_module_is_denied_everything() {
    let root = TempRoot::new();
    let s = RightsService::with_root(&root.0);
    assert_eq!(s.decide(&id(), Capability::FsRead, None), Decision::Deny);
    assert!(s.rows(&id(), None).is_empty());
    assert!(s.profiles(&id()).is_empty());
}

#[test]
fn a_workspace_without_a_say_leaves_ask_as_ask() {
    let root = TempRoot::new();
    let s = service(&root);
    // Fresh module: user value defaults to Ask, no workspace → Ask (never a hidden grant).
    assert_eq!(s.decide(&id(), Capability::FsRead, None), Decision::Ask);
    assert_eq!(
        s.decide(&id(), Capability::FsRead, Some("never-touched")),
        Decision::Ask
    );
}

// ---- rows ---------------------------------------------------------------------------

#[test]
fn rows_follow_the_manifest_and_carry_every_column() {
    let root = TempRoot::new();
    let mut s = service(&root);
    let m = id();
    s.set_user(&m, Capability::UiRail, RightValue::Always)
        .unwrap();
    s.set_workspace(&m, "w", Capability::UiPane, Some(RightValue::Never))
        .unwrap();
    let rows = s.rows(&m, Some("w"));
    let caps: Vec<_> = rows.iter().map(|r| r.cap).collect();
    assert_eq!(
        caps,
        manifest().capabilities,
        "manifest order, one row each"
    );
    let by = |c: Capability| rows.iter().find(|r| r.cap == c).unwrap().clone();
    let rail = by(Capability::UiRail);
    assert!(rail.accepted);
    assert_eq!(rail.user, RightValue::Always);
    assert_eq!(rail.workspace, None);
    assert_eq!(rail.effective, Decision::Allow);
    assert_eq!(rail.description, Capability::UiRail.describe());
    let pane = by(Capability::UiPane);
    assert_eq!(pane.user, RightValue::Ask);
    assert_eq!(pane.workspace, Some(RightValue::Never));
    assert_eq!(pane.effective, Decision::Deny);
    let skills = by(Capability::SkillsMaterialize);
    assert!(!skills.accepted);
    assert_eq!(skills.effective, Decision::Deny);
}

// ---- persistence --------------------------------------------------------------------

#[test]
fn user_and_workspace_rights_round_trip_through_disk() {
    let root = TempRoot::new();
    let m = id();
    {
        let mut s = service(&root);
        s.set_profile(&m, Some("High security")).unwrap();
        s.set_user(&m, Capability::UiRail, RightValue::Never)
            .unwrap();
        s.set_workspace(&m, "proj a/b", Capability::UiPane, Some(RightValue::Always))
            .unwrap();
        s.set_workspace(
            &m,
            "proj a/b",
            Capability::UiCommands,
            Some(RightValue::Never),
        )
        .unwrap();
        s.set_workspace(&m, "proj a/b", Capability::UiCommands, None)
            .unwrap();
    }
    let user_path = root.0.join("acme__avada-files.json");
    assert!(user_path.is_file(), "user file at {user_path:?}");
    let ws_path = root
        .0
        .join("workspaces")
        .join(store::workspace_dir_name("proj a/b"))
        .join("rights.json");
    assert!(ws_path.is_file(), "workspace file at {ws_path:?}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        for p in [&user_path, &ws_path] {
            let mode = std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{p:?} must be owner-only");
        }
    }
    // The JSON is the SDK wire format: profile name + overrides keyed by cap name.
    let text = std::fs::read_to_string(&user_path).unwrap();
    assert!(text.contains("\"High security\""), "{text}");
    assert!(text.contains("\"ui.rail\": \"never\""), "{text}");

    // A fresh service on the same root sees the same truth.
    let mut s = RightsService::with_root(&root.0);
    s.register(record());
    s.load_workspace("proj a/b");
    assert_eq!(s.user_rights(&m).profile.as_deref(), Some("High security"));
    assert_eq!(s.user_value(&m, Capability::UiRail), RightValue::Never);
    assert_eq!(s.user_value(&m, Capability::FsRead), RightValue::Always);
    assert_eq!(
        s.workspace_value(&m, Capability::UiPane, Some("proj a/b")),
        Some(RightValue::Always)
    );
    assert_eq!(
        s.workspace_value(&m, Capability::UiCommands, Some("proj a/b")),
        None,
        "a cleared override is gone from disk"
    );
    assert_eq!(
        s.decide(&m, Capability::UiPane, Some("proj a/b")),
        Decision::Allow
    );
    assert_eq!(s.decide(&m, Capability::UiPane, None), Decision::Ask);
}

#[test]
fn a_missing_file_is_defaults_and_a_corrupt_one_is_an_error() {
    let root = TempRoot::new();
    let store = RightsStore::new(&root.0);
    assert_eq!(store.load_user(&id()).unwrap(), ModuleRights::default());
    assert!(store.load_workspace("nope").unwrap().is_empty());
    std::fs::create_dir_all(&root.0).unwrap();
    std::fs::write(store.user_path(&id()), b"{ not json").unwrap();
    assert!(store.load_user(&id()).is_err());
    // The service still comes up (defaults) and does not overwrite the bad file.
    let s = service(&root);
    assert_eq!(s.user_value(&id(), Capability::FsRead), RightValue::Ask);
    assert_eq!(
        std::fs::read(store.user_path(&id())).unwrap(),
        b"{ not json"
    );
}

#[test]
fn workspace_dir_names_are_safe_and_distinct() {
    let a = store::workspace_dir_name("proj a/b");
    let b = store::workspace_dir_name("proj a_b");
    assert_ne!(a, b, "sanitising must not merge keys: {a} vs {b}");
    assert!(a.starts_with("proj_a_b-"), "{a}");
    assert!(
        a.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')),
        "{a}"
    );
    assert!(!store::workspace_dir_name("../..").contains('/'));
    assert!(!store::workspace_dir_name("").starts_with('-'));
}

#[test]
fn default_root_is_under_the_data_dir_modules_rights() {
    let root = RightsStore::default_root();
    assert!(
        root.ends_with(PathBuf::from("modules").join("rights")),
        "{root:?}"
    );
}

// ---- profiles -----------------------------------------------------------------------

#[test]
fn a_profile_preselects_rows_and_an_override_beats_it() {
    let root = TempRoot::new();
    let mut s = service(&root);
    let m = id();
    assert_eq!(s.profiles(&m).len(), 1);
    assert_eq!(s.profiles(&m)[0].name, "High security");

    s.set_profile(&m, Some("High security")).unwrap();
    assert_eq!(s.user_value(&m, Capability::FsRead), RightValue::Always);
    assert_eq!(s.user_value(&m, Capability::WorkspaceRead), RightValue::Ask);
    // A cap the profile is silent on stays at the default — no hidden grant.
    assert_eq!(s.user_value(&m, Capability::UiRail), RightValue::Ask);

    // Per-row override wins over the profile...
    s.set_user(&m, Capability::FsRead, RightValue::Never)
        .unwrap();
    assert_eq!(s.user_value(&m, Capability::FsRead), RightValue::Never);
    assert_eq!(s.decide(&m, Capability::FsRead, None), Decision::Deny);
    // ...and survives switching the profile off and on.
    s.set_profile(&m, None).unwrap();
    assert_eq!(s.user_value(&m, Capability::FsRead), RightValue::Never);
    assert_eq!(s.user_value(&m, Capability::WorkspaceRead), RightValue::Ask);
    s.set_profile(&m, Some("High security")).unwrap();
    assert_eq!(s.user_value(&m, Capability::FsRead), RightValue::Never);
    // Clearing the override falls back to the profile.
    s.clear_user(&m, Capability::FsRead).unwrap();
    assert_eq!(s.user_value(&m, Capability::FsRead), RightValue::Always);
}

#[test]
fn an_unknown_profile_is_refused_and_changes_nothing() {
    let root = TempRoot::new();
    let mut s = service(&root);
    let err = s.set_profile(&id(), Some("Nope")).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(s.user_rights(&id()).profile, None);
    assert!(!RightsStore::new(&root.0).user_path(&id()).exists());
}

// ---- held updates -------------------------------------------------------------------

/// The fixture's six capabilities plus `net.fetch`.
const WIDER: [&str; 7] = [
    "fs.read",
    "workspace.read",
    "ui.rail",
    "ui.pane",
    "ui.commands",
    "skills.materialize",
    "net.fetch",
];

fn candidate(version: &str, caps: &[&str]) -> Manifest {
    // Drop `[skills]` so a candidate may also drop skills.materialize (the SDK rejects
    // skill paths without that capability; contributions likewise pin the three ui.* caps).
    let mut text = FIXTURE
        .replace("version = \"1.2.0\"", &format!("version = \"{version}\""))
        .replace("[skills]\npaths = [\"skills\"]", "");
    let list = caps
        .iter()
        .map(|c| format!("\"{c}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let start = text.find("capabilities = [").unwrap();
    let end = start + text[start..].find(']').unwrap() + 1;
    text.replace_range(start..end, &format!("capabilities = [{list}]"));
    Manifest::parse(&text).expect("candidate manifest parses")
}

#[test]
fn an_update_that_adds_a_capability_is_held_until_accepted() {
    let root = TempRoot::new();
    let mut s = service(&root);
    let m = id();
    // The fixture's contributions need ui.rail/ui.pane/ui.commands, so a candidate
    // keeps those; this one adds fs.write and drops skills.materialize.
    let cand = candidate(
        "1.3.0",
        &[
            "fs.read",
            "fs.write",
            "workspace.read",
            "ui.rail",
            "ui.pane",
            "ui.commands",
        ],
    );
    let held = s
        .check_update(&m, &cand)
        .expect("fs.write is new → held")
        .clone();
    assert_eq!(held.from.to_string(), "1.2.0");
    assert_eq!(held.to.to_string(), "1.3.0");
    assert_eq!(
        held.added,
        BTreeSet::from([Capability::FsWrite]),
        "added = the widened set"
    );
    assert_eq!(
        held.removed,
        BTreeSet::from([Capability::SkillsMaterialize])
    );
    assert_eq!(s.held.len(), 1);
    // Until accepted the running module's rights are untouched.
    assert_eq!(s.decide(&m, Capability::FsWrite, None), Decision::Deny);

    let next = s.accept_held(&m).expect("was held");
    let expected: BTreeSet<_> = [
        Capability::FsRead,
        Capability::FsWrite,
        Capability::WorkspaceRead,
        Capability::UiRail,
        Capability::UiPane,
        Capability::UiCommands,
    ]
    .into_iter()
    .collect();
    assert_eq!(
        next, expected,
        "accepted − removed + added; the declined skills cap stays out"
    );
    assert!(s.held.is_empty());
    assert_eq!(s.accept_held(&m), None, "accept is one-shot");
}

#[test]
fn an_update_that_widens_nothing_is_not_held_and_reject_drops_a_held_one() {
    let root = TempRoot::new();
    let mut s = service(&root);
    let m = id();
    let narrower = candidate(
        "1.2.1",
        &[
            "fs.read",
            "workspace.read",
            "ui.rail",
            "ui.pane",
            "ui.commands",
        ],
    );
    assert!(s.check_update(&m, &narrower).is_none());
    assert!(s.held.is_empty());

    let wider = candidate("2.0.0", &WIDER);
    assert!(s.check_update(&m, &wider).is_some());
    let dropped = s.reject_held(&m).expect("was held");
    assert_eq!(dropped.to.to_string(), "2.0.0");
    assert!(s.held.is_empty());
    assert_eq!(s.reject_held(&m), None);
    assert_eq!(
        s.record(&m).unwrap().accepted,
        record().accepted,
        "reject leaves the record alone"
    );
}

// ---- ask queue ----------------------------------------------------------------------

#[test]
fn the_ask_queue_is_fifo_coalesces_duplicates_and_answers_by_id() {
    let mut q = AskQueue::default();
    assert!(q.is_empty());
    let a = q.push(id(), Capability::FsRead, Some("w".into()));
    let b = q.push(id(), Capability::UiPane, None);
    let again = q.push(id(), Capability::FsRead, Some("w".into()));
    assert_eq!(a, again, "same question → same toast");
    assert_ne!(a, b);
    assert_eq!(q.len(), 2);
    assert_eq!(q.front().unwrap().id, a);
    assert_eq!(q.get(b).unwrap().cap, Capability::UiPane);
    let taken = q.take(b).unwrap();
    assert_eq!(taken.workspace, None);
    assert_eq!(q.take(b), None, "answered once");
    assert_eq!(q.front().unwrap().id, a);
    q.drop_module(&id());
    assert!(q.is_empty());
}

#[test]
fn ask_answers_write_exactly_what_they_say() {
    let root = TempRoot::new();
    let mut s = service(&root);
    let m = id();
    const WS: &str = "w";

    // allow once: allowed now, nothing persisted, asked again next time.
    let ask = s.ask(&m, Capability::FsRead, Some(WS));
    let (pending, d) = s.answer(ask, AskAnswer::AllowOnce).unwrap().unwrap();
    assert_eq!((pending.cap, d), (Capability::FsRead, Decision::Allow));
    assert_eq!(s.decide(&m, Capability::FsRead, Some(WS)), Decision::Ask);
    assert!(
        s.answer(ask, AskAnswer::Always).unwrap().is_none(),
        "stale id"
    );

    // always: user column.
    let ask = s.ask(&m, Capability::FsRead, Some(WS));
    let (_, d) = s.answer(ask, AskAnswer::Always).unwrap().unwrap();
    assert_eq!(d, Decision::Allow);
    assert_eq!(s.user_value(&m, Capability::FsRead), RightValue::Always);
    assert_eq!(s.decide(&m, Capability::FsRead, None), Decision::Allow);

    // never: user column, denied.
    let ask = s.ask(&m, Capability::UiPane, None);
    let (_, d) = s.answer(ask, AskAnswer::Never).unwrap().unwrap();
    assert_eq!(d, Decision::Deny);
    assert_eq!(s.user_value(&m, Capability::UiPane), RightValue::Never);

    // workspace: only this workspace; elsewhere still asks.
    let ask = s.ask(&m, Capability::UiRail, Some(WS));
    let (_, d) = s.answer(ask, AskAnswer::Workspace).unwrap().unwrap();
    assert_eq!(d, Decision::Allow);
    assert_eq!(s.user_value(&m, Capability::UiRail), RightValue::Workspace);
    assert_eq!(
        s.workspace_value(&m, Capability::UiRail, Some(WS)),
        Some(RightValue::Always)
    );
    assert_eq!(s.decide(&m, Capability::UiRail, Some(WS)), Decision::Allow);
    assert_eq!(
        s.decide(&m, Capability::UiRail, Some("other")),
        Decision::Ask
    );
    assert_eq!(s.decide(&m, Capability::UiRail, None), Decision::Ask);

    // workspace answer with no workspace in the ask: allow once, nothing written.
    let ask = s.ask(&m, Capability::UiCommands, None);
    let (_, d) = s.answer(ask, AskAnswer::Workspace).unwrap().unwrap();
    assert_eq!(d, Decision::Allow);
    assert_eq!(s.user_value(&m, Capability::UiCommands), RightValue::Ask);

    // Everything the answers wrote is on disk.
    let mut fresh = RightsService::with_root(&root.0);
    fresh.register(record());
    fresh.load_workspace(WS);
    assert_eq!(fresh.decide(&m, Capability::FsRead, None), Decision::Allow);
    assert_eq!(fresh.decide(&m, Capability::UiPane, None), Decision::Deny);
    assert_eq!(
        fresh.decide(&m, Capability::UiRail, Some(WS)),
        Decision::Allow
    );
}

#[test]
fn uninstalling_drops_pending_asks_and_held_updates_but_keeps_the_files() {
    let root = TempRoot::new();
    let mut s = service(&root);
    let m = id();
    s.set_user(&m, Capability::FsRead, RightValue::Always)
        .unwrap();
    s.ask(&m, Capability::UiPane, None);
    let wider = candidate("2.0.0", &WIDER);
    assert!(s.check_update(&m, &wider).is_some());
    s.unregister(&m);
    assert!(s.asks.is_empty());
    assert!(s.held.is_empty());
    assert!(s.modules().is_empty());
    assert_eq!(s.decide(&m, Capability::FsRead, None), Decision::Deny);
    assert!(RightsStore::new(&root.0).user_path(&m).is_file());
    // Reinstall: the choice comes back.
    s.register(record());
    assert_eq!(s.decide(&m, Capability::FsRead, None), Decision::Allow);
}

/// The install store verifies before `register`; this just shows the record shape the
/// service accepts survives signing with a test-only in-memory key (never written).
#[test]
fn a_signed_record_verifies_and_registers() {
    let key = b"test-only-key-not-a-secret-never-on-disk";
    let signed = SignedInstallRecord::sign(record(), key, "test").unwrap();
    let verified = signed.verify(key).unwrap().clone();
    let root = TempRoot::new();
    let mut s = RightsService::with_root(&root.0);
    s.register(verified);
    assert_eq!(s.modules().len(), 1);
    assert!(signed.verify(b"wrong").is_err());
}
