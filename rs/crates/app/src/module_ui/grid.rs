//! The cell grid a tier-5 module paints into (contract `host.grid.set`), the keymap it
//! declares for that surface (`host.keymap.declare`), and the chord → action resolution
//! that turns a keystroke into the `module.grid.key` notification the module receives.
//!
//! The split is the whole point of tier 5: **the module owns the text, the host owns the
//! pixels, and the host owns the keyboard**. A module never sees a raw chord it did not ask
//! for — it declares the actions it understands plus one or more presets binding chords to
//! them, and the host resolves every keystroke against the user's chosen preset and the
//! user's per-action overrides before sending it on. That is what makes an editor module
//! rebindable in Preferences without the module knowing Preferences exists.
//!
//! Thread-local, exactly like [`super::rows`]: this is window-thread UI state, and the
//! fold-in, the projection and the key router all already run there.

use avada_core::module::grid::{DeclareKeymap, GridFrame, GridKey};
use avada_core::rights::ModuleId;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};

/// One surface's last frame plus the counter the pane's cache watches. Same shape and same
/// reason as [`super::rows::Surface`]: a frame is not a file, so there is no mtime.
#[derive(Default)]
struct Surface {
    frame: GridFrame,
    revision: u64,
}

/// The user's choices over a module's declared keymap: which preset a surface uses, and
/// which chord an individual action was rebound to.
///
/// Overrides are keyed `<owner/repo>.<action>` — namespaced by module, because two editors
/// both declaring `move.left` are two different actions, and an override set for one must
/// not silently rebind the other.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct KeymapPrefs {
    /// `<owner/repo>#<surface>` → preset name. Absent means the module's own default.
    #[serde(default)]
    presets: BTreeMap<String, String>,
    /// `<owner/repo>.<action>` → chord. A `None` value is an explicit unbind, matching the
    /// shape [`crate::keybindings::Keymap`] already persists for the app's own bindings.
    #[serde(default)]
    bindings: BTreeMap<String, Option<String>>,
}

thread_local! {
    static SURFACES: RefCell<HashMap<String, Surface>> = RefCell::new(HashMap::new());
    static KEYMAPS: RefCell<HashMap<String, DeclareKeymap>> = RefCell::new(HashMap::new());
    static PREFS: RefCell<KeymapPrefs> = RefCell::new(KeymapPrefs::default());
}

/// The store key, the same `<owner/repo>#<id>` shape the rail and the row store use.
fn key(module: &ModuleId, surface: &str) -> String {
    crate::leftpanel::entry_key(module, surface)
}

/// The override key for one action of one module.
fn action_key(module: &ModuleId, action: &str) -> String {
    format!("{}.{action}", module.as_str())
}

#[cfg(test)]
thread_local! {
    /// Where this thread reads and writes its prefs file. The stores above are already
    /// thread-local, so the file has to be too: a test that exercises persistence must not
    /// write into the real user's config directory, and two tests on two threads must not
    /// race over one path.
    static PREFS_PATH: RefCell<Option<std::path::PathBuf>> = const { RefCell::new(None) };
}

/// Point this thread's prefs file at `path` (whose parent must exist).
#[cfg(test)]
pub(crate) fn set_prefs_path(path: std::path::PathBuf) {
    PREFS_PATH.with(|p| *p.borrow_mut() = Some(path));
}

/// The file the user's presets and rebinds live in, beside the app's own keybindings.
fn prefs_path() -> std::path::PathBuf {
    #[cfg(test)]
    if let Some(p) = PREFS_PATH.with(|p| p.borrow().clone()) {
        return p;
    }
    avada_core::persistence::paths::user_data_dir().join("module-keymaps.json")
}

/// Replace `frame.surface`'s frame. Frames replace frames — the contract has no damage
/// tracking, because a module that restarted cannot know what the host still has on screen.
pub fn set_frame(module: &ModuleId, frame: GridFrame) {
    SURFACES.with(|s| {
        let mut s = s.borrow_mut();
        let e = s.entry(key(module, &frame.surface)).or_default();
        e.frame = frame;
        e.revision += 1;
    });
}

/// What `surface` last painted; `None` when it has never painted.
pub fn frame(module: &ModuleId, surface: &str) -> Option<GridFrame> {
    SURFACES.with(|s| {
        s.borrow()
            .get(&key(module, surface))
            .map(|e| e.frame.clone())
    })
}

/// How many frames `surface` has painted. Zero for a surface that never has.
pub fn generation(module: &ModuleId, surface: &str) -> u64 {
    SURFACES.with(|s| {
        s.borrow()
            .get(&key(module, surface))
            .map_or(0, |e| e.revision)
    })
}

/// Remember a surface's declared keymap. Kept even with no pane open: the declaration is
/// what the rebinding UI reads, so it has to outlive every window of the surface.
pub fn set_keymap(module: &ModuleId, keymap: DeclareKeymap) {
    KEYMAPS.with(|k| {
        k.borrow_mut().insert(key(module, &keymap.surface), keymap);
    });
}

/// `surface`'s declared keymap, if it ever declared one.
pub fn keymap(module: &ModuleId, surface: &str) -> Option<DeclareKeymap> {
    KEYMAPS.with(|k| k.borrow().get(&key(module, surface)).cloned())
}

/// Drop everything belonging to `module`. Called when the host says the module is gone: a
/// dead module's last frame must not keep sitting on screen as if it were live.
///
/// The frame is cleared rather than removed for the same reason [`super::rows::forget`]
/// clears: the pane is still open, and its projection only notices a *new* revision.
pub fn forget(module: &ModuleId) {
    let prefix = format!("{}#", module.as_str());
    SURFACES.with(|s| {
        let mut s = s.borrow_mut();
        for k in s
            .keys()
            .filter(|k| k.starts_with(&prefix))
            .cloned()
            .collect::<Vec<_>>()
        {
            if let Some(e) = s.get_mut(&k) {
                e.frame = GridFrame::default();
                e.revision += 1;
            }
        }
    });
    // The keymap goes too. It described a process that is no longer there, and a stale
    // declaration would let the rebinding UI offer actions nothing can perform.
    KEYMAPS.with(|k| k.borrow_mut().retain(|k, _| !k.starts_with(&prefix)));
}

/// Canonicalize a chord so the module's spelling and the host's spelling meet: modifiers
/// lowercased and ordered `ctrl+alt+shift`, the key lowercased and de-aliased.
///
/// Presets are written by module authors in whatever dialect their editor uses (`C-x`,
/// `Ctrl+X`, `ctrl+x`), and the host generates its own from key events. Comparing raw
/// strings would make a preset work or not work depending on how it was typed.
///
/// Two rules here are less obvious than they look, and both come from editor dialects:
///
/// * **The last part is always the key**, and only the parts before it can be modifiers.
///   Matching on the word alone would read the `s` in a bare `s` binding — one of the most
///   used keys in a modal editor — as an abbreviation for Shift and leave the chord with
///   no key at all.
/// * **A lone capital letter means Shift**, so Helix's `G` and the host's `shift+g` are one
///   chord. Only when nothing else modifies it: `Ctrl+S` is styling, not Ctrl+Shift+S.
pub fn normalize_chord(chord: &str) -> String {
    let parts: Vec<&str> = chord.split(['+', '-']).filter(|p| !p.is_empty()).collect();
    // A chord that *is* a separator (`+`, `-`) splits into nothing: it is its own key.
    let Some((key_raw, mods)) = parts.split_last() else {
        return alias(&chord.trim().to_ascii_lowercase());
    };
    let (mut ctrl, mut alt, mut shift) = (false, false, false);
    for part in mods {
        match part.to_ascii_lowercase().as_str() {
            "c" | "ctrl" | "control" | "cmd" | "command" | "super" | "meta" => ctrl = true,
            "a" | "alt" | "opt" | "option" => alt = true,
            "s" | "shift" => shift = true,
            _ => {}
        }
    }
    let key = alias(&key_raw.to_ascii_lowercase());
    if !ctrl && !alt && key_raw.len() == 1 && key_raw.chars().all(|c| c.is_ascii_uppercase()) {
        shift = true;
    }
    let mut out = String::new();
    for (on, name) in [(ctrl, "ctrl"), (alt, "alt"), (shift, "shift")] {
        if on {
            out.push_str(name);
            out.push('+');
        }
    }
    out.push_str(&key);
    out
}

/// The one spelling of a key token. The left column is every dialect seen in the wild; the
/// right is what [`crate::keybindings::KeyTok::token`] produces, so a chord built from a
/// real key event and a chord parsed from a preset land on the same string.
fn alias(key: &str) -> String {
    match key {
        "esc" => "escape",
        "ret" | "return" | "cr" => "enter",
        "spc" | "spacebar" => "space",
        "left" => "arrowleft",
        "right" => "arrowright",
        "up" => "arrowup",
        "down" => "arrowdown",
        "del" => "delete",
        "bs" => "backspace",
        "tab" | "enter" | "space" | "escape" | "delete" | "backspace" => key,
        other => other,
    }
    .to_string()
}

/// The preset a surface is actually using: the user's choice if they made one and it still
/// exists, else the module's own default.
fn chosen_preset(module: &ModuleId, km: &DeclareKeymap) -> BTreeMap<String, String> {
    let picked = PREFS.with(|p| p.borrow().presets.get(&key(module, &km.surface)).cloned());
    let preset = picked
        .as_deref()
        .and_then(|name| km.preset(name))
        .or_else(|| km.default_preset());
    preset.map(|p| p.bindings.clone()).unwrap_or_default()
}

/// The chord → action map actually in force for a surface: the chosen preset, with every
/// action the user rebound taken out of it and put back under the user's chord.
///
/// Taking the action out first is what makes a rebind a *move* rather than an addition —
/// otherwise the old chord would keep firing the action the user just moved off it.
fn effective(module: &ModuleId, km: &DeclareKeymap) -> BTreeMap<String, String> {
    let mut map: BTreeMap<String, String> = chosen_preset(module, km)
        .into_iter()
        .map(|(chord, action)| (normalize_chord(&chord), action))
        .collect();
    PREFS.with(|p| {
        for (id, chord) in &p.borrow().bindings {
            let Some(action) = id
                .strip_prefix(module.as_str())
                .and_then(|rest| rest.strip_prefix('.'))
            else {
                continue;
            };
            // An override for an action this surface does not declare belongs to another
            // surface of the same module; leaving it alone is how both stay rebindable.
            if !km.has_action(action) {
                continue;
            }
            map.retain(|_, a| a != action);
            if let Some(chord) = chord {
                map.insert(normalize_chord(chord), action.to_string());
            }
        }
    });
    map
}

/// The action a chord fires on `surface`, or `None` when nothing is bound to it.
pub fn resolve(module: &ModuleId, surface: &str, chord: &str) -> Option<String> {
    let km = keymap(module, surface)?;
    effective(module, &km).get(&normalize_chord(chord)).cloned()
}

/// Build the notification for one keystroke on `surface`.
///
/// `text` is what the chord would type if nothing consumed it. An unbound chord is still
/// sent: a text editor's most common key is the one with no binding at all, and the module
/// is the only side that knows whether a bare character means "insert" or "nothing".
pub fn key_event(module: &ModuleId, surface: &str, chord: &str, text: Option<String>) -> GridKey {
    let chord = normalize_chord(chord);
    GridKey {
        action: resolve(module, surface, &chord),
        surface: surface.to_string(),
        key: chord,
        text,
    }
}

/// Every action of a surface with the chord that currently fires it — what a rebinding UI
/// lists. An action with no chord at all is listed too, unbound: a user cannot rebind a key
/// they cannot see.
pub fn binding_rows(module: &ModuleId, surface: &str) -> Vec<(String, String, String)> {
    let Some(km) = keymap(module, surface) else {
        return Vec::new();
    };
    let map = effective(module, &km);
    km.actions
        .iter()
        .map(|a| {
            let chord = map
                .iter()
                .find(|(_, action)| *action == &a.id)
                .map(|(chord, _)| chord.clone())
                .unwrap_or_default();
            let label = if a.label.is_empty() {
                a.id.clone()
            } else {
                a.label.clone()
            };
            (a.id.clone(), label, chord)
        })
        .collect()
}

/// Rebind one action of a module. `None` unbinds it outright.
pub fn set_binding(module: &ModuleId, action: &str, chord: Option<&str>) {
    PREFS.with(|p| {
        p.borrow_mut()
            .bindings
            .insert(action_key(module, action), chord.map(normalize_chord));
    });
}

/// Undo a rebind, putting the action back on whatever its preset says.
pub fn clear_binding(module: &ModuleId, action: &str) {
    PREFS.with(|p| {
        p.borrow_mut().bindings.remove(&action_key(module, action));
    });
}

/// Choose which of a surface's declared presets is in force (`None` restores the module's
/// own default).
pub fn set_preset(module: &ModuleId, surface: &str, preset: Option<&str>) {
    PREFS.with(|p| {
        let mut p = p.borrow_mut();
        match preset {
            Some(name) => p.presets.insert(key(module, surface), name.to_string()),
            None => p.presets.remove(&key(module, surface)),
        };
    });
}

/// The preset name in force for a surface, if the user picked one.
pub fn preset_of(module: &ModuleId, surface: &str) -> Option<String> {
    PREFS.with(|p| p.borrow().presets.get(&key(module, surface)).cloned())
}

/// The Preferences row id for one action of one surface.
///
/// Namespaced with a `module:` prefix so the *one* keybindings editor can list the app's
/// own bindings and every module's side by side: the app's ids are bare
/// (`pane.toggleZoom`), so a prefixed id can never collide with one, and the editor routes
/// a row by whether [`parse_row_id`] recognises it.
pub fn row_id(module: &ModuleId, surface: &str, action: &str) -> String {
    format!("module:{}#{surface}/{action}", module.as_str())
}

/// Split a Preferences row id back into `(module, surface, action)`; `None` for one of the
/// app's own binding ids.
pub fn parse_row_id(id: &str) -> Option<(ModuleId, String, String)> {
    let rest = id.strip_prefix("module:")?;
    let (module, rest) = rest.split_once('#')?;
    let (surface, action) = rest.split_once('/')?;
    if surface.is_empty() || action.is_empty() {
        return None;
    }
    Some((
        ModuleId::new(module).ok()?,
        surface.to_string(),
        action.to_string(),
    ))
}

/// Every surface that has declared a keymap, in a stable order. What the Preferences
/// editor enumerates — a surface with no *open pane* is still listed, because a user who
/// closed the editor pane has not stopped wanting to rebind it.
pub fn declared() -> Vec<(ModuleId, String)> {
    KEYMAPS.with(|k| {
        let k = k.borrow();
        let mut keys: Vec<&String> = k.keys().collect();
        keys.sort();
        keys.iter()
            .filter_map(|key| {
                let (module, surface) = crate::leftpanel::split_key(key)?;
                Some((module, surface.to_string()))
            })
            .collect()
    })
}

/// Whether the user rebound (or explicitly unbound) one action.
pub fn overridden(module: &ModuleId, action: &str) -> bool {
    PREFS.with(|p| {
        p.borrow()
            .bindings
            .contains_key(&action_key(module, action))
    })
}

/// Whether any module action anywhere carries an override — the "Reset all" gate.
pub fn any_overridden() -> bool {
    PREFS.with(|p| !p.borrow().bindings.is_empty())
}

/// Drop every rebind of every module (the editor's "Reset all"). Presets are left alone:
/// a preset is a *choice of dialect*, not an override of one, and resetting the keys a
/// user moved should not also throw them back from Helix to VS Code.
pub fn clear_all_bindings() {
    PREFS.with(|p| p.borrow_mut().bindings.clear());
}

/// The presets a surface declares, as `(name, label, active)` — what a preset picker
/// lists. `active` marks the one actually in force, whether the user chose it or it is the
/// module's own default, so the menu always shows a checkmark somewhere.
pub fn preset_rows(module: &ModuleId, surface: &str) -> Vec<(String, String, bool)> {
    let Some(km) = keymap(module, surface) else {
        return Vec::new();
    };
    let chosen = preset_of(module, surface);
    let active = chosen
        .as_deref()
        .filter(|name| km.preset(name).is_some())
        .map(str::to_string)
        .or_else(|| km.default_preset().map(|p| p.name.clone()));
    km.presets
        .iter()
        .map(|p| {
            let label = if p.label.is_empty() {
                p.name.clone()
            } else {
                p.label.clone()
            };
            (p.name.clone(), label, active.as_deref() == Some(&p.name))
        })
        .collect()
}

/// Spell a captured keystroke in the module dialect. The inverse of [`chord_parts`], and
/// the reason the editor can hand a module a chord without knowing what a module is.
pub fn chord_of(ctrl: bool, alt: bool, shift: bool, key: &str) -> String {
    let mut out = String::new();
    for (on, name) in [(ctrl, "ctrl"), (alt, "alt"), (shift, "shift")] {
        if on {
            out.push_str(name);
            out.push('+');
        }
    }
    out.push_str(&alias(&key.to_ascii_lowercase()));
    out
}

/// The preset a surface falls back to with no user choice — the one the module nominates,
/// or the first it declares. The picker needs it to
/// know which row means "put it back the way the module shipped it" — that row clears the
/// user's choice rather than pinning the same name, so a module that later renames or
/// re-points its default is followed rather than frozen.
pub fn default_preset_name(module: &ModuleId, surface: &str) -> Option<String> {
    keymap(module, surface)?
        .default_preset()
        .map(|p| p.name.clone())
}

/// The display chips for a normalized chord (`ctrl+shift+h` → `Ctrl`, `Shift`, `H`), the
/// same shape [`crate::keybindings::Chord::parts`] produces so both kinds of row render
/// through one component.
///
/// The control chip says `Ctrl`, not [`crate::keybindings::CTRL_LABEL`]: a module chord's
/// `ctrl` is the *physical* Control key (see `crate::grid_chord`), which on macOS is the
/// one key `CTRL_LABEL` deliberately does not name.
pub fn chord_parts(chord: &str) -> Vec<String> {
    if chord.is_empty() {
        return Vec::new();
    }
    let parts: Vec<&str> = chord.split('+').filter(|p| !p.is_empty()).collect();
    let Some((key, mods)) = parts.split_last() else {
        return Vec::new();
    };
    let mut out: Vec<String> = mods
        .iter()
        .map(|m| match *m {
            "ctrl" => "Ctrl".to_string(),
            "alt" => "Alt".to_string(),
            "shift" => "Shift".to_string(),
            other => other.to_string(),
        })
        .collect();
    out.push(key_label(key));
    out
}

/// One key token's chip. Covers the six named keys [`alias`] admits beyond
/// [`crate::keybindings::KeyTok`]'s vocabulary, then defers to it so a shared key looks
/// identical in both halves of the editor.
fn key_label(key: &str) -> String {
    match key {
        "home" => "Home".to_string(),
        "end" => "End".to_string(),
        "pageup" => "PgUp".to_string(),
        "pagedown" => "PgDn".to_string(),
        "delete" => "Del".to_string(),
        "backspace" => "Bksp".to_string(),
        other => crate::keybindings::KeyTok::from_token(other)
            .map(|t| t.label())
            .unwrap_or_else(|| other.to_uppercase()),
    }
}

/// Read the persisted presets and rebinds. A missing or corrupt file is no preferences at
/// all, never an error: the module's own defaults are always a working keymap.
pub fn load_prefs() {
    let parsed = std::fs::read_to_string(prefs_path())
        .ok()
        .and_then(|raw| serde_json::from_str::<KeymapPrefs>(&raw).ok())
        .unwrap_or_default();
    PREFS.with(|p| *p.borrow_mut() = parsed);
}

/// Persist the presets and rebinds (best-effort, like the app's own keymap).
pub fn save_prefs() {
    let json = PREFS.with(|p| serde_json::to_string_pretty(&*p.borrow()));
    match json {
        Ok(json) => {
            if let Err(e) =
                avada_core::persistence::paths::write_atomic(&prefs_path(), json.as_bytes())
            {
                tracing::debug!("module keymap save failed: {e}");
            }
        }
        Err(e) => tracing::debug!("module keymap serialize failed: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use avada_core::module::grid::{GridAction, GridLine, GridSpan, KeymapPreset};

    fn id(s: &str) -> ModuleId {
        ModuleId::new(s).expect("a valid module id")
    }

    fn km(surface: &str) -> DeclareKeymap {
        DeclareKeymap {
            surface: surface.into(),
            actions: vec![
                GridAction {
                    id: "move.left".into(),
                    label: "Move left".into(),
                },
                GridAction {
                    id: "move.right".into(),
                    label: String::new(),
                },
                GridAction {
                    id: "file.save".into(),
                    label: "Save".into(),
                },
            ],
            presets: vec![
                KeymapPreset {
                    name: "helix".into(),
                    label: "Helix".into(),
                    bindings: [
                        ("h", "move.left"),
                        ("l", "move.right"),
                        ("C-s", "file.save"),
                    ]
                    .into_iter()
                    .map(|(c, a)| (c.to_string(), a.to_string()))
                    .collect(),
                },
                KeymapPreset {
                    name: "vscode".into(),
                    label: "VS Code".into(),
                    bindings: [("arrowleft", "move.left"), ("Ctrl+S", "file.save")]
                        .into_iter()
                        .map(|(c, a)| (c.to_string(), a.to_string()))
                        .collect(),
                },
            ],
            default_preset: Some("helix".into()),
        }
    }

    fn reset() {
        PREFS.with(|p| *p.borrow_mut() = KeymapPrefs::default());
        KEYMAPS.with(|k| k.borrow_mut().clear());
        SURFACES.with(|s| s.borrow_mut().clear());
    }

    fn frame_of(surface: &str, text: &str) -> GridFrame {
        GridFrame {
            surface: surface.into(),
            cols: 80,
            rows: 24,
            lines: vec![GridLine {
                spans: vec![GridSpan::plain(text)],
            }],
            cursor: None,
            status: String::new(),
        }
    }

    /// The pane's cache key is the revision, so a repaint that does not move it would leave
    /// the previous frame on screen forever — the exact bug the row store guards against.
    #[test]
    fn every_frame_moves_the_revision_and_a_gone_module_blanks_its_panes() {
        reset();
        let m = id("bshuler/avada-editor");
        assert_eq!(
            generation(&m, "editor"),
            0,
            "an unpainted surface is at zero"
        );
        assert!(frame(&m, "editor").is_none());

        set_frame(&m, frame_of("editor", "hello"));
        let first = generation(&m, "editor");
        assert!(first > 0);
        assert_eq!(frame(&m, "editor").unwrap().lines[0].text(), "hello");

        // The same frame said again is still a new revision: the store cannot tell an
        // idempotent repaint from a real one without comparing, and redrawing is cheaper
        // than being wrong.
        set_frame(&m, frame_of("editor", "hello"));
        assert!(generation(&m, "editor") > first);

        set_keymap(&m, km("editor"));
        let before = generation(&m, "editor");
        forget(&m);
        assert!(
            frame(&m, "editor").unwrap().lines.is_empty(),
            "a dead module keeps no text"
        );
        assert!(
            generation(&m, "editor") > before,
            "and the blanking is itself a change the projection must see"
        );
        assert!(
            keymap(&m, "editor").is_none(),
            "a keymap describing a dead process would offer actions nothing can perform"
        );
    }

    /// Two surfaces of one module, and two modules, are four separate grids — the same
    /// reason the row store is keyed by both halves.
    #[test]
    fn a_grid_is_per_module_and_per_surface() {
        reset();
        let (a, b) = (id("bshuler/avada-editor"), id("acme/avada-editor"));
        set_frame(&a, frame_of("editor", "a"));
        set_frame(&a, frame_of("diff", "d"));
        set_frame(&b, frame_of("editor", "b"));
        assert_eq!(frame(&a, "editor").unwrap().lines[0].text(), "a");
        assert_eq!(frame(&a, "diff").unwrap().lines[0].text(), "d");
        assert_eq!(frame(&b, "editor").unwrap().lines[0].text(), "b");
    }

    /// Presets are written in whichever dialect the module author's editor uses; the host
    /// generates its own from key events. Both have to land on one string.
    #[test]
    fn a_chord_has_one_spelling_however_it_was_written() {
        for spelling in ["C-S-x", "ctrl+shift+x", "Cmd+Shift+X", "shift-ctrl-x"] {
            assert_eq!(normalize_chord(spelling), "ctrl+shift+x", "{spelling}");
        }
        assert_eq!(normalize_chord("esc"), "escape");
        assert_eq!(normalize_chord("Left"), "arrowleft");
        assert_eq!(normalize_chord("A-ret"), "alt+enter");
        assert_eq!(normalize_chord("h"), "h");
    }

    #[test]
    fn the_default_preset_resolves_and_the_user_can_choose_another() {
        reset();
        let m = id("bshuler/avada-editor");
        set_keymap(&m, km("editor"));

        assert_eq!(resolve(&m, "editor", "h").as_deref(), Some("move.left"));
        assert_eq!(
            resolve(&m, "editor", "ctrl+s").as_deref(),
            Some("file.save"),
            "the preset's `C-s` and the host's `ctrl+s` are one chord"
        );
        assert!(resolve(&m, "editor", "arrowleft").is_none());

        set_preset(&m, "editor", Some("vscode"));
        assert_eq!(preset_of(&m, "editor").as_deref(), Some("vscode"));
        assert_eq!(
            resolve(&m, "editor", "arrowleft").as_deref(),
            Some("move.left")
        );
        assert!(
            resolve(&m, "editor", "h").is_none(),
            "presets replace, not merge"
        );

        // A preset the module does not declare — a stale preference, or a module that
        // dropped one in an update — falls back to the module's default rather than to no
        // keymap at all.
        set_preset(&m, "editor", Some("emacs"));
        assert_eq!(resolve(&m, "editor", "h").as_deref(), Some("move.left"));

        set_preset(&m, "editor", None);
        assert!(preset_of(&m, "editor").is_none());
        assert_eq!(resolve(&m, "editor", "h").as_deref(), Some("move.left"));
    }

    /// A rebind moves an action; it does not add a second chord for it. If the preset chord
    /// kept firing, the user would have rebound nothing.
    #[test]
    fn a_rebind_moves_the_action_off_its_old_chord_and_an_unbind_silences_it() {
        reset();
        let m = id("bshuler/avada-editor");
        set_keymap(&m, km("editor"));

        set_binding(&m, "move.left", Some("A-h"));
        assert_eq!(resolve(&m, "editor", "alt+h").as_deref(), Some("move.left"));
        assert!(resolve(&m, "editor", "h").is_none());

        set_binding(&m, "move.right", None);
        assert!(resolve(&m, "editor", "l").is_none(), "an explicit unbind");
        assert_eq!(
            resolve(&m, "editor", "ctrl+s").as_deref(),
            Some("file.save"),
            "and it touches nothing else"
        );

        clear_binding(&m, "move.left");
        assert_eq!(resolve(&m, "editor", "h").as_deref(), Some("move.left"));
        assert!(resolve(&m, "editor", "alt+h").is_none());
    }

    /// Overrides are namespaced by module, because two editors both declaring `move.left`
    /// are two different actions.
    #[test]
    fn one_module_s_rebind_does_not_touch_another_s_identical_action() {
        reset();
        let (a, b) = (id("bshuler/avada-editor"), id("acme/avada-editor"));
        set_keymap(&a, km("editor"));
        set_keymap(&b, km("editor"));

        set_binding(&a, "move.left", Some("A-h"));
        assert_eq!(resolve(&a, "editor", "alt+h").as_deref(), Some("move.left"));
        assert_eq!(
            resolve(&b, "editor", "h").as_deref(),
            Some("move.left"),
            "the other module still has its own default"
        );
        assert!(resolve(&b, "editor", "alt+h").is_none());
    }

    /// The most common key in an editor is the one with no binding, so an unresolved chord
    /// is still delivered — carrying what it would have typed.
    #[test]
    fn an_unbound_chord_still_travels_with_the_text_it_would_type() {
        reset();
        let m = id("bshuler/avada-editor");
        set_keymap(&m, km("editor"));

        let bound = key_event(&m, "editor", "C-s", None);
        assert_eq!(bound.action.as_deref(), Some("file.save"));
        assert_eq!(bound.key, "ctrl+s");
        assert_eq!(bound.surface, "editor");

        let typed = key_event(&m, "editor", "q", Some("q".into()));
        assert!(typed.action.is_none());
        assert_eq!(typed.text.as_deref(), Some("q"));

        // A surface that declared no keymap at all resolves nothing and still delivers.
        let none = key_event(&m, "never-declared", "h", Some("h".into()));
        assert!(none.action.is_none());
        assert_eq!(none.key, "h");
    }

    /// A rebinding UI has to list an action that nothing fires, or the user cannot give it
    /// a key.
    #[test]
    fn the_binding_rows_show_every_action_including_the_unbound_ones() {
        reset();
        let m = id("bshuler/avada-editor");
        assert!(
            binding_rows(&m, "editor").is_empty(),
            "nothing declared yet"
        );

        let mut declared = km("editor");
        declared.actions.push(GridAction {
            id: "file.quit".into(),
            label: "Quit".into(),
        });
        set_keymap(&m, declared);
        set_binding(&m, "move.left", Some("A-h"));

        let rows = binding_rows(&m, "editor");
        assert_eq!(rows.len(), 4);
        assert_eq!(
            rows[0],
            (
                "move.left".to_string(),
                "Move left".to_string(),
                "alt+h".to_string()
            ),
            "the user's chord, not the preset's"
        );
        assert_eq!(
            rows[1].1, "move.right",
            "an action with no label is listed under its id"
        );
        assert_eq!(rows[3].2, "", "nothing binds quit, and it is listed anyway");
    }

    /// The persisted shape has to survive a round trip, including the `null` unbind.
    #[test]
    fn the_preferences_round_trip_through_json() {
        reset();
        let m = id("bshuler/avada-editor");
        set_preset(&m, "editor", Some("vscode"));
        set_binding(&m, "move.left", Some("A-h"));
        set_binding(&m, "move.right", None);

        let json = PREFS.with(|p| serde_json::to_string(&*p.borrow()).unwrap());
        let back: KeymapPrefs = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.presets.get("bshuler/avada-editor#editor").unwrap(),
            "vscode"
        );
        assert_eq!(
            back.bindings.get("bshuler/avada-editor.move.left").unwrap(),
            &Some("alt+h".to_string())
        );
        assert_eq!(
            back.bindings
                .get("bshuler/avada-editor.move.right")
                .unwrap(),
            &None
        );
        assert_eq!(
            serde_json::from_str::<KeymapPrefs>("{}").unwrap(),
            KeymapPrefs::default(),
            "an empty file is no preferences, not a parse error"
        );
    }

    /// The whole point of the `module:` namespace: one editor lists two stores, and the row
    /// id alone decides which one a capture writes to. An app id must never parse.
    #[test]
    fn a_row_id_round_trips_and_an_app_binding_id_is_not_one() {
        let m = id("bshuler/avada-editor");
        let raw = row_id(&m, "editor", "move.left");
        assert_eq!(raw, "module:bshuler/avada-editor#editor/move.left");

        let (back, surface, action) = parse_row_id(&raw).expect("its own id parses");
        assert_eq!(back, m);
        assert_eq!(surface, "editor");
        assert_eq!(action, "move.left");

        // An action id with dots in it survives, because the split is on the *first* slash
        // after the surface and an action id may not contain one.
        assert_eq!(
            parse_row_id(&row_id(&m, "diff", "file.save.as")).unwrap().2,
            "file.save.as"
        );

        for app_id in [
            "pane.toggleZoom",
            "module:bshuler/avada-editor",
            "module:bshuler/avada-editor#editor",
            "module:bshuler/avada-editor#/move.left",
            "module:bshuler/avada-editor#editor/",
            "module:not a module id#editor/move.left",
        ] {
            assert!(parse_row_id(app_id).is_none(), "{app_id}");
        }
    }

    /// The editor captures a keystroke as four host-shaped fields and has to hand the module
    /// store one string in the module's dialect — and get chips back for it.
    #[test]
    fn a_captured_keystroke_spells_a_module_chord_and_a_chord_spells_its_chips() {
        assert_eq!(chord_of(false, false, false, "h"), "h");
        assert_eq!(chord_of(true, false, false, "S"), "ctrl+s");
        assert_eq!(chord_of(true, true, true, "x"), "ctrl+alt+shift+x");
        assert_eq!(
            chord_of(false, false, false, "esc"),
            "escape",
            "the capture is aliased on the way in, so it matches a preset's spelling"
        );

        assert_eq!(chord_parts("h"), vec!["H"]);
        assert_eq!(chord_parts("ctrl+shift+x"), vec!["Ctrl", "Shift", "X"]);
        assert_eq!(
            chord_parts("alt+arrowleft"),
            vec![
                "Alt".to_string(),
                crate::keybindings::KeyTok::from_token("arrowleft")
                    .unwrap()
                    .label()
            ],
            "a key both halves know renders identically in both"
        );
        assert_eq!(chord_parts("pageup"), vec!["PgUp"]);
        assert_eq!(chord_parts("f11"), vec!["F11"]);
        assert!(
            chord_parts("").is_empty(),
            "an unbound row has no chips, which is how the editor knows to say so"
        );

        // Round trip: whatever the capture spells, the chips describe.
        assert_eq!(
            chord_parts(&chord_of(true, false, true, "Home")),
            vec!["Ctrl", "Shift", "Home"]
        );
    }

    /// The editor enumerates surfaces, not panes: closing an editor pane does not stop a
    /// user wanting to rebind it.
    #[test]
    fn every_declared_surface_is_listed_in_a_stable_order() {
        reset();
        let (a, b) = (id("bshuler/avada-editor"), id("acme/avada-editor"));
        assert!(declared().is_empty());

        set_keymap(&b, km("editor"));
        set_keymap(&a, km("editor"));
        set_keymap(&a, km("diff"));

        assert_eq!(
            declared(),
            vec![
                (b.clone(), "editor".to_string()),
                (a.clone(), "diff".to_string()),
                (a.clone(), "editor".to_string()),
            ],
            "sorted by key, so the editor's category order does not shuffle between builds"
        );

        // A surface whose module died is not listed: its actions could not be performed.
        forget(&a);
        assert_eq!(declared(), vec![(b, "editor".to_string())]);
    }

    /// "Overridden" marks the rows a reset would change, and gates the "Reset all" button.
    #[test]
    fn an_override_is_visible_per_row_and_in_aggregate_and_reset_all_spares_the_presets() {
        reset();
        let m = id("bshuler/avada-editor");
        set_keymap(&m, km("editor"));
        assert!(!any_overridden());
        assert!(!overridden(&m, "move.left"));

        set_binding(&m, "move.left", Some("A-h"));
        assert!(overridden(&m, "move.left"));
        assert!(!overridden(&m, "move.right"));
        assert!(any_overridden());

        // An explicit unbind is an override too — it is a stored decision, not an absence.
        set_binding(&m, "move.right", None);
        assert!(overridden(&m, "move.right"));

        set_preset(&m, "editor", Some("vscode"));
        clear_all_bindings();
        assert!(!any_overridden());
        assert_eq!(
            preset_of(&m, "editor").as_deref(),
            Some("vscode"),
            "a preset is a choice of dialect, not an override of one: reset all leaves it"
        );
        assert_eq!(
            resolve(&m, "editor", "arrowleft").as_deref(),
            Some("move.left"),
            "and the preset is still what resolves"
        );
    }

    /// What the pane menu draws. Exactly one row is ticked at all times, so the menu never
    /// looks like nothing is chosen.
    #[test]
    fn the_preset_rows_always_tick_exactly_one() {
        reset();
        let m = id("bshuler/avada-editor");
        assert!(preset_rows(&m, "editor").is_empty(), "nothing declared yet");
        assert!(default_preset_name(&m, "editor").is_none());

        set_keymap(&m, km("editor"));
        assert_eq!(default_preset_name(&m, "editor").as_deref(), Some("helix"));
        assert_eq!(
            preset_rows(&m, "editor"),
            vec![
                ("helix".to_string(), "Helix".to_string(), true),
                ("vscode".to_string(), "VS Code".to_string(), false),
            ],
            "the module's own default is ticked before the user has chosen anything"
        );

        set_preset(&m, "editor", Some("vscode"));
        let rows = preset_rows(&m, "editor");
        assert!(!rows[0].2 && rows[1].2);

        // A stale choice falls back to the default, and the tick follows the fallback rather
        // than pointing at a preset that no longer exists.
        set_preset(&m, "editor", Some("emacs"));
        let rows = preset_rows(&m, "editor");
        assert!(rows[0].2 && !rows[1].2);

        // A preset with no label is listed under its name rather than as a blank row. A
        // module that nominates no default still has one — the first it declared — so the
        // tick and the "put it back" row agree rather than both pointing at nothing.
        let mut plain = km("editor");
        plain.presets[1].label = String::new();
        plain.default_preset = None;
        set_keymap(&m, plain);
        set_preset(&m, "editor", None);
        assert_eq!(
            preset_rows(&m, "editor"),
            vec![
                ("helix".to_string(), "Helix".to_string(), true),
                ("vscode".to_string(), "vscode".to_string(), false),
            ]
        );
        assert_eq!(default_preset_name(&m, "editor").as_deref(), Some("helix"));
    }

    /// A scratch prefs file for this thread, wired up and returned.
    fn scratch_prefs(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("avada-mk-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch dir");
        let path = dir.join("module-keymaps.json");
        let _ = std::fs::remove_file(&path);
        set_prefs_path(path.clone());
        path
    }

    /// Both halves of a choice have to survive a restart: which dialect a surface is on, and
    /// every per-action rebind — including an explicit unbind, which is a choice too and
    /// would silently come back as the preset's chord if it were merely dropped.
    #[test]
    fn every_choice_survives_a_restart_and_an_absent_file_is_simply_no_choices() {
        let path = scratch_prefs("roundtrip");
        let m = id("bshuler/avada-editor");
        set_keymap(&m, km("editor"));

        load_prefs();
        assert_eq!(
            preset_of(&m, "editor"),
            None,
            "a file that is not there is not an error"
        );

        set_preset(&m, "editor", Some("vscode"));
        set_binding(&m, "move.left", Some("ctrl+b"));
        set_binding(&m, "file.save", None); // an explicit unbind
        save_prefs();
        assert!(path.exists(), "the choice reached the disk");

        // A restart: the in-memory store is gone, the file is not.
        PREFS.with(|p| *p.borrow_mut() = KeymapPrefs::default());
        assert_eq!(preset_of(&m, "editor"), None);
        load_prefs();
        assert_eq!(preset_of(&m, "editor").as_deref(), Some("vscode"));
        assert!(overridden(&m, "move.left") && overridden(&m, "file.save"));
        let chords: Vec<(String, String)> = binding_rows(&m, "editor")
            .into_iter()
            .map(|(id, _, chord)| (id, chord))
            .collect();
        assert_eq!(
            chords,
            vec![
                ("move.left".to_string(), "ctrl+b".to_string()),
                ("move.right".to_string(), String::new()),
                ("file.save".to_string(), String::new()), // the unbind out-ranks vscode's Ctrl+S
            ],
            "an unbind is a stored choice, not the absence of one"
        );

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
