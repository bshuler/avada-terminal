//! The cell-grid surface a module paints an editor into (UI tier 5).
//!
//! Tier 1 gives a module a list of rows; tier 5 gives it a rectangle of character cells
//! and the keystrokes that land in it. That is the whole difference, and it is the
//! difference between a file browser and an editor: rows have no columns, no cursor and
//! no notion of a keystroke belonging to them.
//!
//! **The module owns the text; the host owns the pixels.** A module holds the rope, the
//! selections and the syntax tree, and sends whole frames of already-laid-out lines; the
//! host draws them in a monospace font and never asks what they mean. Nothing is
//! incremental: a frame replaces a frame, the same way [`crate::rail::SetRows`] replaces
//! rows. Damage tracking would need the two sides to agree on what changed, and the two
//! sides are separate processes that can restart independently — a full frame is the only
//! statement that is true no matter what the other side missed.
//!
//! **Keys are resolved before they arrive.** The host turns a chord into an action id
//! using the preset the user chose plus their per-binding overrides (see
//! [`DeclareKeymap`]), so the module never parses a chord and the user edits editor
//! bindings in the same keymap file as everything else. A chord that no binding claims
//! still arrives, carrying its text if it produced any, because "insert what was typed"
//! is not a binding anyone should have to write out.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One frame of a grid surface: `host.grid.set` params.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GridFrame {
    /// The surface id — the same contribution id [`crate::rail::SetRows::entry`] uses, and
    /// the `surface` the pane was spawned with.
    pub surface: String,
    /// Width the module laid the frame out at. The host does not re-wrap; if this
    /// disagrees with the pane's current width the frame is simply drawn as sent, and the
    /// module will get a [`GridResize`] telling it the truth.
    pub cols: u16,
    /// Height, in the same spirit as [`GridFrame::cols`].
    pub rows: u16,
    /// The lines, top to bottom. Fewer than `rows` means the rest is blank; more is
    /// clipped, because a module that overruns should not be able to push the pane's
    /// own furniture off screen.
    #[serde(default)]
    pub lines: Vec<GridLine>,
    /// Where the caret sits, or `None` when the surface has no caret to show (a preview,
    /// a pane that lost focus).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<GridCursor>,
    /// One line of the module's own status text, drawn in the pane's footer. Empty means
    /// no footer at all rather than an empty one.
    #[serde(default)]
    pub status: String,
}

/// One line of a frame, as styled runs rather than cells.
///
/// A run, not a cell, is the unit on the wire because a cell-per-cell frame is mostly
/// repetition — a line of source is a handful of colours — and because the host draws
/// runs: one text element per run is the difference between a redraw the window thread
/// can afford and one it cannot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GridLine {
    /// Left to right; the first span starts at column zero and each one begins where the
    /// last ended.
    #[serde(default)]
    pub spans: Vec<GridSpan>,
}

impl GridLine {
    /// The line's text with the styling dropped. What a test asserts on, and what the
    /// host measures when it needs the line's width in cells.
    pub fn text(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }
}

/// A run of cells sharing one style.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GridSpan {
    /// The characters. One `char` is one cell; the host does not do wide-character
    /// arithmetic in contract 1, so a module that renders CJK lays it out itself.
    pub text: String,
    /// Foreground: either `#rrggbb` or one of the host's theme role names (`fg`,
    /// `subtext`, `accent`, `red`, …). A name the host does not know falls back to the
    /// pane's default ink rather than to black — an unreadable frame is worse than an
    /// unstyled one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fg: Option<String>,
    /// Background, resolved like [`GridSpan::fg`]; absent means the pane's background.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bg: Option<String>,
    /// Bold weight.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bold: bool,
    /// Italic slant.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub italic: bool,
    /// Underline.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub underline: bool,
}

impl GridSpan {
    /// An unstyled run. The overwhelmingly common case, so it is worth not spelling five
    /// defaults at every call site.
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ..Self::default()
        }
    }

    /// A run in one colour.
    pub fn fg(text: impl Into<String>, fg: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            fg: Some(fg.into()),
            ..Self::default()
        }
    }
}

/// The caret's position and shape within a frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GridCursor {
    /// Row within the frame, zero-based.
    pub line: u16,
    /// Column within the frame, zero-based and counted in cells.
    pub col: u16,
    /// What to draw. Modal editors mean this, which is why it is on the wire at all: the
    /// shape *is* the mode indicator, and only the module knows the mode.
    #[serde(default)]
    pub shape: CursorShape,
}

/// How the caret is drawn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CursorShape {
    /// A filled cell. The default because a modal editor's normal mode is where it starts.
    #[default]
    Block,
    /// A thin vertical bar between cells: insert.
    Bar,
    /// A line under the cell: replace.
    Underline,
}

/// `module.grid.key` params: one keystroke that landed in a focused grid surface.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GridKey {
    /// Which surface had focus.
    pub surface: String,
    /// The chord in the host's spelling (`ctrl+s`, `esc`, `shift+tab`, `a`). Sent even
    /// when it resolved to an action, so a module can log or fall back on the raw chord.
    pub key: String,
    /// The action id the chord resolved to, from the module's own
    /// [`DeclareKeymap::actions`]. `None` when no binding claimed the chord.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// The text the keystroke would insert, when it is a plain printable one. `None` for
    /// chords with a modifier and for named keys, so a module can insert `text` blindly
    /// without also inserting a tab or a `ctrl+s`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// `module.grid.resize` params: the pane's usable size, in cells.
///
/// Sent when the pane is created, resized, or its font size changes, and never as a
/// question — the module re-lays out and sends a frame when it is ready, or does not.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GridResize {
    /// Which surface.
    #[serde(default)]
    pub surface: String,
    /// Usable width in cells.
    pub cols: u16,
    /// Usable height in cells.
    pub rows: u16,
}

/// One thing a grid surface can be asked to do, named so a keymap can bind to it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GridAction {
    /// Stable within the module (`move_line_down`). The host namespaces it under the
    /// module id when it writes it into the keymap file, so two editors may share a name.
    pub id: String,
    /// What the keymap page calls it.
    #[serde(default)]
    pub label: String,
}

/// A named set of bindings the module ships and the user can choose between.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeymapPreset {
    /// Stable within the module (`helix`, `vim`, `emacs`).
    pub name: String,
    /// What the picker calls it.
    #[serde(default)]
    pub label: String,
    /// Chord → action id. Chords are in the host's spelling, the same one
    /// [`GridKey::key`] uses.
    #[serde(default)]
    pub bindings: BTreeMap<String, String>,
}

/// `host.keymap.declare` params.
///
/// A module declares what it can do and how it would like that bound; the *user's* keymap
/// file has the last word, one binding at a time, under the module's id. That split is the
/// whole point of the tier: an editor ships a Helix preset and a Vim preset without
/// anybody having to choose for the user, and a user who wants one key different does not
/// have to fork a preset to get it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclareKeymap {
    /// Which surface these bindings apply to. They are live only while a pane showing it
    /// has focus.
    pub surface: String,
    /// Every action the surface answers to. An override naming an action absent from here
    /// is kept but never fires — the same rule the keymap loader already follows for
    /// unknown ids, so uninstalling a module does not silently eat the user's line.
    #[serde(default)]
    pub actions: Vec<GridAction>,
    /// The presets, in the order the picker should show them.
    #[serde(default)]
    pub presets: Vec<KeymapPreset>,
    /// Which preset applies when the user has not chosen. `None`, or a name that is not in
    /// `presets`, means the first one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_preset: Option<String>,
}

impl DeclareKeymap {
    /// The preset the user gets before choosing: `default_preset` when it names one that
    /// exists, else the first declared, else nothing.
    pub fn default_preset(&self) -> Option<&KeymapPreset> {
        self.default_preset
            .as_deref()
            .and_then(|want| self.presets.iter().find(|p| p.name == want))
            .or_else(|| self.presets.first())
    }

    /// Look a preset up by name.
    pub fn preset(&self, name: &str) -> Option<&KeymapPreset> {
        self.presets.iter().find(|p| p.name == name)
    }

    /// Whether `id` is one of the declared actions.
    pub fn has_action(&self, id: &str) -> bool {
        self.actions.iter().any(|a| a.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_frame_round_trips_and_omits_what_it_does_not_use() {
        let frame = GridFrame {
            surface: "editor".into(),
            cols: 80,
            rows: 24,
            lines: vec![GridLine {
                spans: vec![GridSpan::fg("fn", "accent"), GridSpan::plain(" main()")],
            }],
            cursor: Some(GridCursor {
                line: 0,
                col: 3,
                shape: CursorShape::Bar,
            }),
            status: "1:4".into(),
        };
        let wire = serde_json::to_value(&frame).unwrap();
        assert_eq!(
            wire["lines"][0]["spans"][1],
            json!({ "text": " main()" }),
            "an unstyled run carries nothing but its text: {wire}"
        );
        assert_eq!(wire["cursor"]["shape"], "bar");
        assert_eq!(
            serde_json::from_value::<GridFrame>(wire).unwrap(),
            frame,
            "and everything that was dropped had a default that restores it"
        );
    }

    /// The host draws lines, but the tests — and any module that wants to check its own
    /// work — read them.
    #[test]
    fn a_line_reads_back_as_its_text() {
        let line = GridLine {
            spans: vec![
                GridSpan::fg("let", "accent"),
                GridSpan::plain(" x = "),
                GridSpan::fg("1", "green"),
            ],
        };
        assert_eq!(line.text(), "let x = 1");
        assert_eq!(GridLine::default().text(), "");
    }

    /// A cursorless frame and a caret at the origin are different facts, and `skip` on an
    /// `Option` is what keeps them different on the wire.
    #[test]
    fn no_cursor_is_not_a_cursor_at_zero() {
        let blind = GridFrame {
            surface: "editor".into(),
            ..GridFrame::default()
        };
        let wire = serde_json::to_value(&blind).unwrap();
        assert!(wire.get("cursor").is_none(), "{wire}");
        assert_eq!(
            serde_json::from_value::<GridFrame>(wire).unwrap().cursor,
            None
        );
    }

    #[test]
    fn a_key_with_no_binding_still_carries_what_it_would_type() {
        let typed: GridKey = serde_json::from_value(json!({
            "surface": "editor", "key": "a", "text": "a"
        }))
        .unwrap();
        assert_eq!(typed.action, None);
        assert_eq!(typed.text.as_deref(), Some("a"));

        let bound: GridKey = serde_json::from_value(json!({
            "surface": "editor", "key": "ctrl+s", "action": "save"
        }))
        .unwrap();
        assert_eq!(bound.action.as_deref(), Some("save"));
        assert_eq!(
            bound.text, None,
            "a modifier chord types nothing, or every ctrl+s would also insert an s"
        );
    }

    #[test]
    fn the_default_preset_is_named_or_first_and_never_missing_by_surprise() {
        let mut km = DeclareKeymap {
            surface: "editor".into(),
            actions: vec![GridAction {
                id: "save".into(),
                label: "Save".into(),
            }],
            presets: vec![
                KeymapPreset {
                    name: "helix".into(),
                    ..KeymapPreset::default()
                },
                KeymapPreset {
                    name: "vim".into(),
                    ..KeymapPreset::default()
                },
            ],
            default_preset: None,
        };
        assert_eq!(km.default_preset().map(|p| p.name.as_str()), Some("helix"));
        km.default_preset = Some("vim".into());
        assert_eq!(km.default_preset().map(|p| p.name.as_str()), Some("vim"));
        km.default_preset = Some("nano".into());
        assert_eq!(
            km.default_preset().map(|p| p.name.as_str()),
            Some("helix"),
            "a name nobody declared falls back rather than leaving the surface unbound"
        );
        assert!(km.has_action("save"));
        assert!(!km.has_action("quit"));
        assert!(km.preset("vim").is_some());
        assert!(km.preset("nano").is_none());

        km.presets.clear();
        assert!(km.default_preset().is_none());
    }
}
