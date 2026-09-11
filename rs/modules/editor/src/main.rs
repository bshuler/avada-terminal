//! `avada-editor`: a tier-5 grid editor for Avada Terminal.
//!
//! The module owns the text; the host owns the pixels. Everything this process does is
//! one of four things: read a file through `host.fs.read`, write one through
//! `host.fs.write`, hand the host a full frame with `host.grid.set`, and answer the keys
//! the host has already resolved against the user's keymap.
//!
//! `avada-editor --manifest` prints the embedded `avada.toml` and exits.

mod buffer;
mod edit;
mod editor;
mod keymap;
mod render;

/// The manifest this module presents in its hello (the repo's `avada.toml`).
pub const MANIFEST: &str = include_str!("../avada.toml");

/// The rail entry, which is also the grid surface and the prefs page: one module, one
/// name, three surfaces that are all the same editor.
pub const ENTRY: &str = "editor";

/// The commands the palette offers, in manifest order.
pub const COMMANDS: &[(&str, &str)] = &[
    ("open", "Editor: Open a file"),
    ("save", "Editor: Save"),
    ("close", "Editor: Close the current buffer"),
    ("next", "Editor: Next buffer"),
    ("prev", "Editor: Previous buffer"),
];

fn main() {
    if std::env::args().any(|a| a == "--manifest") {
        print!("{MANIFEST}");
        return;
    }
    if let Err(e) = run() {
        eprintln!("avada-editor: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(unix))]
fn run() -> Result<(), avada_module_sdk::client::ClientError> {
    eprintln!("avada-editor: this module needs the Unix socket transport (AVADA_MODULE_FD)");
    Ok(())
}

#[cfg(unix)]
fn run() -> Result<(), avada_module_sdk::client::ClientError> {
    use avada_module_sdk::client::{self, ClientError, Connection};
    use avada_module_sdk::contract::{methods, ErrorCode, Message, RpcError};
    use avada_module_sdk::grid::{GridKey, GridResize};
    use avada_module_sdk::manifest::{Manifest, UiTier};
    use avada_module_sdk::rail::{RailEntry, RegisterRail, Row, RowTarget, SetRows};
    use avada_module_sdk::Capability;
    use editor::{Editor, Mode};
    use serde_json::{json, Value};
    use std::io::{Read, Write};
    use std::path::{Path, PathBuf};

    /// Everything the loop carries that is not the buffers themselves.
    struct State {
        ed: Editor,
        /// Whether a pane has been spawned for the `editor` surface. The host has no
        /// "is my pane open" call; a refused `host.grid.set` is the only report, so this
        /// is a belief that gets corrected rather than a fact.
        pane: bool,
        /// The mode a freshly opened buffer starts in, from the `start_mode` preference.
        start: Mode,
    }

    let manifest = Manifest::parse(MANIFEST).map_err(|e| ClientError::Handshake(e.to_string()))?;
    let mut conn = client::from_env()?;
    // The two optional grid methods are the whole of a tier-5 contribution's promise: a
    // module that draws a grid but will not take a keystroke is a picture, not a pane.
    let served: Vec<String> = methods::MODULE_REQUIRED_V1
        .iter()
        .chain(methods::MODULE_OPTIONAL.iter())
        .map(|m| m.to_string())
        .collect();
    let hello = conn.handshake(manifest, served)?;
    let workspace: Option<PathBuf> = hello
        .workspace
        .as_ref()
        .and_then(|w| w.root.as_ref())
        .map(PathBuf::from);

    let mut st = State {
        ed: Editor::default(),
        pane: false,
        start: Mode::Normal,
    };

    fn toast<R: Read, W: Write>(
        conn: &mut Connection<R, W>,
        text: &str,
        level: &str,
    ) -> Result<(), ClientError> {
        if conn.has(Capability::UiToast) && !text.is_empty() {
            conn.call(methods::HOST_TOAST, json!({ "text": text, "level": level }))?;
        }
        Ok(())
    }

    /// Hand the host a frame. A refused call means the pane is gone, which is news rather
    /// than a failure: the module keeps its buffers and repaints when one opens again.
    fn paint<R: Read, W: Write>(
        conn: &mut Connection<R, W>,
        st: &mut State,
    ) -> Result<(), ClientError> {
        if !st.pane || !conn.has(Capability::UiPane) {
            return Ok(());
        }
        let frame = serde_json::to_value(render::frame(&st.ed))
            .map_err(|e| ClientError::Handshake(e.to_string()))?;
        match conn.call(methods::HOST_GRID_SET, frame) {
            Ok(_) => Ok(()),
            Err(ClientError::Rpc(_)) => {
                st.pane = false;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// The buffer list, as rows under the rail entry.
    fn push_rows<R: Read, W: Write>(
        conn: &mut Connection<R, W>,
        st: &State,
    ) -> Result<(), ClientError> {
        if !conn.has(Capability::UiRail) {
            return Ok(());
        }
        let rows = st
            .ed
            .buffers
            .iter()
            .enumerate()
            .map(|(i, b)| Row {
                id: format!("buf-{i}"),
                label: b.name(),
                detail: b
                    .path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                depth: 0,
                expandable: false,
                expanded: false,
                icon: None,
                // `modified` is one of the marks the host already colours, so a dirty
                // buffer reads the same here as it does in the Git entry.
                marks: if b.dirty() {
                    vec!["modified".into()]
                } else {
                    Vec::new()
                },
                data: json!({ "index": i }),
            })
            .collect();
        conn.call(
            methods::HOST_ROWS_SET,
            serde_json::to_value(SetRows {
                entry: ENTRY.into(),
                target: RowTarget::Rail,
                rows,
            })
            .map_err(|e| ClientError::Handshake(e.to_string()))?,
        )?;
        Ok(())
    }

    /// Make sure there is a pane to paint into, spawning one if there is not.
    fn ensure_pane<R: Read, W: Write>(
        conn: &mut Connection<R, W>,
        st: &mut State,
    ) -> Result<(), ClientError> {
        if st.pane {
            return Ok(());
        }
        if !conn.has(Capability::PanesSpawn) {
            toast(conn, "Opening panes was not permitted", "error")?;
            return Ok(());
        }
        match conn.call(
            methods::HOST_PANES_SPAWN,
            json!({ "kind": "module", "surface": keymap::SURFACE }),
        ) {
            Ok(_) => st.pane = true,
            Err(ClientError::Rpc(e)) => toast(conn, &e.message, "error")?,
            Err(e) => return Err(e),
        }
        Ok(())
    }

    /// Read a file through the host and open it. Returns the human-facing complaint when
    /// it could not be opened, so the caller can decide between a toast and an RPC error.
    fn open_path<R: Read, W: Write>(
        conn: &mut Connection<R, W>,
        st: &mut State,
        path: &Path,
    ) -> Result<Result<(), String>, ClientError> {
        // A file that is already open is switched to, not re-read: the buffer may hold
        // unsaved work, and `Editor::open` would ignore the text anyway.
        if st.ed.paths().iter().any(|p| p.as_deref() == Some(path)) {
            st.ed.open(path, "");
            return Ok(Ok(()));
        }
        if !conn.has(Capability::FsRead) {
            return Ok(Err("Reading files was not permitted".into()));
        }
        let value = match conn.call(
            methods::HOST_FS_READ,
            json!({ "path": path.display().to_string() }),
        ) {
            Ok(v) => v,
            // A path outside the workspace, a missing file, a file over the host's read
            // cap: all of them are things the user did, not protocol failures.
            Err(ClientError::Rpc(e) | ClientError::Protocol(e)) => return Ok(Err(e.message)),
            Err(e) => return Err(e),
        };
        let Some(text) = value["text"].as_str() else {
            // The host answers `bytes_b64` for anything that is not UTF-8. An editor that
            // opened it would offer to write the bytes back mangled.
            return Ok(Err(format!("{} is not a text file", path.display())));
        };
        st.ed.open(path, text);
        st.ed.mode = st.start;
        Ok(Ok(()))
    }

    /// Write the current buffer back through the host.
    fn save<R: Read, W: Write>(
        conn: &mut Connection<R, W>,
        st: &mut State,
    ) -> Result<Result<String, String>, ClientError> {
        let Some(path) = st.ed.buf().path.clone() else {
            return Ok(Err("This buffer has no file to save to".into()));
        };
        if !conn.has(Capability::FsWrite) {
            return Ok(Err("Writing files was not permitted".into()));
        }
        match conn.call(
            methods::HOST_FS_WRITE,
            json!({ "path": path.display().to_string(), "text": st.ed.buf().to_text() }),
        ) {
            Ok(_) => {
                st.ed.buf_mut().mark_saved();
                let msg = format!("Saved {}", path.display());
                st.ed.message = Some("saved".into());
                Ok(Ok(msg))
            }
            Err(ClientError::Rpc(e) | ClientError::Protocol(e)) => {
                st.ed.message = Some(e.message.clone());
                Ok(Err(e.message))
            }
            Err(e) => Err(e),
        }
    }

    /// The `start_mode` preference, read out of a `host.prefs.get` or a
    /// `module.prefs.changed`. Anything else, including nothing, means Normal.
    fn start_mode(values: &Value) -> Mode {
        match values["start_mode"].as_str() {
            Some("insert") => Mode::Insert,
            _ => Mode::Normal,
        }
    }

    // ---- registration --------------------------------------------------------------

    if conn.has(Capability::UiRail) {
        conn.call(
            methods::HOST_RAIL_REGISTER,
            serde_json::to_value(RegisterRail {
                entries: vec![RailEntry {
                    id: ENTRY.into(),
                    label: "Editor".into(),
                    icon: None,
                    tier: UiTier::Data,
                    module: None,
                    // After Files (10) and Git (20): you find a file before you edit it.
                    order: 30,
                    component: None,
                }],
            })
            .map_err(|e| ClientError::Handshake(e.to_string()))?,
        )?;
    }
    if conn.has(Capability::UiCommands) {
        let commands: Vec<Value> = COMMANDS
            .iter()
            .map(|(id, label)| json!({ "id": id, "label": label }))
            .collect();
        conn.call(
            methods::HOST_COMMAND_REGISTER,
            json!({ "commands": commands }),
        )?;
    }
    if conn.has(Capability::UiPane) {
        // Declared before any pane exists, deliberately: the keybindings page should list
        // this editor's actions whether or not the user has one open right now.
        conn.call(
            methods::HOST_KEYMAP_DECLARE,
            serde_json::to_value(keymap::declare())
                .map_err(|e| ClientError::Handshake(e.to_string()))?,
        )?;
    }
    if conn.has(Capability::UiPrefs) {
        conn.call(
            methods::HOST_PREFS_DECLARE,
            json!({ "page": {
                "id": ENTRY,
                "title": "Editor",
                "fields": [{
                    "key": "start_mode",
                    "label": "Start in",
                    "kind": "choice",
                    "choices": ["normal", "insert"],
                    "default": "normal",
                    "help": "Set this to insert when you use the modeless keymap: it has no key that leaves normal mode.",
                }],
            }}),
        )?;
        if let Ok(v) = conn.call(methods::HOST_PREFS_GET, json!({})) {
            st.start = start_mode(&v["values"]);
            st.ed.mode = st.start;
        }
    }
    if conn.has(Capability::EventsSubscribe) {
        // How the Files entry opens a file here. Modules cannot call each other — a
        // module may only call `host.*` — so the host's event bus is the whole of the
        // integration, and it is the right shape: Files says what happened, not who
        // should react.
        conn.call(
            methods::HOST_EVENTS_SUBSCRIBE,
            json!({ "kinds": [methods::events::FILES_REVEAL] }),
        )?;
    }
    push_rows(&mut conn, &st)?;

    // ---- the loop ------------------------------------------------------------------

    while let Some(msg) = conn.recv()? {
        match msg {
            Message::Request(req) => {
                // What the request wants done after it has been answered, so that a
                // second conversation with the host never happens inside the first.
                let mut after: Option<String> = None;
                let resp = match req.method.as_str() {
                    methods::MODULE_ACTIVATE | methods::MODULE_DEACTIVATE => req.ok(json!({})),
                    methods::MODULE_ROUTE_INVOKE | methods::MODULE_EVENT => req.ok(json!({})),
                    methods::MODULE_PREFS_CHANGED => {
                        st.start = start_mode(&req.params["values"]);
                        req.ok(json!({}))
                    }
                    methods::MODULE_ROW_ACTIVATE => {
                        if let Some(i) = req.params["data"]["index"].as_u64() {
                            let i = i as usize;
                            if i < st.ed.buffers.len() {
                                st.ed.current = i;
                                st.ed.top = 0;
                                after = Some("show".into());
                            }
                        }
                        req.ok(json!({}))
                    }
                    methods::MODULE_COMMAND_INVOKE => {
                        let id = req.params["id"].as_str().unwrap_or_default();
                        match id {
                            "open" => match req.params["args"]["path"].as_str() {
                                Some(p) => {
                                    after = Some(format!("open:{p}"));
                                    req.ok(json!({}))
                                }
                                None => req.err(RpcError::new(
                                    ErrorCode::InvalidParams,
                                    "editor: open needs a path",
                                )),
                            },
                            "save" => {
                                after = Some("save".into());
                                req.ok(json!({}))
                            }
                            "close" => {
                                st.ed.close();
                                after = Some("repaint".into());
                                req.ok(json!({}))
                            }
                            "next" | "prev" => {
                                st.ed.cycle(id == "next");
                                after = Some("show".into());
                                req.ok(json!({}))
                            }
                            other => req.err(RpcError::new(
                                ErrorCode::MethodNotFound,
                                format!("editor has no command {other}"),
                            )),
                        }
                    }
                    other => req.err(RpcError::new(
                        ErrorCode::MethodNotFound,
                        format!("editor does not serve {other}"),
                    )),
                };
                conn.respond(resp)?;

                match after.as_deref() {
                    Some("show") => {
                        ensure_pane(&mut conn, &mut st)?;
                    }
                    Some("save") => {
                        let outcome = save(&mut conn, &mut st)?;
                        match outcome {
                            Ok(m) => toast(&mut conn, &m, "info")?,
                            Err(m) => toast(&mut conn, &m, "error")?,
                        }
                    }
                    Some(rest) if rest.starts_with("open:") => {
                        let path = PathBuf::from(&rest["open:".len()..]);
                        match open_path(&mut conn, &mut st, &path)? {
                            Ok(()) => ensure_pane(&mut conn, &mut st)?,
                            Err(m) => toast(&mut conn, &m, "error")?,
                        }
                    }
                    _ => {}
                }
                if after.is_some() {
                    paint(&mut conn, &mut st)?;
                    push_rows(&mut conn, &st)?;
                }
            }

            Message::Notification(n) if n.method == methods::MODULE_SHUTDOWN => return Ok(()),

            Message::Notification(n) if n.method == methods::MODULE_GRID_KEY => {
                let Ok(k) = serde_json::from_value::<GridKey>(n.params) else {
                    continue;
                };
                if k.surface != keymap::SURFACE {
                    continue;
                }
                // A key arriving is proof a pane exists, whatever this process believed.
                st.pane = true;
                if edit::key(&mut st.ed, &k) == edit::Effect::Save {
                    let outcome = save(&mut conn, &mut st)?;
                    if let Err(m) = outcome {
                        toast(&mut conn, &m, "error")?;
                    }
                }
                paint(&mut conn, &mut st)?;
                push_rows(&mut conn, &st)?;
            }

            Message::Notification(n) if n.method == methods::MODULE_GRID_RESIZE => {
                let Ok(r) = serde_json::from_value::<GridResize>(n.params) else {
                    continue;
                };
                if r.surface != keymap::SURFACE {
                    continue;
                }
                st.pane = true;
                st.ed.cols = r.cols;
                st.ed.rows = r.rows;
                st.ed.follow_cursor();
                paint(&mut conn, &mut st)?;
            }

            Message::Notification(n) if n.method == methods::MODULE_EVENT => {
                if n.params["kind"].as_str() != Some(methods::events::FILES_REVEAL) {
                    continue;
                }
                let payload = &n.params["payload"];
                let Some(path) = payload["path"].as_str().map(PathBuf::from) else {
                    continue;
                };
                // A reveal for a path the host said is outside the workspace would be
                // refused by `host.fs.read` anyway; there is nothing to check here.
                let _ = &workspace;
                match open_path(&mut conn, &mut st, &path)? {
                    Ok(()) => {
                        if let Some(line) = payload["line"].as_u64() {
                            let col = payload["col"].as_u64().unwrap_or(1);
                            // Both are 1-based on the wire, as they are everywhere a
                            // human types `file:line:col`.
                            st.ed
                                .buf_mut()
                                .goto((line.max(1) - 1) as usize, (col.max(1) - 1) as usize);
                            st.ed.follow_cursor();
                        }
                        ensure_pane(&mut conn, &mut st)?;
                        paint(&mut conn, &mut st)?;
                        push_rows(&mut conn, &st)?;
                    }
                    Err(m) => toast(&mut conn, &m, "error")?,
                }
            }

            Message::Notification(n) if n.method == methods::MODULE_PREFS_CHANGED => {
                st.start = start_mode(&n.params["values"]);
            }

            _ => {}
        }
    }
    Ok(())
}
