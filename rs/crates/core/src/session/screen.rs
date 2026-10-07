//! Rendered-screen serializer: drive an `alacritty_terminal` `Term` from the pty byte
//! stream and serialize its grid to clean text for control `mode:"screen"` reads.
//! This replaces the renderer's xterm.js serialize — a capability GAIN: screen reads
//! need no GUI.
//!
//! Exact text is **best-effort parity** with xterm. Divergences to expect:
//!   * line wrapping at the right margin may break at a different column,
//!   * wide (CJK/emoji) chars: the trailing spacer cell is dropped so the glyph
//!     appears once (xterm serialize does the same, but column counts can differ),
//!   * trailing whitespace on each line and trailing blank lines are trimmed.
//!
//! The "is this pane awaiting input?" heuristic (`control::output::detectAwaitingInput`)
//! is meant to run on THIS rendered text, not the raw stream — keep it a separate
//! concern (owned by `core-io`); this module only produces the clean screen.

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;

/// Lines of history the mirror keeps — what a re-attaching view's scrollback is rebuilt
/// from. Screen reads still serialize only the visible viewport.
pub const SCROLLBACK_LINES: usize = 2000;

/// A live VTE screen: an `alacritty_terminal` `Term` fed incrementally from the pty
/// byte stream, plus its ANSI parser. One per session, mirroring how the renderer
/// kept a live xterm per pane. `render()` produces the clean text for a screen read.
pub struct Screen {
    term: Term<VoidListener>,
    parser: Processor,
    cols: usize,
    rows: usize,
}

impl Screen {
    /// A blank screen of `cols`×`rows`, keeping [`SCROLLBACK_LINES`] of history so a
    /// re-attaching view's scrollback can be rebuilt from it (see [`Screen::repaint`]).
    #[tracing::instrument(level = "debug")]
    pub fn new(cols: u16, rows: u16) -> Self {
        let cols = (cols as usize).max(1);
        let rows = (rows as usize).max(1);
        let config = Config {
            scrolling_history: SCROLLBACK_LINES,
            ..Config::default()
        };
        let term = Term::new(config, &TermSize::new(cols, rows), VoidListener);
        Self {
            term,
            parser: Processor::new(),
            cols,
            rows,
        }
    }

    /// Feed a raw output chunk (same bytes the renderer's terminal would receive).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn advance(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    /// Whether the program on the other end has **bracketed paste** mode on (DECSET 2004).
    ///
    /// The headless mirror is fed the same bytes the GUI grid is, so it sees the same mode
    /// switches — which is what lets a daemon-backed pane prepare a paste correctly without
    /// a renderer anywhere in the process. Callers must `sync_screen()` first; a mode read
    /// off a stale mirror is the one way this can answer for a program that has since exited.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn bracketed_paste(&self) -> bool {
        use alacritty_terminal::term::TermMode;
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// Current grid dimensions `(cols, rows)` — what remote clients must emulate at.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn dims(&self) -> (u16, u16) {
        (self.cols as u16, self.rows as u16)
    }

    /// Resize the screen grid. No-op if unchanged dimensions are passed.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let cols = (cols as usize).max(1);
        let rows = (rows as usize).max(1);
        if cols == self.cols && rows == self.rows {
            return;
        }
        self.term.resize(TermSize::new(cols, rows));
        self.cols = cols;
        self.rows = rows;
    }

    /// Serialize the visible grid to clean text: one line per screen row, trailing
    /// whitespace trimmed per line, trailing blank lines dropped. ANSI styling is
    /// already consumed by the parser, so the output is plain characters only.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn render(&self) -> String {
        let grid = self.term.grid();
        let mut lines: Vec<String> = Vec::with_capacity(self.rows);
        for l in 0..self.rows as i32 {
            let row = &grid[Line(l)];
            let mut s = String::with_capacity(self.cols);
            for c in 0..self.cols {
                let cell = &row[Column(c)];
                // The placeholder cell after a wide char carries a blank; skip it so
                // the wide glyph is emitted exactly once.
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                s.push(cell.c);
            }
            // Empty alacritty cells hold a space; trim them off the right edge.
            while s.ends_with(' ') {
                s.pop();
            }
            lines.push(s);
        }
        while matches!(lines.last(), Some(l) if l.is_empty()) {
            lines.pop();
        }
        lines.join("\n")
    }
}

impl Screen {
    /// Serialize the mirror back to **ANSI**: a self-contained byte sequence that, fed to a
    /// fresh terminal of the same size, reproduces this session — the scrollback, every
    /// visible cell with its colours and attributes, the cursor position and visibility,
    /// and the input modes a client routes keys and mouse by.
    ///
    /// This is the whole of a re-attaching view's seed. The raw replay is a rolling
    /// 128 KiB window, so for a program that redraws in place with cursor-*relative* moves
    /// (Claude Code's interface, any diffing TUI) its front edge lands mid-frame and
    /// replaying it draws those moves from the wrong origin — garbling the scrollback as
    /// well as the screen. The mirror saw every byte, so its history and screen are clean.
    ///
    /// History is written as plain lines from the top, so it scrolls into the client's
    /// scrollback; soft-wrapped rows are written full width without a line break, so the
    /// client still knows they wrap and can reflow them. With the alternate screen up, the
    /// primary (history, screen and cursor) is painted first and the alternate screen on
    /// top, so leaving vim/less shows the shell underneath.
    ///
    /// Callers must `sync_screen()` first. `&mut` only because reading the hidden primary
    /// grid means swapping it in; the mirror is left exactly as it was.
    #[tracing::instrument(level = "debug", skip(self))]
    pub fn repaint(&mut self) -> String {
        use alacritty_terminal::term::TermMode;
        let mode = *self.term.mode();
        let mut out = String::with_capacity(self.cols * self.rows + 64);
        // Reset attributes, leave any alternate screen, make sure rows wrap, then clear the
        // screen AND the scrollback — a seed fed to a grid that already holds a copy must not
        // stack a second one. ED 2 before ED 3: on the primary screen ED 2 pushes the
        // viewport into history, which ED 3 then drops.
        out.push_str("\x1b[0m\x1b[?1049l\x1b[?7h\x1b[H\x1b[2J\x1b[3J");

        if mode.contains(TermMode::ALT_SCREEN) {
            // alacritty only exposes the active grid. Swap the primary in to read it; swapping
            // back resets the alternate grid, so restore it from a copy.
            let alt = self.term.grid().clone();
            self.term.swap_alt();
            paint_grid(self.term.grid(), self.cols, &mut out);
            push_cursor(self.term.grid(), &mut out);
            self.term.swap_alt();
            *self.term.grid_mut() = alt;
            // DECSET 1049 saves the primary cursor just placed, then clears the alternate.
            out.push_str("\x1b[?1049h\x1b[H");
        }
        paint_grid(self.term.grid(), self.cols, &mut out);
        out.push_str("\x1b[0m");

        let set = |on: bool, n: u16, out: &mut String| {
            out.push_str(&format!("\x1b[?{n}{}", if on { 'h' } else { 'l' }));
        };
        set(mode.contains(TermMode::APP_CURSOR), 1, &mut out);
        set(mode.contains(TermMode::LINE_WRAP), 7, &mut out);
        set(mode.contains(TermMode::MOUSE_REPORT_CLICK), 1000, &mut out);
        set(mode.contains(TermMode::MOUSE_DRAG), 1002, &mut out);
        set(mode.contains(TermMode::MOUSE_MOTION), 1003, &mut out);
        set(mode.contains(TermMode::FOCUS_IN_OUT), 1004, &mut out);
        set(mode.contains(TermMode::SGR_MOUSE), 1006, &mut out);
        set(mode.contains(TermMode::BRACKETED_PASTE), 2004, &mut out);
        out.push_str(if mode.contains(TermMode::APP_KEYPAD) {
            "\x1b="
        } else {
            "\x1b>"
        });
        push_cursor(self.term.grid(), &mut out);
        set(mode.contains(TermMode::SHOW_CURSOR), 25, &mut out);
        out
    }
}

/// Write every line of `grid` — history from the oldest, then the screen — top to bottom
/// from the home position, so the last `screen_lines` land on screen and the rest scroll
/// into the receiver's scrollback. Leaves the pen at its default.
fn paint_grid(grid: &alacritty_terminal::grid::Grid<Cell>, cols: usize, out: &mut String) {
    use alacritty_terminal::grid::Dimensions;
    let top = -(grid.history_size() as i32);
    let bottom = grid.screen_lines() as i32;
    let mut pen = Pen::default();
    let mut wrapped = false;
    for l in top..bottom {
        let row = &grid[Line(l)];
        let wraps = row[Column(cols - 1)].flags.contains(Flags::WRAPLINE);
        // A wrapped row is written full width so the next row's first glyph wraps onto it
        // (re-creating the soft wrap); any other row stops at its last non-blank or styled
        // cell, the clear already painted the rest.
        let end = if wraps {
            cols
        } else {
            (0..cols)
                .rev()
                .find(|&c| {
                    let cell = &row[Column(c)];
                    cell.c != ' ' || Pen::of(cell) != Pen::default()
                })
                .map_or(0, |c| c + 1)
        };
        if wrapped && end == 0 {
            // A blank continuation: one space still triggers the wrap that marks it so.
            out.push(' ');
        }
        let mut first = wrapped;
        for c in 0..end {
            let cell = &row[Column(c)];
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            let want = Pen::of(cell);
            if want != pen {
                out.push_str(&want.sgr());
                pen = want;
            }
            out.push(cell.c);
            if let Some(zw) = cell.zerowidth() {
                out.extend(zw.iter());
            }
            if first && pen != Pen::default() {
                // The glyph that wrapped may have scrolled a new line in, and a scrolled-in
                // line is filled with the pen's background. Erase the rest of it plain.
                out.push_str("\x1b[0m\x1b[K");
                pen = Pen::default();
            }
            first = false;
        }
        if pen != Pen::default() {
            // Same reason: the line feed below scrolls with the pen's background.
            out.push_str("\x1b[0m");
            pen = Pen::default();
        }
        if !wraps && l + 1 < bottom {
            out.push_str("\r\n");
        }
        wrapped = wraps;
    }
}

/// Place the cursor where `grid` has it (screen-relative, 1-based).
fn push_cursor(grid: &alacritty_terminal::grid::Grid<Cell>, out: &mut String) {
    let cursor = grid.cursor.point;
    out.push_str(&format!(
        "\x1b[{};{}H",
        cursor.line.0 + 1,
        cursor.column.0 + 1
    ));
}

/// The graphic rendition one cell is drawn with — what [`Screen::repaint`] diffs between
/// cells so it only emits an SGR where the style actually changes.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Pen {
    fg: alacritty_terminal::vte::ansi::Color,
    bg: alacritty_terminal::vte::ansi::Color,
    flags: Flags,
}

impl Default for Pen {
    fn default() -> Self {
        use alacritty_terminal::vte::ansi::{Color, NamedColor};
        Self {
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            flags: Flags::empty(),
        }
    }
}

impl Pen {
    /// The style flags a repaint reproduces; layout flags (wrap, wide-char spacers) are not
    /// style and must not split a run.
    const STYLE: Flags = Flags::BOLD
        .union(Flags::DIM)
        .union(Flags::ITALIC)
        .union(Flags::ALL_UNDERLINES)
        .union(Flags::INVERSE)
        .union(Flags::HIDDEN)
        .union(Flags::STRIKEOUT);

    fn of(cell: &alacritty_terminal::term::cell::Cell) -> Self {
        Self {
            fg: cell.fg,
            bg: cell.bg,
            flags: cell.flags & Self::STYLE,
        }
    }

    /// A full SGR for this pen, starting from a reset so no attribute leaks across.
    fn sgr(&self) -> String {
        let mut p: Vec<String> = vec!["0".into()];
        for (flag, code) in [
            (Flags::BOLD, "1"),
            (Flags::DIM, "2"),
            (Flags::ITALIC, "3"),
            (Flags::UNDERLINE, "4"),
            (Flags::DOUBLE_UNDERLINE, "21"),
            (Flags::UNDERCURL, "4:3"),
            (Flags::DOTTED_UNDERLINE, "4:4"),
            (Flags::DASHED_UNDERLINE, "4:5"),
            (Flags::INVERSE, "7"),
            (Flags::HIDDEN, "8"),
            (Flags::STRIKEOUT, "9"),
        ] {
            if self.flags.contains(flag) {
                p.push(code.into());
            }
        }
        if let Some(c) = color_sgr(self.fg, false) {
            p.push(c);
        }
        if let Some(c) = color_sgr(self.bg, true) {
            p.push(c);
        }
        format!("\x1b[{}m", p.join(";"))
    }
}

/// The SGR parameter selecting `color` as foreground (or background), `None` for the
/// default (which the leading reset already selected).
fn color_sgr(color: alacritty_terminal::vte::ansi::Color, bg: bool) -> Option<String> {
    use alacritty_terminal::vte::ansi::{Color, NamedColor};
    let base = if bg { 40 } else { 30 };
    match color {
        Color::Spec(rgb) => Some(format!("{};2;{};{};{}", base + 8, rgb.r, rgb.g, rgb.b)),
        Color::Indexed(i) => Some(format!("{};5;{i}", base + 8)),
        Color::Named(n) => {
            let i = n as usize;
            match i {
                0..=7 => Some((base + i).to_string()),
                8..=15 => Some((base + 60 + i - 8).to_string()),
                _ => match n {
                    // A dim named colour is how alacritty records SGR 2 on a palette colour;
                    // the DIM flag already says so, so emit the base colour.
                    NamedColor::DimBlack
                    | NamedColor::DimRed
                    | NamedColor::DimGreen
                    | NamedColor::DimYellow
                    | NamedColor::DimBlue
                    | NamedColor::DimMagenta
                    | NamedColor::DimCyan
                    | NamedColor::DimWhite => {
                        let k = i - NamedColor::DimBlack as usize;
                        Some((base + k).to_string())
                    }
                    _ => None,
                },
            }
        }
    }
}

#[cfg(test)]
impl Screen {
    /// Like [`Screen::render`], but history included — the whole scrollback, then the screen.
    pub(crate) fn render_all(&self) -> String {
        use alacritty_terminal::grid::Dimensions;
        let grid = self.term.grid();
        let mut lines: Vec<String> = Vec::new();
        for l in -(grid.history_size() as i32)..self.rows as i32 {
            let row = &grid[Line(l)];
            let mut s: String = (0..self.cols)
                .map(|c| &row[Column(c)])
                .filter(|cell| !cell.flags.contains(Flags::WIDE_CHAR_SPACER))
                .map(|cell| cell.c)
                .collect();
            while s.ends_with(' ') {
                s.pop();
            }
            lines.push(s);
        }
        while matches!(lines.last(), Some(l) if l.is_empty()) {
            lines.pop();
        }
        lines.join("\n")
    }

    /// Whether row `line` (negative = history) is soft-wrapped onto the next.
    pub(crate) fn wraps(&self, line: i32) -> bool {
        self.term.grid()[Line(line)][Column(self.cols - 1)]
            .flags
            .contains(Flags::WRAPLINE)
    }

    /// The background of one cell.
    pub(crate) fn bg(&self, line: i32, col: usize) -> alacritty_terminal::vte::ansi::Color {
        self.term.grid()[Line(line)][Column(col)].bg
    }
}

/// One-shot convenience: render `bytes` onto a fresh `cols`×`rows` screen and return
/// the serialized text. Equivalent to `new` + `advance` + `render`; handy for tests
/// and for rendering a captured replay buffer without keeping a live `Screen`.
#[tracing::instrument(level = "debug", ret)]
pub fn render_bytes(cols: u16, rows: u16, bytes: &[u8]) -> String {
    let mut screen = Screen::new(cols, rows);
    screen.advance(bytes);
    screen.render()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_plain_text_on_the_first_line() {
        assert_eq!(render_bytes(20, 5, b"hello world"), "hello world");
    }

    #[test]
    fn handles_crlf_line_breaks() {
        assert_eq!(
            render_bytes(20, 5, b"line one\r\nline two"),
            "line one\nline two"
        );
    }

    #[test]
    fn trims_trailing_blank_lines_and_trailing_spaces() {
        // Cursor writes a short line then a few blank rows follow.
        assert_eq!(render_bytes(20, 6, b"top\r\n\r\n   \r\n"), "top");
    }

    #[test]
    fn clear_screen_and_home_resets_then_writes() {
        // SGR color + clear + home, then text — styling must not leak into the text.
        let bytes = b"junk\x1b[2J\x1b[H\x1b[31mRED\x1b[0m";
        assert_eq!(render_bytes(20, 5, bytes), "RED");
    }

    #[test]
    fn absolute_cursor_positioning_places_text() {
        // Move to row 3, col 5 (1-based) and write — rows 1-2 stay blank but precede
        // content, so they're kept; row 3 has content.
        let out = render_bytes(20, 5, b"\x1b[3;5HX");
        let lines: Vec<&str> = out.split('\n').collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "");
        assert_eq!(lines[1], "");
        assert_eq!(lines[2], "    X"); // 4 leading spaces → column 5
    }

    #[test]
    fn carriage_return_overwrites_in_place() {
        assert_eq!(render_bytes(20, 3, b"aaaa\rbb"), "bbaa");
    }

    #[test]
    fn strips_sgr_styling_sequences() {
        assert_eq!(
            render_bytes(40, 3, b"\x1b[1;32mgreen bold\x1b[0m text"),
            "green bold text"
        );
    }

    /// A Claude-Code-style interface: a frame drawn once, then redrawn in place by moving
    /// the cursor UP from wherever it is (`CSI n A`) and rewriting lines — no absolute
    /// positioning anywhere.
    fn relative_redraw_stream() -> (Vec<u8>, usize) {
        let mut b: Vec<u8> = b"$ claude\r\n".to_vec();
        b.extend_from_slice(b"\x1b[1mheader\x1b[0m\r\n> \x1b[32mtyping\x1b[0m\r\nstatus 0");
        // Everything from here on is relative; a seed that starts here has lost its origin.
        let cut = b.len();
        for i in 1..=3 {
            b.extend_from_slice(b"\r\x1b[2A\x1b[2K\x1b[1;35mheader ");
            b.extend_from_slice(i.to_string().as_bytes());
            b.extend_from_slice(b"\x1b[0m\r\n\r\n\x1b[2Kstatus ");
            b.extend_from_slice(i.to_string().as_bytes());
        }
        (b, cut)
    }

    #[test]
    fn a_truncated_relative_redraw_garbles_and_the_repaint_does_not() {
        let (stream, cut) = relative_redraw_stream();
        let mut live = Screen::new(30, 8);
        live.advance(&stream);
        let want = live.render();
        assert_eq!(want, "$ claude\nheader 3\n> typing\nstatus 3");

        // The bug: a seed whose front edge fell inside the redraws draws them from the wrong
        // origin (here, the top-left), so the pane shows a scrambled screen.
        let truncated = render_bytes(30, 8, &stream[cut..]);
        assert_ne!(truncated, want, "the truncated replay should garble");

        // The fix: the same truncated replay, then the mirror's repaint, lands on the screen
        // exactly as the program left it — text, styles, cursor and all.
        let mut seeded = Screen::new(30, 8);
        seeded.advance(&stream[cut..]);
        seeded.advance(live.repaint().as_bytes());
        assert_eq!(seeded.render(), want);
        assert_eq!(
            seeded.repaint(),
            live.repaint(),
            "styles and cursor survive"
        );
        // And the program's next relative redraw lands where it should.
        let next = b"\r\x1b[2A\x1b[2Kheader 4\r\n\r\n\x1b[2Kstatus 4";
        live.advance(next);
        seeded.advance(next);
        assert_eq!(seeded.render(), live.render());
        assert_eq!(seeded.render(), "$ claude\nheader 4\n> typing\nstatus 4");
    }

    #[test]
    fn repaint_round_trips_colours_wide_chars_and_modes() {
        let mut s = Screen::new(20, 4);
        s.advance(
            "\x1b[?2004h\x1b[?1h\x1b[38;5;208mor\x1b[48;2;1;2;3mbg\x1b[0m \x1b[4;7m宽\x1b[0m\r\n\x1b[91mbright\x1b[?25l"
                .as_bytes(),
        );
        let mut copy = Screen::new(20, 4);
        copy.advance(s.repaint().as_bytes());
        assert_eq!(copy.render(), s.render());
        assert_eq!(copy.repaint(), s.repaint());
        assert!(copy.bracketed_paste());
    }

    #[test]
    fn repaint_restores_the_alternate_screen_over_the_primary() {
        let mut s = Screen::new(20, 4);
        s.advance(b"old\r\nmore\r\nlines\r\nhere\r\nshell $ \x1b[?1049h\x1b[Hvim\x1b[2;3H");
        let alt_before = s.render();
        let mut copy = Screen::new(20, 4);
        copy.advance(s.repaint().as_bytes());
        assert_eq!(copy.render(), "vim");
        // Reading the hidden primary must leave the mirror's alternate screen untouched.
        assert_eq!(s.render(), alt_before);
        assert_eq!(copy.repaint(), s.repaint(), "cursor and modes survive");
        // Leaving the alternate screen shows the shell, history and cursor intact.
        copy.advance(b"\x1b[?1049l");
        s.advance(b"\x1b[?1049l");
        assert_eq!(copy.render(), "more\nlines\nhere\nshell $");
        assert_eq!(copy.render_all(), s.render_all());
        copy.advance(b"ls");
        assert_eq!(copy.render(), "more\nlines\nhere\nshell $ ls");
    }

    #[test]
    fn repaint_carries_the_scrollback() {
        let mut s = Screen::new(20, 4);
        for i in 0..50 {
            s.advance(format!("\x1b[3{}mline {i}\x1b[0m\r\n", i % 8).as_bytes());
        }
        s.advance(b"$ ");
        let mut copy = Screen::new(20, 4);
        copy.advance(s.repaint().as_bytes());
        assert!(copy.render_all().starts_with("line 0\nline 1\n"));
        assert_eq!(copy.render_all(), s.render_all());
        assert_eq!(copy.repaint(), s.repaint(), "colours and cursor survive");
        // Feeding a seed to a grid that already holds one replaces it rather than stacking.
        copy.advance(s.repaint().as_bytes());
        assert_eq!(copy.render_all(), s.render_all());
    }

    #[test]
    fn the_scrollback_is_capped() {
        let mut s = Screen::new(20, 4);
        for i in 0..SCROLLBACK_LINES + 100 {
            s.advance(format!("{i}\r\n").as_bytes());
        }
        let all = s.render_all();
        assert_eq!(all.lines().count(), SCROLLBACK_LINES + 3);
        // 2100 numbered lines plus the blank one the cursor sits on; 4 on screen.
        assert!(all.starts_with("97\n"), "oldest lines dropped");
    }

    #[test]
    fn repaint_keeps_soft_wraps_including_a_wide_char_at_the_edge() {
        let mut s = Screen::new(10, 4);
        // A 15-char line wraps once; then 9 cells + a wide char that can't fit in the last
        // column wraps leaving a spacer; then enough lines to push both into history.
        s.advance(b"abcdefghijklmno\r\n123456789\xe5\xae\xbd!\r\nx\r\ny\r\nz\r\nw");
        let mut copy = Screen::new(10, 4);
        copy.advance(s.repaint().as_bytes());
        assert_eq!(copy.render_all(), s.render_all());
        assert_eq!(copy.repaint(), s.repaint());
        for l in -4..4 {
            assert_eq!(copy.wraps(l), s.wraps(l), "row {l}");
        }
        assert!(copy.wraps(-4) && copy.wraps(-2), "both wraps survive");
        // Reflow at a wider width rejoins the wrapped lines.
        copy.resize(20, 4);
        assert!(copy
            .render_all()
            .starts_with("abcdefghijklmno\n123456789宽!"));
    }

    #[test]
    fn a_coloured_row_does_not_bleed_into_the_lines_below() {
        use alacritty_terminal::vte::ansi::{Color, NamedColor};
        let mut s = Screen::new(10, 3);
        // Coloured rows, a coloured row that wraps, then blank rows, enough to scroll.
        s.advance(b"\x1b[41mred\x1b[0m\r\n\r\n\x1b[44mabcdefghijkl\x1b[0m\r\n\r\n\r\n\r\nend");
        let mut copy = Screen::new(10, 3);
        copy.advance(s.repaint().as_bytes());
        assert_eq!(copy.render_all(), s.render_all());
        let plain = Color::Named(NamedColor::Background);
        for l in -5..3 {
            for c in 0..10 {
                assert_eq!(copy.bg(l, c), s.bg(l, c), "cell {l},{c}");
            }
        }
        assert_eq!(copy.bg(-4, 0), plain, "the blank row under red stays plain");
    }

    #[test]
    fn resize_reflows_and_keeps_rendering() {
        let mut s = Screen::new(10, 4);
        s.advance(b"hello");
        assert_eq!(s.render(), "hello");
        s.resize(20, 6);
        s.advance(b" again");
        assert!(s.render().contains("hello"));
        assert!(s.render().contains("again"));
    }
}
