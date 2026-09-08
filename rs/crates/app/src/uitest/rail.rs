//! Track H4: the module rail — the strip's module buttons and the rows a module projects
//! beneath the one that is active.
//!
//! These drive the panel from the host's own [`RailEvent`](avada_core::module::RailEvent)s
//! through the same projection `paneview::resync` uses
//! ([`fill_rail`](crate::paneview::fill_rail)), so a test failing here means a real module
//! registering a real entry would be unreachable with a mouse — not merely that a Slint
//! property failed to store a string.
#![allow(unused_imports)]
use super::*;

use crate::leftpanel::{entry_key, ModuleRail};
use crate::paneview::{fill_rail, LEFT_MODE_RAIL, LEFT_MODE_WORKSPACE};
use avada_core::module::{RailEntry, RailEvent, Row};
use avada_core::rights::ModuleId;

fn id(s: &str) -> ModuleId {
    ModuleId::new(s).expect("a valid owner/repo module id")
}

/// Built through serde because `tier` is `UiTier`, the SDK's type, and the app crate does
/// not depend on the SDK — this is exactly the payload a module's `host.rail.register`
/// puts on the wire. Tier 1 is the row-list surface the panel draws.
fn entry(id: &str, label: &str, order: i32) -> RailEntry {
    serde_json::from_value(serde_json::json!({
        "id": id, "label": label, "tier": 1, "order": order
    }))
    .expect("a rail entry")
}

fn row(id: &str, label: &str, expandable: bool) -> Row {
    Row {
        id: id.into(),
        label: label.into(),
        detail: String::new(),
        depth: 0,
        expandable,
        expanded: false,
        icon: None,
        marks: vec![],
        data: serde_json::json!({ "path": id }),
    }
}

/// The first-party marketplace as it actually registers: the module id is its GitHub
/// `owner/repo`, and the panel finds it by the entry it contributed, never by a
/// hardcoded id.
fn marketplace() -> (ModuleId, RailEvent) {
    let m = id("bshuler/avada-marketplace");
    (
        m.clone(),
        RailEvent::Registered {
            module: m,
            entries: vec![entry("browse", "Marketplace", 0)],
        },
    )
}

/// Put the panel in the state a module's registration leaves it in: the built-in modes on
/// the strip, plus whatever `rail` holds.
fn install_rail(w: &crate::AppWindow, rail: &ModuleRail) {
    install_modes(w);
    fill_rail(w, rail);
}

/// The strip has to keep drawing the built-ins in their fixed order and hang the module
/// buttons off the end of them — a module that reorders the panel's own views would be a
/// module rearranging the user's furniture.
#[test]
fn module_entries_follow_the_built_in_modes_on_one_strip() {
    ui(|| {
        let w = window();
        let mut rail = ModuleRail::default();
        rail.apply(marketplace().1);
        rail.apply(RailEvent::Registered {
            module: id("acme/avada-files"),
            entries: vec![entry("tree", "Files (acme)", 5)],
        });
        install_rail(&w, &rail);

        // By ROLE, not by label alone: a section head repeats the same word as a `Text`,
        // and only one of the two is pressable (see `by_role`).
        let x = |label: &str| {
            let found = only(&w, label, AccessibleRole::Button);
            let size = found.size();
            assert!(
                size.width > 0.0 && size.height > 0.0,
                "`{label}` laid out to {}x{} — nothing can click it",
                size.width,
                size.height
            );
            found.absolute_position().x
        };
        let workspace = x("Workspace");
        let (market, tree) = (x("Marketplace"), x("Files (acme)"));
        assert!(
            workspace < market && market < tree,
            "module entries follow the built-in, in `order`: {workspace} {market} {tree}"
        );
    });
}

/// The strip used to be hidden unless a tool was favourited. A module that registers an
/// entry has to bring it back, or its button exists and is invisible.
#[test]
fn a_module_entry_shows_the_strip_even_with_no_favourite_tools() {
    ui(|| {
        let w = window();
        let lp = w.global::<crate::LeftPanelAdapter>();
        lp.set_open(true);
        // Only the workspace mode — the strip's own "nothing to switch between" case.
        lp.set_modes(
            std::rc::Rc::new(slint::VecModel::from(vec![crate::LeftModeRow {
                label: "Workspace".into(),
                icon: 0,
                brand: slint::Color::from_rgb_u8(0, 0, 0),
            }]))
            .into(),
        );
        assert!(
            by_role(&w, "Workspace", AccessibleRole::Button).is_empty(),
            "one built-in mode alone must not draw a strip"
        );

        let mut rail = ModuleRail::default();
        rail.apply(marketplace().1);
        fill_rail(&w, &rail);
        assert_eq!(
            by_role(&w, "Marketplace", AccessibleRole::Button).len(),
            1,
            "a registered entry brings the strip back"
        );
        assert_eq!(
            by_role(&w, "Workspace", AccessibleRole::Button).len(),
            1,
            "and the built-in comes with it"
        );
    });
}

/// THE regression this file exists for: the click has to reach Rust carrying the entry's
/// key, and it has to leave the panel in the rail mode.
#[test]
fn clicking_a_module_entry_reaches_rust_with_its_key() {
    ui(|| {
        let w = window();
        let (module, registered) = marketplace();
        let mut rail = ModuleRail::default();
        rail.apply(registered);
        install_rail(&w, &rail);

        let got = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        {
            let got = got.clone();
            w.global::<crate::RailAdapter>()
                .on_activate(move |key| got.borrow_mut().push(key.to_string()));
        }

        let found = only(&w, "Marketplace", AccessibleRole::Button);
        click(&w, &found);

        assert_eq!(
            got.borrow().as_slice(),
            [entry_key(&module, "browse")],
            "the click must carry `<owner/repo>#<entry-id>`, not a display label"
        );
        assert_eq!(
            w.global::<crate::LeftPanelAdapter>().get_mode(),
            LEFT_MODE_RAIL,
            "and it must put the panel into the module mode"
        );
        assert_eq!(
            w.global::<crate::RailAdapter>().get_active(),
            entry_key(&module, "browse").as_str()
        );
    });
}

/// A module's rows are its only tier-1 surface; if the list does not render, the module
/// has nothing.
#[test]
fn the_active_entrys_rows_render_and_a_click_carries_the_gesture() {
    ui(|| {
        let w = window();
        let (module, registered) = marketplace();
        let mut rail = ModuleRail::default();
        rail.apply(registered);
        rail.apply(RailEvent::Rows {
            module: module.clone(),
            entry: "browse".into(),
            rows: vec![
                row("installed", "Installed", true),
                row("all", "All", false),
            ],
        });
        assert!(rail.activate(&entry_key(&module, "browse")));
        install_rail(&w, &rail);
        w.global::<crate::LeftPanelAdapter>()
            .set_mode(LEFT_MODE_RAIL);

        let got = std::rc::Rc::new(std::cell::RefCell::new(
            Vec::<(String, String, String)>::new(),
        ));
        {
            let got = got.clone();
            w.global::<crate::RailAdapter>()
                .on_row_activate(move |key, row, gesture| {
                    got.borrow_mut()
                        .push((key.to_string(), row.to_string(), gesture.to_string()));
                });
        }

        let installed = only(&w, "Installed", AccessibleRole::Button);
        let all = only(&w, "All", AccessibleRole::Button);
        assert!(
            installed.absolute_position().y < all.absolute_position().y,
            "rows keep the order the module projected them in"
        );

        click(&w, &installed);
        click(&w, &all);
        let key = entry_key(&module, "browse");
        assert_eq!(
            got.borrow().as_slice(),
            [
                (key.clone(), "installed".into(), "toggle".into()),
                (key, "all".into(), "open".into()),
            ],
            "an expandable row toggles, a leaf opens — the gesture is not the same for both"
        );
    });
}

/// Until the module projects anything the panel must say so, rather than drawing an empty
/// box that reads as a hung panel.
#[test]
fn an_entry_with_no_rows_yet_shows_its_empty_text() {
    ui(|| {
        let w = window();
        let (module, registered) = marketplace();
        let mut rail = ModuleRail::default();
        rail.apply(registered);
        assert!(rail.activate(&entry_key(&module, "browse")));
        install_rail(&w, &rail);
        w.global::<crate::RailAdapter>()
            .set_empty_text("No modules installed".into());
        w.global::<crate::LeftPanelAdapter>()
            .set_mode(LEFT_MODE_RAIL);

        assert!(
            !by_label(&w, "No modules installed").is_empty(),
            "an entry that has projected nothing yet must show its empty state"
        );
        assert!(
            !by_role(&w, "Marketplace", AccessibleRole::Text).is_empty(),
            "and the section head still names the active entry"
        );

        rail.apply(RailEvent::Rows {
            module,
            entry: "browse".into(),
            rows: vec![row("installed", "Installed", false)],
        });
        fill_rail(&w, &rail);
        assert!(
            by_label(&w, "No modules installed").is_empty(),
            "the empty state must go away once rows arrive"
        );
        assert_eq!(by_role(&w, "Installed", AccessibleRole::Button).len(), 1);
    });
}

/// The rail is a projection, so an entry that goes away has to go away — and it must not
/// leave the panel sitting on a head with no module behind it.
#[test]
fn an_entry_leaves_when_its_module_does_and_the_panel_falls_back() {
    ui(|| {
        let w = window();
        let (module, registered) = marketplace();
        let mut rail = ModuleRail::default();
        rail.apply(registered);
        assert!(rail.activate(&entry_key(&module, "browse")));
        install_rail(&w, &rail);
        let lp = w.global::<crate::LeftPanelAdapter>();
        lp.set_mode(LEFT_MODE_RAIL);
        assert_eq!(by_role(&w, "Marketplace", AccessibleRole::Button).len(), 1);

        assert!(
            rail.apply(RailEvent::Gone {
                module: module.clone()
            }),
            "the host said the module is gone and it took the active entry with it"
        );
        fill_rail(&w, &rail);
        if lp.get_mode() == LEFT_MODE_RAIL && rail.active.is_none() {
            lp.set_mode(LEFT_MODE_WORKSPACE);
        }

        assert!(
            by_label(&w, "Marketplace").is_empty(),
            "a crashed module must leave neither a button nor a section head behind"
        );
        assert!(
            !w.global::<crate::RailAdapter>().get_present(),
            "and the strip stops claiming a rail"
        );
        assert_eq!(
            lp.get_mode(),
            LEFT_MODE_WORKSPACE,
            "the panel falls back to the workspace tree"
        );
    });
}

/// The strip is glyphs and the rows are text, so the accessible label is the only name
/// either has — for a screen reader and for every test above. A module that ships an icon
/// must not lose it.
#[test]
fn every_rail_control_announces_itself() {
    ui(|| {
        let w = window();
        let module = id("bshuler/avada-marketplace");
        let mut rail = ModuleRail::default();
        rail.apply(RailEvent::Registered {
            module: module.clone(),
            entries: vec![serde_json::from_value(serde_json::json!({
                "id": "browse", "label": "Marketplace", "tier": 1, "order": 0,
                "icon": "M 2 2 L 14 2 L 14 14 L 2 14 Z"
            }))
            .expect("a rail entry with an inline icon")],
        });
        rail.apply(RailEvent::Rows {
            module: module.clone(),
            entry: "browse".into(),
            rows: vec![row("installed", "Installed", true)],
        });
        assert!(rail.activate(&entry_key(&module, "browse")));
        install_rail(&w, &rail);
        w.global::<crate::LeftPanelAdapter>()
            .set_mode(LEFT_MODE_RAIL);

        assert_eq!(
            by_role(&w, "Marketplace", AccessibleRole::Button).len(),
            1,
            "an entry with its own icon still announces its label"
        );
        assert_eq!(
            by_role(&w, "Installed", AccessibleRole::Button).len(),
            1,
            "and so does every row"
        );
        assert_eq!(
            by_role(&w, "Back to the workspace", AccessibleRole::Button).len(),
            1,
            "the way out of a module's surface is reachable too"
        );
    });
}
