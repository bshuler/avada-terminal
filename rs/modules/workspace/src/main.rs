//! `avada-workspace`: the Workspace rail entry for Avada Terminal.
//!
//! A tier-1 module: it speaks the module contract over the pipe named by `AVADA_MODULE_FD`,
//! reads the project's `.avada/project.json`, the saved-workspace library and the workspace
//! sets through `host.fs.list` and `host.fs.read`, hands the host a flat row list with
//! `host.rows.set`, writes a pane's note back with `host.fs.write`, and turns row gestures
//! into `host.panes.spawn`. It never touches `std::fs`: the host's workspace scoping is the
//! only thing keeping a read inside the project, and a module that reached around it would
//! be lying about what it can see.
//!
//! `avada-workspace --manifest` prints the embedded `avada.toml` and exits.

mod app;
mod model;
mod rows;
mod tree;

/// The manifest this module presents in its hello (the repo's `avada.toml`).
pub const MANIFEST: &str = include_str!("../avada.toml");

fn main() {
    if std::env::args().any(|a| a == "--manifest") {
        print!("{MANIFEST}");
        return;
    }
    if let Err(e) = run() {
        eprintln!("avada-workspace: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(unix))]
fn run() -> Result<(), avada_module_sdk::client::ClientError> {
    eprintln!("avada-workspace: this module needs the Unix pipe transport (AVADA_MODULE_FD)");
    Ok(())
}

/// The disk, as seen through the host.
///
/// It borrows the connection rather than owning it, because the module has exactly one
/// connection and the main loop needs it back the instant a sweep is done. The SDK makes
/// that safe: [`avada_module_sdk::client::Connection::call`] queues any request that
/// arrives while it is waiting, so sweeping a directory in the middle of answering
/// `module.row.activate` cannot lose the next click.
#[cfg(unix)]
struct HostFs<'a, R: std::io::Read, W: std::io::Write> {
    conn: &'a mut avada_module_sdk::client::Connection<R, W>,
    /// Set when the *pipe* failed rather than the directory. A failed listing is a row;
    /// a failed pipe is the end of the process, and the two must not look alike.
    broken: Option<avada_module_sdk::client::ClientError>,
}

#[cfg(unix)]
impl<R: std::io::Read, W: std::io::Write> HostFs<'_, R, W> {
    /// One host call, sorting a refusal (a row) from a dead pipe (the end).
    fn ask(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        use avada_module_sdk::client::ClientError;
        if self.broken.is_some() {
            return Err("disconnected".into());
        }
        match self.conn.call(method, params) {
            Ok(v) => Ok(v),
            // A denied capability or a path outside the workspace is a normal answer: the
            // human sees why that row says what it says and the module keeps running.
            Err(e @ (ClientError::Rpc(_) | ClientError::Protocol(_))) => Err(rpc_message(&e)),
            Err(e) => {
                let text = e.to_string();
                self.broken = Some(e);
                Err(text)
            }
        }
    }
}

#[cfg(unix)]
impl<R: std::io::Read, W: std::io::Write> tree::Fs for HostFs<'_, R, W> {
    fn list(&mut self, dir: &std::path::Path) -> Result<Vec<tree::Entry>, String> {
        use avada_module_sdk::contract::methods;
        let value = self.ask(
            methods::HOST_FS_LIST,
            serde_json::json!({ "path": dir.display().to_string() }),
        )?;
        let entries = value["entries"].as_array().cloned().unwrap_or_default();
        Ok(entries
            .iter()
            .filter_map(|e| {
                Some(tree::Entry {
                    name: e["name"].as_str()?.to_string(),
                    kind: e["kind"].as_str().unwrap_or("other").to_string(),
                })
            })
            .collect())
    }

    fn read(&mut self, path: &std::path::Path) -> Result<String, String> {
        use avada_module_sdk::contract::methods;
        let value = self.ask(
            methods::HOST_FS_READ,
            serde_json::json!({ "path": path.display().to_string() }),
        )?;
        value["text"]
            .as_str()
            .map(str::to_string)
            // A file the host answered for but could not give text for is binary or was
            // truncated; either way it is not a workspace file, and saying so is better
            // than handing the parser an empty string and reporting a syntax error.
            .ok_or_else(|| format!("{} is not text", tree::name_of(path)))
    }

    fn write(&mut self, path: &std::path::Path, text: &str) -> Result<(), String> {
        use avada_module_sdk::contract::methods;
        self.ask(
            methods::HOST_FS_WRITE,
            serde_json::json!({ "path": path.display().to_string(), "text": text }),
        )?;
        Ok(())
    }
}

/// The host's own words for a refused call, without the SDK's `rpc: ` prefix — the row is
/// 260 pixels wide and the prefix says nothing a human needs.
#[cfg(unix)]
fn rpc_message(e: &avada_module_sdk::client::ClientError) -> String {
    use avada_module_sdk::client::ClientError;
    match e {
        ClientError::Rpc(r) | ClientError::Protocol(r) => r.message.clone(),
        other => other.to_string(),
    }
}

#[cfg(unix)]
fn run() -> Result<(), avada_module_sdk::client::ClientError> {
    use avada_module_sdk::client::{self, ClientError, Connection};
    use avada_module_sdk::contract::{methods, ErrorCode, Message, RpcError};
    use avada_module_sdk::manifest::{Manifest, UiTier};
    use avada_module_sdk::rail::{Gesture, RailEntry, RegisterRail, SetRows};
    use avada_module_sdk::Capability;
    use serde_json::{json, Value};
    use std::io::{Read, Write};
    use std::path::PathBuf;

    use app::{App, Outcome, Spawn, COMMANDS};

    let manifest = Manifest::parse(MANIFEST).map_err(|e| ClientError::Handshake(e.to_string()))?;
    let mut conn = client::from_env()?;
    let served = methods::MODULE_REQUIRED_V1
        .iter()
        .map(|m| m.to_string())
        .collect();
    let hello = conn.handshake(manifest, served)?;

    let mut app = App::new();
    let root = hello
        .workspace
        .as_ref()
        .and_then(|w| w.root.as_ref())
        .map(PathBuf::from);

    fn push_rows<R: Read, W: Write>(
        conn: &mut Connection<R, W>,
        app: &App,
    ) -> Result<(), ClientError> {
        if !conn.has(Capability::UiRail) {
            return Ok(());
        }
        let payload = serde_json::to_value(SetRows {
            target: avada_module_sdk::rail::RowTarget::Rail,
            entry: rows::ENTRY.into(),
            rows: rows::rows(
                app.nodes(),
                &app.state.expanded,
                app.state.selected.as_deref(),
            ),
        })
        .map_err(|e| ClientError::Handshake(e.to_string()))?;
        conn.call(methods::HOST_ROWS_SET, payload)?;
        Ok(())
    }

    fn toast<R: Read, W: Write>(
        conn: &mut Connection<R, W>,
        text: &str,
        level: &str,
    ) -> Result<(), ClientError> {
        if conn.has(Capability::UiToast) {
            conn.call(methods::HOST_TOAST, json!({ "text": text, "level": level }))?;
        }
        Ok(())
    }

    /// Ask the host for every pane an outcome named, then say so if it could not.
    fn spawn_all<R: Read, W: Write>(
        conn: &mut Connection<R, W>,
        spawn: &[Spawn],
    ) -> Result<(), ClientError> {
        if spawn.is_empty() {
            return Ok(());
        }
        if !conn.has(Capability::PanesSpawn) {
            return toast(conn, "Opening panes was not permitted", "error");
        }
        for pane in spawn {
            let mut params = json!({ "kind": pane.kind });
            if let Some(path) = &pane.path {
                params["path"] = json!(path);
            }
            conn.call(methods::HOST_PANES_SPAWN, params)?;
        }
        Ok(())
    }

    if conn.has(Capability::UiRail) {
        let payload = serde_json::to_value(RegisterRail {
            entries: vec![RailEntry {
                id: rows::ENTRY.into(),
                label: "Workspace".into(),
                icon: None,
                tier: UiTier::Data,
                module: None,
                // Just behind Files: this is the entry the old built-in workspace panel
                // occupied, and it sat second in the rail.
                order: 20,
                component: None,
            }],
        })
        .map_err(|e| ClientError::Handshake(e.to_string()))?;
        conn.call(methods::HOST_RAIL_REGISTER, payload)?;
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
    if conn.has(Capability::EventsSubscribe) {
        // The filter box and every "reveal this path" in the app arrive as events. Without
        // them the entry still browses; it just cannot be driven from outside.
        conn.call(
            methods::HOST_EVENTS_SUBSCRIBE,
            json!({ "kinds": [methods::events::RAIL_QUERY, methods::events::FILES_REVEAL] }),
        )?;
    }

    // First paint, before any activation: the workspace from the hello is enough to draw.
    {
        let mut fs = HostFs {
            conn: &mut conn,
            broken: None,
        };
        app.set_workspace(&mut fs, root);
        if let Some(e) = fs.broken {
            return Err(e);
        }
    }
    push_rows(&mut conn, &app)?;

    while let Some(msg) = conn.recv()? {
        match msg {
            Message::Request(req) => {
                let mut fs = HostFs {
                    conn: &mut conn,
                    broken: None,
                };
                let outcome: Option<Outcome> = match req.method.as_str() {
                    methods::MODULE_ACTIVATE => {
                        let root = req.params["workspace"]["root"].as_str().map(PathBuf::from);
                        // A hello without a workspace and an activate without one mean the
                        // same thing; re-pointing at `None` is what empties the rail when
                        // the human closes the last window of a project.
                        app.set_workspace(&mut fs, root);
                        app.activate(&mut fs);
                        // The sweep just ran; without a repaint the rail would keep
                        // showing what was on screen before the entry was opened.
                        Some(Outcome::repainted())
                    }
                    methods::MODULE_DEACTIVATE => {
                        app.deactivate();
                        None
                    }
                    methods::MODULE_COMMAND_INVOKE => {
                        let id = req.params["id"].as_str().unwrap_or_default().to_string();
                        let args = req.params.get("args").cloned().unwrap_or(json!({}));
                        Some(app.command(&mut fs, &id, &args))
                    }
                    methods::MODULE_ROW_ACTIVATE => {
                        let row = req.params["row"].as_str().unwrap_or_default().to_string();
                        let data = req.params.get("data").cloned().unwrap_or(Value::Null);
                        // An unknown gesture from a newer host is read as a plain click:
                        // refusing the request would make a future host's new gesture look
                        // like a broken module.
                        let gesture = serde_json::from_value::<Gesture>(
                            req.params.get("gesture").cloned().unwrap_or(Value::Null),
                        )
                        .unwrap_or(Gesture::Open);
                        Some(app.row_activate(&mut fs, &row, &data, gesture))
                    }
                    _ => None,
                };
                if let Some(e) = fs.broken {
                    return Err(e);
                }
                let (resp, out) = match (req.method.as_str(), outcome) {
                    (methods::MODULE_DEACTIVATE, _) => (req.ok(json!({})), None),
                    (_, Some(out)) => (req.ok(out.result.clone()), Some(out)),
                    (_, None) => (
                        req.err(RpcError::new(
                            ErrorCode::MethodNotFound,
                            format!("workspace does not serve {}", req.method),
                        )),
                        None,
                    ),
                };
                // Answer first. Opening panes is a second, independent conversation, and
                // the host should not be left holding an unanswered request through it.
                conn.respond(resp)?;
                if let Some(out) = out {
                    if let Some((text, level)) = &out.toast {
                        toast(&mut conn, text, level)?;
                    }
                    spawn_all(&mut conn, &out.spawn)?;
                    if out.repaint {
                        push_rows(&mut conn, &app)?;
                    }
                }
            }
            Message::Notification(n) if n.method == methods::MODULE_SHUTDOWN => return Ok(()),
            Message::Notification(n) if n.method == methods::MODULE_EVENT => {
                let kind = n.params["kind"].as_str().unwrap_or_default().to_string();
                let payload = n.params.get("payload").cloned().unwrap_or(Value::Null);
                let mut fs = HostFs {
                    conn: &mut conn,
                    broken: None,
                };
                let out = app.event(&mut fs, &kind, &payload);
                if let Some(e) = fs.broken {
                    return Err(e);
                }
                if out.repaint {
                    push_rows(&mut conn, &app)?;
                }
            }
            // `module.prefs.changed`, and anything a newer host invents: nothing to do.
            _ => {}
        }
    }
    Ok(())
}
