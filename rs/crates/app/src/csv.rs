//! Table viewer pane (docs/viewer-panes-plan.md, track V2): RFC 4180 CSV/TSV parsing into
//! rows and cells. Owned by the V2 track.
//!
//! Two halves. [`parse`] turns a file's text into records of fields — quoted fields,
//! doubled quotes, embedded line breaks, CRLF, a trailing newline, ragged rows — with
//! no dependency beyond `std`, because a `csv` crate would be a Cargo.toml change and
//! this parser is forty lines. [`Layout`] then decides how those records are *drawn*:
//! the width of every column, which columns are numbers (and so sit flush right), and
//! the padded, escaped text each cell hands to the markdown channel the view already has.
//!
//! The seam with `viewpane.rs` is deliberately thin: `parse` + `Layout::of` in, plain
//! `String`s out. Nothing here knows what a `ViewRow` is.

use std::fmt;
use std::path::Path;

/// The widest a column is allowed to grow, in characters. A cell longer than this is
/// elided with `…` rather than pushing every other column off the right edge — one
/// 4,000-character JSON blob in a "notes" column must not hide the id column beside it.
pub const MAX_COL_CHARS: usize = 64;

/// The one thing a lenient reader still refuses: a quote that opens and never closes.
/// Everything past it would be one field swallowing the rest of the file, so the
/// honest answer is "this is not CSV", said with the line the quote opened on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// 1-based line of the offending opening quote.
    pub line: usize,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unterminated quoted field starting on line {}",
            self.line
        )
    }
}

impl std::error::Error for ParseError {}

/// The field separator a file's extension promises: a tab for `.tsv`/`.tab`, a comma
/// for everything else. Extension-driven for the same reason [`crate::viewpane::kind_for_file`]
/// is — sniffing the content would make the same file open differently on different days.
#[tracing::instrument(level = "debug", ret)]
pub fn delimiter_for(path: &Path) -> char {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "tsv" | "tab" => '\t',
        _ => ',',
    }
}

/// The format's name for a notice: what the reader was asked to parse the file as.
#[tracing::instrument(level = "debug", ret)]
pub fn format_name(delim: char) -> &'static str {
    if delim == '\t' {
        "TSV"
    } else {
        "CSV"
    }
}

/// RFC 4180 records. Lenient wherever leniency loses nothing:
///
/// - `\n`, `\r\n` and a lone `\r` all end a record outside quotes; inside quotes every
///   byte is kept verbatim, so a multi-line cell stays one cell.
/// - `""` inside a quoted field is one `"`. A `"` in the middle of an unquoted field is
///   a literal `"` (spreadsheets write these).
/// - Text after a closing quote and before the delimiter is kept rather than rejected.
/// - A record with no content at all (a blank line, the trailing newline) is skipped —
///   an empty *line* is not a row of empty cells.
/// - Rows keep their own field count; squaring them off is the layout's job.
///
/// The one hard error is an unterminated quote, see [`ParseError`].
#[tracing::instrument(level = "debug", skip_all)]
pub fn parse(text: &str, delim: char) -> Result<Vec<Vec<String>>, ParseError> {
    let mut records: Vec<Vec<String>> = Vec::new();
    let mut record: Vec<String> = Vec::new();
    let mut field = String::new();
    // Whether the record in progress has seen anything — a delimiter, a quote or a
    // character. A record that saw nothing is the blank line that gets skipped.
    let mut seen = false;
    let mut line = 1usize;
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '"' && field.is_empty() {
            // A quote at the start of a field opens a quoted one, which runs to its
            // closing quote however many lines that is. Later in a field it is a literal.
            seen = true;
            let opened_on = line;
            let mut closed = false;
            while let Some(q) = chars.next() {
                if q == '"' {
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        field.push('"');
                    } else {
                        closed = true;
                        break;
                    }
                } else {
                    if q == '\n' {
                        line += 1;
                    }
                    field.push(q);
                }
            }
            if !closed {
                return Err(ParseError { line: opened_on });
            }
            // Anything between the closing quote and the delimiter is kept verbatim.
            while let Some(&t) = chars.peek() {
                if t == delim || t == '\n' || t == '\r' {
                    break;
                }
                field.push(t);
                chars.next();
            }
            continue;
        }
        if c == delim {
            seen = true;
            record.push(std::mem::take(&mut field));
            continue;
        }
        if c == '\n' || c == '\r' {
            if c == '\r' && chars.peek() == Some(&'\n') {
                chars.next();
            }
            line += 1;
            if seen || !field.is_empty() {
                record.push(std::mem::take(&mut field));
                records.push(std::mem::take(&mut record));
            }
            seen = false;
            continue;
        }
        seen = true;
        field.push(c);
    }
    if seen || !field.is_empty() {
        record.push(field);
        records.push(record);
    }
    Ok(records)
}

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
    use std::path::PathBuf;

    fn rows(text: &str) -> Vec<Vec<String>> {
        parse(text, ',').expect("parses")
    }

    fn strs(v: &[Vec<String>]) -> Vec<Vec<&str>> {
        v.iter()
            .map(|r| r.iter().map(String::as_str).collect())
            .collect()
    }

    #[test]
    fn plain_fields_split_on_the_comma() {
        assert_eq!(
            strs(&rows("a,b,c\n1,2,3\n")),
            vec![vec!["a", "b", "c"], vec!["1", "2", "3"]]
        );
    }

    #[test]
    fn a_quoted_field_keeps_the_delimiter_inside_it() {
        assert_eq!(
            strs(&rows("name,note\n\"Smith, John\",ok\n")),
            vec![vec!["name", "note"], vec!["Smith, John", "ok"]]
        );
    }

    #[test]
    fn a_doubled_quote_is_one_quote() {
        assert_eq!(
            strs(&rows("\"say \"\"hi\"\"\",x\n")),
            vec![vec!["say \"hi\"", "x"]]
        );
    }

    #[test]
    fn an_embedded_newline_does_not_make_two_rows() {
        let got = rows("id,text\n1,\"line one\nline two\"\n2,after\n");
        assert_eq!(
            strs(&got),
            vec![
                vec!["id", "text"],
                vec!["1", "line one\nline two"],
                vec!["2", "after"]
            ]
        );
    }

    #[test]
    fn crlf_ends_a_record_without_leaving_a_cr_in_the_last_cell() {
        assert_eq!(
            strs(&rows("a,b\r\n1,2\r\n")),
            vec![vec!["a", "b"], vec!["1", "2"]]
        );
        // A CRLF inside quotes is content, and stays.
        assert_eq!(strs(&rows("\"x\r\ny\",z\r\n")), vec![vec!["x\r\ny", "z"]]);
    }

    #[test]
    fn a_lone_cr_also_ends_a_record() {
        assert_eq!(
            strs(&rows("a,b\r1,2\r")),
            vec![vec!["a", "b"], vec!["1", "2"]]
        );
    }

    #[test]
    fn a_trailing_newline_is_not_an_extra_row_and_a_missing_one_loses_nothing() {
        assert_eq!(rows("a,b\n1,2\n").len(), 2);
        assert_eq!(rows("a,b\n1,2").len(), 2);
        assert_eq!(rows("a,b\n1,2\n\n\n").len(), 2);
    }

    #[test]
    fn a_blank_line_in_the_middle_is_skipped_but_a_row_of_empty_cells_is_kept() {
        assert_eq!(
            strs(&rows("a,b\n\n1,2\n,\n")),
            vec![vec!["a", "b"], vec!["1", "2"], vec!["", ""]]
        );
    }

    #[test]
    fn a_ragged_row_keeps_its_own_field_count() {
        let got = rows("a,b,c\n1\n1,2,3,4\n");
        assert_eq!(got[1].len(), 1);
        assert_eq!(got[2].len(), 4);
    }

    #[test]
    fn a_trailing_comma_is_an_empty_last_field() {
        assert_eq!(strs(&rows("a,b,\n")), vec![vec!["a", "b", ""]]);
    }

    #[test]
    fn a_quote_inside_an_unquoted_field_is_a_literal() {
        assert_eq!(strs(&rows("5\" disk,x\n")), vec![vec!["5\" disk", "x"]]);
    }

    #[test]
    fn text_after_a_closing_quote_is_kept_rather_than_refused() {
        assert_eq!(strs(&rows("\"a\"b,c\n")), vec![vec!["ab", "c"]]);
    }

    #[test]
    fn an_unterminated_quote_is_an_error_naming_its_line() {
        let err = parse("a,b\n1,\"open\n2,3\n", ',').unwrap_err();
        assert_eq!(err, ParseError { line: 2 });
        assert_eq!(
            err.to_string(),
            "unterminated quoted field starting on line 2"
        );
    }

    #[test]
    fn a_header_only_file_is_one_record_and_an_empty_file_none() {
        assert_eq!(rows("a,b,c\n").len(), 1);
        assert!(rows("").is_empty());
        assert!(rows("\n\n").is_empty());
    }

    #[test]
    fn the_delimiter_comes_from_the_extension() {
        assert_eq!(delimiter_for(&PathBuf::from("/x/data.tsv")), '\t');
        assert_eq!(delimiter_for(&PathBuf::from("/x/DATA.TSV")), '\t');
        assert_eq!(delimiter_for(&PathBuf::from("/x/data.tab")), '\t');
        assert_eq!(delimiter_for(&PathBuf::from("/x/data.csv")), ',');
        assert_eq!(delimiter_for(&PathBuf::from("/x/data")), ',');
        assert_eq!(format_name('\t'), "TSV");
        assert_eq!(format_name(','), "CSV");
    }

    #[test]
    fn a_tab_splits_a_tsv_and_a_comma_inside_it_does_not() {
        let got = parse("a\tb\n1,5\t2\n", '\t').unwrap();
        assert_eq!(strs(&got), vec![vec!["a", "b"], vec!["1,5", "2"]]);
        // And the other way round: a tab in a CSV is content.
        assert_eq!(strs(&rows("a\tb,c\n")), vec![vec!["a\tb", "c"]]);
    }

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
