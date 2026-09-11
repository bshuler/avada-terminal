//! Keystrokes and actions in; buffer changes out.
//!
//! # The one rule
//!
//! The host resolves keys against the user's chosen preset before the module sees them,
//! so a [`GridKey`] carries an `action` when a binding claimed the chord and a `text`
//! when the keystroke was a plain printable character. Both can be present at once —
//! `i` under the Helix preset is `action: "mode.insert"`, `text: "i"` — and which one
//! wins is the whole of this module's modality:
//!
//! * **Insert mode**: if the key produced text, type it; otherwise run the action.
//! * **Normal mode**: run the action; text with no action behind it is ignored.
//!
//! No exception list is needed. Named keys (`escape`, the arrows, `enter`, `backspace`)
//! and modifier chords (`ctrl+s`) carry no `text` at all, so they keep working in insert
//! mode by the same rule that types a letter. And the Normal arm is what keeps an
//! unbound letter — `q` in Helix normal mode — from quietly landing in the file.

use crate::editor::{Editor, Mode};
use avada_module_sdk::grid::GridKey;

/// Something the caller must do that needs the host, and therefore cannot happen here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// The buffer changed, or the caret moved: repaint.
    Repaint,
    /// Write the current buffer through `host.fs.write`, then repaint.
    Save,
}

/// Apply one keystroke. Returns what the caller still owes the host.
pub fn key(ed: &mut Editor, k: &GridKey) -> Effect {
    ed.message = None;
    if ed.mode == Mode::Insert {
        if let Some(t) = k.text.as_deref().filter(|t| !t.is_empty()) {
            ed.buf_mut().insert(t);
            ed.follow_cursor();
            return Effect::Repaint;
        }
    }
    match k.action.as_deref() {
        Some(a) => action(ed, a),
        None => Effect::Repaint,
    }
}

/// Apply one named action.
pub fn action(ed: &mut Editor, id: &str) -> Effect {
    let past = ed.past_end();
    match id {
        "mode.insert" => enter_insert(ed, |_| {}),
        "mode.insert.line.start" => enter_insert(ed, |e| e.buf_mut().line_start()),
        "mode.append" => enter_insert(ed, |e| e.buf_mut().move_right(true)),
        "mode.append.line.end" => enter_insert(ed, |e| e.buf_mut().line_end(true)),
        "mode.normal" => {
            ed.mode = Mode::Normal;
            // Insert mode may have parked the caret one past the last character, where a
            // block cursor has nothing to sit on.
            ed.buf_mut().clamp_normal();
            ed.buf_mut().break_group();
        }

        "move.left" => ed.buf_mut().move_left(),
        "move.right" => ed.buf_mut().move_right(past),
        "move.up" => ed.buf_mut().move_up(past),
        "move.down" => ed.buf_mut().move_down(past),
        "move.word.next" => ed.buf_mut().word_next(),
        "move.word.prev" => ed.buf_mut().word_prev(),
        "move.word.end" => ed.buf_mut().word_end(),
        "move.line.start" => ed.buf_mut().line_start(),
        "move.line.end" => ed.buf_mut().line_end(past),
        "move.doc.start" => ed.buf_mut().doc_start(),
        "move.doc.end" => ed.buf_mut().doc_end(),
        "move.page.up" => {
            let h = ed.text_rows() as isize;
            ed.buf_mut().move_lines(-h, past);
        }
        "move.page.down" => {
            let h = ed.text_rows() as isize;
            ed.buf_mut().move_lines(h, past);
        }

        "edit.delete" => ed.buf_mut().delete(),
        "edit.delete.back" => ed.buf_mut().delete_back(),
        "edit.delete.line" => ed.buf_mut().delete_line(),
        "edit.newline" => ed.buf_mut().newline(),
        "edit.open.below" => enter_insert(ed, |e| e.buf_mut().open_below()),
        "edit.open.above" => enter_insert(ed, |e| e.buf_mut().open_above()),
        "edit.undo" => {
            if !ed.buf_mut().undo() {
                ed.message = Some("nothing to undo".into());
            }
        }
        "edit.redo" => {
            if !ed.buf_mut().redo() {
                ed.message = Some("nothing to redo".into());
            }
        }

        "file.save" => {
            ed.follow_cursor();
            return Effect::Save;
        }
        "buffer.next" => ed.cycle(true),
        "buffer.prev" => ed.cycle(false),

        // An action the host resolved but this version does not implement is not an
        // error: a user's saved rebinding outlives the release that named it.
        _ => {}
    }
    ed.follow_cursor();
    Effect::Repaint
}

/// Switch to insert mode, running `f` first so the motion happens while the caret is
/// still allowed to move the way normal mode moves.
fn enter_insert(ed: &mut Editor, f: impl FnOnce(&mut Editor)) {
    f(ed);
    ed.mode = Mode::Insert;
    ed.buf_mut().break_group();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn ed(text: &str) -> Editor {
        let mut e = Editor::default();
        e.open(Path::new("/w/a.rs"), text);
        e
    }

    fn press(e: &mut Editor, action: Option<&str>, text: Option<&str>) -> Effect {
        key(
            e,
            &GridKey {
                surface: "editor".into(),
                key: "?".into(),
                action: action.map(String::from),
                text: text.map(String::from),
            },
        )
    }

    #[test]
    fn an_unbound_letter_in_normal_mode_does_not_reach_the_file() {
        let mut e = ed("abc\n");
        press(&mut e, None, Some("q"));
        assert_eq!(e.buf().to_text(), "abc\n");
        assert!(!e.buf().dirty());
    }

    #[test]
    fn in_insert_mode_the_text_wins_over_the_action() {
        let mut e = ed("abc\n");
        press(&mut e, Some("mode.insert"), Some("i"));
        assert_eq!(e.mode, Mode::Insert);
        // Under a modal preset `i` still resolves to mode.insert; in insert mode it must
        // be typed instead, or the letter `i` would be untypeable.
        press(&mut e, Some("mode.insert"), Some("i"));
        assert_eq!(e.buf().to_text(), "iabc\n");
    }

    #[test]
    fn named_keys_keep_working_in_insert_mode() {
        let mut e = ed("abc\n");
        press(&mut e, Some("mode.append.line.end"), Some("A"));
        press(&mut e, Some("edit.newline"), None);
        press(&mut e, None, Some("x"));
        assert_eq!(e.buf().to_text(), "abc\nx\n");
        press(&mut e, Some("mode.normal"), None);
        assert_eq!(e.mode, Mode::Normal);
    }

    #[test]
    fn leaving_insert_mode_pulls_the_caret_back_onto_a_character() {
        let mut e = ed("ab\n");
        action(&mut e, "mode.append.line.end");
        assert_eq!(
            e.buf().col(),
            2,
            "insert mode may sit past the last character"
        );
        action(&mut e, "mode.normal");
        assert_eq!(e.buf().col(), 1);
    }

    #[test]
    fn normal_mode_stops_the_caret_on_the_last_character_and_insert_mode_does_not() {
        let mut e = ed("ab\n");
        action(&mut e, "move.line.end");
        assert_eq!(e.buf().col(), 1);
        action(&mut e, "mode.insert");
        action(&mut e, "move.line.end");
        assert_eq!(e.buf().col(), 2);
    }

    #[test]
    fn open_below_indents_like_the_line_it_came_from() {
        let mut e = ed("    let x = 1;\n");
        action(&mut e, "edit.open.below");
        press(&mut e, None, Some("y"));
        assert_eq!(e.buf().to_text(), "    let x = 1;\n    y\n");
        assert_eq!(e.mode, Mode::Insert);
    }

    #[test]
    fn open_above_puts_the_line_before_the_current_one() {
        let mut e = ed("  b\n");
        action(&mut e, "edit.open.above");
        press(&mut e, None, Some("a"));
        assert_eq!(e.buf().to_text(), "  a\n  b\n");
    }

    #[test]
    fn a_typed_word_undoes_in_one_step_but_a_motion_breaks_the_group() {
        let mut e = ed("");
        action(&mut e, "mode.insert");
        for c in "hello".chars() {
            press(&mut e, None, Some(&c.to_string()));
        }
        action(&mut e, "move.left");
        for c in "XY".chars() {
            press(&mut e, None, Some(&c.to_string()));
        }
        assert_eq!(e.buf().to_text(), "hellXYo");
        action(&mut e, "edit.undo");
        assert_eq!(e.buf().to_text(), "hello");
        action(&mut e, "edit.undo");
        assert_eq!(e.buf().to_text(), "");
        assert!(!e.buf_mut().undo());
    }

    #[test]
    fn undo_with_nothing_behind_it_says_so_instead_of_doing_nothing_silently() {
        let mut e = ed("abc\n");
        action(&mut e, "edit.undo");
        assert_eq!(e.message.as_deref(), Some("nothing to undo"));
        // ...and the next keystroke clears the note.
        press(&mut e, Some("move.right"), None);
        assert!(e.message.is_none());
    }

    #[test]
    fn redo_walks_back_up_the_history() {
        let mut e = ed("abc\n");
        action(&mut e, "edit.delete.line");
        assert_eq!(e.buf().to_text(), "");
        action(&mut e, "edit.undo");
        assert_eq!(e.buf().to_text(), "abc\n");
        action(&mut e, "edit.redo");
        assert_eq!(e.buf().to_text(), "");
    }

    #[test]
    fn save_is_the_callers_problem() {
        let mut e = ed("abc\n");
        assert_eq!(action(&mut e, "file.save"), Effect::Save);
        assert_eq!(action(&mut e, "move.right"), Effect::Repaint);
    }

    #[test]
    fn an_action_this_version_does_not_know_is_ignored_rather_than_fatal() {
        let mut e = ed("abc\n");
        assert_eq!(action(&mut e, "edit.surround.delete"), Effect::Repaint);
        assert_eq!(e.buf().to_text(), "abc\n");
    }

    #[test]
    fn page_motions_are_a_screen_at_a_time() {
        let mut e = ed(&"x\n".repeat(100));
        e.rows = 11; // ten text rows
        action(&mut e, "move.page.down");
        assert_eq!(e.buf().line(), 10);
        action(&mut e, "move.page.up");
        assert_eq!(e.buf().line(), 0);
    }
}
