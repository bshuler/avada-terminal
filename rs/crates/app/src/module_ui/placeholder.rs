//! The module placeholder pane (track H1): what a `module:` pane shows while its module is
//! crashed, disabled, not installed or broken.
//!
//! Two halves, kept apart so the mapping is testable without a window:
//!
//! * [`view`] is a pure function from the host's [`ModuleStatus`] and the pane's
//!   [`ModulePaneRef`] to a [`PlaceholderView`] — the reason, the heading, the sentence the
//!   user reads and which of the three buttons apply. A running or starting module yields
//!   `present == false`: nothing to draw, the module's own surface owns the rect.
//! * [`slot`] / [`fill`] turn views into `ModulePlaceholderSlot`s and push them into the
//!   `ModulePlaceholderAdapter` global (`ui/types.slint`, `ui/placeholder.slint`), one per
//!   pane uid. The grid never loses the slot: the pane rect stays whatever the module does,
//!   and only what is painted inside it changes.
//!
//! The controller (`state.rs`/`app.rs`, track H4) is expected to call [`fill`] whenever a
//! module's status or the set of module panes changes, and to wire the three adapter
//! callbacks — `reopen`, `open-another`, `install` — each of which carries the pane uid and
//! module id.

// The controller side (`state.rs`/`app.rs`) is track H4's; until it calls `fill`, only the
// tests reach this module, and a non-test build would otherwise flag every item unused.
#![allow(dead_code)]

use avada_core::module::ModuleStatus;
use avada_core::tools::kind::ModulePaneRef;
use slint::ComponentHandle;

/// Why the placeholder is showing, as the adapter spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// The process exited without being asked; the supervisor is restarting it.
    Crashed,
    /// Restarts were exhausted or the user turned it off; reopening starts it again.
    Disabled,
    /// No install record: the workspace names a module this machine does not have.
    NotInstalled,
    /// The binary or manifest no longer matches the install record; needs a reinstall.
    Broken,
}

impl Reason {
    /// The `reason` string the Slint side switches on.
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Crashed => "crashed",
            Reason::Disabled => "disabled",
            Reason::NotInstalled => "not-installed",
            Reason::Broken => "broken",
        }
    }
}

/// Everything the placeholder pane draws for one module pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaceholderView {
    /// False while the module runs (or is starting): draw nothing, the module owns the rect.
    pub present: bool,
    /// `owner/repo`.
    pub module_id: String,
    /// The version the workspace pinned, empty when it recorded none.
    pub pinned_version: String,
    /// `None` exactly when `present` is false.
    pub reason: Option<Reason>,
    /// Heading: the module's display name when known, else the repo half of the id.
    pub title: String,
    /// The one-line explanation under the heading.
    pub message: String,
    /// The host's own words (crash count, disable/broken reason); empty if none.
    pub detail: String,
    /// Offer "Reopen" (crashed, disabled).
    pub show_reopen: bool,
    /// Offer "Install from marketplace" (not installed, broken).
    pub show_install: bool,
}

/// What a disabled or crashed module tells the user to do.
pub const REOPEN_HINT: &str = "Reopen it, or open another pane like it.";
/// What a missing module tells the user to do.
pub const INSTALL_HINT: &str = "Install it from the marketplace to bring this pane back.";
/// What a broken module tells the user to do.
pub const REINSTALL_HINT: &str = "Reinstall it from the marketplace to bring this pane back.";

/// Map a module's status and the pane that shows it to the placeholder's contents.
/// `name` is the module's display name from its manifest when the host has one.
pub fn view(status: &ModuleStatus, pane: &ModulePaneRef, name: Option<&str>) -> PlaceholderView {
    let module_id = pane.id.to_string();
    let title = name
        .filter(|n| !n.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| repo_half(&module_id));
    let pinned_version = pane.pin.as_ref().map(|v| v.to_string()).unwrap_or_default();
    let absent = |reason: Option<Reason>, message: String, detail: String| PlaceholderView {
        present: reason.is_some(),
        module_id: module_id.clone(),
        pinned_version: pinned_version.clone(),
        reason,
        title: title.clone(),
        message,
        detail,
        show_reopen: matches!(reason, Some(Reason::Crashed | Reason::Disabled)),
        show_install: matches!(reason, Some(Reason::NotInstalled | Reason::Broken)),
    };
    match status {
        ModuleStatus::Starting | ModuleStatus::Running => {
            absent(None, String::new(), String::new())
        }
        ModuleStatus::Crashed { restarts } => absent(
            Some(Reason::Crashed),
            format!("The module crashed. {REOPEN_HINT}"),
            match restarts {
                0 => String::new(),
                1 => "Restarted once in the last minute.".to_string(),
                n => format!("Restarted {n} times in the last minute."),
            },
        ),
        ModuleStatus::Disabled { reason } => absent(
            Some(Reason::Disabled),
            format!("The module is disabled. {REOPEN_HINT}"),
            reason.trim().to_string(),
        ),
        ModuleStatus::NotInstalled => absent(
            Some(Reason::NotInstalled),
            format!("This module is not installed. {INSTALL_HINT}"),
            String::new(),
        ),
        ModuleStatus::Broken { reason } => absent(
            Some(Reason::Broken),
            format!("The module no longer matches what was installed. {REINSTALL_HINT}"),
            reason.trim().to_string(),
        ),
    }
}

/// The repo half of `owner/repo`, the whole id if there is no slash.
fn repo_half(id: &str) -> String {
    id.rsplit('/').next().unwrap_or(id).to_string()
}

/// One adapter row for the pane `pane_uid`. Returns `None` when the view is not present:
/// a running module gets no slot, so its placeholder draws nothing.
pub fn slot(pane_uid: &str, v: &PlaceholderView) -> Option<crate::ModulePlaceholderSlot> {
    if !v.present {
        return None;
    }
    Some(crate::ModulePlaceholderSlot {
        pane_uid: pane_uid.into(),
        module_id: v.module_id.as_str().into(),
        pinned_version: v.pinned_version.as_str().into(),
        reason: v.reason.map(Reason::as_str).unwrap_or("").into(),
        title: v.title.as_str().into(),
        message: v.message.as_str().into(),
        detail: v.detail.as_str().into(),
        show_reopen: v.show_reopen,
        show_install: v.show_install,
    })
}

/// Replace the adapter's slots with `slots` (one per module pane whose module is not
/// running) and set `present` accordingly. Call from the UI thread.
pub fn fill(w: &crate::AppWindow, slots: Vec<crate::ModulePlaceholderSlot>) {
    let a = w.global::<crate::ModulePlaceholderAdapter>();
    a.set_present(!slots.is_empty());
    a.set_slots(std::rc::Rc::new(slint::VecModel::from(slots)).into());
}

#[cfg(test)]
mod tests {
    use super::*;
    use avada_core::tools::kind::Version;

    fn pane(pin: Option<&str>) -> ModulePaneRef {
        ModulePaneRef::new(
            "acme/avada-files",
            "browser",
            pin.map(|p| Version::parse(p).expect("version")),
        )
        .expect("valid ref")
    }

    #[test]
    fn a_running_or_starting_module_has_no_placeholder() {
        for s in [ModuleStatus::Running, ModuleStatus::Starting] {
            let v = view(&s, &pane(None), None);
            assert!(!v.present, "{s:?}");
            assert_eq!(v.reason, None);
            assert!(!v.show_reopen && !v.show_install);
            assert!(slot("p1", &v).is_none());
        }
    }

    #[test]
    fn crashed_and_disabled_say_reopen_or_open_another() {
        let crashed = view(
            &ModuleStatus::Crashed { restarts: 2 },
            &pane(Some("1.2.0")),
            Some("Files"),
        );
        assert!(crashed.present);
        assert_eq!(crashed.reason, Some(Reason::Crashed));
        assert_eq!(crashed.title, "Files");
        assert_eq!(crashed.module_id, "acme/avada-files");
        assert_eq!(crashed.pinned_version, "1.2.0");
        assert!(crashed.message.contains(REOPEN_HINT), "{}", crashed.message);
        assert_eq!(crashed.detail, "Restarted 2 times in the last minute.");
        assert!(crashed.show_reopen && !crashed.show_install);

        let disabled = view(
            &ModuleStatus::Disabled {
                reason: "crashed 3 times in a minute".into(),
            },
            &pane(None),
            None,
        );
        assert_eq!(disabled.reason, Some(Reason::Disabled));
        assert!(disabled.message.contains(REOPEN_HINT));
        assert_eq!(disabled.detail, "crashed 3 times in a minute");
        assert!(disabled.show_reopen && !disabled.show_install);
    }

    #[test]
    fn not_installed_and_broken_send_the_user_to_the_marketplace() {
        let missing = view(&ModuleStatus::NotInstalled, &pane(Some("0.9.1")), None);
        assert_eq!(missing.reason, Some(Reason::NotInstalled));
        assert!(missing.message.contains("install"), "{}", missing.message);
        assert!(
            missing.message.contains("marketplace"),
            "{}",
            missing.message
        );
        assert!(missing.show_install && !missing.show_reopen);
        assert_eq!(missing.pinned_version, "0.9.1");

        let broken = view(
            &ModuleStatus::Broken {
                reason: "binary hash changed".into(),
            },
            &pane(None),
            None,
        );
        assert_eq!(broken.reason, Some(Reason::Broken));
        assert!(broken.message.contains("marketplace"));
        assert_eq!(broken.detail, "binary hash changed");
        assert!(broken.show_install && !broken.show_reopen);
    }

    #[test]
    fn the_title_falls_back_to_the_repo_half_of_the_id() {
        let v = view(&ModuleStatus::NotInstalled, &pane(None), None);
        assert_eq!(v.title, "avada-files");
        let v = view(&ModuleStatus::NotInstalled, &pane(None), Some("  "));
        assert_eq!(v.title, "avada-files", "a blank name is no name");
    }

    #[test]
    fn a_slot_carries_the_pane_uid_and_the_reason_string() {
        let v = view(&ModuleStatus::Crashed { restarts: 1 }, &pane(None), None);
        let s = slot("pane-7", &v).expect("present");
        assert_eq!(s.pane_uid.as_str(), "pane-7");
        assert_eq!(s.reason.as_str(), "crashed");
        assert_eq!(s.module_id.as_str(), "acme/avada-files");
        assert_eq!(s.pinned_version.as_str(), "");
        assert!(s.show_reopen && !s.show_install);
    }

    #[test]
    fn every_reason_has_a_distinct_adapter_string() {
        let all = [
            Reason::Crashed,
            Reason::Disabled,
            Reason::NotInstalled,
            Reason::Broken,
        ];
        let strs: std::collections::BTreeSet<&str> = all.iter().map(|r| r.as_str()).collect();
        assert_eq!(strs.len(), all.len());
        assert!(strs.contains("not-installed"));
    }
}
