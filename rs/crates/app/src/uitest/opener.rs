//! Track 3b-3: a file opens into the module that claimed its type.
//!
//! Wave A step 3 makes a panel *type* a module — a markdown module claims `.md` and its
//! preview surface renders the file, in place of the built-in [`PaneKind::Markdown`]. The
//! host projects the installed manifests into a table of [`Opener`]s (proved in core's
//! `openers_project_every_pane_opens_claim_in_a_stable_order`); this file proves the app's
//! half: it *consumes* that table at the file-open choke point.
//!
//! Two claims a core unit test cannot make. First, [`State::opener_for_path`] reads the
//! table the way the two open sites read it — lowercase the extension, take the first
//! match, which is the tie the host already sorted for. Second, [`crate::command::dispatch`]
//! of [`Command::FilesOpen`] actually *routes*: a claimed type becomes a module pane and a
//! `doc.open` event is queued for the module, while an unclaimed type still falls to the
//! built-in viewer with nothing queued. `docpane` proves what happens once that `doc.open`
//! comes back as a `host.doc.set`; this proves the open that sends it.
#![allow(unused_imports)]
use super::*;

use avada_core::module::methods::events::DOC_OPEN;
use avada_core::module::Opener;
use avada_core::rights::ModuleId;
use avada_core::tools::kind::{ModulePaneRef, PaneKind};
use crate::command::{dispatch, Command};
use crate::state::State;

/// A fresh state and a manager that spawns nothing — a view pane has no session, so the
/// channel end is never touched. Shared by every case here.
fn fixtures() -> (State, avada_core::session_manager::SessionManager) {
    let state = State::new(crate::theme::load_font(1.0));
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    (state, avada_core::session_manager::SessionManager::new(tx))
}

/// One claim: module `id`'s pane `surface` opens `ext`. Built by hand rather than through
/// the host, because the projection is core's to test — the app's job is to read whatever
/// table it is handed.
fn opener(ext: &str, id: &str, surface: &str) -> Opener {
    Opener {
        ext: ext.into(),
        module: ModuleId::new(id).unwrap(),
        surface: surface.into(),
    }
}

/// The newest pane on the active tab — the one the open just added.
fn last_kind(state: &State) -> PaneKind {
    state.active_tab().panes.last().expect("a pane was opened").kind.clone()
}

/// The resolver is the whole routing decision in one place: lowercase the extension, and
/// when two modules claim it, the first in the host's sorted table wins. A path with no
/// extension, or one nobody claims, resolves to nothing so the caller keeps its fallback.
#[test]
fn opener_for_path_takes_the_first_claim_and_lowercases_the_extension() {
    let (mut state, _mgr) = fixtures();
    state.set_module_openers(vec![
        // Deliberately out of the host's order to prove the app does not re-sort: it trusts
        // the table and takes the first match. The host guarantees the order; here the first
        // `md` entry is `avada-markdown`, so it is the one that must win.
        opener("md", "bshuler/avada-markdown", "preview"),
        opener("md", "zed/notes", "notes"),
    ]);

    // A claimed extension resolves to the first claim's surface, case-insensitively.
    let hit = state
        .opener_for_path(std::path::Path::new("/proj/README.MD"))
        .expect("a claimed extension resolves");
    assert_eq!(hit.id.as_str(), "bshuler/avada-markdown");
    assert_eq!(hit.surface, "preview");

    // No extension, and an unclaimed one, both fall through to the caller's own default.
    assert!(state.opener_for_path(std::path::Path::new("/proj/Makefile")).is_none());
    assert!(state.opener_for_path(std::path::Path::new("/proj/data.csv")).is_none());
}

/// The open that routes. With a module claiming `md`, opening a `.md` file makes a module
/// pane on its surface — not the built-in `Markdown` — and queues exactly one `doc.open`
/// carrying the surface and the path, the module's cue to read the file and typeset it.
#[test]
fn files_open_routes_a_claimed_type_to_its_module_and_emits_doc_open() {
    let (mut state, mgr) = fixtures();
    state.set_module_openers(vec![opener("md", "bshuler/avada-markdown", "preview")]);

    dispatch(&mut state, Command::FilesOpen("/proj/notes.md".into()), &mgr);

    match last_kind(&state) {
        PaneKind::Module(m) => {
            assert_eq!(m.id.as_str(), "bshuler/avada-markdown");
            assert_eq!(m.surface, "preview");
        }
        other => panic!("a claimed .md must open the module pane, got {other:?}"),
    }

    let events = state.take_module_events();
    assert_eq!(events.len(), 1, "exactly one doc.open, not zero and not a storm");
    let (kind, payload) = &events[0];
    assert_eq!(kind, DOC_OPEN);
    assert_eq!(payload["surface"], "preview");
    assert_eq!(payload["path"], "/proj/notes.md");
}

/// The fallback, unchanged. With nothing claiming `md`, the same open still yields the
/// built-in renderer and queues no event — a module that was never installed cannot be
/// handed a document to render.
#[test]
fn files_open_falls_back_to_the_builtin_when_nothing_claims_the_type() {
    let (mut state, mgr) = fixtures();
    // A claim on a *different* extension must not leak onto `md`.
    state.set_module_openers(vec![opener("rs", "acme/rust", "code")]);

    dispatch(&mut state, Command::FilesOpen("/proj/notes.md".into()), &mgr);

    assert_eq!(
        last_kind(&state),
        PaneKind::Markdown,
        "an unclaimed .md keeps the built-in preview"
    );
    assert!(
        state.take_module_events().is_empty(),
        "the built-in viewer reads the file itself; there is no module to cue"
    );
}
