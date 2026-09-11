//! The document a tier-5 module typesets into (contract `host.doc.set`), the reader half of
//! tier 5.
//!
//! Tier 5 has two shapes and this is the one that takes no keys back. A [grid](super::grid)
//! is a rectangle of cells the module owns and the reader edits; a *document* is a stream of
//! parsed [`Block`]s the module authors and the host only renders — a markdown preview, a
//! rendered man page, a diff summary. **The module owns the text, the host owns the
//! pixels**: the block list carries structure (a heading's level, a table's alignments, a
//! mermaid source) and the host decides every glyph, because only the renderer measuring
//! proportional type knows where a line wraps.
//!
//! This is the store, not the projection. A doc replaces a doc — the contract has no damage
//! list, for the same reason a [frame](super::grid) does not: the two processes restart
//! independently and a whole document is the only statement true no matter what the other
//! side missed. What turns the stored [`Doc`] into the view's rows is
//! [`crate::viewpane::project_doc`], reached from `rows_for_pane` — the same split
//! [`super::rows`] keeps, where the store holds and the view projects.
//!
//! Thread-local, exactly like [`super::rows`] and [`super::grid`]: this is window-thread UI
//! state, and the fold-in, the projection and the pane's cache all already run there.

use avada_core::module::doc::Doc;
use avada_core::rights::ModuleId;
use std::cell::RefCell;
use std::collections::HashMap;

/// One surface's last document plus the counter the pane's cache watches. Same shape and
/// same reason as [`super::rows::Surface`]: a document is not a file, so there is no mtime
/// to compare — the revision is the only thing that says the content moved.
#[derive(Default)]
struct Surface {
    doc: Doc,
    revision: u64,
}

thread_local! {
    static SURFACES: RefCell<HashMap<String, Surface>> = RefCell::new(HashMap::new());
}

/// The store key, the same `<owner/repo>#<id>` shape the rail, the row store and the grid
/// store use, so the four stores are read the same way even though they never share an
/// entry for one surface.
fn key(module: &ModuleId, surface: &str) -> String {
    crate::leftpanel::entry_key(module, surface)
}

/// Replace `doc.surface`'s document. Documents replace documents — the contract has no
/// incremental update, because a module that restarted cannot know what the host still has
/// on screen and a whole document is the only message correct from both sides of a restart.
pub fn set(module: &ModuleId, doc: Doc) {
    SURFACES.with(|s| {
        let mut s = s.borrow_mut();
        let e = s.entry(key(module, &doc.surface)).or_default();
        e.doc = doc;
        e.revision += 1;
    });
}

/// What `surface` last typeset; `None` when it has never spoken. An empty document is a
/// real document (`Some` with no blocks), distinct from a surface that never sent one.
pub fn doc(module: &ModuleId, surface: &str) -> Option<Doc> {
    SURFACES.with(|s| s.borrow().get(&key(module, surface)).map(|e| e.doc.clone()))
}

/// How many documents `surface` has typeset. Zero for a surface that never has — the same
/// value every non-doc pane reports, so the projection cache treats "never spoke" and "not
/// a doc pane" alike.
pub fn generation(module: &ModuleId, surface: &str) -> u64 {
    SURFACES.with(|s| {
        s.borrow()
            .get(&key(module, surface))
            .map_or(0, |e| e.revision)
    })
}

/// Drop every document belonging to `module`. Called when the host says the module is gone:
/// a dead module's last word must not keep sitting on screen as if it were live.
///
/// Cleared, not removed, for the reason [`super::rows::forget`] and [`super::grid::forget`]
/// clear: the pane is still open and its projection only notices a *new* revision, so
/// removing the entry would let the cache hand back the document the module left behind.
pub fn forget(module: &ModuleId) {
    let prefix = format!("{}#", module.as_str());
    SURFACES.with(|s| {
        let mut s = s.borrow_mut();
        for k in s
            .keys()
            .filter(|k| k.starts_with(&prefix))
            .cloned()
            .collect::<Vec<_>>()
        {
            if let Some(e) = s.get_mut(&k) {
                e.doc = Doc::default();
                e.revision += 1;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use avada_core::module::doc::Block;

    fn id(s: &str) -> ModuleId {
        ModuleId::new(s).expect("a valid module id")
    }

    fn heading(text: &str) -> Block {
        Block::Heading {
            level: 1,
            text: text.into(),
        }
    }

    /// A doc surface is per-module and per-id, exactly like the row store: the store must
    /// not let one module's `preview` be another's, nor two surfaces of one module collide.
    #[test]
    fn a_surface_is_per_module_and_per_id() {
        let (a, b) = (id("bshuler/avada-markdown"), id("acme/avada-docs"));
        set(&a, Doc::new("preview", vec![heading("A")]));
        set(&b, Doc::new("preview", vec![heading("B1"), heading("B2")]));
        set(&a, Doc::new("other", vec![]));
        assert_eq!(doc(&a, "preview").unwrap().blocks.len(), 1);
        assert_eq!(doc(&b, "preview").unwrap().blocks.len(), 2);
        assert_eq!(
            doc(&a, "other").unwrap().blocks.len(),
            0,
            "an empty document is a document, not an absence"
        );
        assert!(
            doc(&a, "never-spoken").is_none(),
            "a surface that never spoke has no document at all"
        );
    }

    /// The revision is the cache key: a document that changed without it moving would leave
    /// the pane showing the previous one forever, and a gone module must empty its panes.
    #[test]
    fn every_replacement_moves_the_revision_and_a_gone_module_empties_its_panes() {
        let m = id("bshuler/avada-markdown");
        assert_eq!(
            generation(&m, "preview"),
            0,
            "an unspoken surface is at zero"
        );
        set(&m, Doc::new("preview", vec![heading("one")]));
        let first = generation(&m, "preview");
        assert!(first > 0);
        // Same document, said again: still a new revision. The store cannot tell an
        // idempotent resend from a real change without comparing, and redrawing is cheaper
        // than being wrong.
        set(&m, Doc::new("preview", vec![heading("one")]));
        assert!(generation(&m, "preview") > first);

        let before = generation(&m, "preview");
        forget(&m);
        assert!(
            doc(&m, "preview").unwrap().blocks.is_empty(),
            "a dead module keeps no blocks"
        );
        assert!(
            generation(&m, "preview") > before,
            "and the emptying is itself a change the projection must see"
        );
    }
}
