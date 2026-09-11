//! Table viewer typesetting (docs/viewer-panes-plan.md, track V2): how a set of parsed
//! records is *drawn*. Owned by the V2 track.
//!
//! The parsing half — turning a file's bytes into records of fields — moved to the
//! shared [`avada_csv_parse`] crate, so the app's built-in table view and the
//! `avada-table` module cannot drift. What stays here is the projection the host owns:
//! [`Layout`] decides the width of every column, which columns are numbers (and so sit
//! flush right), and the padded, escaped text each cell hands to the markdown channel
//! the view already has. This is exactly the work `project_doc` does for a `dense`
//! [`Block::Table`](avada_module_sdk::doc::Block): the module ships raw cells, the host
//! measures and pads them.
//!
//! The seam with `viewpane.rs` is deliberately thin: records in, plain `String`s out.
//! Nothing here knows what a `ViewRow` is.

/// The widest a column is allowed to grow, in characters. A cell longer than this is
/// elided with `…` rather than pushing every other column off the right edge — one
/// 4,000-character JSON blob in a "notes" column must not hide the id column beside it.
pub const MAX_COL_CHARS: usize = 64;

/// How a set of records is drawn: one width per column, and which columns are numeric.
/// Computed once per file and applied to every cell, so a column is exactly as wide as
/// its widest (capped) member and no wider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// Column widths in characters of *display* text, each at most the cap.
    pub widths: Vec<usize>,
    /// A column whose every non-empty body cell parses as a number. The header is not
    /// consulted — "qty" is not a number, and the column still is.
    pub numeric: Vec<bool>,
}

impl Layout {
    /// Measure `records` (first one the header) with cells wider than `cap` elided.
    /// A ragged file's column count is its widest row's, so nothing is dropped.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn of(records: &[Vec<String>], cap: usize) -> Self {
        let cols = records.iter().map(Vec::len).max().unwrap_or(0);
        let mut widths = vec![0usize; cols];
        let mut numeric = vec![true; cols];
        let mut has_body = vec![false; cols];
        for (i, rec) in records.iter().enumerate() {
            for (j, cell) in rec.iter().enumerate() {
                widths[j] = widths[j].max(display(cell, cap).chars().count());
                if i > 0 && !cell.trim().is_empty() {
                    has_body[j] = true;
                    if cell.trim().parse::<f64>().is_err() {
                        numeric[j] = false;
                    }
                }
            }
        }
        for j in 0..cols {
            numeric[j] = numeric[j] && has_body[j];
        }
        Self { widths, numeric }
    }

    /// The column count — the widest row's.
    #[tracing::instrument(level = "debug", ret)]
    pub fn columns(&self) -> usize {
        self.widths.len()
    }

    /// Column `j` of `record` as the escaped, padded markdown the view draws — `""`
    /// for a column the row does not reach.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn cell(&self, record: &[String], j: usize, cap: usize) -> String {
        let raw = record.get(j).map(String::as_str).unwrap_or("");
        let shown = display(raw, cap);
        escape_markdown(&pad(&shown, self.widths[j], self.numeric[j]))
    }
}

/// A cell as one line of at most `cap` characters: an embedded line break becomes `¶`
/// (a pilcrow says "there was a break here" in every font this app ships), a tab a
/// space, and anything past the cap is cut and marked with `…`.
#[tracing::instrument(level = "debug", ret)]
pub fn display(cell: &str, cap: usize) -> String {
    let flat: String = cell
        .replace("\r\n", "¶")
        .chars()
        .map(|c| match c {
            '\n' | '\r' => '¶',
            '\t' => ' ',
            c => c,
        })
        .collect();
    if flat.chars().count() <= cap {
        return flat;
    }
    let mut out: String = flat.chars().take(cap.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Pad `s` to `width` characters with U+00A0 — a space markdown does not collapse and
/// [`crate::viewpane`]'s copy path trims away again — on the left for a number so the
/// units line up, on the right for words.
#[tracing::instrument(level = "debug", ret)]
pub fn pad(s: &str, width: usize, right: bool) -> String {
    let n = s.chars().count();
    if n >= width {
        return s.to_string();
    }
    let fill: String = "\u{a0}".repeat(width - n);
    if right {
        format!("{fill}{s}")
    } else {
        format!("{s}{fill}")
    }
}

/// Every character markdown could read as syntax, backslashed so a cell that says
/// `*not bold*` shows the asterisks. The same set `highlight.rs` escapes: what
/// round-trips a source line round-trips a cell.
const META: &str = "\\`*_{}[]()#+-.!<>&\"'~|";

/// `s` as markdown that renders as exactly `s`.
#[tracing::instrument(level = "debug", ret)]
pub fn escape_markdown(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if META.contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recs(v: &[&[&str]]) -> Vec<Vec<String>> {
        v.iter()
            .map(|r| r.iter().map(|s| s.to_string()).collect())
            .collect()
    }

    #[test]
    fn widths_are_the_widest_cell_per_column_and_ragged_rows_widen_the_grid() {
        let l = Layout::of(
            &recs(&[
                &["id", "name"],
                &["1", "Ada"],
                &["22"],
                &["3", "Grace", "x"],
            ]),
            64,
        );
        assert_eq!(l.columns(), 3);
        assert_eq!(l.widths, vec![2, 5, 1]);
    }

    #[test]
    fn a_column_of_numbers_is_numeric_and_the_header_does_not_count() {
        let l = Layout::of(
            &recs(&[
                &["qty", "name", "price", "none"],
                &["4", "bolt", "1.20", ""],
                &["", "nut", "-3e2", ""],
            ]),
            64,
        );
        assert_eq!(l.numeric, vec![true, false, true, false]);
    }

    #[test]
    fn a_cell_past_the_cap_is_elided_rather_than_widening_the_column() {
        let long = "x".repeat(200);
        let l = Layout::of(&recs(&[&["a"], &[long.as_str()]]), 10);
        assert_eq!(l.widths, vec![10]);
        let shown = display(&long, 10);
        assert_eq!(shown.chars().count(), 10);
        assert!(shown.ends_with('…'));
        assert_eq!(display("short", 10), "short");
    }

    #[test]
    fn a_line_break_inside_a_cell_shows_as_a_pilcrow_and_a_tab_as_a_space() {
        assert_eq!(display("a\nb\r\nc\td", 64), "a¶b¶c d");
    }

    #[test]
    fn padding_is_nbsp_on_the_side_that_lines_the_column_up() {
        assert_eq!(pad("ab", 4, false), "ab\u{a0}\u{a0}");
        assert_eq!(pad("12", 4, true), "\u{a0}\u{a0}12");
        assert_eq!(pad("abcd", 2, false), "abcd");
    }

    #[test]
    fn a_cell_is_escaped_so_markdown_draws_it_verbatim() {
        assert_eq!(
            escape_markdown("*a* <b> [c](d)"),
            "\\*a\\* \\<b\\> \\[c\\]\\(d\\)"
        );
        assert_eq!(escape_markdown("plain 123"), "plain 123");
        let l = Layout::of(&recs(&[&["n", "v"], &["*x*", "7"]]), 64);
        assert_eq!(l.cell(&recs(&[&["*x*", "7"]])[0], 0, 64), "\\*x\\*");
        // A numeric column is padded on the left, a missing cell is empty padding.
        assert_eq!(l.cell(&recs(&[&["*x*"]])[0], 1, 64), "\u{a0}");
    }
}
