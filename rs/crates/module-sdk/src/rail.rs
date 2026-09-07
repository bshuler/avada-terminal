//! Left-panel rail entries and the rows beneath them (UI tier 1).

use crate::manifest::{ModuleId, UiTier};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One rail entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RailEntry {
    /// Contribution id from the manifest (`files`). The host shows it as `<module>/<id>`.
    pub id: String,
    /// Display label.
    pub label: String,
    /// Path to an SVG relative to the module's install directory. The host reads
    /// the file itself; the module never sends pixels for a tier-1 icon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Tier requested (1 or 2 for a rail entry).
    pub tier: UiTier,
    /// Owner; filled by the host, ignored if the module sets it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<ModuleId>,
    /// Sort key among entries of the same module; the host orders modules by
    /// install order and lets the user drag.
    #[serde(default)]
    pub order: i32,
    /// Slot component source (tier 2 only): a `.slint` file path relative to the
    /// install directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component: Option<String>,
}

/// A row shown under a rail entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    /// Stable within the entry.
    pub id: String,
    /// Primary text.
    pub label: String,
    /// Secondary text.
    #[serde(default)]
    pub detail: String,
    /// Nesting depth for tree-shaped lists.
    #[serde(default)]
    pub depth: u8,
    /// Has children that can be expanded.
    #[serde(default)]
    pub expandable: bool,
    /// Currently expanded.
    #[serde(default)]
    pub expanded: bool,
    /// Icon path (see [`RailEntry::icon`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Marks for the row (`modified`, `untracked`, …); the host maps known marks
    /// to colours and ignores unknown ones.
    #[serde(default)]
    pub marks: Vec<String>,
    /// Opaque payload echoed back in `module.row.activate`.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub data: Value,
}

/// `host.rail.register` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterRail {
    /// Full set; replaces earlier registrations from this module.
    pub entries: Vec<RailEntry>,
}

/// `host.rows.set` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetRows {
    /// Entry id.
    pub entry: String,
    /// Full list; replaces.
    pub rows: Vec<Row>,
}

/// `module.row.activate` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowActivate {
    /// Entry id.
    pub entry: String,
    /// Row id.
    pub row: String,
    /// The payload the module put on the row.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub data: Value,
    /// Which gesture: `open`, `toggle`, `context`.
    pub gesture: Gesture,
}

/// How a row was activated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Gesture {
    /// Click / Enter.
    Open,
    /// Expand or collapse.
    Toggle,
    /// Right-click.
    Context,
}

/// Rules a tier-1 rail entry must satisfy before the host accepts it.
pub fn validate_entry(e: &RailEntry) -> Result<(), String> {
    if e.id.is_empty()
        || !e
            .id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(format!("rail entry id `{}` must be lowercase-kebab", e.id));
    }
    if e.label.trim().is_empty() {
        return Err(format!("rail entry `{}` has an empty label", e.id));
    }
    match e.tier {
        UiTier::Data => {
            if e.component.is_some() {
                return Err(format!(
                    "rail entry `{}` is tier 1 but names a component",
                    e.id
                ));
            }
        }
        UiTier::Slot => {
            if e.component.is_none() {
                return Err(format!(
                    "rail entry `{}` is tier 2 but names no component",
                    e.id
                ));
            }
        }
        other => {
            return Err(format!(
                "rail entry `{}` cannot use tier {}",
                e.id,
                u8::from(other)
            ))
        }
    }
    for p in [e.icon.as_deref(), e.component.as_deref()]
        .into_iter()
        .flatten()
    {
        if p.starts_with('/') || p.split('/').any(|s| s == "..") {
            return Err(format!(
                "rail entry `{}` path `{p}` must be relative and inside the module",
                e.id
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry() -> RailEntry {
        RailEntry {
            id: "files".into(),
            label: "Files".into(),
            icon: Some("icons/files.svg".into()),
            tier: UiTier::Data,
            module: None,
            order: 0,
            component: None,
        }
    }

    #[test]
    fn entry_rules() {
        assert_eq!(validate_entry(&entry()), Ok(()));
        let mut e = entry();
        e.id = "Files".into();
        assert!(validate_entry(&e).is_err());
        let mut e = entry();
        e.label = " ".into();
        assert!(validate_entry(&e).is_err());
        let mut e = entry();
        e.tier = UiTier::Slot;
        assert!(validate_entry(&e).is_err(), "slot needs a component");
        e.component = Some("ui/files.slint".into());
        assert_eq!(validate_entry(&e), Ok(()));
        e.tier = UiTier::Data;
        assert!(
            validate_entry(&e).is_err(),
            "data must not name a component"
        );
        let mut e = entry();
        e.tier = UiTier::Pixels;
        assert!(validate_entry(&e).is_err());
        let mut e = entry();
        e.icon = Some("../../etc/passwd".into());
        assert!(validate_entry(&e).is_err());
        let mut e = entry();
        e.icon = Some("/abs.svg".into());
        assert!(validate_entry(&e).is_err());
    }

    #[test]
    fn rows_round_trip_with_opaque_data() {
        let rows = SetRows {
            entry: "files".into(),
            rows: vec![Row {
                id: "src".into(),
                label: "src".into(),
                detail: String::new(),
                depth: 0,
                expandable: true,
                expanded: false,
                icon: None,
                marks: vec!["modified".into()],
                data: json!({"path": "/w/src"}),
            }],
        };
        let v = serde_json::to_value(&rows).unwrap();
        assert_eq!(v["rows"][0]["data"]["path"], "/w/src");
        assert_eq!(serde_json::from_value::<SetRows>(v).unwrap(), rows);
        let act = RowActivate {
            entry: "files".into(),
            row: "src".into(),
            data: Value::Null,
            gesture: Gesture::Toggle,
        };
        let v = serde_json::to_value(&act).unwrap();
        assert!(v.get("data").is_none());
        assert_eq!(v["gesture"], "toggle");
        assert_eq!(serde_json::from_value::<RowActivate>(v).unwrap(), act);
    }
}
