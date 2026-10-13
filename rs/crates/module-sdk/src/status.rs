//! Status-tray cards (the bottom Arcane drawer): `host.status.set` and
//! `module.status.activate`.
//!
//! A module owns a set of cards identified by `id`; the host shows them as
//! `<module>/<id>`. Setting an id that exists updates it in place, so a module may call
//! this every tick with the current values. A card at [`StatusLevel::Needs`] is something
//! the human has to answer: the host lifts it into the attention cards and tells the module
//! when it is clicked.

use serde::{Deserialize, Serialize};

/// How loudly a card speaks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StatusLevel {
    /// Plain status.
    #[default]
    Info,
    /// Healthy.
    Ok,
    /// Degraded; worth a glance.
    Warn,
    /// Needs the human's response.
    Needs,
}

/// One card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusItem {
    /// Stable within the module.
    pub id: String,
    /// Small caption (`AI MODEL`).
    pub label: String,
    /// The value shown under it (`Arcane-7B`).
    #[serde(default)]
    pub value: String,
    /// One glyph, drawn at the card's left. Empty means the default.
    #[serde(default)]
    pub glyph: String,
    /// See [`StatusLevel`].
    #[serde(default)]
    pub level: StatusLevel,
    /// Lower sorts first.
    #[serde(default)]
    pub order: i32,
    /// Report a click back as `module.status.activate`.
    #[serde(default)]
    pub activatable: bool,
    /// Show as a `label  value` line in the top welcome banner instead of a tray card.
    #[serde(default)]
    pub banner: bool,
}

/// Params of `host.status.set`: upsert `items`, then drop every id in `remove`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetStatus {
    /// Cards to add or update.
    #[serde(default)]
    pub items: Vec<StatusItem>,
    /// Ids to take away.
    #[serde(default)]
    pub remove: Vec<String>,
}

/// Params of `module.status.activate` (notification): the card with this id was clicked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusActivate {
    /// The module-local id.
    pub id: String,
}

/// Most cards one module may hold, and the longest text on one: the tray is a strip, not
/// a log, and a module in a loop must not be able to grow the host without bound.
pub const MAX_ITEMS: usize = 16;
/// Longest `label`/`value`, in chars.
pub const MAX_TEXT: usize = 120;

/// Why an item cannot be shown, or `None` when it can.
pub fn validate_item(i: &StatusItem) -> Option<String> {
    if i.id.trim().is_empty() {
        return Some("status id must not be empty".into());
    }
    if i.id.chars().count() > 64 {
        return Some(format!("status id `{}` is longer than 64 chars", i.id));
    }
    if i.label.trim().is_empty() {
        return Some(format!("status `{}` needs a label", i.id));
    }
    if i.label.chars().count() > MAX_TEXT || i.value.chars().count() > MAX_TEXT {
        return Some(format!("status `{}` text is longer than {MAX_TEXT} chars", i.id));
    }
    if i.glyph.chars().count() > 4 {
        return Some(format!("status `{}` glyph must be one symbol", i.id));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_fill_in_and_levels_are_lowercase() {
        let s: SetStatus = serde_json::from_value(json!({
            "items": [{ "id": "m", "label": "AI MODEL", "level": "needs" }]
        }))
        .unwrap();
        assert_eq!(s.items[0].level, StatusLevel::Needs);
        assert_eq!(s.items[0].value, "");
        assert!(!s.items[0].activatable);
        assert!(s.remove.is_empty());
    }

    #[test]
    fn validation_names_the_offender() {
        let ok = StatusItem {
            id: "m".into(),
            label: "L".into(),
            value: "v".into(),
            glyph: "✦".into(),
            level: StatusLevel::Ok,
            order: 0,
            activatable: false,
            banner: false,
        };
        assert_eq!(validate_item(&ok), None);
        let mut bad = ok.clone();
        bad.id = " ".into();
        assert!(validate_item(&bad).unwrap().contains("id"));
        let mut bad = ok.clone();
        bad.value = "x".repeat(MAX_TEXT + 1);
        assert!(validate_item(&bad).unwrap().contains("longer"));
        let mut bad = ok;
        bad.glyph = "abcde".into();
        assert!(validate_item(&bad).unwrap().contains("glyph"));
    }
}
