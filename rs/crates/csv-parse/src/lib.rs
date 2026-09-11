//! The RFC 4180 CSV/TSV parser, shared by the app's built-in table view and the
//! `bshuler/avada-table` module.
//!
//! This is the half of a table view that a *module* owns: it turns a file's text into
//! the tier-5 [`Block`] document IR — one [`Block::Table`] flagged `dense`, its cells
//! left **raw** (verbatim field text, alignment unset). It runs with no glyph metrics
//! and no column budget, because measuring the columns, padding them monospace and
//! deriving a numeric column's flush-right alignment are the host's to do when it
//! *projects* the document. The app keeps that projection (`viewpane::project_doc`);
//! a module ships this parse and hands the host a [`Doc`](avada_module_sdk::doc::Doc)
//! over `host.doc.set`.
//!
//! [`parse`] does the RFC 4180 work — quoted fields, doubled quotes, embedded line
//! breaks, CRLF, a trailing newline, ragged rows — with no dependency beyond `std`,
//! because a `csv` crate would be a Cargo.toml change and this parser is forty lines.
//! [`parse_table`] wraps it into the [`Block`] document the module ships, capping the
//! row count and turning every way a file can fail to be a table into a single
//! [`Block::Notice`] the reader sees in the pane.

use avada_module_sdk::doc::{Block, Cell};
use std::fmt;
use std::path::Path;

/// The most records a table view reads. A file with more is parsed to the cap and a
/// [`Block::Notice`] says how many rows were left — a fact about the document, so it
/// belongs to the parse rather than to any pixel. The header counts as a record, so a
/// capped table keeps the header plus `MAX_LINES - 1` body rows.
pub const MAX_LINES: usize = 5_000;

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
/// for everything else. Extension-driven for the same reason the host's `kind_for_file`
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

/// Parse a delimited file into the tier-5 [`Block`] document a table view draws: one
/// `dense` [`Block::Table`] whose cells are the **raw** fields, capped at [`MAX_LINES`]
/// records. The header is the first record; the rest are body rows.
///
/// Every way a file fails to be a table is a single [`Block::Notice`] rather than an
/// error, because each is something the reader did — an empty file, a file that is not
/// CSV — not a protocol fault: an unterminated quote names its line, an empty file says
/// so, and a file past the cap keeps the head of it and a "more rows" note. The host
/// projection turns the raw cells into the padded, aligned grid; this side ships only
/// what the bytes said.
#[tracing::instrument(level = "debug", ret)]
pub fn parse_table(text: &str, delim: char) -> Vec<Block> {
    let records = match parse(text, delim) {
        Ok(r) => r,
        Err(e) => {
            return vec![Block::Notice {
                text: format!("Cannot parse as {}: {e}", format_name(delim)),
            }];
        }
    };
    if records.is_empty() {
        return vec![Block::Notice {
            text: "Empty file".to_string(),
        }];
    }
    let total = records.len();
    let shown = &records[..total.min(MAX_LINES)];
    let headers: Vec<Cell> = shown[0].iter().map(|f| Cell::plain(f.clone())).collect();
    let rows: Vec<Vec<Cell>> = shown[1..]
        .iter()
        .map(|rec| rec.iter().map(|f| Cell::plain(f.clone())).collect())
        .collect();
    let mut blocks = vec![Block::Table {
        headers,
        rows,
        dense: true,
    }];
    if total > MAX_LINES {
        blocks.push(Block::Notice {
            text: format!("… {} more rows not shown", total - MAX_LINES),
        });
    }
    blocks
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

    // ---- parse_table: the Block document the module ships ----------------------

    fn table(blocks: &[Block]) -> (&[Cell], &[Vec<Cell>]) {
        match &blocks[0] {
            Block::Table {
                headers,
                rows,
                dense,
            } => {
                assert!(*dense, "a CSV table is a data grid, not a prose table");
                (headers, rows)
            }
            other => panic!("expected a table, got {other:?}"),
        }
    }

    fn texts(cells: &[Cell]) -> Vec<&str> {
        cells.iter().map(|c| c.text.as_str()).collect()
    }

    #[test]
    fn parse_table_ships_a_dense_table_of_raw_cells() {
        let blocks = parse_table("name,qty\nbolt,4\n\"a *b*\",12\n", ',');
        assert_eq!(blocks.len(), 1);
        let (headers, rows) = table(&blocks);
        assert_eq!(texts(headers), vec!["name", "qty"]);
        assert_eq!(rows.len(), 2);
        // The cell is raw: the asterisks and quotes arrive verbatim, unescaped, unaligned.
        assert_eq!(texts(&rows[1]), vec!["a *b*", "12"]);
        assert!(rows[1].iter().all(|c| c.align == 0), "raw cells are unaligned");
    }

    #[test]
    fn parse_table_names_an_empty_file_and_a_parse_error() {
        assert_eq!(
            parse_table("", ','),
            vec![Block::Notice {
                text: "Empty file".to_string()
            }]
        );
        assert_eq!(
            parse_table("a,b\n1,\"open\n2,3\n", ','),
            vec![Block::Notice {
                text: "Cannot parse as CSV: unterminated quoted field starting on line 2"
                    .to_string()
            }]
        );
        // A TSV names the format it was read as.
        match &parse_table("\"open\n", '\t')[0] {
            Block::Notice { text } => assert!(text.starts_with("Cannot parse as TSV:")),
            other => panic!("expected a notice, got {other:?}"),
        }
    }

    #[test]
    fn parse_table_caps_the_rows_and_counts_the_rest() {
        let mut body = String::from("n\n");
        for i in 1..=(MAX_LINES + 10) {
            body.push_str(&format!("{i}\n"));
        }
        let blocks = parse_table(&body, ',');
        assert_eq!(blocks.len(), 2, "the table and a cap notice");
        let (headers, rows) = table(&blocks);
        assert_eq!(texts(headers), vec!["n"]);
        // The header is one of the MAX_LINES kept records, so the body is one short of it.
        assert_eq!(rows.len(), MAX_LINES - 1);
        assert_eq!(
            blocks[1],
            Block::Notice {
                text: "… 11 more rows not shown".to_string()
            }
        );
    }

    #[test]
    fn parse_table_header_only_is_a_table_with_no_body() {
        let blocks = parse_table("a,b,c\n", ',');
        let (headers, rows) = table(&blocks);
        assert_eq!(texts(headers), vec!["a", "b", "c"]);
        assert!(rows.is_empty());
    }
}
