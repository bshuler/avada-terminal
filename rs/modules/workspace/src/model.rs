//! The two file formats this module reads: a workspace and a workspace set.
//!
//! ## Why the format is declared here rather than imported
//!
//! A module cannot depend on `avada-core` — it is a separate process built from a separate
//! repository, and the only thing it shares with the host is the wire contract. What the
//! host and this module actually agree on is therefore not a Rust type, it is *the JSON on
//! disk*: `{ "format": "avada", "version": 1, "workspace": { … } }`. So the format is
//! re-declared here, and [`tests::the_pretty_round_trip_is_byte_identical`] is what keeps
//! the declaration honest.
//!
//! ## Why every field is optional and nothing is `deny_unknown_fields`
//!
//! A workspace file written by a newer host must still load here, and a file this module
//! rewrites must not lose the parts it did not understand. Field *declaration order* is
//! the canonical on-disk order, and `skip_serializing_if = "Option::is_none"` is what
//! makes a two-space pretty re-serialisation reproduce the input bytes exactly — the
//! round-trip contract the host states in `workspace/model.rs`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The magic `format` discriminator of a versioned workspace container.
pub const ENVELOPE_FORMAT: &str = "avada";
/// The newest workspace-envelope `version` this module reads and the version it writes.
pub const ENVELOPE_VERSION: u32 = 1;
/// The magic `format` discriminator of a versioned workspace-set container.
pub const SET_FORMAT: &str = "avada-set";
/// The newest set-envelope `version` this module reads.
pub const SET_VERSION: u32 = 1;

/// The pre-rename `format` a set file may still carry (`hyperpanes-set`), read but never
/// written — the same both-ways compatibility the host keeps in `compat`.
pub const LEGACY_SET_FORMAT: &str = "hyperpanes-set";

/// The repo-local project directory, relative to the checkout root.
pub const PROJECT_DIR: &str = ".avada";
/// The pre-rename project directory, still read when only it exists.
pub const LEGACY_PROJECT_DIR: &str = ".hyperpanes";
/// The repo-local project file inside [`PROJECT_DIR`].
pub const PROJECT_FILE: &str = "project.json";

/// One terminal pane: an optional shell command plus presentation/launch hints.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaneSpec {
    /// Tab-strip / pane-header text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Accent colour.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// The command line the pane runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Literal argv for a direct (no-shell) spawn with `command`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    /// Working directory, absolute or relative to the file's own directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Shell override.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    /// Per-pane font size.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font_size: Option<u32>,
    /// Free-form per-pane metadata; `pane.kind` rides in here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<BTreeMap<String, String>>,
    /// The pane's live session uid at snapshot time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uid: Option<String>,
    /// Whether new Claude replies in this pane are spoken aloud.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub talk: Option<bool>,
    /// A human-written line about the work this pane is for — *why* it exists, not what it
    /// runs. The one field of a pane a human writes by hand, and the one this module
    /// edits: geometry and commands are reconstructible from habit, intent is not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The `meta` key the pane kind rides in.
pub const META_KIND_KEY: &str = "pane.kind";

impl PaneSpec {
    /// What kind of pane this is, read out of `meta["pane.kind"]`; empty for a plain
    /// terminal, which is every pane written before tool panes existed.
    pub fn kind(&self) -> &str {
        self.meta
            .as_ref()
            .and_then(|m| m.get(META_KIND_KEY))
            .map(String::as_str)
            .unwrap_or("")
    }

    /// The best single line to show for this pane: its label, else its command, else its
    /// kind, else the word every unnamed shell deserves.
    pub fn title(&self) -> String {
        for candidate in [self.label.as_deref(), self.command.as_deref()] {
            if let Some(text) = candidate.map(str::trim).filter(|s| !s.is_empty()) {
                return text.to_string();
            }
        }
        match self.kind() {
            "" => "Terminal".to_string(),
            other => other.to_string(),
        }
    }
}

/// One tab (group): a layout plus its panes and per-slot split state.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupSpec {
    /// Tab title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Layout name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layout: Option<String>,
    /// The panes, in slot order.
    #[serde(default)]
    pub panes: Vec<PaneSpec>,
    /// Per-slot split fractions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sizes: Option<Vec<f64>>,
    /// Main-stack split fraction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub main_fraction: Option<f64>,
    /// Index of the focused pane.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focused: Option<u32>,
    /// Index of the maximised pane.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zoomed: Option<u32>,
    /// Whether the app owns this tab and refuses to close it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<bool>,
}

impl GroupSpec {
    /// The tab's title, or a description of what is in it.
    pub fn title(&self) -> String {
        match self
            .title
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(t) => t.to_string(),
            None if self.panes.len() == 1 => self.panes[0].title(),
            None => format!("{} panes", self.panes.len()),
        }
    }
}

/// Saved OS-window geometry.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowBounds {
    /// Left edge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x: Option<i64>,
    /// Top edge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub y: Option<i64>,
    /// Width in pixels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<i64>,
    /// Height in pixels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<i64>,
    /// Whether the window was maximised.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maximized: Option<bool>,
    /// Whether the window was full-screen.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fullscreen: Option<bool>,
}

/// One OS window: its tabs (groups), the active tab index, and optional bounds.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowSpec {
    /// Window title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Index of the active tab.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<u32>,
    /// Saved geometry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bounds: Option<WindowBounds>,
    /// The tabs.
    #[serde(default)]
    pub groups: Vec<GroupSpec>,
}

/// The top-level workspace file. Panes may be described at any nesting level
/// (`panes` / `groups` / `windows`); all three slots are optional.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceFile {
    /// Display name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Layout name for the `panes` shorthand.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layout: Option<String>,
    /// One tab's worth of panes, the simplest shape a workspace file can have.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub panes: Option<Vec<PaneSpec>>,
    /// One window's worth of tabs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub groups: Option<Vec<GroupSpec>>,
    /// Index of the active tab.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<u32>,
    /// The full shape: every window, each with its own tabs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub windows: Option<Vec<WindowSpec>>,
}

/// The versioned on-disk container. Field declaration order is the canonical file order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceEnvelope {
    /// Always [`ENVELOPE_FORMAT`].
    pub format: String,
    /// Always [`ENVELOPE_VERSION`] on write.
    pub version: u32,
    /// The payload.
    pub workspace: WorkspaceFile,
}

impl WorkspaceEnvelope {
    /// Wrap a workspace payload in the current-version envelope.
    pub fn wrap(workspace: WorkspaceFile) -> Self {
        WorkspaceEnvelope {
            format: ENVELOPE_FORMAT.to_string(),
            version: ENVELOPE_VERSION,
            workspace,
        }
    }
}

/// One member of a set: a reference to a workspace file, not an inline copy of it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetMember {
    /// Path to the member workspace file, absolute or relative to the set file.
    pub path: String,
    /// Display name; absent falls back to the workspace's own name, then the file stem.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// A named collection of workspace references.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSet {
    /// Human name of the set.
    pub name: String,
    /// The member workspaces, in the order they should be opened.
    #[serde(default)]
    pub members: Vec<SetMember>,
}

/// Whether text is one of *our* versioned files — it carries a `format` discriminator —
/// rather than a bare object that has to be judged on its contents.
///
/// Every field of a workspace is optional, which is what lets a legacy file be as small as
/// `{"panes":[…]}`; the price is that `{"name":"x","dependencies":{}}` also parses. A
/// caller that has to tell a workspace from every other `.json` in a repository needs this
/// to know whether the file said whose it was.
pub fn claims_envelope(raw: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|v| v.as_object().map(|o| o.contains_key("format")))
        .unwrap_or(false)
}

/// Parse workspace-file text, accepting both shapes: the versioned envelope and the bare
/// legacy object ("version 0"). A bare object is never mistaken for an envelope — the
/// legacy schema has no `format` field.
pub fn parse_workspace(raw: &str) -> Result<WorkspaceFile, String> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| format!("invalid JSON: {e}"))?;
    let Some(obj) = value.as_object() else {
        return Err("a workspace file must be a JSON object".to_string());
    };
    if !obj.contains_key("format") {
        return serde_json::from_value(value).map_err(|e| format!("invalid workspace: {e}"));
    }
    check_envelope(obj, &[ENVELOPE_FORMAT], ENVELOPE_VERSION, "workspace")?;
    let payload = obj
        .get("workspace")
        .cloned()
        .ok_or_else(|| "avada workspace is missing the \"workspace\" payload".to_string())?;
    serde_json::from_value(payload).map_err(|e| format!("invalid workspace payload: {e}"))
}

/// Parse set-file text, accepting the versioned envelope, the pre-rename `format`, and a
/// bare legacy object. Mirrors [`parse_workspace`] one for one.
pub fn parse_set(raw: &str) -> Result<WorkspaceSet, String> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| format!("invalid JSON: {e}"))?;
    let Some(obj) = value.as_object() else {
        return Err("a set file must be a JSON object".to_string());
    };
    if !obj.contains_key("format") {
        return serde_json::from_value(value).map_err(|e| format!("invalid workspace set: {e}"));
    }
    check_envelope(
        obj,
        &[SET_FORMAT, LEGACY_SET_FORMAT],
        SET_VERSION,
        "workspace set",
    )?;
    let payload = obj
        .get("set")
        .cloned()
        .ok_or_else(|| "avada workspace set is missing the \"set\" payload".to_string())?;
    serde_json::from_value(payload).map_err(|e| format!("invalid workspace set payload: {e}"))
}

/// The half of envelope parsing the two formats share: the `format` discriminator and a
/// `version` this build is not too old to read.
fn check_envelope(
    obj: &serde_json::Map<String, serde_json::Value>,
    formats: &[&str],
    max: u32,
    what: &str,
) -> Result<(), String> {
    match obj.get("format").and_then(|f| f.as_str()) {
        Some(f) if formats.contains(&f) => {}
        other => {
            return Err(format!(
                "not a avada {what}: \"format\" is {:?}, expected \"{}\"",
                other.unwrap_or("<non-string>"),
                formats[0]
            ))
        }
    }
    match obj.get("version").and_then(serde_json::Value::as_u64) {
        Some(v) if (1..=max as u64).contains(&v) => Ok(()),
        Some(v) => Err(format!(
            "{what} version {v} is newer than this build understands (max {max}) — \
             update avada to open it"
        )),
        None => Err(format!(
            "avada {what} is missing a numeric \"version\" field"
        )),
    }
}

/// Serialise a workspace back into envelope text, exactly as the host writes it: the
/// current envelope, two-space pretty, one trailing newline.
pub fn to_envelope_text(file: &WorkspaceFile) -> Result<String, String> {
    let mut text = serde_json::to_string_pretty(&WorkspaceEnvelope::wrap(file.clone()))
        .map_err(|e| e.to_string())?;
    text.push('\n');
    Ok(text)
}

/// Normalise any workspace file into a flat list of windows. Precedence:
/// `windows` (verbatim, groupless dropped) → `groups` (one window) → `panes` (one window,
/// one tab). Empty for contentless input. The host's `io::windows_of`, restated.
pub fn windows_of(file: &WorkspaceFile) -> Vec<WindowSpec> {
    if let Some(windows) = &file.windows {
        if !windows.is_empty() {
            return windows
                .iter()
                .filter(|w| !w.groups.is_empty())
                .cloned()
                .collect();
        }
    }
    if let Some(groups) = &file.groups {
        if !groups.is_empty() {
            return vec![WindowSpec {
                title: file.name.clone(),
                active: file.active,
                groups: groups.clone(),
                ..Default::default()
            }];
        }
    }
    if let Some(panes) = &file.panes {
        if !panes.is_empty() {
            return vec![WindowSpec {
                title: file.name.clone(),
                groups: vec![GroupSpec {
                    title: file.name.clone(),
                    layout: file.layout.clone(),
                    panes: panes.clone(),
                    ..Default::default()
                }],
                ..Default::default()
            }];
        }
    }
    Vec::new()
}

/// Every pane in the file, in window/tab/slot order.
pub fn pane_count(file: &WorkspaceFile) -> usize {
    windows_of(file)
        .iter()
        .flat_map(|w| w.groups.iter())
        .map(|g| g.panes.len())
        .sum()
}

/// Address of one pane inside a workspace file, in the shape the *file* is written in
/// rather than the normalised shape [`windows_of`] produces.
///
/// Editing has to name a pane in the original tree or a write would silently rewrite a
/// `panes`-shorthand file into the full three-level form, which is a much larger change
/// than the human asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneAddr {
    /// Index into `windows`, or `None` when the file has no `windows`.
    pub window: Option<usize>,
    /// Index into `groups`, or `None` when the file is the `panes` shorthand.
    pub group: Option<usize>,
    /// Index into the pane list.
    pub pane: usize,
}

/// The pane at `addr`, borrowed mutably, or `None` when the address does not exist.
pub fn pane_at(file: &mut WorkspaceFile, addr: PaneAddr) -> Option<&mut PaneSpec> {
    let groups: &mut Vec<GroupSpec> = match addr.window {
        Some(w) => &mut file.windows.as_mut()?.get_mut(w)?.groups,
        None => match addr.group {
            Some(_) => file.groups.as_mut()?,
            None => return file.panes.as_mut()?.get_mut(addr.pane),
        },
    };
    let g = addr.group?;
    groups.get_mut(g)?.panes.get_mut(addr.pane)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENVELOPE: &str = r#"{
  "format": "avada",
  "version": 1,
  "workspace": {
    "name": "demo",
    "windows": [
      {
        "title": "left",
        "groups": [
          {
            "title": "build",
            "panes": [
              {
                "label": "cargo",
                "command": "cargo watch",
                "note": "chasing the pty resize race"
              }
            ]
          }
        ]
      }
    ]
  }
}"#;

    #[test]
    fn the_pretty_round_trip_is_byte_identical() {
        // The contract the host states in `workspace/model.rs`: parse, re-serialise, and
        // the bytes must come back. This is what stops a module from quietly reordering
        // or dropping a field in a file a human also edits by hand.
        let parsed: WorkspaceEnvelope = serde_json::from_str(ENVELOPE).unwrap();
        assert_eq!(serde_json::to_string_pretty(&parsed).unwrap(), ENVELOPE);
    }

    #[test]
    fn a_bare_object_reads_as_version_zero() {
        let ws = parse_workspace(r#"{"name":"old","panes":[{"command":"vim"}]}"#).unwrap();
        assert_eq!(ws.name.as_deref(), Some("old"));
        assert_eq!(pane_count(&ws), 1);
    }

    #[test]
    fn an_envelope_from_a_newer_build_says_so_rather_than_half_loading() {
        let e = parse_workspace(r#"{"format":"avada","version":9,"workspace":{}}"#).unwrap_err();
        assert!(e.contains("newer than this build"), "{e}");
        let e = parse_workspace(r#"{"format":"tmux","version":1,"workspace":{}}"#).unwrap_err();
        assert!(e.contains("expected \"avada\""), "{e}");
        let e = parse_workspace(r#"{"format":"avada","workspace":{}}"#).unwrap_err();
        assert!(e.contains("numeric \"version\""), "{e}");
    }

    #[test]
    fn a_set_reads_in_both_the_new_and_the_pre_rename_format() {
        for format in [SET_FORMAT, LEGACY_SET_FORMAT] {
            let raw = format!(
                r#"{{"format":"{format}","version":1,"set":{{"name":"Work","members":[{{"path":"a.avada"}}]}}}}"#
            );
            let set = parse_set(&raw).unwrap();
            assert_eq!(set.name, "Work");
            assert_eq!(set.members[0].path, "a.avada");
        }
        // And a workspace is not a set, however well-formed it is.
        assert!(parse_set(r#"{"format":"avada","version":1,"workspace":{}}"#).is_err());
    }

    #[test]
    fn the_three_shapes_all_normalise_to_windows() {
        let panes = parse_workspace(r#"{"name":"p","panes":[{"command":"a"}]}"#).unwrap();
        let groups =
            parse_workspace(r#"{"name":"g","groups":[{"panes":[{"command":"a"}]}]}"#).unwrap();
        for file in [&panes, &groups] {
            let w = windows_of(file);
            assert_eq!(w.len(), 1);
            assert_eq!(w[0].groups.len(), 1);
            assert_eq!(w[0].groups[0].panes.len(), 1);
        }
        // A window with no tabs describes nothing and is dropped rather than drawn empty.
        let empty = parse_workspace(r#"{"windows":[{"title":"x","groups":[]}]}"#).unwrap();
        assert!(windows_of(&empty).is_empty());
        assert_eq!(pane_count(&empty), 0);
    }

    #[test]
    fn a_pane_is_addressed_in_the_shape_the_file_was_written_in() {
        let mut short = parse_workspace(r#"{"panes":[{"command":"a"},{"command":"b"}]}"#).unwrap();
        let addr = PaneAddr {
            window: None,
            group: None,
            pane: 1,
        };
        pane_at(&mut short, addr).unwrap().note = Some("second".into());
        // The shorthand stayed a shorthand: no `windows` key appeared.
        assert!(short.windows.is_none() && short.groups.is_none());
        assert_eq!(
            short.panes.as_ref().unwrap()[1].note.as_deref(),
            Some("second")
        );

        // And an address that names nothing is `None`, not a panic.
        assert!(pane_at(
            &mut short,
            PaneAddr {
                window: Some(0),
                group: Some(0),
                pane: 0
            }
        )
        .is_none());
    }

    #[test]
    fn a_pane_shows_its_label_then_its_command_then_its_kind() {
        let mut p = PaneSpec::default();
        assert_eq!(p.title(), "Terminal");
        p.meta = Some(BTreeMap::from([(
            META_KIND_KEY.to_string(),
            "claude".to_string(),
        )]));
        assert_eq!(p.kind(), "claude");
        assert_eq!(p.title(), "claude");
        p.command = Some("  cargo test  ".into());
        assert_eq!(p.title(), "cargo test");
        p.label = Some("Tests".into());
        assert_eq!(p.title(), "Tests");
    }

    #[test]
    fn writing_produces_exactly_what_the_host_writes() {
        let ws = parse_workspace(ENVELOPE).unwrap();
        let text = to_envelope_text(&ws).unwrap();
        assert_eq!(text, format!("{ENVELOPE}\n"));
    }
}
