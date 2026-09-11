//! The block-level markdown parser, shared by the app's built-in preview and the
//! `bshuler/avada-markdown` module.
//!
//! This is the half of a markdown preview that a *module* owns: it turns source text
//! into the tier-5 [`Block`] document IR — the block level and no more, every block's
//! text left **raw** (only its `\r` dropped, inline markers intact). It runs with no
//! palette, no glyph metrics and no line-width budget, because clipping a line to a
//! width and mapping a block to a paint role are the host's to do when it *projects*
//! the document. The app keeps that projection (`viewpane::project_doc`); a module
//! ships this parse and hands the host a [`Doc`](avada_module_sdk::doc::Doc) over
//! `host.doc.set`.
//!
//! Only [`parse_markdown`] and [`strip_inline`] are public — the parser and the one
//! inline reducer a heading needs. Everything else is the parser's own machinery.

use avada_module_sdk::doc::{Block, Cell};

/// The most lines a preview reads. A document longer than this is parsed to the cap and
/// a [`Block::Notice`] says how many lines were left — a fact about the document, so it
/// belongs to the parse rather than to any pixel.
const MAX_LINES: usize = 5_000;

/// Parse markdown into the tier-5 [`Block`] document IR — the block level and no
/// more, every block's text left **raw** (only its `\r` dropped, inline markers
/// intact). This is the half a module owns: it runs with no palette, no glyph
/// metrics and no line-width budget, because clipping a line to a width and
/// mapping a block to a paint role are the host's to do when it projects.
///
/// Fences win over every other rule while open, so a `# comment` inside a shell
/// snippet stays code. A blank line closes what was open and a run collapses to
/// one [`Block::Space`]. The line cap and the empty-file note ride along as
/// [`Block::Notice`]s: "5000 lines is all a preview reads" is a fact about the
/// document, not about any pixel, so it belongs to the parse.
#[tracing::instrument(level = "debug", ret)]
pub fn parse_markdown(text: &str) -> Vec<Block> {
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let cap = total.min(MAX_LINES);
    let mut blocks: Vec<Block> = Vec::new();
    // The leading column of every open list level, innermost last. Self-correcting
    // — a shallower item pops back to its own level — so it never needs clearing
    // at a block boundary.
    let mut stack: Vec<usize> = Vec::new();
    // The block a following line may flow into: an open paragraph, list item or
    // quote. `None` after anything that closed one.
    let mut open: Option<usize> = None;
    let mut i = 0usize;

    while i < cap {
        let raw = lines[i].strip_suffix('\r').unwrap_or(lines[i]);
        // Trailing space is never content — it is markdown's hard break, which
        // `hard_break` reads off `raw` instead.
        let trimmed = raw.trim();

        // A fence is checked first and consumed whole, which is what keeps every
        // rule below it from firing on a line of code.
        if let Some((mark, info)) = fence_open(trimmed) {
            let mut j = i + 1;
            while j < cap && !fence_closes(lines[j], mark) {
                j += 1;
            }
            let body = lines[i + 1..j.min(cap)]
                .iter()
                .map(|l| l.strip_suffix('\r').unwrap_or(l))
                .collect::<Vec<_>>()
                .join("\n");
            blocks.push(if is_mermaid(info) {
                Block::Mermaid { source: body }
            } else {
                Block::Code {
                    lang: info.split_whitespace().next().unwrap_or("").to_string(),
                    text: body,
                }
            });
            open = None;
            i = j + 1;
            continue;
        }

        // A blank line closes whatever was open and leaves air. Runs of them
        // collapse to one: five blank lines are a typing habit, not five gaps.
        if trimmed.is_empty() {
            open = None;
            if !blocks.is_empty() && !matches!(blocks.last(), Some(Block::Space)) {
                blocks.push(Block::Space);
            }
            i += 1;
            continue;
        }

        // A table, header and delimiter together, folded into one block. Checked
        // before the paragraph rules so the header line is never swallowed as prose.
        if i + 1 < cap {
            if let Some(aligns) = table_at(raw, lines[i + 1]) {
                let headers = table_cells(&split_cells(raw), &aligns);
                let mut body_rows: Vec<Vec<Cell>> = Vec::new();
                let mut j = i + 2;
                while j < cap {
                    let body = lines[j].strip_suffix('\r').unwrap_or(lines[j]);
                    if !body.contains('|') || body.trim().is_empty() {
                        break;
                    }
                    body_rows.push(table_cells(&split_cells(body), &aligns));
                    j += 1;
                }
                blocks.push(Block::Table {
                    headers,
                    rows: body_rows,
                });
                open = None;
                i = j;
                continue;
            }
        }

        if let Some(block) = heading(trimmed) {
            blocks.push(block);
            open = None;
            i += 1;
            continue;
        }

        // A setext underline retitles the paragraph above it, and beats the rule
        // below because `---` under prose is a heading in every dialect.
        if let Some(k) = open.filter(|k| matches!(blocks[*k], Block::Prose { .. })) {
            if let Some(level) = setext(trimmed) {
                // The filter already proved this is prose; take its text without
                // holding the borrow into the reassignment below.
                let text = match &blocks[k] {
                    Block::Prose { text } => strip_inline(text),
                    _ => String::new(),
                };
                blocks[k] = Block::Heading { level, text };
                open = None;
                i += 1;
                continue;
            }
        }

        if is_rule(trimmed) {
            blocks.push(Block::Rule);
            open = None;
            i += 1;
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix('>') {
            let body = rest.strip_prefix(' ').unwrap_or(rest);
            match open.filter(|k| matches!(blocks[*k], Block::Quote { .. })) {
                Some(k) => {
                    if let Some(t) = block_text_mut(&mut blocks[k]) {
                        flow_into_block(t, body);
                    }
                }
                None => {
                    blocks.push(Block::Quote {
                        text: body.to_string(),
                    });
                    open = Some(blocks.len() - 1);
                }
            }
            if hard_break(raw) {
                open = None;
            }
            i += 1;
            continue;
        }

        if let Some(item) = list_item(trimmed) {
            let depth = nest(column_of(raw), &mut stack);
            blocks.push(Block::Bullet {
                depth: depth as u8,
                marker: item.marker,
                check: item.check as i8,
                text: item.body.to_string(),
            });
            open = if hard_break(raw) {
                None
            } else {
                Some(blocks.len() - 1)
            };
            i += 1;
            continue;
        }

        // An indented block is only code where there is nothing for it to be a
        // continuation of; under an open list item the same indent means "still
        // the same item".
        if open.is_none() && stack.is_empty() && (raw.starts_with("    ") || raw.starts_with('\t'))
        {
            blocks.push(Block::Code {
                lang: String::new(),
                text: raw.trim_end().to_string(),
            });
            i += 1;
            continue;
        }

        if let Some(k) = open {
            if let Some(t) = block_text_mut(&mut blocks[k]) {
                flow_into_block(t, trimmed);
            }
            if hard_break(raw) {
                open = None;
            }
            i += 1;
            continue;
        }

        // Prose back at column 0 ends every open list: the indent that held the
        // items nested is gone.
        if column_of(raw) == 0 {
            stack.clear();
        }
        blocks.push(Block::Prose {
            text: trimmed.to_string(),
        });
        open = if hard_break(raw) {
            None
        } else {
            Some(blocks.len() - 1)
        };
        i += 1;
    }

    // Trailing air is the file's final newlines, not a block.
    while matches!(blocks.last(), Some(Block::Space)) {
        blocks.pop();
    }
    if blocks.is_empty() {
        blocks.push(Block::Notice {
            text: "Empty file".to_string(),
        });
    } else if total > MAX_LINES {
        blocks.push(Block::Notice {
            text: format!("… {} more lines not shown", total - MAX_LINES),
        });
    }
    blocks
}

/// Whether a fence's info string opens a mermaid block. Mermaid is only ever the
/// first word — ```` ```mermaid {init: …} ```` is legal and still a diagram.
#[tracing::instrument(level = "debug", ret)]
fn is_mermaid(info: &str) -> bool {
    info.trim()
        .split(|c: char| c.is_whitespace() || c == '{')
        .next()
        .is_some_and(|w| w.eq_ignore_ascii_case("mermaid"))
}

/// The opening line of a fenced block → its marker character and info string.
/// A backtick fence's info string may not itself contain a backtick, which is
/// what stops ``` `` `code` `` ``` from opening a block.
#[tracing::instrument(level = "debug", ret)]
fn fence_open(trimmed: &str) -> Option<(char, &str)> {
    for mark in ['`', '~'] {
        let run = trimmed.chars().take_while(|c| *c == mark).count();
        if run >= 3 {
            let info = &trimmed[run..];
            if mark == '`' && info.contains('`') {
                continue;
            }
            return Some((mark, info));
        }
    }
    None
}

/// Whether `line` closes a fence opened with `mark`: the same character, three or
/// more, and nothing else.
#[tracing::instrument(level = "debug", ret)]
fn fence_closes(line: &str, mark: char) -> bool {
    let t = line.trim();
    t.chars().take_while(|c| *c == mark).count() >= 3
        && t.trim_start_matches(mark).trim().is_empty()
}

/// `#`/`##`/`###+` → the matching [`Block::Heading`]. `None` when the line is not
/// a heading — including `#hashtag`, which needs the space ATX requires.
///
/// `level` is the raw hash count (1..=6); the host's projection clamps `####` and
/// deeper to its H3 styling. A heading's inline markers are stripped here rather than
/// kept, because a heading is drawn with a plain `Text`: it needs a font weight, and
/// `StyledText` has no property for one — the one place the block level resolves inline
/// markup.
#[tracing::instrument(level = "debug", ret)]
fn heading(trimmed: &str) -> Option<Block> {
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &trimmed[hashes..];
    let body = rest.strip_prefix(' ')?.trim();
    // A closing run of `#` is chrome, but only when a space separates it — `# C#`
    // is a heading about C#.
    let body = {
        let bare = body.trim_end_matches('#');
        if bare.len() < body.len() && (bare.is_empty() || bare.ends_with(' ')) {
            bare.trim_end()
        } else {
            body
        }
    };
    Some(Block::Heading {
        level: hashes as u8,
        text: strip_inline(body),
    })
}

/// A setext underline → the heading level it makes the paragraph above it: `1` for the
/// `=` underline, `2` for `-`. Two dashes are required so a lone `-` stays a list item.
#[tracing::instrument(level = "debug", ret)]
fn setext(trimmed: &str) -> Option<u8> {
    let t = trimmed.trim_end();
    if !t.is_empty() && t.chars().all(|c| c == '=') {
        return Some(1);
    }
    if t.len() >= 2 && t.chars().all(|c| c == '-') {
        return Some(2);
    }
    None
}

/// One list item, taken apart.
struct Item<'a> {
    /// `"2."` for an ordered item, empty for an unordered one.
    marker: String,
    /// -1 none, 0 unchecked, 1 checked.
    check: i32,
    /// What is left after the marker and the task box.
    body: &'a str,
}

/// `- x` / `* x` / `+ x` / `1. x` / `- [x] x` → the item, or `None`.
#[tracing::instrument(level = "debug")]
fn list_item(trimmed: &str) -> Option<Item<'_>> {
    let mut marker = String::new();
    let rest = match ["- ", "* ", "+ "]
        .iter()
        .find_map(|m| trimmed.strip_prefix(m))
    {
        Some(r) => r,
        None => {
            // An ordered item: digits, then `.` or `)`, then a space.
            let digits = trimmed.chars().take_while(|c| c.is_ascii_digit()).count();
            if digits == 0 || digits > 9 {
                return None;
            }
            let after = &trimmed[digits..];
            let r = [". ", ") "].iter().find_map(|m| after.strip_prefix(m))?;
            marker = format!("{}.", &trimmed[..digits]);
            r
        }
    };
    let rest = rest.trim_start();
    // The task box is the one inline construct that survives as data rather than
    // as markup: it is drawn as a box, not typeset.
    let (check, body) = if let Some(b) = rest.strip_prefix("[ ]") {
        (0, b.strip_prefix(' ').unwrap_or(b))
    } else if let Some(b) = rest
        .strip_prefix("[x]")
        .or_else(|| rest.strip_prefix("[X]"))
    {
        (1, b.strip_prefix(' ').unwrap_or(b))
    } else {
        (-1, rest)
    };
    Some(Item {
        marker,
        check,
        body,
    })
}

/// The nesting depth an item at column `lead` sits at, updating the open stack.
/// Capped, because past six levels the text column is narrower than the indent
/// leading to it.
#[tracing::instrument(level = "debug", ret)]
fn nest(lead: usize, stack: &mut Vec<usize>) -> i32 {
    while stack.last().is_some_and(|&top| lead < top) {
        stack.pop();
    }
    match stack.last() {
        Some(&top) if lead > top => stack.push(lead),
        None => stack.push(lead),
        _ => {}
    }
    (stack.len() as i32 - 1).min(5)
}

/// The column a line's text starts at, tabs expanded to the next multiple of four.
#[tracing::instrument(level = "debug", ret)]
fn column_of(raw: &str) -> usize {
    let mut n = 0;
    for c in raw.chars() {
        match c {
            ' ' => n += 1,
            '\t' => n += 4 - n % 4,
            _ => break,
        }
    }
    n
}

/// Whether the line asked for a break rather than for the next line to flow into
/// it: markdown's two trailing spaces, or a trailing backslash.
#[tracing::instrument(level = "debug", ret)]
fn hard_break(raw: &str) -> bool {
    raw.ends_with("  ") || raw.ends_with('\\')
}

/// Append a continuation line to an open block's raw text. Joined with a space,
/// not a newline, because the block is one wrapped paragraph and the host decides
/// where it breaks. This neither clips nor drops: the text stays raw for the host's
/// projection to clip, the same raw a module would ship.
#[tracing::instrument(level = "debug", ret)]
fn flow_into_block(text: &mut String, more: &str) {
    if !text.is_empty() {
        text.push(' ');
    }
    text.push_str(more.trim_end());
}

/// The text a continuation flows into — the three block kinds `open` ever points
/// at. `None` for anything else, so a stray continuation is dropped, not
/// mis-attached.
//
// Not `#[instrument(ret)]`: logging the return would borrow the `&mut String` out
// past the generated closure, which the borrow checker rejects.
fn block_text_mut(block: &mut Block) -> Option<&mut String> {
    match block {
        Block::Prose { text } | Block::Quote { text } | Block::Bullet { text, .. } => Some(text),
        _ => None,
    }
}

/// One markdown table line as [`Cell`]s: padded or clipped to the column count the
/// delimiter row fixed and tagged with each column's alignment, but keeping the
/// cell text raw.
#[tracing::instrument(level = "debug", ret)]
fn table_cells(cells: &[String], aligns: &[i32]) -> Vec<Cell> {
    aligns
        .iter()
        .enumerate()
        .map(|(i, &align)| Cell {
            text: cells.get(i).map(String::as_str).unwrap_or("").to_string(),
            align: align as i8,
        })
        .collect()
}

/// `| a | b |` over `|---|:--:|` → one alignment per column, or `None`.
///
/// Both lines have to agree on the column count. That is what stops a paragraph
/// that happens to contain a pipe from eating the line under it.
#[tracing::instrument(level = "debug", ret)]
fn table_at(head: &str, delim: &str) -> Option<Vec<i32>> {
    let delim = delim.strip_suffix('\r').unwrap_or(delim);
    if !head.contains('|') || !delim.contains('|') {
        return None;
    }
    let cols: Vec<i32> = split_cells(delim)
        .iter()
        .map(|c| {
            let (left, right) = (c.starts_with(':'), c.ends_with(':'));
            let dashes = c.trim_matches(':');
            if dashes.is_empty() || !dashes.chars().all(|ch| ch == '-') {
                return -1;
            }
            match (left, right) {
                (true, true) => 1,
                (false, true) => 2,
                _ => 0,
            }
        })
        .collect();
    if cols.is_empty() || cols.iter().any(|&a| a < 0) || split_cells(head).len() != cols.len() {
        return None;
    }
    Some(cols)
}

/// The cells of one table line: split on `|`, with the optional leading and
/// trailing fence dropped. An escaped `\|` stays inside its cell.
#[tracing::instrument(level = "debug", ret)]
fn split_cells(line: &str) -> Vec<String> {
    let mut cells = vec![String::new()];
    let mut esc = false;
    for c in line.trim().chars() {
        let cur = cells.last_mut().expect("never emptied");
        if esc {
            if c != '|' {
                cur.push('\\');
            }
            cur.push(c);
            esc = false;
        } else if c == '\\' {
            esc = true;
        } else if c == '|' {
            cells.push(String::new());
        } else {
            cur.push(c);
        }
    }
    if cells.first().is_some_and(|c| c.is_empty()) {
        cells.remove(0);
    }
    if cells.len() > 1 && cells.last().is_some_and(|c| c.trim().is_empty()) {
        cells.pop();
    }
    cells.iter().map(|c| c.trim().to_string()).collect()
}

/// Inline markdown reduced to the words it was wrapping. Used only where the row
/// is drawn with a plain `Text` instead of Slint's `StyledText` — headings, which
/// need the font weight `StyledText` has no property for.
#[tracing::instrument(level = "debug", ret)]
pub fn strip_inline(src: &str) -> String {
    let ch: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < ch.len() {
        match ch[i] {
            '\\' if ch.get(i + 1).is_some_and(|c| c.is_ascii_punctuation()) => {
                out.push(ch[i + 1]);
                i += 2;
            }
            '`' | '*' => i += 1,
            '~' if ch.get(i + 1) == Some(&'~') => i += 2,
            // `_` only at a word edge: snake_case is a word, not emphasis.
            '_' if !(i > 0
                && ch[i - 1].is_alphanumeric()
                && ch.get(i + 1).is_some_and(|c| c.is_alphanumeric())) =>
            {
                i += 1
            }
            // `[text](url)` and `[text][ref]` keep the text and drop the target.
            '[' => i += 1,
            '!' if ch.get(i + 1) == Some(&'[') => i += 1,
            ']' => {
                i += 1;
                let close = match ch.get(i) {
                    Some('(') => Some(')'),
                    Some('[') => Some(']'),
                    _ => None,
                };
                if let Some(close) = close {
                    i += 1;
                    while i < ch.len() && ch[i] != close {
                        i += 1;
                    }
                    i += usize::from(i < ch.len());
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out.trim().to_string()
}

/// `---`, `***`, `___` (three or more, nothing else on the line).
#[tracing::instrument(level = "debug", ret)]
fn is_rule(trimmed: &str) -> bool {
    let t = trimmed.trim_end();
    for c in ['-', '*', '_'] {
        if t.len() >= 3 && t.chars().all(|ch| ch == c) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `strip_inline` keeps the words and drops the markup, with two edges that bite:
    /// `_` inside a word is not emphasis, and a link keeps its text but not its target.
    #[test]
    fn strip_inline_keeps_words_and_drops_markup() {
        assert_eq!(strip_inline("a *b* _c_ `d`"), "a b c d");
        assert_eq!(strip_inline("call some_long_name now"), "call some_long_name now");
        assert_eq!(strip_inline("see [the docs](http://x/y) here"), "see the docs here");
        assert_eq!(strip_inline("see [the docs][ref]"), "see the docs");
        assert_eq!(strip_inline("~~gone~~ and \\*kept\\*"), "gone and *kept*");
    }

    /// The ATX heading rules the parser folds into one `Heading` block: the hash count is
    /// the level, `#hashtag` is prose (no space), and a trailing `#` run is chrome.
    #[test]
    fn headings_carry_their_level_and_need_a_space() {
        assert_eq!(
            parse_markdown("# Title"),
            vec![Block::Heading { level: 1, text: "Title".into() }]
        );
        assert_eq!(
            parse_markdown("### Deep ###"),
            vec![Block::Heading { level: 3, text: "Deep".into() }]
        );
        // No space after the hashes: not a heading, so it stays prose.
        assert_eq!(
            parse_markdown("#hashtag"),
            vec![Block::Prose { text: "#hashtag".into() }]
        );
    }

    /// A setext underline retitles the paragraph above it: `=` is level 1, `--` is level 2.
    /// This is the case that drove the `Option<u8>` return — the app used to read a paint
    /// role out of it, which no module could.
    #[test]
    fn setext_underline_retitles_the_paragraph_above() {
        assert_eq!(
            parse_markdown("Title\n====="),
            vec![Block::Heading { level: 1, text: "Title".into() }]
        );
        assert_eq!(
            parse_markdown("Subtitle\n---"),
            vec![Block::Heading { level: 2, text: "Subtitle".into() }]
        );
        // A lone dash under prose is a list, not a heading: setext needs two.
        assert_eq!(setext("-"), None);
        assert_eq!(setext("="), Some(1));
    }

    /// A fence is consumed whole and wins over every rule inside it — a `#` line in a
    /// shell snippet stays code, not a heading — and a `mermaid` info string is a diagram.
    #[test]
    fn fences_win_over_inner_rules_and_mermaid_is_a_diagram() {
        assert_eq!(
            parse_markdown("```sh\n# not a heading\n```"),
            vec![Block::Code { lang: "sh".into(), text: "# not a heading".into() }]
        );
        assert_eq!(
            parse_markdown("```mermaid\ngraph TD\n```"),
            vec![Block::Mermaid { source: "graph TD".into() }]
        );
    }

    /// A GFM table folds header, delimiter and body into one `Table`, and the delimiter
    /// row sets each column's alignment (0 left, 1 centre, 2 right).
    #[test]
    fn a_table_folds_into_one_block_with_alignments() {
        let blocks = parse_markdown("| a | b | c |\n|:--|:-:|--:|\n| 1 | 2 | 3 |");
        assert_eq!(
            blocks,
            vec![Block::Table {
                headers: vec![Cell::aligned("a", 0), Cell::aligned("b", 1), Cell::aligned("c", 2)],
                rows: vec![vec![Cell::aligned("1", 0), Cell::aligned("2", 1), Cell::aligned("3", 2)]],
            }]
        );
    }

    /// Nested list items carry a capped depth, and a task box survives as `check` data
    /// rather than as `[x]` markup in the text.
    #[test]
    fn lists_nest_and_task_boxes_survive_as_data() {
        assert_eq!(
            parse_markdown("- one\n  - two"),
            vec![
                Block::Bullet { depth: 0, marker: String::new(), check: -1, text: "one".into() },
                Block::Bullet { depth: 1, marker: String::new(), check: -1, text: "two".into() },
            ]
        );
        assert_eq!(
            parse_markdown("- [x] done\n- [ ] todo"),
            vec![
                Block::Bullet { depth: 0, marker: String::new(), check: 1, text: "done".into() },
                Block::Bullet { depth: 0, marker: String::new(), check: 0, text: "todo".into() },
            ]
        );
    }

    /// A run of blank lines collapses to one `Space`, and trailing air is dropped: five
    /// blank lines are a typing habit, not five gaps, and the file's final newline is not
    /// a block.
    #[test]
    fn blank_runs_collapse_and_trailing_air_is_dropped() {
        assert_eq!(
            parse_markdown("a\n\n\n\nb\n\n"),
            vec![
                Block::Prose { text: "a".into() },
                Block::Space,
                Block::Prose { text: "b".into() },
            ]
        );
    }

    /// The two documents that become a single `Notice`: an empty file, and one past the
    /// line cap, which is parsed to the cap with a note of how many lines were left.
    #[test]
    fn empty_and_over_cap_documents_end_in_a_notice() {
        assert_eq!(
            parse_markdown(""),
            vec![Block::Notice { text: "Empty file".into() }]
        );
        let long: String = (1..=MAX_LINES + 7).map(|n| format!("line {n}\n")).collect();
        let blocks = parse_markdown(&long);
        assert_eq!(
            blocks.last(),
            Some(&Block::Notice { text: "… 7 more lines not shown".into() })
        );
    }
}
