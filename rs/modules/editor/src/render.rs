//! [`Editor`] in, [`GridFrame`] out.
//!
//! The module owns the text and the host owns the pixels, so everything here is cells and
//! theme role names — never a colour, never a font size. A frame replaces a frame: there
//! is no incremental painting to get wrong.

use crate::editor::Editor;
use crate::keymap::SURFACE;
use avada_module_sdk::grid::{CursorShape, GridCursor, GridFrame, GridLine, GridSpan};

/// The narrowest line-number gutter, so that a 40-line file and a 400-line one do not
/// shift the text sideways as you scroll between them.
const MIN_NUMBER_WIDTH: usize = 3;

/// How wide the gutter is for a buffer of `lines` lines: the number, then one space.
fn gutter_width(lines: usize) -> usize {
    let digits = lines.to_string().len().max(MIN_NUMBER_WIDTH);
    digits + 1
}

/// Paint the current buffer.
pub fn frame(ed: &Editor) -> GridFrame {
    let buf = ed.buf();
    let h = ed.text_rows();
    let gutter = gutter_width(buf.len_lines());
    let width = usize::from(ed.cols).saturating_sub(gutter);

    let mut lines = Vec::with_capacity(h);
    for i in ed.top..ed.top + h {
        if i >= buf.len_lines() {
            // Past the end of the file. An empty line rather than a `~`: the tilde is a
            // vi habit, and a host that draws its own background needs nothing from us.
            lines.push(GridLine { spans: Vec::new() });
            continue;
        }
        let number = format!("{:>w$} ", i + 1, w = gutter - 1);
        let text = buf.line_text(i);
        // Horizontal scrolling is not implemented; a long line is cut at the pane edge
        // rather than wrapped, so that a line's screen row and its number stay the same
        // thing.
        let text: String = text.chars().take(width).collect();
        lines.push(GridLine {
            spans: vec![GridSpan::fg(number, "subtext"), GridSpan::plain(text)],
        });
    }

    GridFrame {
        surface: SURFACE.into(),
        cols: ed.cols,
        rows: h as u16,
        lines,
        cursor: Some(GridCursor {
            line: buf.line().saturating_sub(ed.top) as u16,
            col: (gutter + buf.col()).min(usize::from(ed.cols).saturating_sub(1)) as u16,
            // The shape is the only unmissable signal of which mode you are in, and it is
            // where you are already looking.
            shape: match ed.mode {
                crate::editor::Mode::Normal => CursorShape::Block,
                crate::editor::Mode::Insert => CursorShape::Bar,
            },
        }),
        status: status(ed),
    }
}

/// The status line the host draws under the grid.
fn status(ed: &Editor) -> String {
    let buf = ed.buf();
    let mut s = format!(
        "{}{}  {}:{}  {}",
        buf.name(),
        if buf.dirty() { " [+]" } else { "" },
        buf.line() + 1,
        buf.col() + 1,
        ed.mode.label(),
    );
    if ed.buffers.len() > 1 {
        s.push_str(&format!("  ({}/{})", ed.current + 1, ed.buffers.len()));
    }
    if let Some(m) = &ed.message {
        s.push_str("  ");
        s.push_str(m);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::Mode;
    use std::path::Path;

    fn ed(text: &str) -> Editor {
        let mut e = Editor {
            rows: 5,
            cols: 20,
            ..Default::default()
        };
        e.open(Path::new("/w/main.rs"), text);
        e
    }

    #[test]
    fn a_frame_is_exactly_as_tall_as_the_pane_minus_its_status_line() {
        let f = frame(&ed("a\nb\n"));
        assert_eq!(f.rows, 4);
        assert_eq!(f.lines.len(), 4);
        assert_eq!(f.surface, "editor");
    }

    #[test]
    fn lines_are_numbered_from_one_in_the_gutter() {
        let f = frame(&ed("alpha\nbeta\n"));
        assert_eq!(f.lines[0].text(), "  1 alpha");
        assert_eq!(f.lines[1].text(), "  2 beta");
        assert_eq!(f.lines[0].spans[0].fg.as_deref(), Some("subtext"));
        assert!(
            f.lines[0].spans[1].fg.is_none(),
            "the text takes the pane's own colour"
        );
    }

    #[test]
    fn past_the_end_of_the_file_is_blank() {
        let f = frame(&ed("only\n"));
        assert_eq!(f.lines[2].text(), "");
        assert_eq!(f.lines[3].text(), "");
    }

    #[test]
    fn the_gutter_widens_for_a_long_file_and_never_narrows_below_three() {
        assert_eq!(gutter_width(9), 4);
        assert_eq!(gutter_width(999), 4);
        assert_eq!(gutter_width(1000), 5);
    }

    #[test]
    fn a_line_wider_than_the_pane_is_cut_not_wrapped() {
        let f = frame(&ed(&"x".repeat(100)));
        // 20 columns, 4 of them gutter.
        assert_eq!(f.lines[0].text().chars().count(), 20);
        assert_eq!(f.lines.len(), 4, "one long line is still one row");
    }

    #[test]
    fn the_cursor_is_reported_relative_to_the_viewport_and_past_the_gutter() {
        let mut e = ed(&"line\n".repeat(40));
        e.buf_mut().goto(20, 2);
        e.follow_cursor();
        let f = frame(&e);
        let c = f.cursor.expect("a grid editor always has a cursor");
        assert_eq!(
            c.line, 3,
            "line 20 sits on the last of four rows from top=17"
        );
        assert_eq!(e.top, 17);
        assert_eq!(c.col, 6, "four columns of gutter plus column two");
    }

    #[test]
    fn the_cursor_shape_is_the_mode() {
        let mut e = ed("abc\n");
        assert_eq!(frame(&e).cursor.unwrap().shape, CursorShape::Block);
        e.mode = Mode::Insert;
        assert_eq!(frame(&e).cursor.unwrap().shape, CursorShape::Bar);
    }

    #[test]
    fn the_status_line_says_where_you_are_and_whether_it_is_saved() {
        let mut e = ed("abc\n");
        assert_eq!(status(&e), "main.rs  1:1  NOR");
        e.buf_mut().insert("z");
        assert_eq!(status(&e), "main.rs [+]  1:2  NOR");
    }

    #[test]
    fn the_status_line_counts_the_buffers_only_when_there_is_more_than_one() {
        let mut e = ed("abc\n");
        assert!(!status(&e).contains('/'));
        e.open(Path::new("/w/other.rs"), "");
        assert!(status(&e).ends_with("(2/2)"));
    }

    #[test]
    fn a_message_rides_along_on_the_status_line() {
        let mut e = ed("abc\n");
        e.message = Some("nothing to undo".into());
        assert!(status(&e).ends_with("nothing to undo"));
    }
}
