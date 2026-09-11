//! `avada-datatree`: a tier-5 interactive data viewer for Avada Terminal.
//!
//! This is a *panel type* that ships as a module. It claims `.json`, `.jsonl` and `.ndjson`
//! in its manifest; the host routes a matching file to its `datatree` surface and cues it
//! with a `doc.open` naming the surface and the path. The module then does the whole of
//! what the built-in JSON view did: read the file through `host.fs.read`, flatten it with
//! the shared [`avada_json_parse`] crate — the same flatten the app's built-in view runs,
//! so the two cannot drift — and fill the pane with a tree of foldable rows through
//! `host.rows.set { target: "pane" }`.
//!
//! Unlike the inert table and markdown viewers, a tree *folds*, and the fold is the
//! module's own. The host echoes a row click back as `module.row.activate`; on a `toggle`
//! the module flips that node in the surface's fold set and re-ships the rows. The host
//! measures nothing and remembers nothing about the tree's shape — **the module owns the
//! fold, the host owns the pixels.**
//!
//! `avada-datatree --manifest` prints the embedded `avada.toml` and exits.

/// The manifest this module presents in its hello (the repo's `avada.toml`).
pub const MANIFEST: &str = include_str!("../avada.toml");

fn main() {
    if std::env::args().any(|a| a == "--manifest") {
        print!("{MANIFEST}");
        return;
    }
    if let Err(e) = run() {
        eprintln!("avada-datatree: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(unix))]
fn run() -> Result<(), avada_module_sdk::client::ClientError> {
    eprintln!("avada-datatree: this module needs the Unix socket transport (AVADA_MODULE_FD)");
    Ok(())
}

#[cfg(unix)]
fn run() -> Result<(), avada_module_sdk::client::ClientError> {
    use avada_json_parse::{flatten, LineKind};
    use avada_module_sdk::client::{self, ClientError, Connection};
    use avada_module_sdk::contract::{methods, Message};
    use avada_module_sdk::rail::{Gesture, Row, RowActivate, RowTarget, SetRows};
    use avada_module_sdk::Capability;
    use serde_json::json;
    use std::collections::{BTreeSet, HashMap};
    use std::io::{Read, Write};
    use std::path::Path;

    /// One open surface: the file it shows, whether it reads as JSONL, and the set of nodes
    /// whose disclosure differs from the depth default. The fold set is XOR against that
    /// default (see [`avada_json_parse::flatten`]), so it holds only the nodes the reader
    /// actually touched — a handful, even in a file with tens of thousands of containers.
    struct Open {
        path: String,
        jsonl: bool,
        flipped: BTreeSet<String>,
    }

    /// Whether a path reads one document per line rather than one document for the file.
    /// The same rule the built-in view uses, keyed on the extension the opener matched.
    fn is_jsonl(path: &str) -> bool {
        matches!(
            Path::new(path)
                .extension()
                .and_then(|e| e.to_str())
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Some("jsonl" | "ndjson")
        )
    }

    /// The file's text, or the notice to show in its place. A non-UTF-8 file has no tree to
    /// render, so it is named rather than parsed as mojibake.
    fn read_text<R: Read, W: Write>(
        conn: &mut Connection<R, W>,
        path: &str,
    ) -> Result<String, String> {
        if !conn.has(Capability::FsRead) {
            return Err("Reading files was not permitted".into());
        }
        let value = match conn.call(methods::HOST_FS_READ, json!({ "path": path })) {
            Ok(v) => v,
            Err(ClientError::Rpc(e) | ClientError::Protocol(e)) => return Err(e.message),
            Err(e) => return Err(format!("Could not read {path}: {e}")),
        };
        match value["text"].as_str() {
            Some(text) => Ok(text.to_string()),
            None => Err(format!("{path} is not a text file")),
        }
    }

    /// Flatten `open`'s file at its current fold and fill `surface` with the rows.
    ///
    /// Every way the *read* or *parse* can fail is something the reader did — a path outside
    /// the workspace, a missing file, a binary file, malformed JSON — not a protocol fault,
    /// so each becomes a single un-foldable row the reader sees in the pane rather than an
    /// error that tears the module down. A file too large to flatten whole is not an error
    /// either: [`flatten`] caps its lines and reports how many it cut, and the module names
    /// the remainder in a trailing row.
    fn render<R: Read, W: Write>(
        conn: &mut Connection<R, W>,
        surface: &str,
        open: &Open,
    ) -> Result<(), ClientError> {
        if !conn.has(Capability::UiPane) {
            return Ok(());
        }
        let rows = match read_text(conn, &open.path) {
            Ok(text) => match flatten(&text, open.jsonl, &open.flipped) {
                Ok(flat) => {
                    let mut rows: Vec<Row> = flat.lines.iter().map(row_for).collect();
                    if flat.cut > 0 {
                        rows.push(notice_row(&format!("… {} more rows not shown", flat.cut)));
                    }
                    rows
                }
                Err(notice) => vec![notice_row(&notice)],
            },
            Err(notice) => vec![notice_row(&notice)],
        };
        let params = serde_json::to_value(SetRows {
            entry: surface.to_string(),
            target: RowTarget::Pane,
            rows,
        })
        .map_err(|e| ClientError::Handshake(e.to_string()))?;
        // A refused `host.rows.set` means the pane is gone — news, not a failure. The module
        // keeps running and renders again the next time a file is opened into it.
        match conn.call(methods::HOST_ROWS_SET, params) {
            Ok(_) | Err(ClientError::Rpc(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// One flattened line as a rail row. A container is foldable and carries its child
    /// count as detail; a scalar is a leaf carrying its value. The row's `id` is the line's
    /// stable path — the exact string a `toggle` names back — so the fold survives re-flattens.
    fn row_for(line: &avada_json_parse::TreeLine) -> Row {
        let depth = u8::try_from(line.depth).unwrap_or(u8::MAX);
        match &line.kind {
            LineKind::Container { open, kids } => Row {
                id: line.path.clone(),
                label: line.key.clone(),
                detail: kids.clone(),
                depth,
                expandable: true,
                expanded: *open,
                icon: None,
                marks: Vec::new(),
                data: serde_json::Value::Null,
            },
            LineKind::Scalar { value, .. } => Row {
                id: line.path.clone(),
                label: line.key.clone(),
                detail: value.clone(),
                depth,
                expandable: false,
                expanded: false,
                icon: None,
                marks: Vec::new(),
                data: serde_json::Value::Null,
            },
        }
    }

    /// A single un-foldable row carrying a message — the read/parse failure, or the "N more
    /// rows" tail. A pane's rows are its only surface, so a notice is a row, not a block.
    fn notice_row(text: &str) -> Row {
        Row {
            id: String::new(),
            label: text.to_string(),
            detail: String::new(),
            depth: 0,
            expandable: false,
            expanded: false,
            icon: None,
            marks: Vec::new(),
            data: serde_json::Value::Null,
        }
    }

    let manifest = avada_module_sdk::manifest::Manifest::parse(MANIFEST)
        .map_err(|e| ClientError::Handshake(e.to_string()))?;
    let mut conn = client::from_env()?;
    // A data viewer draws rows and answers clicks; it serves the required set and none of
    // the grid optionals: a tier-5 surface that takes no keystroke is a rows pane, not a grid.
    let served: Vec<String> = methods::MODULE_REQUIRED_V1
        .iter()
        .map(|m| m.to_string())
        .collect();
    let _hello = conn.handshake(manifest, served)?;

    // Per-surface state, keyed by the bare surface id the host uses on the wire in both
    // directions (`host.rows.set { entry }` out, `module.row.activate { entry }` in). The
    // loop is single-threaded, so a plain map needs no lock.
    let mut open: HashMap<String, Open> = HashMap::new();

    if conn.has(Capability::EventsSubscribe) {
        // The whole of the integration: the host says a file was opened into this surface,
        // and the module reads it and draws it. Modules cannot call each other, so the
        // host's event bus is the only wire in — and the right shape, since the opener says
        // what happened, not who should react.
        conn.call(
            methods::HOST_EVENTS_SUBSCRIBE,
            json!({ "kinds": [methods::events::DOC_OPEN] }),
        )?;
    }

    // ---- the loop ------------------------------------------------------------------

    while let Some(msg) = conn.recv()? {
        match msg {
            // A row click comes back as a request the host waits on. `toggle` flips the
            // node's disclosure and re-draws; every other gesture (a leaf's `open`, a
            // right-click `context`) has nothing to fold, so it is acknowledged and dropped
            // — the same rule as the built-in view, where only a container folds.
            Message::Request(req) if req.method == methods::MODULE_ROW_ACTIVATE => {
                if let Ok(act) = serde_json::from_value::<RowActivate>(req.params.clone()) {
                    if act.target == RowTarget::Pane && act.gesture == Gesture::Toggle {
                        if let Some(state) = open.get_mut(&act.entry) {
                            // XOR the node: a toggle of an open node closes it and a toggle
                            // of a closed one opens it, matching the fold set's own XOR rule.
                            if !state.flipped.remove(&act.row) {
                                state.flipped.insert(act.row.clone());
                            }
                            // Re-borrow immutably for the render.
                            let state = &open[&act.entry];
                            render(&mut conn, &act.entry, state)?;
                        }
                    }
                }
                conn.respond(req.ok(json!({})))?;
            }

            // Every other required method is a lifecycle acknowledgement the host only needs
            // to see returned.
            Message::Request(req) => {
                conn.respond(req.ok(json!({})))?;
            }

            Message::Notification(n) if n.method == methods::MODULE_SHUTDOWN => return Ok(()),

            Message::Notification(n) if n.method == methods::MODULE_EVENT => {
                if n.params["kind"].as_str() != Some(methods::events::DOC_OPEN) {
                    continue;
                }
                let payload = &n.params["payload"];
                let (Some(surface), Some(path)) =
                    (payload["surface"].as_str(), payload["path"].as_str())
                else {
                    continue;
                };
                // A fresh open resets the fold: a new file in the pane starts at the depth
                // default, not at the disclosure the last file happened to leave behind.
                let state = Open {
                    path: path.to_string(),
                    jsonl: is_jsonl(path),
                    flipped: BTreeSet::new(),
                };
                render(&mut conn, surface, &state)?;
                open.insert(surface.to_string(), state);
            }

            _ => {}
        }
    }
    Ok(())
}
