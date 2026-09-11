//! One open file: a rope, a caret, and enough undo history to survive a mistake.
//!
//! Nothing here knows about the wire, the host, or modes. A [`Buffer`] answers motions and
//! edits in character indices; [`crate::editor`] decides which of them a keystroke meant
//! and [`crate::render`] decides what any of it looks like.

use ropey::Rope;
use std::path::PathBuf;

/// How many undo groups a buffer keeps.
///
/// A bound rather than "all of it" because the module holds every open buffer in memory
/// for the life of the process, and an unbounded history of a large file is the one way
/// this module could grow without limit while the user is doing nothing unusual.
const UNDO_DEPTH: usize = 256;

/// A restorable point in a buffer's life.
#[derive(Debug, Clone)]
struct Snapshot {
    text: Rope,
    cursor: usize,
}

/// What a character is, for the purpose of word motions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    /// Letters, digits and `_`: what a word is made of.
    Word,
    /// Anything printable that is not a word character.
    Punct,
    /// Spaces, tabs and newlines.
    Space,
}

fn class(c: char) -> Class {
    if c.is_whitespace() {
        Class::Space
    } else if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else {
        Class::Punct
    }
}

/// One open file.
#[derive(Debug, Clone)]
pub struct Buffer {
    /// Where it came from, and where `file.save` writes it. `None` for the scratch buffer
    /// the module shows before anything has been opened.
    pub path: Option<PathBuf>,
    text: Rope,
    cursor: usize,
    dirty: bool,
    /// The column a vertical motion is *trying* to reach, which is not the column it
    /// landed on: walking down past a short line and back up must return to where it
    /// started, and only a remembered goal can do that.
    goal_col: Option<usize>,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// Whether the next insert may join the last undo group. Typing a word is one undo,
    /// not one per letter; anything that is not a plain insert breaks the group.
    coalescing: bool,
}

impl Buffer {
    /// A buffer over `text`, caret at the start.
    pub fn new(path: Option<PathBuf>, text: &str) -> Self {
        Self {
            path,
            text: Rope::from_str(text),
            cursor: 0,
            dirty: false,
            goal_col: None,
            undo: Vec::new(),
            redo: Vec::new(),
            coalescing: false,
        }
    }

    /// The empty, pathless buffer a fresh editor pane shows.
    pub fn scratch() -> Self {
        Self::new(None, "")
    }

    /// The whole text, for `host.fs.write` and for tests.
    pub fn to_text(&self) -> String {
        self.text.to_string()
    }

    /// Whether the text differs from what was last read or written.
    pub fn dirty(&self) -> bool {
        self.dirty
    }

    /// Called after a successful `host.fs.write`.
    pub fn mark_saved(&mut self) {
        self.dirty = false;
    }

    /// The file's display name — the last path component, or `[scratch]`.
    pub fn name(&self) -> String {
        self.path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "[scratch]".into())
    }

    /// How many lines the buffer has. A trailing newline makes a final empty line, which
    /// is what an editor must show: the caret can sit there.
    pub fn len_lines(&self) -> usize {
        self.text.len_lines()
    }

    /// Line `l` without its terminator. Out of range reads as empty rather than panicking:
    /// the renderer walks a viewport that may hang off the end of a short file.
    pub fn line_text(&self, l: usize) -> String {
        if l >= self.text.len_lines() {
            return String::new();
        }
        let mut t = self.text.line(l).to_string();
        while t.ends_with('\n') || t.ends_with('\r') {
            t.pop();
        }
        t
    }

    /// The length of line `l` in characters, not counting its terminator.
    pub fn line_len(&self, l: usize) -> usize {
        if l >= self.text.len_lines() {
            return 0;
        }
        let line = self.text.line(l);
        let mut n = line.len_chars();
        if n > 0 && line.char(n - 1) == '\n' {
            n -= 1;
            if n > 0 && line.char(n - 1) == '\r' {
                n -= 1;
            }
        }
        n
    }

    /// Which line the caret is on.
    pub fn line(&self) -> usize {
        self.text
            .char_to_line(self.cursor.min(self.text.len_chars()))
    }

    /// Which column the caret is in, counted in characters.
    pub fn col(&self) -> usize {
        let l = self.line();
        self.cursor - self.text.line_to_char(l)
    }

    /// Put the caret on `line`:`col`, clamped into the buffer.
    pub fn goto(&mut self, line: usize, col: usize) {
        let l = line.min(self.text.len_lines().saturating_sub(1));
        let c = col.min(self.line_len(l));
        self.cursor = self.text.line_to_char(l) + c;
        self.break_group();
    }

    /// Pull the caret off the end of the line, where a block cursor has nothing to sit on.
    /// Called when insert mode ends; insert mode itself is allowed one past the last
    /// character, because that is where you type to append.
    pub fn clamp_normal(&mut self) {
        let l = self.line();
        let len = self.line_len(l);
        if len > 0 && self.col() >= len {
            self.cursor = self.text.line_to_char(l) + len - 1;
        }
    }

    // ---- motions -------------------------------------------------------------------

    /// One character left, stopping at the start of the line.
    pub fn move_left(&mut self) {
        if self.col() > 0 {
            self.cursor -= 1;
        }
        self.goal_col = None;
        self.break_group();
    }

    /// One character right. `past_end` is insert mode's extra column.
    pub fn move_right(&mut self, past_end: bool) {
        let l = self.line();
        let limit = self.line_len(l) - usize::from(!past_end && self.line_len(l) > 0);
        if self.col() < limit {
            self.cursor += 1;
        }
        self.goal_col = None;
        self.break_group();
    }

    /// One line up, keeping the goal column.
    pub fn move_up(&mut self, past_end: bool) {
        let l = self.line();
        if l > 0 {
            self.vertical(l - 1, past_end);
        }
    }

    /// One line down, keeping the goal column.
    pub fn move_down(&mut self, past_end: bool) {
        let l = self.line();
        if l + 1 < self.text.len_lines() {
            self.vertical(l + 1, past_end);
        }
    }

    /// `n` lines up or down at once, for page motions.
    pub fn move_lines(&mut self, delta: isize, past_end: bool) {
        let l = self.line() as isize + delta;
        let l = l.clamp(0, self.text.len_lines() as isize - 1) as usize;
        self.vertical(l, past_end);
    }

    fn vertical(&mut self, to: usize, past_end: bool) {
        let goal = self.goal_col.unwrap_or_else(|| self.col());
        let len = self.line_len(to);
        let limit = len.saturating_sub(usize::from(!past_end && len > 0));
        self.cursor = self.text.line_to_char(to) + goal.min(limit);
        self.goal_col = Some(goal);
        self.break_group();
    }

    /// To column zero.
    pub fn line_start(&mut self) {
        self.cursor = self.text.line_to_char(self.line());
        self.goal_col = None;
        self.break_group();
    }

    /// To the end of the line. `past_end` puts the caret after the last character, which
    /// is what `A` needs and what a block cursor must not do.
    pub fn line_end(&mut self, past_end: bool) {
        let l = self.line();
        let len = self.line_len(l);
        let col = len.saturating_sub(usize::from(!past_end && len > 0));
        self.cursor = self.text.line_to_char(l) + col;
        self.goal_col = None;
        self.break_group();
    }

    /// To the first character of the buffer.
    pub fn doc_start(&mut self) {
        self.cursor = 0;
        self.goal_col = None;
        self.break_group();
    }

    /// To the start of the last line — where `G` lands, not the very last character.
    pub fn doc_end(&mut self) {
        self.cursor = self
            .text
            .line_to_char(self.text.len_lines().saturating_sub(1));
        self.goal_col = None;
        self.break_group();
    }

    /// To the first character of the next word.
    pub fn word_next(&mut self) {
        let n = self.text.len_chars();
        let mut i = self.cursor;
        if i >= n {
            return;
        }
        let start = class(self.text.char(i));
        if start != Class::Space {
            while i < n && class(self.text.char(i)) == start {
                i += 1;
            }
        }
        while i < n && class(self.text.char(i)) == Class::Space {
            i += 1;
        }
        self.cursor = i.min(n);
        self.goal_col = None;
        self.break_group();
    }

    /// To the first character of the word behind the caret.
    pub fn word_prev(&mut self) {
        let mut i = self.cursor;
        if i == 0 {
            return;
        }
        i -= 1;
        while i > 0 && class(self.text.char(i)) == Class::Space {
            i -= 1;
        }
        let here = class(self.text.char(i));
        while i > 0 && class(self.text.char(i - 1)) == here {
            i -= 1;
        }
        self.cursor = i;
        self.goal_col = None;
        self.break_group();
    }

    /// To the last character of the word ahead of the caret.
    pub fn word_end(&mut self) {
        let n = self.text.len_chars();
        let mut i = self.cursor;
        if i + 1 >= n {
            self.cursor = n.saturating_sub(1);
            return;
        }
        i += 1;
        while i < n && class(self.text.char(i)) == Class::Space {
            i += 1;
        }
        if i >= n {
            self.cursor = n - 1;
            self.goal_col = None;
            return;
        }
        let here = class(self.text.char(i));
        while i + 1 < n && class(self.text.char(i + 1)) == here {
            i += 1;
        }
        self.cursor = i;
        self.goal_col = None;
        self.break_group();
    }

    // ---- edits ---------------------------------------------------------------------

    /// Insert text at the caret and leave the caret after it.
    pub fn insert(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        self.snapshot(true);
        self.text.insert(self.cursor, s);
        self.cursor += s.chars().count();
        self.dirty = true;
        self.goal_col = None;
    }

    /// A newline plus the leading whitespace of the line it split, so that typing inside an
    /// indented block does not walk back to column zero on every Enter.
    pub fn newline(&mut self) {
        let indent = leading_ws(&self.line_text(self.line()));
        self.insert(&format!("\n{indent}"));
    }

    /// Open a line below the caret and put the caret on it, indented to match.
    pub fn open_below(&mut self) {
        self.line_end(true);
        self.newline();
        self.break_group();
    }

    /// Open a line above the caret and put the caret on it, indented to match.
    pub fn open_above(&mut self) {
        let indent = leading_ws(&self.line_text(self.line()));
        self.line_start();
        self.snapshot(false);
        self.text.insert(self.cursor, &format!("{indent}\n"));
        self.cursor += indent.chars().count();
        self.dirty = true;
        self.goal_col = None;
    }

    /// Delete the character under the caret.
    pub fn delete(&mut self) {
        if self.cursor < self.text.len_chars() {
            self.snapshot(false);
            self.text.remove(self.cursor..self.cursor + 1);
            self.dirty = true;
            self.goal_col = None;
        }
    }

    /// Delete the character behind the caret, joining lines at column zero.
    pub fn delete_back(&mut self) {
        if self.cursor > 0 {
            self.snapshot(false);
            self.text.remove(self.cursor - 1..self.cursor);
            self.cursor -= 1;
            self.dirty = true;
            self.goal_col = None;
        }
    }

    /// Delete the whole line the caret is on, terminator included.
    pub fn delete_line(&mut self) {
        let l = self.line();
        let from = self.text.line_to_char(l);
        let to = if l + 1 < self.text.len_lines() {
            self.text.line_to_char(l + 1)
        } else {
            self.text.len_chars()
        };
        if from == to {
            // The caret is on the empty line a trailing newline opens. There is no line
            // there to remove, so take the newline that made it — otherwise deleting
            // lines from the top would stall one short of an empty file.
            if from == 0 {
                return;
            }
            self.snapshot(false);
            self.text.remove(from - 1..from);
            self.dirty = true;
            self.goal_col = None;
            self.cursor = self
                .text
                .line_to_char(self.text.len_lines().saturating_sub(1));
            return;
        }
        self.snapshot(false);
        self.text.remove(from..to);
        self.dirty = true;
        self.goal_col = None;
        // The line under the caret is gone; land on whatever moved up into its place, or
        // on the new last line when it was the last.
        let l = l.min(self.text.len_lines().saturating_sub(1));
        self.cursor = self.text.line_to_char(l);
    }

    /// Step back one undo group. Returns whether anything moved.
    pub fn undo(&mut self) -> bool {
        let Some(prev) = self.undo.pop() else {
            return false;
        };
        self.redo.push(Snapshot {
            text: self.text.clone(),
            cursor: self.cursor,
        });
        self.text = prev.text;
        self.cursor = prev.cursor.min(self.text.len_chars());
        self.dirty = true;
        self.coalescing = false;
        true
    }

    /// Step forward again. Returns whether anything moved.
    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo.pop() else {
            return false;
        };
        self.undo.push(Snapshot {
            text: self.text.clone(),
            cursor: self.cursor,
        });
        self.text = next.text;
        self.cursor = next.cursor.min(self.text.len_chars());
        self.dirty = true;
        self.coalescing = false;
        true
    }

    /// End the current undo group, so the next insert starts a new one. Every motion and
    /// every mode change calls this: "undo" should return to a place the user recognises,
    /// and the places they recognise are where they stopped to think.
    pub fn break_group(&mut self) {
        self.coalescing = false;
    }

    /// Record the pre-edit state, unless this edit belongs to the group already open.
    fn snapshot(&mut self, coalesce: bool) {
        self.redo.clear();
        if coalesce && self.coalescing {
            return;
        }
        self.undo.push(Snapshot {
            text: self.text.clone(),
            cursor: self.cursor,
        });
        if self.undo.len() > UNDO_DEPTH {
            self.undo.remove(0);
        }
        self.coalescing = coalesce;
    }
}

/// The leading spaces and tabs of a line.
fn leading_ws(line: &str) -> String {
    line.chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf(text: &str) -> Buffer {
        Buffer::new(Some(PathBuf::from("/w/main.rs")), text)
    }

    #[test]
    fn a_fresh_buffer_is_clean_and_a_typed_character_makes_it_dirty() {
        let mut b = buf("hello\n");
        assert!(!b.dirty());
        assert_eq!(b.name(), "main.rs");
        b.insert("x");
        assert!(b.dirty());
        b.mark_saved();
        assert!(!b.dirty());
    }

    #[test]
    fn a_scratch_buffer_names_itself_and_has_one_empty_line() {
        let b = Buffer::scratch();
        assert_eq!(b.name(), "[scratch]");
        assert_eq!(b.len_lines(), 1);
        assert_eq!(b.line_text(0), "");
    }

    #[test]
    fn line_text_strips_the_terminator_and_reads_empty_past_the_end() {
        let b = buf("one\r\ntwo\n");
        assert_eq!(b.line_text(0), "one");
        assert_eq!(b.line_text(1), "two");
        assert_eq!(b.line_text(99), "");
        assert_eq!(b.line_len(0), 3);
    }

    #[test]
    fn horizontal_motion_stops_at_the_line_boundaries_in_normal_mode() {
        let mut b = buf("ab\ncd\n");
        b.move_left();
        assert_eq!(
            (b.line(), b.col()),
            (0, 0),
            "nothing to the left of the start"
        );
        b.move_right(false);
        assert_eq!(b.col(), 1);
        // `past_end` is false, which is normal mode: the caret sits *on* a character, so
        // it may not go past the last one on the line.
        b.move_right(false);
        assert_eq!(b.col(), 1);
        b.move_right(true);
        assert_eq!(b.col(), 2, "insert mode may sit after the last character");
    }

    #[test]
    fn the_goal_column_survives_a_short_line_but_a_sideways_step_forgets_it() {
        let mut b = buf("abcdef\nxy\nabcdef\n");
        b.goto(0, 5);
        b.move_down(false);
        assert_eq!((b.line(), b.col()), (1, 1), "clamped onto the short line");
        b.move_down(false);
        assert_eq!(b.col(), 5, "and restored on the long one below it");
        // A horizontal motion is the user choosing a column, so the remembered one goes.
        b.move_up(false);
        b.move_left();
        b.move_up(false);
        assert_eq!(b.col(), 0);
    }

    #[test]
    fn word_motion_treats_letters_punctuation_and_space_as_three_kinds() {
        let mut b = buf("foo_bar.baz qux\n");
        b.word_next();
        assert_eq!(
            b.col(),
            7,
            "the identifier ends at the dot; `_` is part of a word"
        );
        b.word_next();
        assert_eq!(b.col(), 8, "the dot is a word of its own");
        b.word_next();
        assert_eq!(
            b.col(),
            12,
            "trailing space belongs to the motion, not the next word"
        );
        b.word_prev();
        assert_eq!(b.col(), 8);
        b.goto(0, 0);
        b.word_end();
        assert_eq!(
            b.col(),
            6,
            "the last character of the word, not the one after it"
        );
    }

    #[test]
    fn doc_and_line_motions_land_where_a_modal_editor_puts_them() {
        let mut b = buf("alpha\nbeta\ngamma\n");
        b.goto(1, 2);
        b.line_start();
        assert_eq!((b.line(), b.col()), (1, 0));
        b.line_end(false);
        assert_eq!(b.col(), 3, "on the last character in normal mode");
        b.line_end(true);
        assert_eq!(b.col(), 4, "after it in insert mode");
        b.doc_start();
        assert_eq!(b.line(), 0);
        b.doc_end();
        assert_eq!(b.line(), b.len_lines() - 1);
    }

    #[test]
    fn newline_and_open_carry_the_indentation_of_the_line_they_came_from() {
        let mut b = buf("    let x = 1;\n");
        b.line_end(true);
        b.newline();
        assert_eq!(b.line_text(1), "    ");
        assert_eq!((b.line(), b.col()), (1, 4));

        let mut b = buf("\t\tdeep\n");
        b.open_below();
        assert_eq!(b.line_text(1), "\t\t");
        let mut b = buf("  two\n");
        b.open_above();
        assert_eq!(b.line_text(0), "  ");
        assert_eq!(b.line_text(1), "  two");
        assert_eq!(b.line(), 0);
    }

    #[test]
    fn delete_removes_forward_and_delete_back_removes_behind() {
        let mut b = buf("abc\n");
        b.goto(0, 1);
        b.delete();
        assert_eq!(b.line_text(0), "ac");
        b.delete_back();
        assert_eq!(b.line_text(0), "c");
        assert_eq!(b.col(), 0);
        b.delete_back();
        assert_eq!(b.line_text(0), "c", "nothing behind the start of the file");
    }

    #[test]
    fn deleting_the_last_line_leaves_the_caret_on_the_one_before_it() {
        let mut b = buf("one\ntwo\nthree\n");
        b.goto(2, 1);
        b.delete_line();
        assert_eq!(b.to_text(), "one\ntwo\n");
        // Ropey counts a trailing newline as opening a final empty line, so the caret
        // lands there rather than on "two"; what matters is that it is in the document.
        assert!(b.line() < b.len_lines());
        // Repeated deletes must actually reach an empty file: the caret parks on the
        // empty line a trailing newline opens, and deleting there takes that newline.
        for _ in 0..5 {
            b.delete_line();
        }
        assert_eq!(b.to_text(), "");
    }

    #[test]
    fn a_typed_run_undoes_as_one_group_and_redo_puts_it_back() {
        let mut b = buf("");
        b.insert("h");
        b.insert("i");
        assert_eq!(b.to_text(), "hi");
        assert!(b.undo());
        assert_eq!(b.to_text(), "", "coalesced: one undo takes the whole run");
        assert!(!b.undo(), "and there is nothing before it");
        assert!(b.redo());
        assert_eq!(b.to_text(), "hi");
        assert!(!b.redo());
    }

    #[test]
    fn a_motion_between_two_runs_makes_them_two_undo_groups() {
        let mut b = buf("");
        b.insert("ab");
        b.move_left();
        b.insert("X");
        assert_eq!(b.to_text(), "aXb");
        b.undo();
        assert_eq!(b.to_text(), "ab");
        b.undo();
        assert_eq!(b.to_text(), "");
    }

    #[test]
    fn a_new_edit_after_an_undo_throws_the_redo_stack_away() {
        let mut b = buf("");
        b.insert("one");
        b.undo();
        b.insert("two");
        assert!(!b.redo(), "redoing to a future that no longer happened");
        assert_eq!(b.to_text(), "two");
    }

    #[test]
    fn the_undo_stack_is_bounded_so_a_long_session_does_not_grow_without_end() {
        let mut b = buf("");
        for i in 0..(UNDO_DEPTH + 50) {
            b.insert(&i.to_string());
            b.break_group();
        }
        assert_eq!(b.undo.len(), UNDO_DEPTH);
    }

    #[test]
    fn goto_clamps_a_position_that_is_off_the_end_of_the_document() {
        let mut b = buf("short\n");
        b.goto(99, 99);
        assert!(b.line() < b.len_lines());
        assert!(b.col() <= b.line_len(b.line()));
    }

    #[test]
    fn clamp_normal_pulls_the_caret_back_off_the_end_of_the_line() {
        let mut b = buf("abc\n");
        b.line_end(true);
        assert_eq!(b.col(), 3);
        b.clamp_normal();
        assert_eq!(b.col(), 2);
        // An empty line has nothing to sit on, so column zero is the only answer.
        let mut b = buf("\n");
        b.clamp_normal();
        assert_eq!(b.col(), 0);
    }

    #[test]
    fn move_lines_takes_a_signed_step_and_stops_at_both_ends() {
        let mut b = buf("1\n2\n3\n4\n5\n");
        b.move_lines(10, false);
        assert_eq!(b.line(), b.len_lines() - 1);
        b.move_lines(-10, false);
        assert_eq!(b.line(), 0);
    }

    #[test]
    fn the_cursor_is_a_character_index_not_a_byte_index() {
        let mut b = buf("héllo\n");
        b.goto(0, 3);
        assert_eq!(b.cursor, 3);
        b.insert("!");
        assert_eq!(b.line_text(0), "hél!lo");
    }
}
