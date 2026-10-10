//! Pane provenance — who opened a pane, from where, and why.
//!
//! A pane that appears on its own (an agent ran `avada ctl new-pane`, a script POSTed
//! `/command newPane`) used to carry nothing that said where it came from. The answer now
//! rides in the pane's existing string→string `meta` map under the `origin.` prefix, so it
//! is persisted, published on the read-model and settable after the fact with `setMeta`
//! like any other meta — no new field anywhere on the wire:
//!
//! | key                   | written by          | value                                          |
//! |-----------------------|---------------------|------------------------------------------------|
//! | `origin.via`          | ctl CLI / dispatch  | `avada ctl new-pane`, `control-api`, …         |
//! | `origin.at`           | ctl CLI / dispatch  | wall-clock time the pane was opened            |
//! | `origin.why`          | ctl `--why`         | the caller's own one-line reason               |
//! | `origin.by.pane`      | ctl CLI             | the calling pane's id (`AVADA_PANE_ID`)         |
//! | `origin.by.label`     | ctl CLI             | that pane's label at the time                  |
//! | `origin.by.session`   | ctl CLI             | the calling Claude Code session id             |
//! | `origin.by.agent`     | ctl CLI             | the calling agent (`AI_AGENT`)                 |
//! | `origin.by.process`   | ctl CLI             | the caller's process chain, nearest first      |
//! | `origin.cwd`          | ctl CLI             | the caller's working directory                 |
//!
//! The GUI shows [`describe`] as the hover text of the pane header's ⓘ button.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// Every provenance key starts with this.
pub const PREFIX: &str = "origin.";
pub const VIA: &str = "origin.via";
pub const AT: &str = "origin.at";
pub const WHY: &str = "origin.why";
pub const BY_PANE: &str = "origin.by.pane";
pub const BY_LABEL: &str = "origin.by.label";
pub const BY_SESSION: &str = "origin.by.session";
pub const BY_AGENT: &str = "origin.by.agent";
pub const BY_PROCESS: &str = "origin.by.process";
pub const CWD: &str = "origin.cwd";

/// `origin.via` stamped by the server on a `newPane` whose caller recorded none.
pub const VIA_CONTROL_API: &str = "control-api";

/// The `origin.*` subset of a pane's meta (empty when it has none).
pub fn extract(meta: Option<&BTreeMap<String, String>>) -> BTreeMap<String, String> {
    meta.map(|m| {
        m.iter()
            .filter(|(k, v)| k.starts_with(PREFIX) && !v.is_empty())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    })
    .unwrap_or_default()
}

/// Server-side floor for a control-API `newPane`: make sure the spawn spec's `meta` says it
/// came through the control API and when, without overwriting anything the caller sent.
pub fn stamp_spec(spec: &mut serde_json::Value) {
    let Some(obj) = spec.as_object_mut() else {
        return;
    };
    let meta = obj
        .entry("meta")
        .or_insert_with(|| serde_json::Value::Object(Default::default()));
    let Some(meta) = meta.as_object_mut() else {
        return;
    };
    meta.entry(VIA).or_insert_with(|| VIA_CONTROL_API.into());
    meta.entry(AT)
        .or_insert_with(|| utc_stamp(now_secs()).into());
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `YYYY-MM-DD HH:MM:SS UTC` for unix seconds (civil-from-days; no time crate in core).
pub fn utc_stamp(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// The human-readable provenance shown on the pane's ⓘ button. Never empty: a pane with no
/// record says so plainly rather than implying it was opened by hand.
pub fn describe(origin: &BTreeMap<String, String>) -> String {
    if origin.is_empty() {
        return "Origin: not recorded.\nOpened by hand in Avada, restored from an older session,\n\
                or created before origin tracking existed."
            .to_string();
    }
    let get = |k: &str| origin.get(k).map(String::as_str);
    let mut lines = Vec::new();
    lines.push(format!(
        "Opened via {}{}",
        get(VIA).unwrap_or("unknown"),
        get(AT).map(|t| format!(" at {t}")).unwrap_or_default()
    ));
    lines.push(format!(
        "Why: {}",
        get(WHY).unwrap_or("(the caller gave no reason)")
    ));
    match (get(BY_LABEL), get(BY_PANE)) {
        (Some(l), Some(p)) => lines.push(format!("From pane: {l} ({p})")),
        (None, Some(p)) => lines.push(format!("From pane: {p}")),
        _ => {}
    }
    if let Some(a) = get(BY_AGENT) {
        lines.push(format!("Agent: {a}"));
    }
    if let Some(s) = get(BY_SESSION) {
        lines.push(format!("Claude session: {s}"));
    }
    if let Some(p) = get(BY_PROCESS) {
        lines.push(format!("Process: {p}"));
    }
    if let Some(c) = get(CWD) {
        lines.push(format!("Caller cwd: {c}"));
    }
    // Anything else a caller recorded under origin.* still shows.
    let known = [
        VIA, AT, WHY, BY_PANE, BY_LABEL, BY_SESSION, BY_AGENT, BY_PROCESS, CWD,
    ];
    for (k, v) in origin {
        if !known.contains(&k.as_str()) {
            lines.push(format!("{}: {v}", &k[PREFIX.len()..]));
        }
    }
    lines.push("Click to copy.".to_string());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn extract_keeps_only_nonempty_origin_keys() {
        let meta = m(&[("origin.via", "x"), ("origin.why", ""), ("role", "w")]);
        assert_eq!(extract(Some(&meta)), m(&[("origin.via", "x")]));
        assert!(extract(None).is_empty());
    }

    #[test]
    fn stamp_spec_fills_but_never_overwrites() {
        let mut spec = serde_json::json!({ "command": "x" });
        stamp_spec(&mut spec);
        assert_eq!(spec["meta"][VIA], VIA_CONTROL_API);
        assert!(spec["meta"][AT].as_str().unwrap().ends_with(" UTC"));

        let mut spec =
            serde_json::json!({ "meta": { "origin.via": "avada ctl new-pane", "origin.at": "t" } });
        stamp_spec(&mut spec);
        assert_eq!(spec["meta"][VIA], "avada ctl new-pane");
        assert_eq!(spec["meta"][AT], "t");
    }

    #[test]
    fn utc_stamp_known_values() {
        assert_eq!(utc_stamp(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(utc_stamp(951_782_400), "2000-02-29 00:00:00 UTC");
        assert_eq!(utc_stamp(1_791_563_021), "2026-10-09 16:23:41 UTC");
    }

    #[test]
    fn describe_names_the_caller_and_the_reason() {
        let o = m(&[
            (VIA, "avada ctl new-pane"),
            (AT, "2026-10-09 12:23:40 EDT"),
            (WHY, "probe op session reuse"),
            (BY_SESSION, "d1335e23"),
            ("origin.ticket", "B7"),
        ]);
        let d = describe(&o);
        assert!(d.starts_with("Opened via avada ctl new-pane at 2026-10-09 12:23:40 EDT\n"));
        assert!(d.contains("Why: probe op session reuse"));
        assert!(d.contains("Claude session: d1335e23"));
        assert!(d.contains("ticket: B7"));
        assert!(describe(&BTreeMap::new()).starts_with("Origin: not recorded."));
    }
}
