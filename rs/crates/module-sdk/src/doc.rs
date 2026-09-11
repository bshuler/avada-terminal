//! The document surface a module fills with prose (UI tier 5, one-directional).
//!
//! Tier 5 has two shapes. A [grid](crate::grid) is a rectangle of character cells that
//! takes keystrokes back — an editor. A *document* is the other half: a stream of parsed
//! blocks the host typesets and the reader only reads. A markdown preview, a rendered
//! man page, a diff summary — anything whose content is authored elsewhere and shown, not
//! typed into.
//!
//! **The module owns the text; the host owns the pixels** — the same bargain the grid
//! strikes, and the reason a document is a tier-5 surface rather than a tier-1 row list.
//! Rows are single strings the host prints verbatim; a document's blocks carry structure
//! the host is trusted to *render*: a [`Block::Mermaid`] ships its source and the host
//! draws the diagram, a [`Block::Code`] ships a language tag and the host may highlight it,
//! a [`Block::Prose`] ships raw inline markdown and the host wraps it in a proportional
//! font it alone can measure. The module never rasterises anything, because the glyph
//! metrics — where a proportional line breaks, what a mermaid graph looks like — live on
//! the host's side of the process boundary and only there.
//!
//! **There is no `module.doc.*` reply.** A rendered document takes no input: a keystroke
//! in a doc surface is a scroll or a find, both the pane's own furniture, never the
//! module's concern. A surface that needs to answer for its keys is a [grid](crate::grid),
//! not a doc — which is why the whole of this module is one direction, host ← module, sent
//! as [`Doc`] through [`crate::contract::methods::HOST_DOC_SET`].
//!
//! Like a grid frame, a [`Doc`] is not incremental: a doc replaces a doc, because the two
//! processes can restart independently and a whole document is the only statement that is
//! true no matter what the other side missed.

use serde::{Deserialize, Serialize};

/// A whole document: `host.doc.set` params.
///
/// The block list is the entire content; sending a new [`Doc`] for the same `surface`
/// replaces the last one outright.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Doc {
    /// The surface id — the same contribution id [`crate::rail::SetRows::entry`] uses, and
    /// the `surface` the pane was spawned with.
    pub surface: String,
    /// The blocks, top to bottom. Empty is a legal document; the host shows it as such
    /// rather than as an error.
    #[serde(default)]
    pub blocks: Vec<Block>,
}

impl Doc {
    /// A document for one surface from its blocks.
    pub fn new(surface: impl Into<String>, blocks: Vec<Block>) -> Self {
        Self {
            surface: surface.into(),
            blocks,
        }
    }
}

/// One block of a document.
///
/// **Internally tagged and deliberately not `deny_unknown_fields`.** A block is
/// `{ "kind": "prose", "text": "…" }` on the wire, and a host that meets a `kind` it does
/// not know, or a field it does not read, keeps going — this is the forward-compatibility
/// seam that lets a newer module add a block variant, or a field to one, without a
/// contract-version bump the host would refuse to negotiate. Everything optional carries a
/// `#[serde(default)]` so an older sender that omits it round-trips too.
///
/// The variants are the *block* level of markdown and no more: the inline level — the
/// emphasis and links inside [`Block::Prose::text`] — is left as raw markdown for the host
/// to lay out, because only the renderer measuring proportional glyphs can decide where an
/// inline run wraps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Block {
    /// A heading. `level` is 1-based (`1` is the largest); a host that renders only a few
    /// tiers clamps the rest to its smallest rather than dropping the heading.
    Heading {
        /// 1 = top level. Values past the host's deepest tier are clamped, not discarded.
        level: u8,
        /// The heading's text, inline markup already stripped: a heading is drawn with a
        /// weight, and styled-text runs have no property for one.
        #[serde(default)]
        text: String,
    },
    /// A paragraph. `text` is raw inline markdown — emphasis, code spans and links intact —
    /// for the host to wrap and style.
    Prose {
        /// Raw inline markdown; the host owns the wrapping.
        #[serde(default)]
        text: String,
    },
    /// A list item. Lists are flattened to their items: `depth` carries the nesting the
    /// tree would have, so the host indents without needing the tree.
    Bullet {
        /// Nesting depth, zero at the outermost level.
        #[serde(default)]
        depth: u8,
        /// The rendered marker — `"2."` for an ordered item, empty for a bullet, so the
        /// host need not re-derive ordinals it cannot see the siblings of.
        #[serde(default)]
        marker: String,
        /// Task state: `-1` not a task, `0` an unchecked box, `1` a checked one.
        #[serde(default)]
        check: i8,
        /// The item's text, raw inline markdown like [`Block::Prose`].
        #[serde(default)]
        text: String,
    },
    /// A block quote, its `>` markers already removed.
    Quote {
        /// Raw inline markdown.
        #[serde(default)]
        text: String,
    },
    /// A thematic break — a horizontal rule.
    Rule,
    /// A table: a header row and the body rows, each a list of [`Cell`]s. Alignment rides
    /// on the cell rather than a parallel column list, so a row is self-describing.
    Table {
        /// The header cells, left to right.
        #[serde(default)]
        headers: Vec<Cell>,
        /// The body rows, each already padded or clipped to the table's column count by
        /// whoever built it.
        #[serde(default)]
        rows: Vec<Vec<Cell>>,
    },
    /// A fenced or indented code block. `text` is the verbatim source with its own
    /// newlines; `lang` is the fence's info word (`rust`, `sh`, empty for none) so the
    /// host may highlight it. The host owns the highlighting — the module ships only the
    /// source and the tag.
    Code {
        /// The info-string language, or empty.
        #[serde(default)]
        lang: String,
        /// Verbatim source, newlines and all.
        #[serde(default)]
        text: String,
    },
    /// A mermaid diagram, shipped as its **source** — never as a rendered picture. The
    /// host parses and draws it, and falls back to showing the source when it cannot, so a
    /// dialect the host does not speak degrades to a readable code block rather than an
    /// error.
    Mermaid {
        /// The diagram source, newlines and all.
        #[serde(default)]
        source: String,
    },
    /// Vertical air between blocks — a collapsed run of blank lines. A block rather than a
    /// margin because the host draws documents as a flat list and a gap is the only thing
    /// that says "the author left space here".
    Space,
    /// A host-authored aside: an empty-document placeholder, a "more not shown" cap notice,
    /// or a module's own "cannot render this" line. Not markdown content — a note *about*
    /// the document, drawn quietly apart from it.
    Notice {
        /// The note's text.
        #[serde(default)]
        text: String,
    },
}

/// One cell of a [`Block::Table`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cell {
    /// The cell's text, raw inline markdown like [`Block::Prose`].
    #[serde(default)]
    pub text: String,
    /// Column alignment: `0` left, `1` centre, `2` right. Left is the default, so it is the
    /// value an omitted field restores.
    #[serde(default)]
    pub align: i8,
}

impl Cell {
    /// A left-aligned cell — the common case, so the three-field struct literal is worth
    /// not spelling out at every call site.
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            align: 0,
        }
    }

    /// A cell with an explicit alignment.
    pub fn aligned(text: impl Into<String>, align: i8) -> Self {
        Self {
            text: text.into(),
            align,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_doc_round_trips_and_omits_what_it_does_not_use() {
        let doc = Doc::new(
            "preview",
            vec![
                Block::Heading {
                    level: 1,
                    text: "Title".into(),
                },
                Block::Prose {
                    text: "a *paragraph* with a [link](x)".into(),
                },
                Block::Space,
            ],
        );
        let wire = serde_json::to_value(&doc).unwrap();
        // The tag is the discriminator, and a unit-ish variant carries nothing else.
        assert_eq!(wire["blocks"][0]["kind"], "heading");
        assert_eq!(wire["blocks"][2], json!({ "kind": "space" }));
        assert_eq!(
            serde_json::from_value::<Doc>(wire).unwrap(),
            doc,
            "and everything dropped had a default that restores it"
        );
    }

    #[test]
    fn a_bullet_round_trips_with_its_task_state() {
        let bullet = Block::Bullet {
            depth: 1,
            marker: "2.".into(),
            check: 1,
            text: "done".into(),
        };
        let wire = serde_json::to_value(&bullet).unwrap();
        assert_eq!(
            wire,
            json!({ "kind": "bullet", "depth": 1, "marker": "2.", "check": 1, "text": "done" }),
            "a bullet's structure — depth, ordinal marker, checkbox — is all on the wire: {wire}"
        );
        assert_eq!(serde_json::from_value::<Block>(wire).unwrap(), bullet);

        // And an omitted default deserializes back to the plain, unchecked outermost item.
        let bare: Block = serde_json::from_value(json!({ "kind": "bullet", "text": "x" })).unwrap();
        assert_eq!(
            bare,
            Block::Bullet {
                depth: 0,
                marker: String::new(),
                check: 0,
                text: "x".into()
            },
        );
    }

    #[test]
    fn a_table_cells_carry_their_own_alignment() {
        let table = Block::Table {
            headers: vec![Cell::plain("Name"), Cell::aligned("Size", 2)],
            rows: vec![vec![Cell::plain("a.txt"), Cell::aligned("12", 2)]],
        };
        let wire = serde_json::to_value(&table).unwrap();
        assert_eq!(wire["kind"], "table");
        assert_eq!(wire["headers"][1]["align"], 2);
        assert_eq!(
            serde_json::from_value::<Block>(wire).unwrap(),
            table,
            "a table's cells carry their own alignment and restore it verbatim"
        );
        // An omitted align defaults to left (0).
        let cell: Cell = serde_json::from_value(json!({ "text": "x" })).unwrap();
        assert_eq!(cell.align, 0);
    }

    #[test]
    fn an_unknown_field_is_tolerated_not_rejected() {
        // The forward-compatibility contract: a newer sender adds a field, an older host
        // ignores it rather than failing the whole doc.
        let block: Block = serde_json::from_value(json!({
            "kind": "prose",
            "text": "hello",
            "annotation": "from a future version"
        }))
        .expect("an unknown field must not break deserialization");
        assert_eq!(block, Block::Prose { text: "hello".into() });
    }

    #[test]
    fn code_keeps_its_language_and_its_newlines() {
        let code = Block::Code {
            lang: "rust".into(),
            text: "fn main() {}\n// two lines".into(),
        };
        let back: Block = serde_json::from_value(serde_json::to_value(&code).unwrap()).unwrap();
        assert_eq!(back, code);
    }
}
