//! Family B panes: the non-PTY views — the file browser, the file viewer and the
//! markdown preview.
//!
//! A Family A pane (terminal, and every tool pane) is backed by a pty and renders
//! itself; there is nothing for the app to project beyond the surface image. A
//! Family B pane has no pty at all (see D3: it mints a `view-N` uid and never
//! reaches the [`SessionManager`](avada_core::session_manager::SessionManager)),
//! so its content has to be *computed* — and that is what lives here.
//!
//! The design rule this module exists to enforce: **all of the parsing happens in
//! Rust**, never in `.slint`. Slint receives one flat row model whose `role` says
//! how to style each row, and styles it. That keeps the three views deterministic,
//! unit-testable, and immune to the layout-cycle hazards a smarter .slint would
//! invite — and it means one ~150-line `ViewPane` component serves all three kinds
//! instead of three separate renderers.
//!
//! The pane's target is its [`PaneState::cwd`](crate::state::PaneState) — a
//! *directory* for [`PaneKind::FileBrowser`], a *file* for the viewer and the
//! markdown preview. That reuse is deliberate: `PaneSpec.cwd` already round-trips
//! through persistence, so a view pane survives a restart with no format change.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use avada_core::tools::PaneKind;
use slint::{ModelRc, VecModel};

use crate::theme;
use crate::{DiagLabel, DiagNode, PaneCell, PaneDiagram, PaneViewRow};

/// How a row is drawn. The `.slint` side switches on this and nothing else — it
/// never re-parses `text`. Keep these in lock-step with `ViewPane` in
/// `ui/viewpanes.slint`; an unknown role there falls back to [`role::LINE`].
pub mod role {
    /// The synthetic ".." row at the top of a directory listing.
    pub const PARENT: i32 = 0;
    /// A subdirectory.
    pub const DIR: i32 = 1;
    /// A regular file.
    pub const FILE: i32 = 2;
    /// A plain line of a text file.
    pub const LINE: i32 = 3;
    /// Markdown `#`.
    pub const H1: i32 = 4;
    /// Markdown `##`.
    pub const H2: i32 = 5;
    /// Markdown `###` (and deeper — `####` renders as H3 rather than vanishing).
    pub const H3: i32 = 6;
    /// A line inside a fenced code block, or an indented code line.
    pub const CODE: i32 = 7;
    /// A `-` / `*` / `1.` list item.
    pub const BULLET: i32 = 8;
    /// A `>` block quote.
    pub const QUOTE: i32 = 9;
    /// A `---` horizontal rule (draws geometry; `text` is empty).
    pub const RULE: i32 = 10;
    /// An advisory row the app generated — truncation, "empty directory", an IO
    /// error. Never activatable.
    pub const NOTICE: i32 = 11;
    /// A rendered mermaid diagram. The row's `text` is empty and its geometry
    /// rides along in [`super::ViewRow::diagram`].
    pub const DIAGRAM: i32 = 12;
    /// A table's header line. `text` is empty; the content is in
    /// [`super::ViewRow::cells`].
    pub const TABLE_HEAD: i32 = 13;
    /// A table's body line.
    pub const TABLE_ROW: i32 = 14;
    /// The gap a blank line leaves between two blocks. Draws nothing; it exists
    /// so paragraphs are separated by air rather than by a full empty row.
    pub const SPACE: i32 = 15;
    /// A markdown paragraph. Distinct from [`LINE`] because the two want
    /// opposite things: prose wraps and carries inline markup, a viewer line is
    /// verbatim and elides so that row N stays line N.
    pub const PROSE: i32 = 16;
    /// A line of a source file, coloured. Distinct from [`LINE`] only in carrying
    /// [`super::ViewRow::markup`]: the text is the same verbatim line, and the row
    /// must keep the same fixed height so that row N is still line N.
    pub const SOURCE: i32 = 17;
    /// One node of a data pane's tree (see [`crate::datatree`]): a container row that
    /// folds, or a scalar row that does not. `indent` is the depth, `check` the
    /// disclosure (`-1` scalar, `0` folded, `1` open), `detail` the folded child count,
    /// `node` the path a toggle names. `text` is the key alone on a container and
    /// `key: value` on a scalar; the glyph is decoration and never enters it.
    pub const DATA_NODE: i32 = 18;
    /// One row a module projected into a pane of its own (see [`crate::module_ui::rows`]).
    /// The same shape as [`DATA_NODE`] — `indent` the depth, `check` the disclosure
    /// (`-1` flat, `0` folded, `1` open), `detail` the dim trailing column, `node` the row
    /// id the module named — but a separate role, because a data node's click toggles a
    /// fold the app owns and a module row's click goes back to the module.
    pub const MODULE_ROW: i32 = 19;
}

/// One cell of a markdown table.
#[derive(Clone, Debug, PartialEq)]
pub struct TableCell {
    /// Inline markdown with its markers intact — the same contract as
    /// [`ViewRow::text`] on the flowing roles.
    pub text: String,
    /// What the `|:--:|` delimiter row asked for: 0 left, 1 centre, 2 right.
    pub align: i32,
}

/// One projected row. `path` is empty for everything that is not activatable, so
/// the click handler can refuse a row without consulting its role — the same
/// "decide it once, on the producing side" rule the left panel's `blocked` flag
/// follows.
#[derive(Clone, Debug, PartialEq)]
pub struct ViewRow {
    pub role: i32,
    pub text: String,
    /// The dim trailing column: a size + age for a listing, a line number for the
    /// viewer, "" for markdown.
    pub detail: String,
    /// The absolute path this row activates, or "" when the row is inert.
    pub path: PathBuf,
    /// Set only on [`role::DIAGRAM`]. Boxed because it is large and every other
    /// row in a 5,000-row file would otherwise carry the empty space for it.
    pub diagram: Option<Box<crate::mermaid::Diagram>>,
    /// List nesting depth, 0 at the top level. [`role::BULLET`] only. Decided
    /// here rather than in the view because the leading whitespace that implied
    /// it never reaches Slint.
    pub indent: i32,
    /// An ordered item's own number, `"2."` — empty for an unordered one, which
    /// the view draws as a dot. Kept verbatim: a list that renumbers itself
    /// 1, 2, 3 when the author wrote 3, 4, 5 misquotes the document.
    pub marker: String,
    /// A task box: -1 none, 0 unchecked, 1 checked.
    pub check: i32,
    /// [`role::TABLE_HEAD`] and [`role::TABLE_ROW`] only, one entry per column.
    pub cells: Vec<TableCell>,
    /// [`role::SOURCE`] only: `text` again as the markdown markup that carries its
    /// syntax colours (see [`crate::highlight`]). A separate field rather than a
    /// replacement for `text`, because everything else about the row — copying, the
    /// accessible label, the selection — wants the raw source line.
    pub markup: String,
    /// [`role::DATA_NODE`] only: the node's stable path (`$.deps.serde`), the thing
    /// `Command::ViewToggleNode` names. Stable across a toggle where a row index is not.
    pub node: String,
}

impl Default for ViewRow {
    #[tracing::instrument(level = "debug", ret)]
    fn default() -> Self {
        Self::inert(role::LINE, "")
    }
}

impl ViewRow {
    /// A row with nothing to open.
    #[tracing::instrument(level = "debug", skip_all)]
    fn inert(role: i32, text: impl Into<String>) -> Self {
        Self {
            role,
            text: text.into(),
            detail: String::new(),
            path: PathBuf::new(),
            diagram: None,
            indent: 0,
            marker: String::new(),
            check: -1,
            cells: Vec::new(),
            markup: String::new(),
            node: String::new(),
        }
    }

    /// Whether clicking this row does anything (drives the pointer cursor).
    #[tracing::instrument(level = "debug", ret)]
    pub fn activatable(&self) -> bool {
        // A module row has no path — the module decides what its row means — so it says so
        // by its role. Everything else earns the pointer by having something to open.
        self.role == role::MODULE_ROW || !self.path.as_os_str().is_empty()
    }

    /// This row as one line of plain text, for the clipboard.
    ///
    /// What a copy owes the reader is what the pane *showed* them, so the flowing
    /// roles come back with their inline markers resolved rather than raw — the
    /// same words the `StyledText` drew. Structure that was visible *as*
    /// structure (a bullet and its nesting, a quote, a table's columns) is
    /// re-spelled the plain-text way, because a paste has no other way to carry
    /// it. A file viewer's line is the one exception and is copied byte for byte:
    /// there the pane was never rendering markup in the first place.
    #[tracing::instrument(level = "debug", ret)]
    pub fn copy_text(&self) -> String {
        match self.role {
            // Listing rows and the plain viewer: the text is already what was drawn.
            role::PARENT
            | role::DIR
            | role::FILE
            | role::LINE
            | role::SOURCE
            | role::CODE
            | role::NOTICE => self.text.clone(),
            // Headings arrive with their `#` markers already stripped by `heading()`,
            // and the count is not recoverable (`####` and deeper all render as H3),
            // so they are copied as the words they showed.
            role::H1 | role::H2 | role::H3 => self.text.clone(),
            role::QUOTE => format!("> {}", strip_inline(&self.text)),
            role::BULLET => {
                let marker = if self.marker.is_empty() {
                    "-".to_string()
                } else {
                    self.marker.clone()
                };
                let box_ = match self.check {
                    0 => "[ ] ",
                    1 => "[x] ",
                    _ => "",
                };
                format!(
                    "{}{marker} {box_}{}",
                    "  ".repeat(self.indent.max(0) as usize),
                    strip_inline(&self.text)
                )
            }
            role::TABLE_HEAD | role::TABLE_ROW => self
                .cells
                .iter()
                .map(|c| strip_inline(&c.text))
                .collect::<Vec<_>>()
                .join(" | "),
            role::RULE => "---".to_string(),
            // A laid-out diagram keeps no source (see `mermaid::Diagram`) and its
            // shapes are not text, so it copies as the blank line it occupies.
            role::DIAGRAM | role::SPACE => String::new(),
            _ => strip_inline(&self.text),
        }
    }
}

/// Most lines a file viewer will read. A view pane is a *preview*, not an editor:
/// past this the projection stops and appends a [`role::NOTICE`] saying so, rather
/// than pushing a million-row model into Slint.
pub const MAX_LINES: usize = 5_000;

/// Longest single line kept, in chars. A minified bundle is one 3 MB line; the
/// widget would try to lay all of it out.
const MAX_LINE_CHARS: usize = 2_000;

/// Largest file the viewer will read at all, in bytes (2 MiB).
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// Most entries a directory listing keeps. `node_modules` is the reason.
pub const MAX_ENTRIES: usize = 2_000;

// ---------------------------------------------------------------------------
// the three projections
// ---------------------------------------------------------------------------

/// Project `target` for `kind`. The single entry point: the caller never has to
/// know which of the three parsers applies.
///
/// Returns a single [`role::NOTICE`] row rather than an empty model on every
/// failure path — an empty pane looks broken, a pane that says *why* does not.
#[tracing::instrument(level = "debug", ret)]
pub fn rows_for(kind: &PaneKind, target: Option<&str>, palette: usize) -> Vec<ViewRow> {
    let Some(t) = target.filter(|t| !t.is_empty()) else {
        return vec![ViewRow::inert(role::NOTICE, "No path set for this pane")];
    };
    let path = PathBuf::from(t);
    match kind {
        PaneKind::FileBrowser => list_dir(&path),
        PaneKind::FileViewer => read_lines(&path),
        PaneKind::Markdown => markdown_blocks(&path),
        PaneKind::Code => highlight_lines(&path, palette),
        // Reserved in Wave 0 of docs/modules-fanout-plan.md; each gets its own projection
        // in the viewer-panes track (WP1 tree, WP2 grid, WP3 bitmap). Until then the file
        // is shown as text, which is honest for the first two and a NOTICE for the third.
        PaneKind::Data => data_rows(&path, &BTreeSet::new(), palette),
        PaneKind::Table => table_rows(&path),
        PaneKind::Image => vec![ViewRow::inert(
            role::NOTICE,
            "Image preview is not available in this build",
        )],
        // Family A, a module surface (drawn by the module, not projected), or a kind this
        // build does not know: nothing to project.
        PaneKind::Terminal | PaneKind::Tool(_) | PaneKind::Browser | PaneKind::Module(_) => {
            Vec::new()
        }
    }
}

/// Resolve a link clicked in a rendered markdown preview to a file on disk, or `None` when
/// it is not one.
///
/// `target` is the previewed document's own path (a directory for a listing pane); a
/// relative href is relative to the document's *directory*, which is what a markdown link
/// means everywhere else. Fragments and queries are dropped — `design.md#rationale` names a
/// file we can show. Nothing that does not exist resolves, which is what keeps a
/// `javascript:` or a link into a directory that is not there from becoming an action.
#[tracing::instrument(level = "debug", ret)]
pub fn resolve_local_href(target: &Path, href: &str) -> Option<PathBuf> {
    let bare = href.split(['#', '?']).next().unwrap_or(href);
    if bare.is_empty() {
        return None;
    }
    let base = if target.is_dir() {
        target
    } else {
        target.parent()?
    };
    base.join(bare).canonicalize().ok()
}

/// The header line for a view pane: the target's own name, which is all the
/// header has room for. "" when there is no target.
#[tracing::instrument(level = "debug", ret)]
pub fn view_title(kind: &PaneKind, target: Option<&str>) -> String {
    if !kind.is_view() {
        return String::new();
    }
    // A module pane has no file behind it, so there is no path to shorten. What it does
    // have is a module and a surface, which is exactly what `ui_name` spells.
    if kind.module().is_some() {
        return kind.ui_name();
    }
    match target.filter(|t| !t.is_empty()) {
        // The full path is the honest title, but it is also 90 chars of noise in a
        // 26px header; the last two components read as a location without wrapping.
        Some(t) => tail_two(Path::new(t)),
        None => String::new(),
    }
}

/// The last two path components ("avada/src"), or the whole path when it is
/// already that short. Never empty for a non-empty path — a bare "/" returns "/".
#[tracing::instrument(level = "debug", ret)]
fn tail_two(p: &Path) -> String {
    let comps: Vec<String> = p
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    match comps.len() {
        0 => String::new(),
        1 => comps[0].clone(),
        n => {
            let a = &comps[n - 2];
            let b = &comps[n - 1];
            // A root component already carries its separator ("/" or "C:\").
            if a.ends_with('/') || a.ends_with('\\') {
                format!("{a}{b}")
            } else {
                format!("{a}/{b}")
            }
        }
    }
}

/// A directory listing: the ".." row, then subdirectories, then files, each group
/// sorted by name. Dirs-before-files is the convention every file manager uses and
/// the one that makes a deep tree navigable by clicking down the top of the list.
///
/// Sorting is case-insensitive so `README` does not sort above `apps` the way a
/// raw byte comparison would.
#[tracing::instrument(level = "debug", ret)]
pub fn list_dir(dir: &Path) -> Vec<ViewRow> {
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => return vec![ViewRow::inert(role::NOTICE, format!("Cannot read: {e}"))],
    };
    let now = now_secs();
    let mut dirs: Vec<ViewRow> = Vec::new();
    let mut files: Vec<ViewRow> = Vec::new();
    let mut skipped = 0usize;
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        // Dotfiles are shown — this is a developer tool and `.git` / `.avada`
        // are exactly what its user is looking for.
        let md = match ent.metadata() {
            Ok(md) => md,
            // A broken symlink still deserves a row; it just has no size or age.
            Err(_) => {
                files.push(ViewRow {
                    role: role::FILE,
                    text: name,
                    detail: "—".into(),
                    path: ent.path(),
                    diagram: None,
                    ..ViewRow::default()
                });
                continue;
            }
        };
        if dirs.len() + files.len() >= MAX_ENTRIES {
            skipped += 1;
            continue;
        }
        let age = md
            .modified()
            .ok()
            .map(|m| age_label(m, now))
            .unwrap_or_default();
        if md.is_dir() {
            dirs.push(ViewRow {
                role: role::DIR,
                text: name,
                detail: age,
                path: ent.path(),
                diagram: None,
                ..ViewRow::default()
            });
        } else {
            let detail = if age.is_empty() {
                size_label(md.len())
            } else {
                format!("{} · {age}", size_label(md.len()))
            };
            files.push(ViewRow {
                role: role::FILE,
                text: name,
                detail,
                path: ent.path(),
                diagram: None,
                ..ViewRow::default()
            });
        }
    }
    let key = |r: &ViewRow| r.text.to_lowercase();
    dirs.sort_by_key(key);
    files.sort_by_key(key);

    let mut rows = Vec::with_capacity(dirs.len() + files.len() + 2);
    if let Some(parent) = dir.parent() {
        rows.push(ViewRow {
            role: role::PARENT,
            text: "..".into(),
            detail: String::new(),
            path: parent.to_path_buf(),
            diagram: None,
            ..ViewRow::default()
        });
    }
    let empty = dirs.is_empty() && files.is_empty();
    rows.append(&mut dirs);
    rows.append(&mut files);
    if empty {
        rows.push(ViewRow::inert(role::NOTICE, "Empty directory"));
    }
    if skipped > 0 {
        rows.push(ViewRow::inert(
            role::NOTICE,
            format!("… {skipped} more entries not shown"),
        ));
    }
    rows
}

/// A file's lines, numbered, bounded by [`MAX_LINES`] / [`MAX_LINE_CHARS`].
#[tracing::instrument(level = "debug", ret)]
pub fn read_lines(file: &Path) -> Vec<ViewRow> {
    let text = match read_text(file) {
        Ok(t) => t,
        Err(row) => return vec![*row],
    };
    let mut rows: Vec<ViewRow> = Vec::new();
    let mut total = 0usize;
    for (i, line) in text.lines().enumerate() {
        total = i + 1;
        if i >= MAX_LINES {
            continue;
        }
        rows.push(ViewRow {
            role: role::LINE,
            text: clip(line),
            detail: (i + 1).to_string(),
            path: PathBuf::new(),
            diagram: None,
            ..ViewRow::default()
        });
    }
    if rows.is_empty() {
        rows.push(ViewRow::inert(role::NOTICE, "Empty file"));
    } else if total > MAX_LINES {
        rows.push(ViewRow::inert(
            role::NOTICE,
            format!("… {} more lines not shown", total - MAX_LINES),
        ));
    }
    rows
}

/// A source file's lines, numbered like [`read_lines`] and coloured.
///
/// Whole-file rather than per-line because a block comment, a triple-quoted string
/// and a template literal all outlive the line that opened them — see
/// [`crate::highlight::markup_lines`]. The palette is an argument rather than a
/// lookup because the colours are baked into the markup: a theme change has to
/// re-project, which is why it is part of the cache [`Fingerprint`].
#[tracing::instrument(level = "debug", ret)]
pub fn highlight_lines(file: &Path, palette: usize) -> Vec<ViewRow> {
    let text = match read_text(file) {
        Ok(t) => t,
        Err(row) => return vec![*row],
    };
    let ext = file
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let total = text.lines().count();
    let shown: Vec<String> = text.lines().take(MAX_LINES).map(clip).collect();
    let refs: Vec<&str> = shown.iter().map(String::as_str).collect();
    // `kind_for_file` already asked `is_source`, so this is `Some` unless the file was
    // renamed underneath the pane — in which case an uncoloured viewer is the right
    // answer, not an error.
    let pal = theme::ui_palette(palette);
    let markup = crate::highlight::markup_lines(&refs, &ext, &pal).unwrap_or_else(|| {
        refs.iter()
            .map(|l| crate::highlight::plain_markup(l, &pal))
            .collect()
    });
    let mut rows: Vec<ViewRow> = shown
        .into_iter()
        .zip(markup)
        .enumerate()
        .map(|(i, (line, markup))| ViewRow {
            role: role::SOURCE,
            text: line,
            markup,
            detail: (i + 1).to_string(),
            ..ViewRow::default()
        })
        .collect();
    if rows.is_empty() {
        rows.push(ViewRow::inert(role::NOTICE, "Empty file"));
    } else if total > MAX_LINES {
        rows.push(ViewRow::inert(
            role::NOTICE,
            format!("… {} more lines not shown", total - MAX_LINES),
        ));
    }
    rows
}

/// A markdown file as styled blocks. The block level is parsed here — headings,
/// lists, quotes, tables, fences, rules — and only the block level: each row's
/// *inline* markup is handed to Slint's `StyledText` with its markers intact.
///
/// That split is the whole design. Inline markup is the half that has to wrap
/// mid-sentence, and only the renderer that measures the glyphs can decide where
/// a line breaks; nothing on this side can, because the UI font is proportional
/// and the only metrics Rust has here are the terminal's monospace cell. So Rust
/// keeps every decision a parser makes and the framework keeps the one decision a
/// typesetter makes.
///
/// Fences win over every other rule while open, so a `# comment` inside a shell
/// snippet stays code.
#[tracing::instrument(level = "debug", ret)]
pub fn markdown_blocks(file: &Path) -> Vec<ViewRow> {
    let text = match read_text(file) {
        Ok(t) => t,
        Err(row) => return vec![*row],
    };
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let cap = total.min(MAX_LINES);
    let mut rows: Vec<ViewRow> = Vec::new();
    // The leading column of every open list level, innermost last. Self-correcting
    // — a shallower item pops back to its own level — so it never needs clearing
    // at a block boundary.
    let mut stack: Vec<usize> = Vec::new();
    // The row a following line may flow into: an open paragraph, list item or
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
            let body: Vec<String> = lines[i + 1..j.min(cap)]
                .iter()
                .map(|l| l.strip_suffix('\r').unwrap_or(l).to_string())
                .collect();
            if is_mermaid(info) {
                rows.append(&mut diagram_rows(&body));
            } else {
                rows.extend(body.iter().map(|l| ViewRow::inert(role::CODE, clip(l))));
            }
            open = None;
            i = j + 1;
            continue;
        }

        // A blank line closes whatever was open and leaves air. Runs of them
        // collapse to one: five blank lines are a typing habit, not five gaps.
        if trimmed.is_empty() {
            open = None;
            if !rows.is_empty() && rows.last().map(|r| r.role) != Some(role::SPACE) {
                rows.push(ViewRow::inert(role::SPACE, ""));
            }
            i += 1;
            continue;
        }

        // A table, header and delimiter together. Checked before the paragraph
        // rules so the header line is never swallowed as prose.
        if i + 1 < cap {
            if let Some(aligns) = table_at(raw, lines[i + 1]) {
                rows.push(table_row(role::TABLE_HEAD, &split_cells(raw), &aligns));
                let mut j = i + 2;
                while j < cap {
                    let body = lines[j].strip_suffix('\r').unwrap_or(lines[j]);
                    if !body.contains('|') || body.trim().is_empty() {
                        break;
                    }
                    rows.push(table_row(role::TABLE_ROW, &split_cells(body), &aligns));
                    j += 1;
                }
                open = None;
                i = j;
                continue;
            }
        }

        if let Some(row) = heading(trimmed) {
            rows.push(row);
            open = None;
            i += 1;
            continue;
        }

        // A setext underline retitles the paragraph above it, and beats the rule
        // below because `---` under prose is a heading in every dialect.
        if let Some(k) = open.filter(|k| rows[*k].role == role::PROSE) {
            if let Some(level) = setext(trimmed) {
                rows[k].role = level;
                rows[k].text = strip_inline(&rows[k].text);
                open = None;
                i += 1;
                continue;
            }
        }

        if is_rule(trimmed) {
            rows.push(ViewRow::inert(role::RULE, ""));
            open = None;
            i += 1;
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix('>') {
            let body = rest.strip_prefix(' ').unwrap_or(rest);
            match open.filter(|k| rows[*k].role == role::QUOTE) {
                Some(k) => flow_into(&mut rows[k], body),
                None => {
                    rows.push(ViewRow::inert(role::QUOTE, clip(body)));
                    open = Some(rows.len() - 1);
                }
            }
            if hard_break(raw) {
                open = None;
            }
            i += 1;
            continue;
        }

        if let Some(item) = list_item(trimmed) {
            let mut row = ViewRow::inert(role::BULLET, clip(item.body));
            row.indent = nest(column_of(raw), &mut stack);
            row.marker = item.marker;
            row.check = item.check;
            rows.push(row);
            open = if hard_break(raw) {
                None
            } else {
                Some(rows.len() - 1)
            };
            i += 1;
            continue;
        }

        // An indented block is only code where there is nothing for it to be a
        // continuation of; under an open list item the same indent means "still
        // the same item".
        if open.is_none() && stack.is_empty() && (raw.starts_with("    ") || raw.starts_with('\t'))
        {
            rows.push(ViewRow::inert(role::CODE, clip(raw.trim_end())));
            i += 1;
            continue;
        }

        if let Some(k) = open {
            flow_into(&mut rows[k], trimmed);
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
        rows.push(ViewRow::inert(role::PROSE, clip(trimmed)));
        open = if hard_break(raw) {
            None
        } else {
            Some(rows.len() - 1)
        };
        i += 1;
    }

    // Trailing air is the file's final newlines, not a block.
    while rows.last().map(|r| r.role) == Some(role::SPACE) {
        rows.pop();
    }
    if rows.is_empty() {
        rows.push(ViewRow::inert(role::NOTICE, "Empty file"));
    } else if total > MAX_LINES {
        rows.push(ViewRow::inert(
            role::NOTICE,
            format!("… {} more lines not shown", total - MAX_LINES),
        ));
    }
    rows
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

/// A mermaid fence body, as either one diagram row or the code block it was.
///
/// Falling back to code is the whole point of the `Result`: mermaid has a dozen
/// dialects and this renders some of them, so the unsupported case is a normal
/// outcome, not an error path. The reader gets the source plus a line saying why —
/// which is strictly more than the preview showed before diagrams existed.
#[tracing::instrument(level = "debug", ret)]
fn diagram_rows(src: &[String]) -> Vec<ViewRow> {
    match crate::mermaid::render(&src.join("\n")) {
        Ok(d) => vec![ViewRow {
            role: role::DIAGRAM,
            text: String::new(),
            detail: String::new(),
            path: PathBuf::new(),
            diagram: Some(Box::new(d)),
            ..ViewRow::default()
        }],
        Err(why) => {
            let mut rows: Vec<ViewRow> = src
                .iter()
                .map(|l| ViewRow::inert(role::CODE, clip(l)))
                .collect();
            rows.push(ViewRow::inert(role::NOTICE, format!("mermaid: {why}")));
            rows
        }
    }
}

/// `#`/`##`/`###+` → the matching heading row. `None` when the line is not a
/// heading — including `#hashtag`, which needs the space ATX requires.
///
/// A heading's inline markers are stripped rather than kept, because a heading is
/// drawn with a plain `Text`: it needs a font weight, and `StyledText` has no
/// property for one.
#[tracing::instrument(level = "debug", ret)]
fn heading(trimmed: &str) -> Option<ViewRow> {
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
    let r = match hashes {
        1 => role::H1,
        2 => role::H2,
        // `####` and deeper share H3's styling rather than disappearing.
        _ => role::H3,
    };
    Some(ViewRow::inert(r, clip(&strip_inline(body))))
}

/// A setext underline → the level it makes the paragraph above it. `=` is H1 and
/// `-` is H2; two dashes are required so a lone `-` stays a list item.
#[tracing::instrument(level = "debug", ret)]
fn setext(trimmed: &str) -> Option<i32> {
    let t = trimmed.trim_end();
    if !t.is_empty() && t.chars().all(|c| c == '=') {
        return Some(role::H1);
    }
    if t.len() >= 2 && t.chars().all(|c| c == '-') {
        return Some(role::H2);
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

/// Append a continuation line to an open block. Joined with a space, not a
/// newline, because the row is one wrapped paragraph and the renderer decides
/// where it breaks.
#[tracing::instrument(level = "debug", ret)]
fn flow_into(row: &mut ViewRow, more: &str) {
    if row.text.chars().count() >= MAX_LINE_CHARS {
        return;
    }
    if !row.text.is_empty() {
        row.text.push(' ');
    }
    row.text.push_str(more.trim_end());
    if row.text.chars().count() > MAX_LINE_CHARS {
        row.text = clip(&row.text);
    }
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

/// One table line as a row. Short lines are padded and long ones truncated to the
/// header's column count, so every row in a table has the same shape.
#[tracing::instrument(level = "debug", ret)]
fn table_row(role: i32, cells: &[String], aligns: &[i32]) -> ViewRow {
    let mut row = ViewRow::inert(role, "");
    row.cells = aligns
        .iter()
        .enumerate()
        .map(|(i, &align)| TableCell {
            text: clip(cells.get(i).map(String::as_str).unwrap_or("")),
            align,
        })
        .collect();
    row
}

/// Inline markdown reduced to the words it was wrapping. Used only where the row
/// is drawn with a plain `Text` instead of Slint's `StyledText` — headings, which
/// need the font weight `StyledText` has no property for.
#[tracing::instrument(level = "debug", ret)]
fn strip_inline(src: &str) -> String {
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

/// Read a file as text, refusing what a preview has no business loading. The error
/// arm is a ready-made [`role::NOTICE`] row so every caller reports failure the
/// same way; it is boxed because a `ViewRow` dwarfs the `String` success arm.
#[tracing::instrument(level = "debug", ret)]
fn read_text(file: &Path) -> Result<String, Box<ViewRow>> {
    let md = fs::metadata(file)
        .map_err(|e| Box::new(ViewRow::inert(role::NOTICE, format!("Cannot read: {e}"))))?;
    if md.is_dir() {
        return Err(Box::new(ViewRow::inert(
            role::NOTICE,
            "That is a directory",
        )));
    }
    if md.len() > MAX_FILE_BYTES {
        return Err(Box::new(ViewRow::inert(
            role::NOTICE,
            format!("File is {} — too large to preview", size_label(md.len())),
        )));
    }
    let bytes = fs::read(file)
        .map_err(|e| Box::new(ViewRow::inert(role::NOTICE, format!("Cannot read: {e}"))))?;
    // A NUL in the first block is the same heuristic `grep` uses for "binary".
    if bytes.iter().take(8_000).any(|b| *b == 0) {
        return Err(Box::new(ViewRow::inert(
            role::NOTICE,
            "Binary file — not previewed",
        )));
    }
    // Lossy on purpose: a stray invalid byte should cost one replacement char, not
    // the whole preview.
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Truncate to [`MAX_LINE_CHARS`] *chars* (never bytes — a byte slice can split a
/// UTF-8 sequence) and drop the trailing `\r` a CRLF file leaves on every line.
#[tracing::instrument(level = "debug", ret)]
fn clip(s: &str) -> String {
    let s = s.strip_suffix('\r').unwrap_or(s);
    if s.chars().count() <= MAX_LINE_CHARS {
        return s.to_string();
    }
    let mut out: String = s.chars().take(MAX_LINE_CHARS).collect();
    out.push('…');
    out
}

/// "1.2 KB" — binary units, one decimal past KB, so a listing column stays narrow.
#[tracing::instrument(level = "debug", ret)]
fn size_label(bytes: u64) -> String {
    const K: f64 = 1024.0;
    let b = bytes as f64;
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    for (i, unit) in ["KB", "MB", "GB", "TB"].iter().enumerate() {
        let div = K.powi(i as i32 + 1);
        if b < div * K || *unit == "TB" {
            return format!("{:.1} {unit}", b / div);
        }
    }
    unreachable!("the TB arm is unconditional")
}

/// "3m", "5h", "2d", "Aug 30" — the same shape the left panel's session rows use,
/// so the two lists read as one product.
#[tracing::instrument(level = "debug", ret)]
fn age_label(t: SystemTime, now: u64) -> String {
    let then = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if then == 0 || then > now {
        return String::new();
    }
    let d = now - then;
    match d {
        0..=59 => "now".into(),
        60..=3599 => format!("{}m", d / 60),
        3600..=86_399 => format!("{}h", d / 3600),
        _ => format!("{}d", d / 86_400),
    }
}

#[tracing::instrument(level = "debug", ret)]
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---- V2 table -------------------------------------------------------------
// The grid projection (docs/viewer-panes-plan.md WP2). Parsing and layout live in
// `crate::csv`; this is only the mapping onto rows.

/// A CSV/TSV file as a grid: the first record is the header ([`role::TABLE_HEAD`]),
/// every other one a body row ([`role::TABLE_ROW`]), each with one cell per column.
///
/// Bounded by [`MAX_LINES`] records (with a [`role::NOTICE`] saying how many more
/// there were) and [`crate::csv::MAX_COL_CHARS`] per cell. Every failure — a file
/// that cannot be read, one that is not CSV, an empty one — is a single inert
/// notice row, never a panic: the pump calls this on whatever path the workspace
/// file names.
///
/// The row's `detail` is its 1-based record number (the header is row 1, the way a
/// spreadsheet counts), and its `text` is the verbatim cells joined by ` | ` — what
/// a screen reader announces for the row. A markdown table leaves both empty, which
/// is how the shared 13/14 view block tells a grid from a document table.
#[tracing::instrument(level = "debug", ret)]
pub fn table_rows(file: &Path) -> Vec<ViewRow> {
    let text = match read_text(file) {
        Ok(t) => t,
        Err(row) => return vec![*row],
    };
    let delim = crate::csv::delimiter_for(file);
    let records = match crate::csv::parse(&text, delim) {
        Ok(r) => r,
        Err(e) => {
            return vec![ViewRow::inert(
                role::NOTICE,
                format!("Cannot parse as {}: {e}", crate::csv::format_name(delim)),
            )];
        }
    };
    if records.is_empty() {
        return vec![ViewRow::inert(role::NOTICE, "Empty file")];
    }
    let total = records.len();
    let shown = &records[..total.min(MAX_LINES)];
    let cap = crate::csv::MAX_COL_CHARS;
    let layout = crate::csv::Layout::of(shown, cap);
    let mut rows: Vec<ViewRow> = shown
        .iter()
        .enumerate()
        .map(|(i, rec)| {
            let role = if i == 0 {
                role::TABLE_HEAD
            } else {
                role::TABLE_ROW
            };
            let mut row = ViewRow::inert(role, clip(&rec.join(" | ")));
            row.detail = (i + 1).to_string();
            row.cells = (0..layout.columns())
                .map(|j| TableCell {
                    text: layout.cell(rec, j, cap),
                    align: if layout.numeric[j] { 2 } else { 0 },
                })
                .collect();
            row
        })
        .collect();
    if total > MAX_LINES {
        rows.push(ViewRow::inert(
            role::NOTICE,
            format!("… {} more rows not shown", total - MAX_LINES),
        ));
    }
    rows
}

// ---------------------------------------------------------------------------
// the per-pane model cache
// ---------------------------------------------------------------------------

/// What a projection was computed from. Re-projecting is a disk walk, so the pump
/// must not do it per tick — but it must notice an edit. The fingerprint is the
/// target's path plus its mtime and length, which is exactly what changes when the
/// answer would change.
#[derive(Clone, PartialEq, Eq)]
struct Fingerprint {
    kind: i32,
    target: String,
    mtime: u64,
    len: u64,
    /// Only a source pane reads it, but every pane carries it: the syntax colours are
    /// baked into each row's markup, so a palette change is a content change. Without
    /// this the viewer would keep the old theme's ink until the file was next touched.
    palette: usize,
    /// The one projection input that is not the file: a data pane's fold generation
    /// (see [`crate::datatree::generation`]) or a module pane's row generation (see
    /// [`crate::module_ui::rows::generation`]), so a toggle or a fresh `host.rows.set` is
    /// a cache miss rather than a stale view. Zero for every other kind.
    revision: u64,
}

#[tracing::instrument(level = "debug")]
fn fingerprint(kind: &PaneKind, target: Option<&str>, palette: usize) -> Fingerprint {
    let target = target.unwrap_or_default().to_string();
    let (mtime, len) = fs::metadata(&target)
        .ok()
        .map(|md| {
            let m = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            (m, md.len())
        })
        // A missing target still needs a stable fingerprint, so the "Cannot read"
        // notice is computed once rather than every tick.
        .unwrap_or((0, 0));
    Fingerprint {
        kind: kind.ui_kind(),
        target,
        mtime,
        len,
        palette,
        revision: 0,
    }
}

/// One cached projection.
struct Cached {
    fp: Fingerprint,
    /// Bumped every time this pane is re-projected. The pump has no other way to
    /// ask "did anything change?" — `ModelRc` has no identity comparison — and the
    /// tests assert on it rather than on wall-clock behaviour.
    /// Bumped on every reprojection. Read only by the tests — this slint revision has
    /// no `ModelRc::ptr_eq`, so a generation counter is how "did the model change?"
    /// is asserted at all.
    #[cfg_attr(not(test), allow(dead_code))]
    gen: u64,
    rows: Vec<ViewRow>,
    model: ModelRc<PaneViewRow>,
    /// The human's row selection as (anchor, focus), or `None` for nothing
    /// selected. It lives here, beside the rows it indexes, so it dies with them:
    /// a reprojection replaces this whole struct, and row 40 of the old file has
    /// no claim on row 40 of the new one.
    sel: Option<(usize, usize)>,
}

thread_local! {
    /// Per-pane projections, keyed by pane uid. Holds the *Slint* model, not just
    /// the rows: `pane_item` runs every tick and must be able to hand the model
    /// over with a refcount bump instead of rebuilding a 5,000-row `VecModel`.
    static VIEW_CACHE: RefCell<HashMap<String, Cached>> = RefCell::new(HashMap::new());
    /// Window-wide projection counter (see [`Cached::gen`]).
    static VIEW_GEN: RefCell<u64> = const { RefCell::new(0) };
}

/// The Slint row model for pane `uid`, recomputed only when the target changed on
/// disk. Cheap enough to call from the per-frame pump — the steady state is one
/// `stat` and a refcount bump.
/// The "no diagram" value. `w == 0` is what the view tests, so a blank one is
/// inert without needing a second flag.
#[tracing::instrument(level = "debug", ret)]
fn blank_diagram() -> PaneDiagram {
    PaneDiagram {
        w: 0.0,
        h: 0.0,
        nodes: ModelRc::from(Rc::new(VecModel::from(Vec::<DiagNode>::new()))),
        labels: ModelRc::from(Rc::new(VecModel::from(Vec::<DiagLabel>::new()))),
        lines: Default::default(),
        dashed: Default::default(),
        thick: Default::default(),
        heads: Default::default(),
        diamonds: Default::default(),
    }
}

/// Hand a laid-out diagram to Slint. A straight field-for-field copy: the layout
/// is finished before it gets here, and this side adds no geometry of its own.
#[tracing::instrument(level = "debug", ret)]
fn diagram_model(d: &crate::mermaid::Diagram) -> PaneDiagram {
    PaneDiagram {
        w: d.w,
        h: d.h,
        nodes: ModelRc::from(Rc::new(VecModel::from(
            d.nodes
                .iter()
                .map(|n| DiagNode {
                    x: n.x,
                    y: n.y,
                    w: n.w,
                    h: n.h,
                    shape: n.shape,
                    text: n.text.as_str().into(),
                })
                .collect::<Vec<_>>(),
        ))),
        labels: ModelRc::from(Rc::new(VecModel::from(
            d.labels
                .iter()
                .map(|l| DiagLabel {
                    x: l.x,
                    y: l.y,
                    w: l.w,
                    text: l.text.as_str().into(),
                })
                .collect::<Vec<_>>(),
        ))),
        lines: d.lines.as_str().into(),
        dashed: d.dashed.as_str().into(),
        thick: d.thick.as_str().into(),
        heads: d.heads.as_str().into(),
        diamonds: d.diamonds.as_str().into(),
    }
}

/// Whether a role's text is a wrapping run of inline markdown. These are the
/// rows the view hands to `StyledText`; every other role is either verbatim
/// (a viewer line, a code line) or drawn (a rule, a diagram, air).
#[tracing::instrument(level = "debug", ret)]
fn flows(role: i32) -> bool {
    matches!(role, role::PROSE | role::BULLET | role::QUOTE)
}

/// Inline markdown for the view. A source that will not parse still has to be
/// readable, so it falls back to itself as plain text rather than vanishing.
#[tracing::instrument(level = "debug", ret)]
fn markdown_text(src: &str) -> slint::StyledText {
    slint::StyledText::from_markdown(src)
        .unwrap_or_else(|_| slint::StyledText::from_plain_text(src))
}

/// The non-file input a pane's projection depends on, by kind. Two stores answer this —
/// folds for a data pane, projected rows for a module pane — and no other kind has one.
#[tracing::instrument(level = "debug", ret)]
fn revision(uid: &str, kind: &PaneKind) -> u64 {
    match kind {
        // A module pane may be tier 2 (rows) or tier 5 (a grid), and the same pane can
        // carry both stores over its life — the sum changes whichever side painted, and a
        // sum cannot go backwards the way picking one of the two could.
        PaneKind::Module(m) => crate::module_ui::rows::generation(&m.id, &m.surface)
            .wrapping_add(crate::module_ui::grid::generation(&m.id, &m.surface)),
        _ => crate::datatree::generation(uid),
    }
}

#[tracing::instrument(level = "debug", ret)]
pub fn model_for(
    uid: &str,
    kind: &PaneKind,
    target: Option<&str>,
    palette: usize,
) -> ModelRc<PaneViewRow> {
    let fp = Fingerprint {
        revision: revision(uid, kind),
        ..fingerprint(kind, target, palette)
    };
    VIEW_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if let Some(have) = c.get(uid) {
            if have.fp == fp {
                return have.model.clone();
            }
        }
        let rows = rows_for_pane(uid, kind, target, palette);
        // Built once and cloned: a `PaneDiagram` holds two `ModelRc`s, and minting
        // a fresh empty pair for each of 5,000 plain rows is 10,000 allocations to
        // say "no diagram here".
        let blank = blank_diagram();
        // Same economy for the two fields only markdown uses: parsing 5,000 lines
        // of a source file as inline markdown would mangle its `*`s and cost a
        // full parse per row to do it.
        let no_md = slint::StyledText::default();
        let no_cells: ModelRc<PaneCell> =
            ModelRc::from(Rc::new(VecModel::from(Vec::<PaneCell>::new())));
        let model: ModelRc<PaneViewRow> = ModelRc::from(Rc::new(VecModel::from(
            rows.iter()
                .map(|r| PaneViewRow {
                    role: r.role,
                    text: r.text.as_str().into(),
                    detail: r.detail.as_str().into(),
                    activatable: r.activatable(),
                    diagram: match &r.diagram {
                        Some(d) => diagram_model(d),
                        None => blank.clone(),
                    },
                    // A source row's markup is already markdown — built by `highlight`,
                    // escaped so it round-trips — so it takes the same channel prose does.
                    // `text` stays the raw line for copy, selection and the a11y label.
                    md: if !r.markup.is_empty() {
                        markdown_text(&r.markup)
                    } else if flows(r.role) {
                        markdown_text(&r.text)
                    } else {
                        no_md.clone()
                    },
                    indent: r.indent,
                    marker: r.marker.as_str().into(),
                    check: r.check,
                    cells: if r.cells.is_empty() {
                        no_cells.clone()
                    } else {
                        ModelRc::from(Rc::new(VecModel::from(
                            r.cells
                                .iter()
                                .map(|c| PaneCell {
                                    md: markdown_text(&c.text),
                                    align: c.align,
                                    // The words, not the markup: a grid cell arrives
                                    // padded with NBSPs and markdown-escaped, a table
                                    // cell with its `**` markers, and a reader wants
                                    // neither.
                                    text: strip_inline(&c.text).trim_matches('\u{a0}').into(),
                                })
                                .collect::<Vec<_>>(),
                        )))
                    },
                })
                .collect::<Vec<_>>(),
        )));
        let gen = VIEW_GEN.with(|g| {
            let mut g = g.borrow_mut();
            *g += 1;
            *g
        });
        c.insert(
            uid.to_string(),
            Cached {
                fp,
                gen,
                rows,
                model: model.clone(),
                sel: None,
            },
        );
        model
    })
}

/// Which projection pane `uid` is currently showing. Unequal values mean the rows
/// were rebuilt between the two calls; `None` means the pane has no projection.
#[cfg(test)]
pub fn generation(uid: &str) -> Option<u64> {
    VIEW_CACHE.with(|c| c.borrow().get(uid).map(|e| e.gen))
}

/// The row a click landed on, by index into the model that was on screen. Index is
/// safe here (unlike the session list, which is keyed by id) because the model and
/// the rows are stored together and replaced together — a stale index can only
/// miss, never resolve to a different row's path.
#[tracing::instrument(level = "debug", ret)]
pub fn row_at(uid: &str, index: usize) -> Option<ViewRow> {
    VIEW_CACHE.with(|c| c.borrow().get(uid).and_then(|e| e.rows.get(index).cloned()))
}

/// Pane `uid`'s selection as (anchor, focus) — unordered, in the order the human
/// made it. `None` when nothing is selected or the pane has no projection.
#[tracing::instrument(level = "debug", ret)]
pub fn selection(uid: &str) -> Option<(usize, usize)> {
    VIEW_CACHE.with(|c| c.borrow().get(uid).and_then(|e| e.sel))
}

/// The selected range as (lo, hi), or `None`. What the view and the clipboard both
/// want; [`selection`] is for the code that has to know which end the human is
/// dragging.
#[tracing::instrument(level = "debug", ret)]
pub fn selected_range(uid: &str) -> Option<(usize, usize)> {
    selection(uid).map(|(a, b)| if a <= b { (a, b) } else { (b, a) })
}

/// Move pane `uid`'s selection to `row`, either replacing it (`extend` false) or
/// stretching the existing anchor to reach it (`extend` true). A shift-click with
/// nothing selected yet has no anchor to stretch, so it starts one.
#[tracing::instrument(level = "debug", ret)]
pub fn select_row(uid: &str, row: usize, extend: bool) {
    VIEW_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        let Some(e) = c.get_mut(uid) else { return };
        if row >= e.rows.len() {
            return;
        }
        e.sel = Some(match (extend, e.sel) {
            (true, Some((anchor, _))) => (anchor, row),
            _ => (row, row),
        });
    });
}

/// Select every row of pane `uid`. A no-op on a pane with no projection.
#[tracing::instrument(level = "debug", ret)]
pub fn select_all(uid: &str) {
    VIEW_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        let Some(e) = c.get_mut(uid) else { return };
        e.sel = (!e.rows.is_empty()).then(|| (0, e.rows.len() - 1));
    });
}

/// Drop pane `uid`'s selection. Test-only: in the app a selection is dropped by the thing
/// that made it meaningless — `forget` when the pane goes, a rebuilt projection when the
/// file changes — and never on its own, so an always-compiled version would be dead code.
#[cfg(test)]
pub fn clear_selection(uid: &str) {
    VIEW_CACHE.with(|c| {
        if let Some(e) = c.borrow_mut().get_mut(uid) {
            e.sel = None;
        }
    });
}

/// How many rows pane `uid` is currently showing. `0` when it has no projection
/// (a pty pane, or a view pane whose model has not been built yet), which is also
/// the answer that makes "select all" a no-op there. Test-only for the same reason
/// [`clear_selection`] is: the app reads the rows themselves, never just their count.
#[cfg(test)]
pub fn row_count(uid: &str) -> usize {
    VIEW_CACHE.with(|c| c.borrow().get(uid).map_or(0, |e| e.rows.len()))
}

/// Rows `lo..=hi` of pane `uid` joined into one clipboard string.
///
/// Reads the retained `Vec<ViewRow>` rather than the Slint model: the rows are the
/// projection's own form, and the model has already thrown away everything a copy
/// needs (a heading's level, a table's cells, a bullet's nesting). The range is
/// clamped and order-normalised here so callers may hand over a selection anchor
/// and focus in whichever order the human dragged them.
#[tracing::instrument(level = "debug", ret)]
pub fn copy_range(uid: &str, a: usize, b: usize) -> String {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    VIEW_CACHE.with(|c| {
        let c = c.borrow();
        let Some(e) = c.get(uid) else {
            return String::new();
        };
        if lo >= e.rows.len() {
            return String::new();
        }
        e.rows[lo..=hi.min(e.rows.len() - 1)]
            .iter()
            .map(|r| r.copy_text())
            .collect::<Vec<_>>()
            .join("\n")
    })
}

/// Drop a pane's projection. Called when the pane closes so a long-lived window
/// does not accumulate the row vectors of every view pane it ever had.
#[tracing::instrument(level = "debug", ret)]
pub fn forget(uid: &str) {
    VIEW_CACHE.with(|c| {
        c.borrow_mut().remove(uid);
    });
}

/// The kind a file should open as when it is activated from a browser row: a
/// markdown file gets the preview, everything else the plain viewer. Deterministic
/// and extension-driven — the alternative (sniffing content) would make the same
/// click do different things on different days.
#[tracing::instrument(level = "debug", ret)]
pub fn kind_for_file(path: &Path) -> PaneKind {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "md" | "markdown" | "mdown" | "mkd" => PaneKind::Markdown,
        "csv" | "tsv" | "tab" => PaneKind::Table,
        // Track V3: a bitmap this build can decode opens in the image pane. Before this
        // arm a PNG fell to the plain viewer, whose NUL heuristic then refused it.
        e if crate::imagepane::is_image_ext(e) => PaneKind::Image,
        "json" | "jsonl" | "ndjson" => PaneKind::Data,
        // Anything the highlighter has a grammar for opens coloured; everything else
        // keeps the plain viewer, which is still the honest answer for a `.log`.
        e if crate::highlight::is_source(e) => PaneKind::Code,
        _ => PaneKind::FileViewer,
    }
}

// ---- V1 data tree

/// [`rows_for`] with the pane's own fold state: the one projection input that is not
/// the file. Only a data pane has any; every other kind is [`rows_for`] unchanged.
#[tracing::instrument(level = "debug", ret)]
fn rows_for_pane(uid: &str, kind: &PaneKind, target: Option<&str>, palette: usize) -> Vec<ViewRow> {
    match (kind, target.filter(|t| !t.is_empty())) {
        (PaneKind::Data, Some(t)) => {
            data_rows(Path::new(t), &crate::datatree::flipped(uid), palette)
        }
        // Not the file — the module. Its rows arrive on `host.rows.set` and the pane is
        // whatever the module last said, including nothing at all before it has spoken.
        (PaneKind::Module(m), _) => crate::module_ui::rows::view_rows(&m.id, &m.surface),
        _ => rows_for(kind, target, palette),
    }
}

/// A data pane's rows: the file as a tree, or, when it will not parse, a NOTICE naming
/// the line followed by the plain viewer's rows, so a broken `.json` is still readable
/// and the error is still on screen. The read guards ("too large", "binary", a
/// directory) are [`read_text`]'s, the same as every other viewer.
#[tracing::instrument(level = "debug", ret)]
fn data_rows(file: &Path, flipped: &BTreeSet<String>, palette: usize) -> Vec<ViewRow> {
    let text = match read_text(file) {
        Ok(t) => t,
        Err(row) => return vec![*row],
    };
    let jsonl = matches!(
        file.extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .as_deref(),
        Some("jsonl" | "ndjson")
    );
    match crate::datatree::tree_rows(&text, jsonl, file, flipped, &theme::ui_palette(palette)) {
        Ok(rows) => rows,
        Err(msg) if msg == "Empty file" => vec![ViewRow::inert(role::NOTICE, msg)],
        Err(msg) => {
            let mut rows = vec![ViewRow::inert(role::NOTICE, msg)];
            rows.extend(read_lines(file));
            rows
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slint::Model;
    use std::io::Write;

    /// A scratch directory unique to the calling test, removed and recreated so a
    /// rerun never sees the previous run's files.
    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hp-viewpane-{name}"));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).expect("scratch dir");
        d
    }

    #[test]
    fn a_relative_href_resolves_beside_the_document() {
        let d = scratch("href-rel");
        fs::create_dir_all(d.join("docs")).unwrap();
        fs::write(d.join("README.md"), b"x").unwrap();
        fs::write(d.join("docs/design.md"), b"x").unwrap();
        let doc = d.join("README.md");
        // Relative to the FILE's directory, not to the process cwd.
        let got = resolve_local_href(&doc, "docs/design.md").expect("resolves");
        assert_eq!(got, d.join("docs/design.md").canonicalize().unwrap());
        // A fragment names the same file: a link to a heading is still a link to a document.
        let got = resolve_local_href(&doc, "docs/design.md#rationale").expect("resolves");
        assert_eq!(got, d.join("docs/design.md").canonicalize().unwrap());
        // `..` walks out of the document's directory the way the author meant it to.
        let nested = d.join("docs/design.md");
        let got = resolve_local_href(&nested, "../README.md").expect("resolves");
        assert_eq!(got, doc.canonicalize().unwrap());
    }

    #[test]
    fn an_href_with_nothing_behind_it_resolves_to_nothing() {
        let d = scratch("href-none");
        fs::write(d.join("README.md"), b"x").unwrap();
        let doc = d.join("README.md");
        // Nothing on disk → the caller falls back to `open_link`, which refuses anything that
        // is not http/https/mailto. That refusal is the whole guard against a downloaded
        // document turning a `javascript:` or a missing path into an action.
        assert!(resolve_local_href(&doc, "javascript:alert(1)").is_none());
        assert!(resolve_local_href(&doc, "does/not/exist.md").is_none());
        assert!(resolve_local_href(&doc, "").is_none());
        assert!(resolve_local_href(&doc, "#only-a-fragment").is_none());
    }

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        let mut f = fs::File::create(&p).expect("create");
        f.write_all(body.as_bytes()).expect("write");
        p
    }

    #[test]
    fn a_listing_puts_the_parent_first_then_dirs_then_files() {
        let d = scratch("listing");
        fs::create_dir(d.join("zeta")).unwrap();
        fs::create_dir(d.join("Alpha")).unwrap();
        write(&d, "b.txt", "hi");
        write(&d, "A.txt", "hi");

        let rows = list_dir(&d);
        let shape: Vec<(i32, &str)> = rows.iter().map(|r| (r.role, r.text.as_str())).collect();
        assert_eq!(
            shape,
            vec![
                (role::PARENT, ".."),
                // case-insensitive within each group, dirs before files
                (role::DIR, "Alpha"),
                (role::DIR, "zeta"),
                (role::FILE, "A.txt"),
                (role::FILE, "b.txt"),
            ]
        );
        // The parent row navigates; it is not decoration.
        assert_eq!(rows[0].path, d.parent().unwrap());
        assert!(rows.iter().all(|r| r.activatable()));
    }

    #[test]
    fn an_unreadable_target_reports_why_instead_of_going_blank() {
        let missing = std::env::temp_dir().join("hp-viewpane-nope-does-not-exist");
        let _ = fs::remove_dir_all(&missing);

        for rows in [
            list_dir(&missing),
            read_lines(&missing),
            markdown_blocks(&missing),
        ] {
            assert_eq!(rows.len(), 1, "one notice row, not an empty pane");
            assert_eq!(rows[0].role, role::NOTICE);
            assert!(rows[0].text.starts_with("Cannot read:"), "{}", rows[0].text);
            // A notice is never clickable.
            assert!(!rows[0].activatable());
        }
    }

    #[test]
    fn the_viewer_numbers_lines_and_says_when_it_stopped() {
        let d = scratch("viewer");
        let body: String = (1..=MAX_LINES + 7).map(|i| format!("line {i}\n")).collect();
        let f = write(&d, "long.txt", &body);

        let rows = read_lines(&f);
        assert_eq!(rows.len(), MAX_LINES + 1, "the cap plus one notice");
        assert_eq!(rows[0].detail, "1");
        assert_eq!(rows[0].text, "line 1");
        assert_eq!(rows[MAX_LINES - 1].detail, MAX_LINES.to_string());
        let last = rows.last().unwrap();
        assert_eq!(last.role, role::NOTICE);
        // The count is what was DROPPED, not the file length.
        assert_eq!(last.text, "… 7 more lines not shown");
    }

    #[test]
    fn a_binary_file_is_refused_rather_than_rendered_as_mojibake() {
        let d = scratch("binary");
        let p = d.join("a.bin");
        fs::write(&p, [0x7f, 0x45, 0x4c, 0x46, 0x00, 0x01, 0x02]).unwrap();

        let rows = read_lines(&p);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "Binary file — not previewed");
    }

    #[test]
    fn markdown_maps_each_block_to_its_role() {
        let d = scratch("md");
        let f = write(
            &d,
            "doc.md",
            "# Title\n\
             ## Sub\n\
             #### Deep\n\
             #nothashtag\n\
             \n\
             plain text\n\
             - first\n\
             2. second\n\
             > quoted\n\
             ---\n",
        );

        let blocks = markdown_blocks(&f);
        let got: Vec<(i32, &str)> = blocks.iter().map(|r| (r.role, r.text.as_str())).collect();
        assert_eq!(
            got,
            vec![
                (role::H1, "Title"),
                (role::H2, "Sub"),
                // `####` shares H3 rather than vanishing.
                (role::H3, "Deep"),
                // ATX needs the space, so this is prose.
                (role::PROSE, "#nothashtag"),
                (role::SPACE, ""),
                (role::PROSE, "plain text"),
                (role::BULLET, "first"),
                (role::BULLET, "second"),
                (role::QUOTE, "quoted"),
                (role::RULE, ""),
            ]
        );
        // The ordinal is the author's, kept verbatim: a list that renumbers itself
        // 1, 2, 3 when the author wrote 2, 3, 4 misquotes the document.
        assert_eq!(blocks[6].marker, "");
        assert_eq!(blocks[7].marker, "2.");
    }

    #[test]
    fn a_fence_wins_over_every_other_markdown_rule() {
        let d = scratch("fence");
        let f = write(
            &d,
            "doc.md",
            "before\n```sh\n# not a heading\n- not a bullet\n```\nafter\n",
        );

        let rows = markdown_blocks(&f);
        let got: Vec<(i32, String)> = rows.iter().map(|r| (r.role, r.text.clone())).collect();
        assert_eq!(
            got,
            vec![
                (role::PROSE, "before".into()),
                // the fence lines themselves are dropped, their contents stay verbatim
                (role::CODE, "# not a heading".into()),
                (role::CODE, "- not a bullet".into()),
                (role::PROSE, "after".into()),
            ]
        );
    }

    #[test]
    fn a_paragraph_is_one_row_however_many_lines_it_was_typed_on() {
        let d = scratch("flow");
        let f = write(&d, "doc.md", "one\ntwo\nthree\n\nnext para\n");

        let rows = markdown_blocks(&f);
        let got: Vec<(i32, &str)> = rows.iter().map(|r| (r.role, r.text.as_str())).collect();
        assert_eq!(
            got,
            vec![
                // Joined with a space, not a newline: the row is one wrapping unit
                // and the renderer decides where it breaks.
                (role::PROSE, "one two three"),
                (role::SPACE, ""),
                (role::PROSE, "next para"),
            ]
        );
    }

    #[test]
    fn two_trailing_spaces_break_the_line_instead_of_flowing_into_it() {
        let d = scratch("hardbreak");
        let f = write(&d, "doc.md", "first  \nsecond\n");

        let rows = markdown_blocks(&f);
        let got: Vec<&str> = rows.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(got, vec!["first", "second"]);
        assert!(rows.iter().all(|r| r.role == role::PROSE));
    }

    #[test]
    fn a_run_of_blank_lines_is_one_gap_not_five() {
        let d = scratch("air");
        let f = write(&d, "doc.md", "a\n\n\n\n\nb\n\n\n");

        let rows = markdown_blocks(&f);
        let got: Vec<i32> = rows.iter().map(|r| r.role).collect();
        // And the trailing newlines leave nothing: they are the file ending, not
        // a block.
        assert_eq!(got, vec![role::PROSE, role::SPACE, role::PROSE]);
    }

    #[test]
    fn a_setext_underline_retitles_the_paragraph_above_it() {
        let d = scratch("setext");
        let f = write(&d, "doc.md", "Big\n===\n\nSmall\n---\n\nalone\n\n---\n");

        let rows = markdown_blocks(&f);
        let got: Vec<(i32, &str)> = rows.iter().map(|r| (r.role, r.text.as_str())).collect();
        assert_eq!(
            got,
            vec![
                (role::H1, "Big"),
                (role::SPACE, ""),
                (role::H2, "Small"),
                (role::SPACE, ""),
                (role::PROSE, "alone"),
                (role::SPACE, ""),
                // With a blank line between, the dashes are a rule again.
                (role::RULE, ""),
            ]
        );
    }

    #[test]
    fn a_nested_list_keeps_its_depth_and_its_task_boxes() {
        let d = scratch("nested");
        let f = write(
            &d,
            "doc.md",
            "- top\n  - under\n    - deeper\n  - back\n- [ ] todo\n- [x] done\n",
        );

        let rows = markdown_blocks(&f);
        assert!(rows.iter().all(|r| r.role == role::BULLET));
        let got: Vec<(i32, i32, &str)> = rows
            .iter()
            .map(|r| (r.indent, r.check, r.text.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                (0, -1, "top"),
                (1, -1, "under"),
                (2, -1, "deeper"),
                // The stack pops back rather than counting a fourth level.
                (1, -1, "back"),
                (0, 0, "todo"),
                (0, 1, "done"),
            ]
        );
    }

    #[test]
    fn a_table_becomes_rows_of_aligned_cells() {
        let d = scratch("table");
        let f = write(
            &d,
            "doc.md",
            "| name | qty | cost |\n|:--|:--:|--:|\n| bolt | 4 | 1.20 |\n| nut | 12 |\n\nafter\n",
        );

        let rows = markdown_blocks(&f);
        let got: Vec<i32> = rows.iter().map(|r| r.role).collect();
        assert_eq!(
            got,
            vec![
                role::TABLE_HEAD,
                role::TABLE_ROW,
                role::TABLE_ROW,
                role::SPACE,
                role::PROSE,
            ]
        );
        let aligns: Vec<i32> = rows[0].cells.iter().map(|c| c.align).collect();
        assert_eq!(aligns, vec![0, 1, 2]);
        let head: Vec<&str> = rows[0].cells.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(head, vec!["name", "qty", "cost"]);
        // A short row is padded to the header's shape, so the columns still line up.
        let short: Vec<&str> = rows[2].cells.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(short, vec!["nut", "12", ""]);
    }

    #[test]
    fn a_paragraph_with_a_pipe_in_it_is_not_a_table() {
        let d = scratch("pipe");
        let f = write(&d, "doc.md", "a | b\nnot a delimiter\n");

        let rows = markdown_blocks(&f);
        assert!(rows.iter().all(|r| r.role == role::PROSE));
    }

    #[test]
    fn a_heading_keeps_its_words_and_loses_its_markers() {
        let d = scratch("inline");
        let f = write(
            &d,
            "doc.md",
            "# A **bold** `word` ##\n\nkeep **these** markers\n",
        );

        let rows = markdown_blocks(&f);
        // Stripped, because a heading is drawn with a plain Text: it needs a font
        // weight, and StyledText has no property for one.
        assert_eq!(rows[0].role, role::H1);
        assert_eq!(rows[0].text, "A bold word");
        // Prose keeps them: they are the renderer's input, not ours.
        assert_eq!(rows[2].text, "keep **these** markers");
    }

    #[test]
    fn inline_markers_come_off_without_taking_the_words_with_them() {
        // snake_case is a word, not emphasis; a link keeps its text and drops its
        // target; an escape yields the character it was hiding.
        assert_eq!(strip_inline("a *b* _c_ `d`"), "a b c d");
        assert_eq!(
            strip_inline("call some_long_name now"),
            "call some_long_name now"
        );
        assert_eq!(
            strip_inline("see [the docs](http://x/y) here"),
            "see the docs here"
        );
        assert_eq!(strip_inline("see [the docs][ref]"), "see the docs");
        assert_eq!(strip_inline("~~gone~~ and \\*kept\\*"), "gone and *kept*");
    }

    #[test]
    fn a_tilde_fence_closes_on_tildes_and_ignores_backticks() {
        let d = scratch("tilde");
        let f = write(&d, "doc.md", "~~~\n```\nstill code\n~~~\nout\n");

        let rows = markdown_blocks(&f);
        let got: Vec<(i32, &str)> = rows.iter().map(|r| (r.role, r.text.as_str())).collect();
        assert_eq!(
            got,
            vec![
                (role::CODE, "```"),
                (role::CODE, "still code"),
                (role::PROSE, "out"),
            ]
        );
    }

    #[test]
    fn a_mermaid_fence_collapses_to_one_diagram_row() {
        let d = scratch("mermaid");
        let f = write(
            &d,
            "doc.md",
            "intro\n```mermaid\nflowchart TD\n  A[start] --> B[stop]\n```\nouttro\n",
        );

        let rows = markdown_blocks(&f);
        let got: Vec<i32> = rows.iter().map(|r| r.role).collect();
        assert_eq!(got, vec![role::PROSE, role::DIAGRAM, role::PROSE]);
        let dg = rows[1]
            .diagram
            .as_ref()
            .expect("the row carries its geometry");
        assert_eq!(dg.nodes.len(), 2);
        assert!(dg.w > 0.0 && dg.h > 0.0);
        // The source lines are gone: a diagram replaces its fence, it does not
        // follow it.
        assert!(!rows.iter().any(|r| r.text.contains("flowchart")));
    }

    #[test]
    fn a_dialect_we_cannot_draw_falls_back_to_the_code_it_was() {
        let d = scratch("mermaid-fallback");
        let f = write(&d, "doc.md", "```mermaid\ngantt\n  title Roadmap\n```\n");

        let rows = markdown_blocks(&f);
        assert!(!rows.iter().any(|r| r.role == role::DIAGRAM));
        assert_eq!(rows[0].role, role::CODE);
        assert_eq!(rows[0].text, "gantt");
        let last = rows.last().unwrap();
        assert_eq!(last.role, role::NOTICE);
        assert!(last.text.starts_with("mermaid: gantt"), "{}", last.text);
    }

    #[test]
    fn only_a_mermaid_fence_is_a_diagram() {
        let d = scratch("mermaid-other");
        let f = write(&d, "doc.md", "```rust\nflowchart TD\n A --> B\n```\n");

        let rows = markdown_blocks(&f);
        assert!(rows.iter().all(|r| r.role == role::CODE));
    }

    #[test]
    fn an_unterminated_mermaid_fence_still_draws() {
        let d = scratch("mermaid-open");
        let f = write(&d, "doc.md", "```mermaid\nflowchart LR\n A --> B\n");

        let rows = markdown_blocks(&f);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].role, role::DIAGRAM);
    }

    #[test]
    fn the_cache_reprojects_only_when_the_file_actually_changed() {
        let d = scratch("cache");
        let f = write(&d, "a.txt", "one\n");
        let target = f.display().to_string();
        let uid = "view-1";

        let first = model_for(uid, &PaneKind::FileViewer, Some(&target), 0);
        assert_eq!(first.row_count(), 1);
        let g1 = generation(uid).expect("projected");

        // Same fingerprint → no rebuild at all, which is what makes this safe to
        // call from the per-frame pump.
        let again = model_for(uid, &PaneKind::FileViewer, Some(&target), 0);
        assert_eq!(again.row_count(), 1);
        assert_eq!(
            generation(uid),
            Some(g1),
            "an unchanged target must not reproject"
        );

        // Rewrite with a different length: the fingerprint moves even if the
        // filesystem's mtime resolution would not have caught the edit.
        fs::write(&f, "one\ntwo\n").unwrap();
        let third = model_for(uid, &PaneKind::FileViewer, Some(&target), 0);
        assert_eq!(third.row_count(), 2);
        assert_ne!(generation(uid), Some(g1), "an edit must reproject");

        // row_at reads the rows stored alongside that model.
        assert_eq!(row_at(uid, 1).unwrap().text, "two");
        assert!(row_at(uid, 99).is_none());
        forget(uid);
        assert!(row_at(uid, 0).is_none());
        assert_eq!(generation(uid), None);
    }

    #[test]
    fn a_row_copies_as_the_words_it_showed() {
        // The plain viewer is verbatim — it was never rendering markup.
        assert_eq!(
            ViewRow::inert(role::LINE, "  let x = *p;").copy_text(),
            "  let x = *p;"
        );
        // A preview's prose showed the *resolved* inline markup, so that is what a
        // copy of it owes the reader.
        assert_eq!(
            ViewRow::inert(role::PROSE, "a **bold** and `code`").copy_text(),
            "a bold and code"
        );
        // A heading arrives with its markers already gone (see `heading`).
        assert_eq!(ViewRow::inert(role::H2, "Design").copy_text(), "Design");
        assert_eq!(
            ViewRow::inert(role::QUOTE, "so said he").copy_text(),
            "> so said he"
        );
        assert_eq!(ViewRow::inert(role::RULE, "").copy_text(), "---");
        assert_eq!(ViewRow::inert(role::SPACE, "").copy_text(), "");

        // A bullet re-spells the structure that was drawn as a dot, an ordinal,
        // a task box and an indent — plain text has no other way to carry it.
        let mut b = ViewRow::inert(role::BULLET, "ship it");
        b.indent = 2;
        b.check = 0;
        assert_eq!(b.copy_text(), "    - [ ] ship it");
        b.marker = "3.".into();
        b.check = 1;
        b.indent = 0;
        assert_eq!(b.copy_text(), "3. [x] ship it");

        let mut t = ViewRow::inert(role::TABLE_ROW, "");
        t.cells = vec![
            TableCell {
                text: "**a**".into(),
                align: 0,
            },
            TableCell {
                text: "b".into(),
                align: 0,
            },
        ];
        assert_eq!(t.copy_text(), "a | b");
    }

    #[test]
    fn a_selection_is_a_row_range_that_dies_with_its_projection() {
        let d = scratch("select");
        let f = write(&d, "a.txt", "one\ntwo\nthree\nfour\n");
        let target = f.display().to_string();
        let uid = "view-sel";
        model_for(uid, &PaneKind::FileViewer, Some(&target), 0);
        assert_eq!(row_count(uid), 4);
        assert_eq!(selected_range(uid), None);

        // A plain click starts a one-row selection; shift-click stretches the anchor.
        select_row(uid, 1, false);
        assert_eq!(selected_range(uid), Some((1, 1)));
        select_row(uid, 3, true);
        assert_eq!(selection(uid), Some((1, 3)));
        assert_eq!(copy_range(uid, 1, 3), "two\nthree\nfour");

        // Backwards is the same range: the anchor is where the human started, not
        // where the range begins.
        select_row(uid, 0, true);
        assert_eq!(selection(uid), Some((1, 0)));
        assert_eq!(selected_range(uid), Some((0, 1)));
        assert_eq!(copy_range(uid, 1, 0), "one\ntwo");

        // Past the end is refused rather than clamped: an index that no longer
        // exists names no row, and pretending it named the last one would copy
        // something the human never clicked.
        select_row(uid, 99, false);
        assert_eq!(
            selection(uid),
            Some((1, 0)),
            "an out-of-range row changes nothing"
        );

        select_all(uid);
        assert_eq!(selected_range(uid), Some((0, 3)));
        assert_eq!(copy_range(uid, 0, 3), "one\ntwo\nthree\nfour");
        // A range running off the end still copies what is there.
        assert_eq!(copy_range(uid, 2, 99), "three\nfour");
        assert_eq!(copy_range(uid, 99, 99), "");
        assert_eq!(copy_range("no-such-pane", 0, 0), "");

        clear_selection(uid);
        assert_eq!(selected_range(uid), None);

        // An edit reprojects, and row 3 of the old file has no claim on the new one.
        select_all(uid);
        assert!(selected_range(uid).is_some());
        fs::write(&f, "one\n").unwrap();
        model_for(uid, &PaneKind::FileViewer, Some(&target), 0);
        assert_eq!(
            selected_range(uid),
            None,
            "a reprojection must drop the selection it indexed"
        );
        forget(uid);
        assert_eq!(row_count(uid), 0);
    }

    #[test]
    fn only_family_b_kinds_project_rows() {
        let d = scratch("kinds");
        let f = write(&d, "a.txt", "x\n");
        let t = f.display().to_string();
        assert!(rows_for(&PaneKind::Terminal, Some(&t), 0).is_empty());
        assert!(rows_for(&PaneKind::Tool("claude".into()), Some(&t), 0).is_empty());
        assert!(!rows_for(&PaneKind::FileViewer, Some(&t), 0).is_empty());
        // No target is a notice, not a panic and not an empty pane.
        let none = rows_for(&PaneKind::FileBrowser, None, 0);
        assert_eq!(none[0].role, role::NOTICE);
        assert_eq!(none[0].text, "No path set for this pane");
    }

    #[test]
    fn a_file_opens_in_the_pane_that_can_read_it() {
        // Markdown first: it is the only extension that renders as prose.
        assert_eq!(kind_for_file(Path::new("/a/README.md")), PaneKind::Markdown);
        assert_eq!(kind_for_file(Path::new("/a/README.MD")), PaneKind::Markdown);
        // Then anything `highlight` has a syntax for, whatever its case.
        assert_eq!(kind_for_file(Path::new("/a/main.rs")), PaneKind::Code);
        assert_eq!(kind_for_file(Path::new("/a/App.TSX")), PaneKind::Code);
        assert_eq!(kind_for_file(Path::new("/a/q.sql")), PaneKind::Code);
        // And everything else falls back to the plain viewer, including a file with no
        // extension at all — guessing a language from a shebang is a bigger promise than
        // this pane makes.
        assert_eq!(kind_for_file(Path::new("/a/LICENSE")), PaneKind::FileViewer);
        assert_eq!(
            kind_for_file(Path::new("/a/notes.txt")),
            PaneKind::FileViewer
        );
    }

    /// The source viewer end to end, from a real file on disk to the rows the pane draws.
    /// Every row has to keep its line verbatim — `text` is what a copy yields and what the
    /// screen reader announces — while `markup` carries the same line with colour, and the
    /// two must describe the same bytes or the pane is lying about the file.
    #[test]
    fn a_source_file_reads_back_line_for_line_with_colour_beside_it() {
        let dir = std::env::temp_dir().join(format!("hp-src-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("sample.rs");
        let src = "fn main() {\n    let x = 1; // one\n}\n";
        std::fs::write(&file, src).unwrap();

        let rows = rows_for(&PaneKind::Code, file.to_str(), 0);
        assert_eq!(rows.len(), 3, "one row per line, so row N is line N");
        for (i, (row, line)) in rows.iter().zip(src.lines()).enumerate() {
            assert_eq!(row.role, role::SOURCE);
            assert_eq!(row.text, line, "line {} lost its bytes", i + 1);
            assert_eq!(row.detail, (i + 1).to_string(), "wrong gutter number");
            assert!(
                row.markup.contains("<font color="),
                "line {} came through uncoloured: {:?}",
                i + 1,
                row.markup
            );
        }

        // The colours are baked into the markup, so they cannot be re-themed by the
        // `.slint`; the palette is part of what `rows_for` is asked for, and a different
        // palette has to produce different ink. `Fingerprint` carries it for this reason.
        let latte = rows_for(&PaneKind::Code, file.to_str(), 3);
        assert_eq!(latte.len(), rows.len());
        assert_eq!(
            latte[0].text, rows[0].text,
            "the source itself must not move"
        );
        assert_ne!(
            latte[0].markup, rows[0].markup,
            "a palette switch has to re-ink the source"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A file whose extension `highlight` has no syntax for can still be routed to `Code`
    /// by a rename under a live pane. That must degrade to plain rows, not to an error row
    /// and not to a panic — the file is still perfectly readable without colour.
    #[test]
    fn an_unknown_extension_still_renders_as_source_just_without_colour() {
        let dir = std::env::temp_dir().join(format!("hp-src-x-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("sample.zzz");
        std::fs::write(&file, "alpha\nbeta\n").unwrap();

        let rows = rows_for(&PaneKind::Code, file.to_str(), 0);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].role, role::SOURCE);
        assert_eq!(rows[0].text, "alpha");
        // Uncoloured, but present: the row draws `markup`, so an empty one would be a
        // blank line where the file's text should be.
        assert_eq!(rows[0].markup, "alpha");
        assert!(!rows[0].markup.contains("<font"), "no syntax means no ink");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn labels_stay_narrow_enough_for_the_detail_column() {
        assert_eq!(size_label(0), "0 B");
        assert_eq!(size_label(999), "999 B");
        assert_eq!(size_label(1024), "1.0 KB");
        assert_eq!(size_label(1536), "1.5 KB");
        assert_eq!(size_label(2 * 1024 * 1024), "2.0 MB");
        assert_eq!(size_label(3 * 1024 * 1024 * 1024), "3.0 GB");

        let now = 1_000_000u64;
        assert_eq!(
            age_label(UNIX_EPOCH + std::time::Duration::from_secs(now), now),
            "now"
        );
        assert_eq!(
            age_label(UNIX_EPOCH + std::time::Duration::from_secs(now - 300), now),
            "5m"
        );
        assert_eq!(
            age_label(UNIX_EPOCH + std::time::Duration::from_secs(now - 7200), now),
            "2h"
        );
        assert_eq!(
            age_label(
                UNIX_EPOCH + std::time::Duration::from_secs(now - 3 * 86_400),
                now
            ),
            "3d"
        );
        // A clock skewed into the future must not underflow into a huge age.
        assert_eq!(
            age_label(UNIX_EPOCH + std::time::Duration::from_secs(now + 60), now),
            ""
        );
    }

    #[test]
    fn a_long_line_is_clipped_on_char_boundaries() {
        let wide = "é".repeat(MAX_LINE_CHARS + 50);
        let out = clip(&wide);
        assert_eq!(
            out.chars().count(),
            MAX_LINE_CHARS + 1,
            "the ellipsis is the +1"
        );
        assert!(out.ends_with('…'));
        // CRLF must not leave a visible carriage return.
        assert_eq!(clip("hi\r"), "hi");
    }

    #[test]
    fn the_title_is_the_last_two_components() {
        assert_eq!(
            view_title(&PaneKind::FileBrowser, Some("/Users/me/code/app")),
            "code/app"
        );
        assert_eq!(view_title(&PaneKind::FileViewer, Some("/etc")), "/etc");
        assert_eq!(view_title(&PaneKind::Markdown, Some("")), "");
        // Family A panes have no view title at all.
        assert_eq!(view_title(&PaneKind::Terminal, Some("/Users/me")), "");
    }

    // ---- V2 table ---------------------------------------------------------

    /// The cells of one row as the strings the view will draw.
    fn cells(row: &ViewRow) -> Vec<&str> {
        row.cells.iter().map(|c| c.text.as_str()).collect()
    }

    #[test]
    fn a_csv_becomes_a_head_row_and_body_rows_of_padded_cells() {
        let d = scratch("csv-basic");
        let f = write(
            &d,
            "parts.csv",
            "name,qty,price\nbolt,4,1.20\n\"nut, brass\",12,0.5\n",
        );

        let rows = rows_for(&PaneKind::Table, Some(&f.display().to_string()), 0);
        let roles: Vec<i32> = rows.iter().map(|r| r.role).collect();
        assert_eq!(
            roles,
            vec![role::TABLE_HEAD, role::TABLE_ROW, role::TABLE_ROW]
        );
        // Row 1 is the header, the way a spreadsheet counts.
        let details: Vec<&str> = rows.iter().map(|r| r.detail.as_str()).collect();
        assert_eq!(details, vec!["1", "2", "3"]);

        // Column widths come from the widest cell: "nut, brass" is 10, so "name"
        // carries six NBSPs; the numeric columns are padded on the left instead.
        assert_eq!(
            cells(&rows[0]),
            vec!["name\u{a0}\u{a0}\u{a0}\u{a0}\u{a0}\u{a0}", "qty", "price"]
        );
        assert_eq!(
            cells(&rows[2]),
            vec!["nut, brass", "\u{a0}12", "\u{a0}\u{a0}0\\.5"]
        );
        let aligns: Vec<i32> = rows[1].cells.iter().map(|c| c.align).collect();
        assert_eq!(aligns, vec![0, 2, 2], "numbers sit flush right, words left");

        // What a screen reader announces and what a copy yields are the verbatim
        // cells — no padding, no escapes.
        assert_eq!(rows[2].text, "nut, brass | 12 | 0.5");
        assert_eq!(rows[2].copy_text(), "nut, brass | 12 | 0.5");
        assert!(!rows[1].activatable(), "a grid row opens nothing");
    }

    #[test]
    fn a_tsv_splits_on_tabs_and_a_csv_with_the_same_bytes_does_not() {
        let d = scratch("csv-tsv");
        let body = "a\tb\n1,5\t2\n";
        let tsv = write(&d, "data.tsv", body);
        let csv = write(&d, "data.csv", body);

        let rows = table_rows(&tsv);
        assert_eq!(cells(&rows[0]), vec!["a\u{a0}\u{a0}", "b"]);
        assert_eq!(cells(&rows[1]), vec!["1,5", "2"]);

        // In a `.csv` the tab is content (drawn as a space) and the comma splits: the
        // header is one cell, and the `1` column is numeric so it sits flush right.
        let rows = table_rows(&csv);
        assert_eq!(cells(&rows[0]), vec!["a b", "\u{a0}\u{a0}\u{a0}"]);
        assert_eq!(cells(&rows[1]), vec!["\u{a0}\u{a0}1", "5 2"]);
    }

    #[test]
    fn a_quoted_line_break_stays_one_row_and_shows_as_a_pilcrow() {
        let d = scratch("csv-newline");
        let f = write(&d, "n.csv", "id,note\n1,\"first\nsecond\"\n2,plain\n");
        let rows = table_rows(&f);
        assert_eq!(rows.len(), 3, "an embedded newline is not a row break");
        // ("id" is two wide and numeric, so its `1` is padded on the left.)
        assert_eq!(cells(&rows[1]), vec!["\u{a0}1", "first¶second"]);
        assert_eq!(rows[1].text, "1 | first\nsecond");
    }

    #[test]
    fn a_ragged_csv_is_squared_to_its_widest_row() {
        let d = scratch("csv-ragged");
        let f = write(&d, "r.csv", "a,b\n1\n1,2,3\n");
        let rows = table_rows(&f);
        for r in &rows {
            assert_eq!(r.cells.len(), 3, "every row has the widest row's shape");
        }
        assert_eq!(cells(&rows[0]), vec!["a", "b", "\u{a0}"]);
        assert_eq!(cells(&rows[1]), vec!["1", "\u{a0}", "\u{a0}"]);
    }

    #[test]
    fn a_header_only_csv_is_one_head_row_and_an_empty_one_a_notice() {
        let d = scratch("csv-header-only");
        let f = write(&d, "h.csv", "a,b,c\n");
        let rows = table_rows(&f);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].role, role::TABLE_HEAD);
        assert_eq!(cells(&rows[0]), vec!["a", "b", "c"]);

        let e = write(&d, "e.csv", "");
        let rows = table_rows(&e);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].role, role::NOTICE);
        assert_eq!(rows[0].text, "Empty file");
    }

    #[test]
    fn a_csv_that_will_not_parse_is_one_notice_naming_the_line() {
        let d = scratch("csv-bad");
        let f = write(&d, "bad.csv", "a,b\n1,\"open\n2,3\n");
        let rows = table_rows(&f);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].role, role::NOTICE);
        assert_eq!(
            rows[0].text,
            "Cannot parse as CSV: unterminated quoted field starting on line 2"
        );
        // The same defect in a `.tsv` names the format it was read as.
        let t = write(&d, "bad.tsv", "\"open\n");
        assert!(table_rows(&t)[0].text.starts_with("Cannot parse as TSV:"));
    }

    #[test]
    fn a_vanished_csv_and_a_directory_are_notices_not_panics() {
        let d = scratch("csv-gone");
        let rows = table_rows(&d.join("nope.csv"));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].role, role::NOTICE);
        assert!(
            rows[0].text.starts_with("Cannot read:"),
            "{:?}",
            rows[0].text
        );

        let rows = rows_for(&PaneKind::Table, Some(&d.display().to_string()), 0);
        assert_eq!(rows[0].text, "That is a directory");
        let rows = rows_for(&PaneKind::Table, None, 0);
        assert_eq!(rows[0].text, "No path set for this pane");
    }

    #[test]
    fn a_thousand_rows_fit_and_the_row_cap_ends_in_a_count() {
        let d = scratch("csv-cap");
        let mut body = String::from("n,sq\n");
        for i in 1..=1_000 {
            body.push_str(&format!("{i},{}\n", i * i));
        }
        let f = write(&d, "k.csv", &body);
        let rows = table_rows(&f);
        assert_eq!(rows.len(), 1_001, "a thousand rows are under the cap");
        assert_eq!(rows.last().unwrap().role, role::TABLE_ROW);
        assert_eq!(cells(&rows[1_000]), vec!["1000", "1000000"]);

        let mut body = String::from("n\n");
        for i in 1..=(MAX_LINES + 10) {
            body.push_str(&format!("{i}\n"));
        }
        let f = write(&d, "big.csv", &body);
        let rows = table_rows(&f);
        // The header counts as a record, so the cap keeps MAX_LINES records + 1 notice.
        assert_eq!(rows.len(), MAX_LINES + 1);
        assert_eq!(rows[MAX_LINES - 1].role, role::TABLE_ROW);
        let last = rows.last().unwrap();
        assert_eq!(last.role, role::NOTICE);
        assert_eq!(last.text, "… 11 more rows not shown");
    }

    #[test]
    fn a_cell_past_the_column_cap_is_elided_rather_than_widening_the_column() {
        let d = scratch("csv-wide");
        let long = "x".repeat(300);
        let f = write(&d, "w.csv", &format!("k,v\n1,{long}\n2,short\n"));
        let rows = table_rows(&f);
        let cap = crate::csv::MAX_COL_CHARS;
        let wide = rows[1].cells[1].text.as_str();
        assert_eq!(
            wide.chars().count(),
            cap,
            "cut to the cap, ellipsis included"
        );
        assert!(wide.ends_with('…'));
        // Every other cell in the column is padded to that same width, not to 300.
        assert_eq!(rows[2].cells[1].text.chars().count(), cap);
        assert!(rows[2].cells[1].text.starts_with("short\u{a0}"));
    }

    #[test]
    fn model_for_carries_the_grid_into_the_pane_model() {
        use i_slint_core::styled_text::get_raw_text;
        let d = scratch("csv-model");
        let f = write(&d, "m.csv", "name,qty\nbolt,4\n\"a *b*\",12\n");
        let target = f.display().to_string();
        let uid = "view-table";

        let model = model_for(uid, &PaneKind::Table, Some(&target), 0);
        assert_eq!(model.row_count(), 3);
        let head = model.row_data(0).unwrap();
        assert_eq!(head.role, role::TABLE_HEAD);
        assert_eq!(head.text.as_str(), "name | qty");
        assert_eq!(head.detail.as_str(), "1");
        assert_eq!(head.cells.row_count(), 2);

        // The markdown channel draws the escaped cell as its literal text: the
        // asterisks are shown, not eaten as emphasis.
        let body = model.row_data(2).unwrap();
        let cell = body.cells.row_data(0).unwrap();
        assert_eq!(get_raw_text(&cell.md).trim_end_matches('\u{a0}'), "a *b*");
        assert_eq!(body.cells.row_data(1).unwrap().align, 2);

        // The pane's copy path sees the same rows.
        assert_eq!(row_at(uid, 1).unwrap().copy_text(), "bolt | 4");
    }

    #[test]
    fn a_csv_or_tsv_opens_in_the_table_pane() {
        assert_eq!(kind_for_file(Path::new("/a/data.csv")), PaneKind::Table);
        assert_eq!(kind_for_file(Path::new("/a/DATA.CSV")), PaneKind::Table);
        assert_eq!(kind_for_file(Path::new("/a/data.tsv")), PaneKind::Table);
        assert_eq!(kind_for_file(Path::new("/a/data.tab")), PaneKind::Table);
        // A file that merely mentions the format is not one.
        assert_eq!(kind_for_file(Path::new("/a/csv.txt")), PaneKind::FileViewer);
    }

    // ---- V1 data tree

    #[test]
    fn a_json_file_opens_as_a_data_pane() {
        for name in ["package.json", "PACKAGE.JSON", "log.jsonl", "events.ndjson"] {
            assert_eq!(
                kind_for_file(Path::new(name)),
                PaneKind::Data,
                "{name} is a data file"
            );
        }
        // No YAML or TOML parser is in the tree, and a data pane that cannot parse would
        // only ever show the "not valid JSON" notice: these keep the plain viewer.
        assert_eq!(kind_for_file(Path::new("/a/ci.yaml")), PaneKind::FileViewer);
        assert_eq!(
            kind_for_file(Path::new("/a/Cargo.toml")),
            PaneKind::FileViewer
        );
    }

    /// The data pane end to end: a file on disk, the model the pane draws, and a toggle
    /// through the same cache. Folding is the one input to a projection that is not the
    /// file — if the fold generation ever leaves the fingerprint, the second `model_for`
    /// here is a cache hit and the tree never folds.
    #[test]
    fn a_data_pane_projects_the_tree_and_a_toggle_reprojects_it() {
        let d = scratch("datatree");
        let f = write(
            &d,
            "pkg.json",
            "{\"name\": \"x\", \"deps\": {\"serde\": \"1\", \"tokio\": \"1\"}, \"ok\": true}\n",
        );
        let target = f.display().to_string();
        let uid = "view-datatree-model";
        crate::datatree::forget(uid);
        forget(uid);

        let m = model_for(uid, &PaneKind::Data, Some(&target), 0);
        assert_eq!(m.row_count(), 5, "name, deps, serde, tokio, ok");
        for i in 0..5 {
            assert_eq!(m.row_data(i).unwrap().role, role::DATA_NODE);
        }
        let g1 = generation(uid).expect("projected");

        // The container row is the one that can reach the click; a scalar is inert.
        let deps = row_at(uid, 1).unwrap();
        assert_eq!(deps.text, "deps");
        assert_eq!(deps.node, "$.deps");
        assert_eq!(deps.check, 1, "depth 0 opens by default");
        assert!(deps.activatable());
        assert_eq!(deps.path, f);
        assert!(m.row_data(1).unwrap().activatable);
        let name = row_at(uid, 0).unwrap();
        assert_eq!(name.text, "name: \"x\"");
        assert!(!name.activatable());
        assert!(!m.row_data(0).unwrap().activatable);
        // Slint sees depth as `indent` and the disclosure as `check`.
        assert_eq!(m.row_data(2).unwrap().indent, 1);
        assert_eq!(m.row_data(1).unwrap().check, 1);
        assert_eq!(m.row_data(0).unwrap().check, -1);

        // Same file, same folds: a cache hit.
        let again = model_for(uid, &PaneKind::Data, Some(&target), 0);
        assert_eq!(again.row_count(), 5);
        assert_eq!(generation(uid), Some(g1));

        // Fold `deps`: its two children go, `ok` after it stays, and the row is folded.
        crate::datatree::toggle(uid, "$.deps");
        let folded = model_for(uid, &PaneKind::Data, Some(&target), 0);
        assert_eq!(folded.row_count(), 3, "name, deps (folded), ok");
        assert_ne!(generation(uid), Some(g1), "a toggle must reproject");
        assert_eq!(row_at(uid, 1).unwrap().check, 0);
        assert_eq!(folded.row_data(1).unwrap().check, 0);
        assert_eq!(folded.row_data(1).unwrap().detail, "2 keys");
        assert_eq!(row_at(uid, 2).unwrap().text, "ok: true");

        // And back: the same five rows.
        crate::datatree::toggle(uid, "$.deps");
        let open = model_for(uid, &PaneKind::Data, Some(&target), 0);
        assert_eq!(open.row_count(), 5);
        assert_eq!(row_at(uid, 1).unwrap().check, 1);

        crate::datatree::forget(uid);
        forget(uid);
    }

    #[test]
    fn a_broken_json_file_still_reads_as_text_with_the_error_first() {
        let d = scratch("datatree-broken");
        let f = write(&d, "bad.json", "{\n  \"a\": 1,\n  \"b\": ,\n}\n");
        let rows = rows_for(&PaneKind::Data, f.to_str(), 0);
        assert_eq!(rows[0].role, role::NOTICE);
        assert!(
            rows[0].text.starts_with("Not valid JSON: "),
            "{}",
            rows[0].text
        );
        assert!(rows[0].text.contains("line 3"), "{}", rows[0].text);
        // Then the file, as the plain viewer shows it: the error is on screen and so is
        // the text it is about.
        let plain = read_lines(&f);
        assert_eq!(rows.len(), 1 + plain.len());
        assert_eq!(rows[1].role, role::LINE);
        assert_eq!(rows[1].text, "{");
        assert_eq!(rows[3].text, "  \"b\": ,");

        // An empty file is the same notice every viewer gives, and only that.
        let e = write(&d, "empty.json", "");
        let rows = rows_for(&PaneKind::Data, e.to_str(), 0);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].role, rows[0].text.as_str()),
            (role::NOTICE, "Empty file")
        );

        // The read guards are shared with every other viewer, not reinvented here.
        let rows = rows_for(&PaneKind::Data, d.to_str(), 0);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].role, role::NOTICE);
        assert!(rows[0].text.contains("directory"), "{}", rows[0].text);
        let rows = rows_for(&PaneKind::Data, None, 0);
        assert_eq!(rows[0].text, "No path set for this pane");
    }

    #[test]
    fn a_data_pane_reinks_its_values_when_the_palette_changes() {
        let d = scratch("datatree-palette");
        let f = write(&d, "p.json", "{\"n\": 1}\n");
        let target = f.display().to_string();
        let uid = "view-datatree-palette";
        forget(uid);
        let mocha = model_for(uid, &PaneKind::Data, Some(&target), 0);
        let m0 = row_at(uid, 0).unwrap().markup;
        let g = generation(uid).unwrap();
        let latte = model_for(uid, &PaneKind::Data, Some(&target), 3);
        assert_ne!(generation(uid), Some(g), "a palette switch reprojects");
        assert_ne!(row_at(uid, 0).unwrap().markup, m0);
        assert_eq!(mocha.row_count(), latte.row_count());
        assert_eq!(
            row_at(uid, 0).unwrap().text,
            "n: 1",
            "the words do not move"
        );
        forget(uid);
    }
}
