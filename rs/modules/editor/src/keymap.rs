//! The actions this editor understands, and the three keymaps that reach them.
//!
//! The host owns keymap resolution: it takes the presets declared here, applies whatever
//! the user rebound, and delivers a [`GridKey`](avada_module_sdk::grid::GridKey) whose
//! `action` is already the name of one of [`ACTIONS`]. That is why a preset is data and
//! not code, and why the module never sees a chord it did not declare.
//!
//! # One chord per action
//!
//! `KeymapPreset::bindings` is a map from one chord to one action, so a *sequence* — `gg`,
//! `dd`, `ciw` — cannot be expressed. The modal presets below are therefore honest
//! single-chord approximations of Helix and Vim, not emulations of them; where a real
//! sequence exists the closest single chord takes its place and the README says so.

use avada_module_sdk::grid::{DeclareKeymap, GridAction, KeymapPreset};
use std::collections::BTreeMap;

/// The grid surface this module paints into. One surface, however many buffers are open.
pub const SURFACE: &str = "editor";

/// Every action, with the label the host shows on its keybindings page.
///
/// The names are namespaced by what they do rather than by which editor they came from:
/// a user rebinding `edit.delete.line` should not have to know it is spelled `dd` in one
/// tradition and `x` in another.
pub const ACTIONS: &[(&str, &str)] = &[
    ("mode.insert", "Insert before the cursor"),
    ("mode.insert.line.start", "Insert at the start of the line"),
    ("mode.append", "Insert after the cursor"),
    ("mode.append.line.end", "Insert at the end of the line"),
    ("mode.normal", "Leave insert mode"),
    ("move.left", "Left"),
    ("move.right", "Right"),
    ("move.up", "Up"),
    ("move.down", "Down"),
    ("move.word.next", "Next word"),
    ("move.word.prev", "Previous word"),
    ("move.word.end", "End of word"),
    ("move.line.start", "Start of line"),
    ("move.line.end", "End of line"),
    ("move.doc.start", "Start of file"),
    ("move.doc.end", "End of file"),
    ("move.page.up", "Page up"),
    ("move.page.down", "Page down"),
    ("edit.delete", "Delete the character"),
    ("edit.delete.back", "Delete backwards"),
    ("edit.delete.line", "Delete the line"),
    ("edit.newline", "New line"),
    ("edit.open.below", "Open a line below"),
    ("edit.open.above", "Open a line above"),
    ("edit.undo", "Undo"),
    ("edit.redo", "Redo"),
    ("file.save", "Save"),
    ("buffer.next", "Next buffer"),
    ("buffer.prev", "Previous buffer"),
];

/// Whether `id` is one of [`ACTIONS`]. Used by the tests, and by nothing else: an action
/// the module does not know simply falls through the match in [`crate::edit`]. In a
/// binary crate `pub` exempts nothing from `dead_code` — there is no outside to link it.
#[cfg(test)]
pub fn is_action(id: &str) -> bool {
    ACTIONS.iter().any(|(a, _)| *a == id)
}

fn preset(name: &str, label: &str, pairs: &[(&str, &str)]) -> KeymapPreset {
    KeymapPreset {
        name: name.into(),
        label: label.into(),
        bindings: pairs
            .iter()
            .map(|(chord, action)| ((*chord).to_string(), (*action).to_string()))
            .collect::<BTreeMap<_, _>>(),
    }
}

/// Bindings every preset shares: buffer switching, which no tradition has an opinion
/// about, and which must not collide with a letter in the modal presets.
const COMMON: &[(&str, &str)] = &[
    ("alt+arrowleft", "buffer.prev"),
    ("alt+arrowright", "buffer.next"),
    ("ctrl+s", "file.save"),
];

/// Helix: `hjkl`, `w`/`b`/`e`, `x` deletes a line, `d` deletes a character, `U` redoes.
///
/// Helix has no `0`/`$`; it uses Home and End, and so does this. `g` stands in for `gg`.
const HELIX: &[(&str, &str)] = &[
    ("i", "mode.insert"),
    ("I", "mode.insert.line.start"),
    ("a", "mode.append"),
    ("A", "mode.append.line.end"),
    ("escape", "mode.normal"),
    ("h", "move.left"),
    ("l", "move.right"),
    ("k", "move.up"),
    ("j", "move.down"),
    ("w", "move.word.next"),
    ("b", "move.word.prev"),
    ("e", "move.word.end"),
    ("home", "move.line.start"),
    ("end", "move.line.end"),
    ("g", "move.doc.start"),
    ("G", "move.doc.end"),
    ("ctrl+u", "move.page.up"),
    ("ctrl+d", "move.page.down"),
    ("d", "edit.delete"),
    ("x", "edit.delete.line"),
    ("o", "edit.open.below"),
    ("O", "edit.open.above"),
    ("u", "edit.undo"),
    ("U", "edit.redo"),
];

/// Vim: the same skeleton, with Vim's own answers where the two differ — `0`/`$` for the
/// line ends, `x` for a character, `d` for a line, `ctrl+r` for redo.
const VIM: &[(&str, &str)] = &[
    ("i", "mode.insert"),
    ("I", "mode.insert.line.start"),
    ("a", "mode.append"),
    ("A", "mode.append.line.end"),
    ("escape", "mode.normal"),
    ("h", "move.left"),
    ("l", "move.right"),
    ("k", "move.up"),
    ("j", "move.down"),
    ("w", "move.word.next"),
    ("b", "move.word.prev"),
    ("e", "move.word.end"),
    ("0", "move.line.start"),
    ("$", "move.line.end"),
    ("g", "move.doc.start"),
    ("G", "move.doc.end"),
    ("ctrl+u", "move.page.up"),
    ("ctrl+d", "move.page.down"),
    ("x", "edit.delete"),
    ("d", "edit.delete.line"),
    ("o", "edit.open.below"),
    ("O", "edit.open.above"),
    ("u", "edit.undo"),
    ("ctrl+r", "edit.redo"),
];

/// The modeless preset: no letter is bound, so every letter arrives as text and is typed.
///
/// Nothing here binds `mode.normal`, because there is no way back out of a mode you never
/// entered — a `basic` user sets the `start_mode` preference to `insert` once and never
/// thinks about modes again.
const BASIC: &[(&str, &str)] = &[
    ("arrowleft", "move.left"),
    ("arrowright", "move.right"),
    ("arrowup", "move.up"),
    ("arrowdown", "move.down"),
    ("ctrl+arrowright", "move.word.next"),
    ("ctrl+arrowleft", "move.word.prev"),
    ("home", "move.line.start"),
    ("end", "move.line.end"),
    ("ctrl+home", "move.doc.start"),
    ("ctrl+end", "move.doc.end"),
    ("pageup", "move.page.up"),
    ("pagedown", "move.page.down"),
    ("delete", "edit.delete"),
    ("backspace", "edit.delete.back"),
    ("enter", "edit.newline"),
    ("ctrl+shift+k", "edit.delete.line"),
    ("ctrl+z", "edit.undo"),
    ("ctrl+shift+z", "edit.redo"),
];

/// Insert mode's own keys, which every preset needs and none of them should have to spell.
///
/// A modal preset binds these too: in insert mode a named key carries no `text`, so the
/// action is the only thing that can move the caret or split the line.
const INSERT_KEYS: &[(&str, &str)] = &[
    ("arrowleft", "move.left"),
    ("arrowright", "move.right"),
    ("arrowup", "move.up"),
    ("arrowdown", "move.down"),
    ("enter", "edit.newline"),
    ("backspace", "edit.delete.back"),
    ("delete", "edit.delete"),
];

/// The whole keymap declaration, ready for `host.keymap.declare`.
pub fn declare() -> DeclareKeymap {
    let build = |name: &str, label: &str, keys: &[(&str, &str)]| {
        // Order matters: a preset's own binding must win over the shared defaults, so the
        // generic sets go in first and the preset overwrites them.
        let mut pairs: Vec<(&str, &str)> = INSERT_KEYS.to_vec();
        pairs.extend_from_slice(COMMON);
        pairs.extend_from_slice(keys);
        preset(name, label, &pairs)
    };
    DeclareKeymap {
        surface: SURFACE.into(),
        actions: ACTIONS
            .iter()
            .map(|(id, label)| GridAction {
                id: (*id).to_string(),
                label: (*label).to_string(),
            })
            .collect(),
        presets: vec![
            build("helix", "Helix", HELIX),
            build("vim", "Vim", VIM),
            build("basic", "Modeless", BASIC),
        ],
        default_preset: Some("helix".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_binding_names_an_action_that_exists() {
        for p in declare().presets {
            for (chord, action) in &p.bindings {
                assert!(is_action(action), "{}: {chord} -> unknown {action}", p.name);
            }
        }
    }

    #[test]
    fn the_modeless_preset_leaves_every_letter_to_the_typist() {
        let d = declare();
        let basic = d.preset("basic").expect("basic preset");
        for chord in basic.bindings.keys() {
            let plain_letter =
                chord.chars().count() == 1 && chord.chars().all(|c| c.is_alphanumeric());
            assert!(!plain_letter, "modeless preset stole the letter {chord}");
        }
        // ...and it deliberately offers no way back to normal mode.
        assert!(!basic.bindings.values().any(|a| a == "mode.normal"));
    }

    #[test]
    fn a_preset_binding_beats_the_shared_default() {
        let d = declare();
        // `delete` is `edit.delete` for everyone, but Helix spends `d` on a character and
        // `x` on a line, and Vim spends them the other way round.
        assert_eq!(d.preset("helix").unwrap().bindings["d"], "edit.delete");
        assert_eq!(d.preset("helix").unwrap().bindings["x"], "edit.delete.line");
        assert_eq!(d.preset("vim").unwrap().bindings["x"], "edit.delete");
        assert_eq!(d.preset("vim").unwrap().bindings["d"], "edit.delete.line");
    }

    #[test]
    fn insert_mode_can_always_move_split_and_erase() {
        let d = declare();
        for name in ["helix", "vim", "basic"] {
            let p = d.preset(name).unwrap();
            for (chord, action) in INSERT_KEYS {
                assert_eq!(
                    p.bindings.get(*chord).map(String::as_str),
                    Some(*action),
                    "{name}/{chord}"
                );
            }
            assert_eq!(p.bindings["ctrl+s"], "file.save", "{name}");
        }
    }

    #[test]
    fn helix_is_the_default_and_every_action_is_reachable_from_it() {
        let d = declare();
        assert_eq!(d.default_preset().map(|p| p.name.as_str()), Some("helix"));
        // Not every action needs a chord in every dialect, but an action no preset can
        // reach is dead code wearing a label. `edit.delete.back` is Insert-only in the
        // modal presets, which INSERT_KEYS supplies.
        let reachable: Vec<&str> = ACTIONS
            .iter()
            .map(|(id, _)| *id)
            .filter(|id| {
                d.presets
                    .iter()
                    .any(|p| p.bindings.values().any(|a| a == id))
            })
            .collect();
        assert_eq!(
            reachable.len(),
            ACTIONS.len(),
            "unreachable: {:?}",
            ACTIONS
                .iter()
                .map(|(i, _)| *i)
                .filter(|i| !reachable.contains(i))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn every_binding_in_every_preset_names_an_action_the_editor_declares() {
        // A chord bound to a misspelled action is silent: it falls through the match in
        // `crate::edit` and the key simply does nothing. This is the only thing that
        // would catch it, since the host resolves chords without knowing the action list.
        let d = declare();
        let declared: Vec<&str> = d.actions.iter().map(|a| a.id.as_str()).collect();
        for preset in &d.presets {
            for (chord, action) in &preset.bindings {
                assert!(
                    is_action(action),
                    "{} binds {chord} to `{action}`, which is not a declared action",
                    preset.name
                );
                assert!(declared.contains(&action.as_str()));
            }
        }
    }
}
