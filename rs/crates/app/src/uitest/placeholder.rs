//! Track H1: the module placeholder pane: reopen / open another / install buttons.
//! Owned by the H1 track; `super::*` brings the harness helpers (`ui`, `window`,
//! `click`, `by_label`, `by_id`, ...) into scope.
//!
//! What is proven here, against the real component tree: a `module:` pane (kind 10) whose
//! slot is in the adapter draws the placeholder with the text its reason calls for; each
//! of the three buttons reaches its own adapter callback carrying the pane uid and module
//! id (a mutation the test observes, not a "did not panic"); a pane with no slot draws
//! none of them; two placeholders side by side do not answer for each other.
#![allow(unused_imports)]

use super::*;
use crate::module_ui::placeholder::{self as ph, PlaceholderView, Reason};
use avada_core::module::ModuleStatus;
use avada_core::tools::kind::{ModulePaneRef, Version};
use slint::Model;
use std::cell::RefCell;
use std::rc::Rc;

const MODULE: &str = "acme/avada-files";

fn pane_ref(pin: Option<&str>) -> ModulePaneRef {
    ModulePaneRef::new(
        MODULE,
        "browser",
        pin.map(|p| Version::parse(p).expect("version")),
    )
    .expect("valid ref")
}

/// One module pane, big enough for its body, with the uid the adapter keys on.
fn module_pane(uid: &str, x: f32) -> crate::PaneItem {
    crate::PaneItem {
        title: uid.into(),
        uid: uid.into(),
        x,
        y: 40.0,
        w: 500.0,
        h: 360.0,
        visible: true,
        focused: x == 8.0,
        kind: 10,
        ..Default::default()
    }
}

/// Publish one module pane and the slot its status maps to.
fn install(w: &crate::AppWindow, status: &ModuleStatus, pin: Option<&str>) -> PlaceholderView {
    w.set_panes(Rc::new(slint::VecModel::from(vec![module_pane("m-1", 8.0)])).into());
    let v = ph::view(status, &pane_ref(pin), Some("Files"));
    ph::fill(w, ph::slot("m-1", &v).into_iter().collect());
    v
}

/// Accessible labels of every button the pane currently shows.
fn buttons(w: &crate::AppWindow) -> Vec<String> {
    let mut out = Vec::new();
    for label in [
        format!("Reopen {MODULE}"),
        format!("Install {MODULE} from marketplace"),
        format!("Open another pane like {MODULE}"),
    ] {
        if !by_label(w, &label).is_empty() {
            out.push(label);
        }
    }
    out
}

/// How many elements announce `s` — a `Text` is its own accessible label, so this counts
/// the captions on screen that read exactly `s`.
fn texts(w: &crate::AppWindow, s: &str) -> usize {
    by_label(w, s).len()
}

#[test]
fn a_crashed_module_offers_reopen_and_the_click_reaches_rust_with_its_pane_and_module() {
    ui(|| {
        let w = window();
        let v = install(&w, &ModuleStatus::Crashed { restarts: 2 }, Some("1.2.0"));
        assert_eq!(v.reason, Some(Reason::Crashed));
        assert!(v
            .message
            .contains("Reopen it, or open another pane like it."));

        assert_eq!(
            buttons(&w),
            vec![
                format!("Reopen {MODULE}"),
                format!("Open another pane like {MODULE}")
            ],
            "crashed offers reopen and open-another, never install"
        );
        assert_eq!(texts(&w, "Files"), 1, "the heading is the display name");
        assert_eq!(
            texts(&w, &format!("{MODULE} @ 1.2.0")),
            1,
            "the pin is shown"
        );
        assert_eq!(texts(&w, &v.message), 1, "the crash sentence renders");
        assert_eq!(texts(&w, "Restarted 2 times in the last minute."), 1);

        let got: Rc<RefCell<Option<(String, String)>>> = Rc::new(RefCell::new(None));
        {
            let got = got.clone();
            w.global::<crate::ModulePlaceholderAdapter>()
                .on_reopen(move |uid, id| *got.borrow_mut() = Some((uid.into(), id.into())));
        }
        let b = by_label(&w, &format!("Reopen {MODULE}"));
        assert_eq!(b.len(), 1);
        click(&w, &b[0]);
        assert_eq!(
            got.borrow().clone(),
            Some(("m-1".to_string(), MODULE.to_string())),
            "reopen must carry the pane uid and module id"
        );
    });
}

#[test]
fn a_disabled_module_says_why_and_open_another_reaches_its_callback() {
    ui(|| {
        let w = window();
        let v = install(
            &w,
            &ModuleStatus::Disabled {
                reason: "crashed 3 times in a minute".into(),
            },
            None,
        );
        assert_eq!(v.reason, Some(Reason::Disabled));
        assert_eq!(
            buttons(&w),
            vec![
                format!("Reopen {MODULE}"),
                format!("Open another pane like {MODULE}")
            ]
        );
        assert_eq!(texts(&w, &v.message), 1, "the disabled sentence renders");
        assert_eq!(
            texts(&w, "crashed 3 times in a minute"),
            1,
            "the host's reason shows"
        );
        assert_eq!(texts(&w, MODULE), 1, "no pin: the id alone");

        let got: Rc<RefCell<Vec<(String, String)>>> = Rc::new(RefCell::new(Vec::new()));
        {
            let got = got.clone();
            w.global::<crate::ModulePlaceholderAdapter>()
                .on_open_another(move |uid, id| got.borrow_mut().push((uid.into(), id.into())));
        }
        let b = by_label(&w, &format!("Open another pane like {MODULE}"));
        assert_eq!(b.len(), 1);
        click(&w, &b[0]);
        assert_eq!(
            got.borrow().as_slice(),
            &[("m-1".to_string(), MODULE.to_string())]
        );
    });
}

#[test]
fn a_missing_module_offers_install_from_marketplace_and_the_click_reaches_rust() {
    ui(|| {
        let w = window();
        let v = install(&w, &ModuleStatus::NotInstalled, Some("0.9.1"));
        assert_eq!(v.reason, Some(Reason::NotInstalled));
        assert!(v.message.contains("Install it from the marketplace"));
        assert_eq!(
            buttons(&w),
            vec![
                format!("Install {MODULE} from marketplace"),
                format!("Open another pane like {MODULE}")
            ],
            "not-installed offers install and open-another, never reopen"
        );
        assert_eq!(
            texts(&w, &v.message),
            1,
            "the not-installed sentence renders"
        );

        let got: Rc<RefCell<Option<(String, String)>>> = Rc::new(RefCell::new(None));
        {
            let got = got.clone();
            w.global::<crate::ModulePlaceholderAdapter>()
                .on_install(move |uid, id| *got.borrow_mut() = Some((uid.into(), id.into())));
        }
        let b = by_label(&w, &format!("Install {MODULE} from marketplace"));
        assert_eq!(b.len(), 1);
        click(&w, &b[0]);
        assert_eq!(
            got.borrow().clone(),
            Some(("m-1".to_string(), MODULE.to_string()))
        );
    });
}

#[test]
fn a_broken_module_offers_reinstall_and_shows_the_hash_reason() {
    ui(|| {
        let w = window();
        let v = install(
            &w,
            &ModuleStatus::Broken {
                reason: "binary hash changed".into(),
            },
            None,
        );
        assert_eq!(v.reason, Some(Reason::Broken));
        assert_eq!(
            buttons(&w),
            vec![
                format!("Install {MODULE} from marketplace"),
                format!("Open another pane like {MODULE}")
            ]
        );
        assert_eq!(texts(&w, &v.message), 1);
        assert!(v.message.contains("marketplace"));
        assert_eq!(texts(&w, "binary hash changed"), 1);
    });
}

#[test]
fn a_running_module_draws_no_placeholder_but_keeps_its_pane() {
    ui(|| {
        let w = window();
        let v = install(&w, &ModuleStatus::Running, None);
        assert!(!v.present);
        assert!(!w.global::<crate::ModulePlaceholderAdapter>().get_present());
        assert!(
            buttons(&w).is_empty(),
            "nothing to click while the module runs"
        );
        assert_eq!(texts(&w, "Files"), 0);
        // The pane row is still published: the grid never lost the slot.
        assert_eq!(w.get_panes().row_count(), 1);
    });
}

#[test]
fn two_placeholders_side_by_side_answer_for_their_own_pane() {
    ui(|| {
        let w = window();
        w.set_panes(
            Rc::new(slint::VecModel::from(vec![
                module_pane("m-1", 8.0),
                module_pane("m-2", 560.0),
            ]))
            .into(),
        );
        let crashed = ph::view(
            &ModuleStatus::Crashed { restarts: 1 },
            &pane_ref(None),
            None,
        );
        let other = ModulePaneRef::new("acme/avada-git", "log", None).expect("ref");
        let missing = ph::view(&ModuleStatus::NotInstalled, &other, None);
        ph::fill(
            &w,
            vec![
                ph::slot("m-1", &crashed).expect("present"),
                ph::slot("m-2", &missing).expect("present"),
            ],
        );
        assert!(w.global::<crate::ModulePlaceholderAdapter>().get_present());

        let got: Rc<RefCell<Vec<(String, String)>>> = Rc::new(RefCell::new(Vec::new()));
        {
            let got = got.clone();
            w.global::<crate::ModulePlaceholderAdapter>()
                .on_install(move |uid, id| got.borrow_mut().push((uid.into(), id.into())));
        }
        assert_eq!(by_label(&w, &format!("Reopen {MODULE}")).len(), 1);
        assert!(
            by_label(&w, "Reopen acme/avada-git").is_empty(),
            "missing never offers reopen"
        );
        let b = by_label(&w, "Install acme/avada-git from marketplace");
        assert_eq!(b.len(), 1);
        click(&w, &b[0]);
        assert_eq!(
            got.borrow().as_slice(),
            &[("m-2".to_string(), "acme/avada-git".to_string())],
            "the second pane's install names the second pane"
        );
    });
}
