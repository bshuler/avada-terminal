//! Which agent-tool adapters the user switched off — `{ "disabledTools": [...] }` in
//! `skills-settings.json` under the config dir. Atomic write, defaults to empty.
//!
//! [`crate::skills::Tools`] already models the toggle (`present` vs `disabled`, with
//! `has` gating every write and the materializer sweeping a disabled tool's files back
//! off disk). What was missing was somewhere to keep the answer between runs: until
//! this file existed, `crate::hyperpane::materialize` detected the machine's tools
//! fresh on every start and wrote for all of them, so switching one off could not
//! survive a restart.
//!
//! Loading is forgiving in the same way [`super::control_settings`] is — a missing or
//! corrupt file yields the default rather than failing a start — with one addition:
//! ids are filtered against [`ADAPTERS`], so a typo or an id from a build that knew a
//! tool this one doesn't cannot quietly disable nothing while looking like it did.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::persistence::paths;
use crate::skills::adapters::ADAPTERS;

/// The user's per-tool toggles. Empty (nothing disabled) is the default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SkillsSettings {
    /// Adapter ids ([`crate::skills::AdapterRow::id`]) the user switched off.
    pub disabled_tools: BTreeSet<String>,
}

/// Is `id` an adapter this build knows about?
fn known(id: &str) -> bool {
    ADAPTERS.iter().any(|row| row.id == id)
}

/// Read the settings from the canonical `skills-settings.json`.
#[tracing::instrument(level = "debug", ret)]
pub fn load() -> SkillsSettings {
    load_from(&paths::skills_settings_json())
}

/// Read the settings from `path`, returning the defaults on any error.
#[tracing::instrument(level = "debug", ret)]
pub fn load_from(path: &std::path::Path) -> SkillsSettings {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return SkillsSettings::default();
    };
    // A generic Value so one bad element coerces away instead of failing the parse the
    // way a typed `BTreeSet<String>` would.
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return SkillsSettings::default();
    };
    let disabled_tools = value
        .get("disabledTools")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .filter(|id| known(id))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    SkillsSettings { disabled_tools }
}

/// Persist the settings to the canonical `skills-settings.json` (atomic).
#[tracing::instrument(level = "debug", ret)]
pub fn save(settings: &SkillsSettings) -> std::io::Result<()> {
    save_to(&paths::skills_settings_json(), settings)
}

/// Persist the settings to `path`, atomically, pretty-printed with a 2-space indent to
/// match every other settings file here.
#[tracing::instrument(level = "debug", ret)]
pub fn save_to(path: &std::path::Path, settings: &SkillsSettings) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(settings)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    paths::write_atomic(path, json.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "hp-skills-settings-{}-{tag}.json",
            std::process::id()
        ))
    }

    #[test]
    fn missing_file_yields_defaults() {
        let p = temp_path("missing");
        let _ = std::fs::remove_file(&p);
        assert_eq!(load_from(&p), SkillsSettings::default());
        assert!(load_from(&p).disabled_tools.is_empty());
    }

    #[test]
    fn corrupt_file_yields_defaults() {
        let p = temp_path("corrupt");
        std::fs::write(&p, b"not json {").unwrap();
        assert_eq!(load_from(&p), SkillsSettings::default());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn unknown_ids_and_non_strings_are_dropped() {
        let p = temp_path("filter");
        std::fs::write(
            &p,
            br#"{ "disabledTools": ["claude-code", "not-a-tool", 7, null, "cline"] }"#,
        )
        .unwrap();
        let got = load_from(&p);
        assert_eq!(
            got.disabled_tools,
            BTreeSet::from(["claude-code".to_string(), "cline".to_string()])
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_wrong_shape_is_not_a_disabled_tool() {
        let p = temp_path("shape");
        // An object where an array belongs, and a missing key, both mean "nothing off"
        // rather than "everything off" — the safe direction for a write-side toggle.
        for body in [
            br#"{ "disabledTools": {} }"#.as_slice(),
            br#"{}"#.as_slice(),
        ] {
            std::fs::write(&p, body).unwrap();
            assert!(load_from(&p).disabled_tools.is_empty());
        }
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn save_then_load_round_trips() {
        let p = temp_path("roundtrip");
        let settings = SkillsSettings {
            disabled_tools: BTreeSet::from(["aider".to_string(), "kiro".to_string()]),
        };
        save_to(&p, &settings).unwrap();
        assert_eq!(load_from(&p), settings);
        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(raw.contains("disabledTools"), "camelCase on disk: {raw}");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn every_adapter_id_survives_the_filter() {
        // The filter is only a guard against junk; it must never reject a real id.
        for row in ADAPTERS {
            assert!(known(row.id), "{} should be known", row.id);
        }
    }
}
