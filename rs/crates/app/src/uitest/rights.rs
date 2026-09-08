//! Track H2: the Modules rights page (`ui/prefs_rights.slint`) and its ask toast.
//! Owned by the H2 track; `super::*` brings the harness helpers (`ui`, `window`,
//! `click`, `by_label`, ...) into scope.
//!
//! Every test wires the real adapter callbacks through [`crate::prefs::rights::wire`] into
//! a recorder, so a click that reaches Rust shows up as a decoded [`RightsCommand`] with
//! the module, capability and value the page sent. Deleting a callback arm in the `.slint`
//! (or in `wire`) leaves the recorder empty and the matching test fails — the mutation
//! check the track asked for. Nothing here touches the real rights store: every service
//! roots in a throw-away directory under the OS temp dir.
#![allow(unused_imports)]

use super::*;
use crate::prefs::rights::{fill, wire, RightsCommand};
use avada_core::rights::{
    Capability, DistributionKind, InstallKind, InstallRecord, Manifest, ModuleId, RightValue,
    RightsService,
};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

const FIXTURE: &str = include_str!("../../../module-sdk/tests/fixtures/avada.toml");

/// A rights root that lives only for one test — never the real app-support directory.
struct TempRoot(PathBuf);

impl TempRoot {
    fn new() -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("avada-rights-uitest-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        TempRoot(dir)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn module() -> ModuleId {
    ModuleId::new("acme/avada-files").unwrap()
}

/// The fixture module, installed with everything but `skills.materialize` accepted.
fn record() -> InstallRecord {
    let m = Manifest::parse(FIXTURE).expect("fixture manifest parses");
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
        source: DistributionKind::Source,
        accepted,
        manifest: m,
        installed_at: 1_700_000_000,
        kind: InstallKind::Manual,
    }
}

/// The fixture at another version with another capability list, for a held update.
fn candidate(version: &str, caps: &[&str]) -> Manifest {
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

type Recorder = Rc<RefCell<Vec<RightsCommand>>>;

/// A window with the preferences overlay open on the rights page, the fixture module
/// registered, and every adapter callback recording into the returned list.
fn open(w: &crate::AppWindow, root: &TempRoot) -> (RightsService, Recorder) {
    let mut service = RightsService::with_root(&root.0);
    service.register(record());
    let seen: Recorder = Rc::new(RefCell::new(Vec::new()));
    let sink = seen.clone();
    wire(w, move |cmd| sink.borrow_mut().push(cmd));
    w.set_overlay_kind(2);
    w.global::<crate::RightsAdapter>().set_open(true);
    (service, seen)
}

fn show(w: &crate::AppWindow, service: &RightsService, workspace: Option<&str>) {
    fill(w, service, Some(&module()), workspace);
}

fn click_label(w: &crate::AppWindow, label: &str) {
    let els = by_label(w, label);
    assert_eq!(
        els.len(),
        1,
        "expected exactly one {label:?}, found {}",
        els.len()
    );
    click(w, &els[0]);
}

fn taken(seen: &Recorder) -> Vec<RightsCommand> {
    std::mem::take(&mut *seen.borrow_mut())
}

#[test]
fn the_page_lists_the_module_and_clicking_it_selects_it() {
    ui(|| {
        let root = TempRoot::new();
        let w = window();
        let (service, seen) = open(&w, &root);
        show(&w, &service, None);

        // The list row is named after the manifest's display name, not its id.
        click_label(&w, "Module Files");
        assert_eq!(taken(&seen), vec![RightsCommand::SelectModule(0)]);
    });
}

#[test]
fn a_user_picker_click_names_the_module_capability_and_value() {
    ui(|| {
        let root = TempRoot::new();
        let w = window();
        let (mut service, seen) = open(&w, &root);
        show(&w, &service, None);

        click_label(&w, "fs.read user always");
        let cmds = taken(&seen);
        assert_eq!(
            cmds,
            vec![RightsCommand::SetUser {
                module: module(),
                cap: Capability::FsRead,
                value: RightValue::Always,
            }]
        );

        // Applying what the page sent writes the value; refilling shows it as the chosen
        // pill and resolves the row to `allow`.
        crate::prefs::rights::apply(&mut service, &cmds[0], None).unwrap();
        assert_eq!(
            service.user_value(&module(), Capability::FsRead),
            RightValue::Always
        );
        show(&w, &service, None);
        assert_eq!(by_label(&w, "fs.read effective allow").len(), 1);

        // A different row and a different value: the label carries the capability.
        click_label(&w, "workspace.read user never");
        assert_eq!(
            taken(&seen),
            vec![RightsCommand::SetUser {
                module: module(),
                cap: Capability::WorkspaceRead,
                value: RightValue::Never,
            }]
        );
    });
}

#[test]
fn the_user_picker_offers_all_four_values() {
    ui(|| {
        let root = TempRoot::new();
        let w = window();
        let (service, seen) = open(&w, &root);
        show(&w, &service, None);

        for (label, value) in [
            ("fs.read user never", RightValue::Never),
            ("fs.read user always", RightValue::Always),
            ("fs.read user workspace", RightValue::Workspace),
            ("fs.read user ask", RightValue::Ask),
        ] {
            click_label(&w, label);
            assert_eq!(
                taken(&seen),
                vec![RightsCommand::SetUser {
                    module: module(),
                    cap: Capability::FsRead,
                    value,
                }],
                "{label}"
            );
        }
    });
}

#[test]
fn a_workspace_picker_click_sets_or_clears_the_override() {
    ui(|| {
        let root = TempRoot::new();
        let w = window();
        let (service, seen) = open(&w, &root);
        show(&w, &service, Some("/tmp/ws-a"));

        click_label(&w, "fs.read workspace never");
        assert_eq!(
            taken(&seen),
            vec![RightsCommand::SetWorkspace {
                module: module(),
                cap: Capability::FsRead,
                value: Some(RightValue::Never),
            }]
        );

        click_label(&w, "fs.read workspace ask");
        assert_eq!(
            taken(&seen),
            vec![RightsCommand::SetWorkspace {
                module: module(),
                cap: Capability::FsRead,
                value: Some(RightValue::Ask),
            }]
        );

        click_label(&w, "fs.read workspace unset");
        assert_eq!(
            taken(&seen),
            vec![RightsCommand::SetWorkspace {
                module: module(),
                cap: Capability::FsRead,
                value: None,
            }]
        );
    });
}

#[test]
fn the_workspace_picker_is_inert_without_an_open_workspace() {
    ui(|| {
        let root = TempRoot::new();
        let w = window();
        let (service, seen) = open(&w, &root);
        show(&w, &service, None);

        // The pills are drawn (so the column reads the same) but disabled: a click
        // must not reach Rust, since there is no workspace to write to.
        click_label(&w, "fs.read workspace always");
        assert!(taken(&seen).is_empty());
    });
}

#[test]
fn a_profile_click_names_the_profile_and_none_clears_it() {
    ui(|| {
        let root = TempRoot::new();
        let w = window();
        let (service, seen) = open(&w, &root);
        show(&w, &service, None);

        click_label(&w, "Profile High security");
        assert_eq!(
            taken(&seen),
            vec![RightsCommand::SelectProfile {
                module: module(),
                profile: Some("High security".into()),
            }]
        );

        click_label(&w, "Profile none");
        assert_eq!(
            taken(&seen),
            vec![RightsCommand::SelectProfile {
                module: module(),
                profile: None,
            }]
        );
    });
}

#[test]
fn a_held_update_shows_its_diff_and_accept_and_reject_reach_rust() {
    ui(|| {
        let root = TempRoot::new();
        let w = window();
        let (mut service, seen) = open(&w, &root);

        // fs.write is new and skills.materialize is gone: the update is held.
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
        service
            .check_update(&module(), &cand)
            .expect("the widened manifest is held");
        show(&w, &service, None);

        assert_eq!(by_label(&w, "Held update").len(), 1);
        assert_eq!(by_label(&w, "Held update adds fs.write").len(), 1);
        assert_eq!(
            by_label(&w, "Held update removes skills.materialize").len(),
            1
        );

        click_label(&w, "Accept update");
        assert_eq!(taken(&seen), vec![RightsCommand::HeldAccept(module())]);

        click_label(&w, "Reject update");
        assert_eq!(taken(&seen), vec![RightsCommand::HeldReject(module())]);
    });
}

#[test]
fn without_a_held_update_the_block_is_absent() {
    ui(|| {
        let root = TempRoot::new();
        let w = window();
        let (service, _seen) = open(&w, &root);
        show(&w, &service, None);
        assert!(by_label(&w, "Held update").is_empty());
        assert!(by_label(&w, "Accept update").is_empty());
    });
}

#[test]
fn the_ask_toast_buttons_carry_the_ask_id() {
    ui(|| {
        let root = TempRoot::new();
        let w = window();
        let (mut service, seen) = open(&w, &root);

        let id = service.ask(&module(), Capability::WorkspaceRead, Some("/tmp/ws-a"));
        show(&w, &service, None);

        assert_eq!(by_label(&w, "Capability request").len(), 1);

        click_label(&w, "Allow once");
        assert_eq!(taken(&seen), vec![RightsCommand::AskAllowOnce(id)]);

        click_label(&w, "Allow always");
        assert_eq!(taken(&seen), vec![RightsCommand::AskAllowAlways(id)]);

        click_label(&w, "Deny");
        assert_eq!(taken(&seen), vec![RightsCommand::AskDeny(id)]);
    });
}

#[test]
fn without_a_pending_ask_there_is_no_toast() {
    ui(|| {
        let root = TempRoot::new();
        let w = window();
        let (service, _seen) = open(&w, &root);
        show(&w, &service, None);
        assert!(by_label(&w, "Capability request").is_empty());
        assert!(by_label(&w, "Allow once").is_empty());
    });
}

// ---------------------------------------------------------------------------------
// the two mounts outside the page itself (track H4 landed these for H2, which could
// not touch `ui/app.slint` or the window glue)
// ---------------------------------------------------------------------------------

/// The rights page is reachable the ordinary way — the Preferences rail — and not only
/// through the `RightsAdapter.open` deep link the other tests use. A nav rail that lists
/// every other section but has no way to Modules is exactly the bug this catches.
#[test]
fn the_preferences_rail_has_a_modules_entry_that_opens_the_page() {
    ui(|| {
        let root = TempRoot::new();
        let w = window();
        let mut service = RightsService::with_root(&root.0);
        service.register(record());
        w.set_overlay_kind(2); // Preferences, on its default section
        show(&w, &service, None);

        // The page is not up yet: only the rail entry answers to "Modules".
        assert!(by_role(&w, "Modules", AccessibleRole::Groupbox).is_empty());
        let tab = only(&w, "Modules", AccessibleRole::Tab);
        click(&w, &tab);

        // Now both exist — the tab (checked) and the page it selected.
        assert_eq!(by_role(&w, "Modules", AccessibleRole::Groupbox).len(), 1);
        assert_eq!(
            only(&w, "Modules", AccessibleRole::Tab).accessible_checked(),
            Some(true)
        );
        assert!(!by_label(&w, "Module Files").is_empty());
    });
}

/// The ask toast is mounted at the WINDOW root, not inside Preferences: a module asks for
/// a capability when it needs one, and the answer has to be one click away from whatever
/// the user was doing. With no overlay up the card is on screen and its three buttons
/// reach Rust with the ask's id.
#[test]
fn the_ask_toast_answers_from_the_window_with_no_overlay_open() {
    ui(|| {
        let root = TempRoot::new();
        let w = window();
        let mut service = RightsService::with_root(&root.0);
        service.register(record());
        let seen: Recorder = Rc::new(RefCell::new(Vec::new()));
        let sink = seen.clone();
        wire(&w, move |cmd| sink.borrow_mut().push(cmd));

        // No overlay, no Preferences, nothing open — just the window.
        w.set_overlay_kind(0);
        show(&w, &service, None);
        assert!(by_label(&w, "Capability request").is_empty());

        let id = service.ask(&module(), Capability::WorkspaceRead, None);
        show(&w, &service, None);

        // Exactly one card: the page's copy is not mounted with the overlay down.
        assert_eq!(by_label(&w, "Capability request").len(), 1);
        click_label(&w, "Allow always");
        assert_eq!(taken(&seen), vec![RightsCommand::AskAllowAlways(id)]);
    });
}

/// …and while an overlay is up the window-level card stands down. It would sit under the
/// scrim, unclickable, and a screen reader would find two "Capability request" groupboxes
/// where the user can only answer one.
#[test]
fn the_window_toast_stands_down_while_an_overlay_covers_it() {
    ui(|| {
        let root = TempRoot::new();
        let w = window();
        let (mut service, _seen) = open(&w, &root); // sets overlay-kind 2 + the deep link
        service.ask(&module(), Capability::WorkspaceRead, None);
        show(&w, &service, None);
        assert_eq!(by_label(&w, "Capability request").len(), 1);
    });
}
