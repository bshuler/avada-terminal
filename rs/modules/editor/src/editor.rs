//! Everything the editor knows: the open buffers, which one is current, what mode it is
//! in, and how big the pane is.
//!
//! The wire loop in `main.rs` owns an [`Editor`] and does nothing to it directly except
//! hand it keys and commands; the host's I/O lives entirely on the other side of an
//! [`Effect`](crate::edit::Effect).

use crate::buffer::Buffer;
use std::path::{Path, PathBuf};

/// Whether keystrokes type or command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Letters run actions; unclaimed text is ignored.
    #[default]
    Normal,
    /// Letters are typed; only named keys and modifier chords run actions.
    Insert,
}

impl Mode {
    /// The word the status line shows.
    pub fn label(self) -> &'static str {
        match self {
            Mode::Normal => "NOR",
            Mode::Insert => "INS",
        }
    }
}

/// The module's whole editing state.
#[derive(Debug)]
pub struct Editor {
    /// Every open file, in the order they were opened. Never empty: closing the last
    /// buffer leaves a scratch one, because a pane with no buffer has nothing to paint
    /// and every caller would need a `None` arm for a state the user cannot see.
    pub buffers: Vec<Buffer>,
    /// Index into [`Editor::buffers`].
    pub current: usize,
    /// Whether keystrokes type or command.
    pub mode: Mode,
    /// Pane width in cells, as last reported by `module.grid.resize`.
    pub cols: u16,
    /// Pane height in cells, status line included.
    pub rows: u16,
    /// First visible line — the viewport, scrolled to follow the caret.
    pub top: usize,
    /// A one-shot note for the status line: "saved", or why something failed.
    pub message: Option<String>,
}

impl Default for Editor {
    fn default() -> Self {
        Self {
            buffers: vec![Buffer::scratch()],
            current: 0,
            mode: Mode::Normal,
            // A pane the host has not measured yet still has to render something; these
            // are replaced by the first `module.grid.resize`.
            cols: 80,
            rows: 24,
            top: 0,
            message: None,
        }
    }
}

impl Editor {
    /// The buffer being edited.
    pub fn buf(&self) -> &Buffer {
        &self.buffers[self.current]
    }

    /// The buffer being edited, mutably.
    pub fn buf_mut(&mut self) -> &mut Buffer {
        &mut self.buffers[self.current]
    }

    /// Whether insert mode is letting the caret sit one past the last character.
    pub fn past_end(&self) -> bool {
        self.mode == Mode::Insert
    }

    /// Open `path` with `text`, or switch to it when it is already open.
    ///
    /// Re-opening does not re-read: a buffer with unsaved edits is the user's work, and
    /// silently replacing it with what is on disk would be the one unrecoverable thing
    /// this module could do.
    pub fn open(&mut self, path: &Path, text: &str) {
        if let Some(i) = self
            .buffers
            .iter()
            .position(|b| b.path.as_deref() == Some(path))
        {
            self.current = i;
        } else {
            // The scratch buffer is a placeholder, not a document: the first real file
            // takes its place rather than opening beside it.
            if self.buffers.len() == 1 && self.buffers[0].path.is_none() && !self.buffers[0].dirty()
            {
                self.buffers.clear();
            }
            self.buffers
                .push(Buffer::new(Some(path.to_path_buf()), text));
            self.current = self.buffers.len() - 1;
        }
        self.mode = Mode::Normal;
        self.top = 0;
    }

    /// Drop the current buffer. The last one out leaves a scratch buffer behind.
    pub fn close(&mut self) {
        self.buffers.remove(self.current);
        if self.buffers.is_empty() {
            self.buffers.push(Buffer::scratch());
        }
        self.current = self.current.min(self.buffers.len() - 1);
        self.top = 0;
    }

    /// Move to the next or previous buffer, wrapping.
    pub fn cycle(&mut self, forward: bool) {
        let n = self.buffers.len();
        self.current = if forward {
            (self.current + 1) % n
        } else {
            (self.current + n - 1) % n
        };
        self.top = 0;
    }

    /// How many lines of text the pane shows: everything but the status line.
    pub fn text_rows(&self) -> usize {
        usize::from(self.rows).saturating_sub(1).max(1)
    }

    /// Scroll the viewport the smallest distance that puts the caret back on screen.
    pub fn follow_cursor(&mut self) {
        let line = self.buf().line();
        let h = self.text_rows();
        if line < self.top {
            self.top = line;
        } else if line >= self.top + h {
            self.top = line + 1 - h;
        }
    }

    /// The paths of every open buffer, for the rail's row list.
    pub fn paths(&self) -> Vec<Option<PathBuf>> {
        self.buffers.iter().map(|b| b.path.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_real_file_replaces_the_scratch_buffer() {
        let mut e = Editor::default();
        assert_eq!(e.buffers.len(), 1);
        e.open(Path::new("/w/a.rs"), "fn a() {}\n");
        assert_eq!(e.buffers.len(), 1);
        assert_eq!(e.buf().name(), "a.rs");
    }

    #[test]
    fn a_scratch_buffer_with_edits_in_it_is_kept() {
        let mut e = Editor::default();
        e.buf_mut().insert("notes");
        e.open(Path::new("/w/a.rs"), "");
        assert_eq!(e.buffers.len(), 2);
    }

    #[test]
    fn reopening_a_file_switches_to_it_and_keeps_the_unsaved_edits() {
        let mut e = Editor::default();
        e.open(Path::new("/w/a.rs"), "one\n");
        e.buf_mut().insert("X");
        e.open(Path::new("/w/b.rs"), "two\n");
        e.open(Path::new("/w/a.rs"), "one\n");
        assert_eq!(e.buffers.len(), 2);
        assert_eq!(e.current, 0);
        assert_eq!(e.buf().to_text(), "Xone\n");
    }

    #[test]
    fn closing_the_last_buffer_leaves_a_scratch_one() {
        let mut e = Editor::default();
        e.open(Path::new("/w/a.rs"), "");
        e.close();
        assert_eq!(e.buffers.len(), 1);
        assert_eq!(e.buf().name(), "[scratch]");
    }

    #[test]
    fn cycling_wraps_in_both_directions() {
        let mut e = Editor::default();
        e.open(Path::new("/w/a.rs"), "");
        e.open(Path::new("/w/b.rs"), "");
        e.cycle(true);
        assert_eq!(e.current, 0);
        e.cycle(false);
        assert_eq!(e.current, 1);
    }

    #[test]
    fn the_viewport_follows_the_caret_by_the_smallest_step() {
        // Five cells, four text rows: the last one is the host's status bar.
        let mut e = Editor {
            rows: 5,
            ..Default::default()
        };
        e.open(Path::new("/w/a.rs"), &"x\n".repeat(40));
        e.buf_mut().goto(10, 0);
        e.follow_cursor();
        assert_eq!(
            e.top, 7,
            "scrolling down should just bring the line into view"
        );
        e.buf_mut().goto(2, 0);
        e.follow_cursor();
        assert_eq!(e.top, 2);
    }
}
