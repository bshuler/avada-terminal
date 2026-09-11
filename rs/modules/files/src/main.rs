//! `avada-files`: the Files rail entry for Avada Terminal.
//!
//! A tier-1 module: it speaks the module contract over the socket in `AVADA_MODULE_FD`,
//! reads the workspace through `host.fs.list`, hands the host a flat row list with
//! `host.rows.set`, and turns row gestures into `host.panes.spawn`. It never touches
//! `std::fs` — the host's workspace scoping is what keeps a read inside the project.
//!
//! `avada-files --manifest` prints the embedded `avada.toml` and exits.

mod app;
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
        eprintln!("avada-files: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(unix))]
fn run() -> Result<(), avada_module_sdk::client::ClientError> {
    eprintln!("avada-files: this module needs the Unix socket transport (AVADA_MODULE_FD)");
    Ok(())
}

/// The disk, as seen through the host.
///
/// It borrows the connection rather than owning it, because the module has exactly one
/// connection and the main loop needs it back the instant a listing is done. The SDK makes
/// that safe: [`avada_module_sdk::client::Connection::call`] queues any request that
/// arrives while it is waiting, so listing a directory in the middle of answering
/// `module.row.activate` cannot lose the next click.
#[cfg(unix)]
struct HostFs<'a, R: std::io::Read, W: std::io::Write> {
    conn: &'a mut avada_module_sdk::client::Connection<R, W>,
    /// Set when the *socket* failed rather than the directory. A failed listing is a row;
    /// a failed socket is the end of the process, and the two must not look alike.
    broken: Option<avada_module_sdk::client::ClientError>,
}

#[cfg(unix)]
impl<R: std::io::Read, W: std::io::Write> tree::Fs for HostFs<'_, R, W> {
    fn list(&mut self, dir: &std::path::Path) -> Result<Vec<tree::Entry>, String> {
        use avada_module_sdk::client::ClientError;
        use avada_module_sdk::contract::methods;

        if self.broken.is_some() {
            return Err("disconnected".into());
        }
        let params = serde_json::json!({ "path": dir.display().to_string() });
        let value = match self.conn.call(methods::HOST_FS_LIST, params) {
            Ok(v) => v,
            // A denied capability or a path outside the workspace is a normal answer: the
            // human sees why that directory is empty and the module keeps running.
            Err(e @ (ClientError::Rpc(_) | ClientError::Protocol(_))) => {
                return Err(rpc_message(&e))
            }
            Err(e) => {
                let text = e.to_string();
                self.broken = Some(e);
                return Err(text);
            }
        };
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
    use avada_module_sdk::rail::{RailEntry, RegisterRail, SetRows};
    use avada_module_sdk::Capability;
    use serde_json::{json, Value};
    use std::io::{Read, Write};
    use std::path::PathBuf;

    use app::{App, Outcome, COMMANDS};

    let manifest = Manifest::parse(MANIFEST).map_err(|e| ClientError::Handshake(e.to_string()))?;
    let mut conn = client::from_env()?;
    let served = methods::MODULE_REQUIRED_V1
        .iter()
        .map(|m| m.to_string())
        .collect();
    let hello = conn.handshake(manifest, served)?;

    let mut app = App::new();
    app.set_workspace(
        hello
            .workspace
            .as_ref()
            .and_then(|w| w.root.as_ref())
            .map(PathBuf::from),
    );

    fn push_rows<R: Read, W: Write>(
        conn: &mut Connection<R, W>,
        app: &App,
    ) -> Result<(), ClientError> {
        if !conn.has(Capability::UiRail) {
            return Ok(());
        }
        conn.call(
            methods::HOST_ROWS_SET,
            serde_json::to_value(SetRows {
                target: avada_module_sdk::rail::RowTarget::Rail,
                entry: rows::ENTRY.into(),
                rows: rows::rows(&app.state),
            })
            .map_err(|e| ClientError::Handshake(e.to_string()))?,
        )?;
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

    if conn.has(Capability::UiRail) {
        conn.call(
            methods::HOST_RAIL_REGISTER,
            serde_json::to_value(RegisterRail {
                entries: vec![RailEntry {
                    id: rows::ENTRY.into(),
                    label: "Files".into(),
                    icon: None,
                    tier: UiTier::Data,
                    module: None,
                    // Ahead of the marketplace and of anything a user installs later: this
                    // is the entry the old built-in browser occupied, and muscle memory
                    // puts it at the top of the rail.
                    order: 10,
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
        app.activate(&mut fs);
    }
    push_rows(&mut conn, &app)?;

    while let Some(msg) = conn.recv()? {
        match msg {
            Message::Request(req) => {
                let mut fs = HostFs {
                    conn: &mut conn,
                    broken: None,
                };
                let outcome: Option<Result<Outcome, RpcError>> = match req.method.as_str() {
                    methods::MODULE_ACTIVATE => {
                        app.set_workspace(
                            req.params["workspace"]["root"].as_str().map(PathBuf::from),
                        );
                        app.activate(&mut fs);
                        Some(Ok(Outcome::default()))
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
                        let gesture = req.params["gesture"].as_str().unwrap_or("open").to_string();
                        let data = req.params.get("data").cloned().unwrap_or(Value::Null);
                        Some(app.row_activate(&mut fs, &data, &gesture))
                    }
                    _ => None,
                };
                if let Some(e) = fs.broken {
                    return Err(e);
                }
                let (resp, open) = match (req.method.as_str(), outcome) {
                    (methods::MODULE_DEACTIVATE, _) => (req.ok(json!({})), None),
                    (_, Some(Ok(out))) => {
                        if let Some(text) = &out.toast {
                            toast(&mut conn, text, "info")?;
                        }
                        (req.ok(out.result), out.open)
                    }
                    (_, Some(Err(e))) => {
                        toast(&mut conn, &e.message, "error")?;
                        (req.err(e), None)
                    }
                    (_, None) => (
                        req.err(RpcError::new(
                            ErrorCode::MethodNotFound,
                            format!("files does not serve {}", req.method),
                        )),
                        None,
                    ),
                };
                // Answer first. Opening a pane is a second, independent conversation, and
                // the host should not be left holding an unanswered request through it.
                conn.respond(resp)?;
                if let Some(path) = open {
                    if conn.has(Capability::PanesSpawn) {
                        conn.call(
                            methods::HOST_PANES_SPAWN,
                            json!({ "kind": "file", "path": path.display().to_string() }),
                        )?;
                    } else {
                        toast(&mut conn, "Opening panes was not permitted", "error")?;
                    }
                }
                push_rows(&mut conn, &app)?;
            }
            Message::Notification(n) if n.method == methods::MODULE_SHUTDOWN => return Ok(()),
            Message::Notification(n) if n.method == methods::MODULE_EVENT => {
                let kind = n.params["kind"].as_str().unwrap_or_default().to_string();
                let payload = n.params.get("payload").cloned().unwrap_or(Value::Null);
                let mut fs = HostFs {
                    conn: &mut conn,
                    broken: None,
                };
                app.event(&mut fs, &kind, &payload);
                if let Some(e) = fs.broken {
                    return Err(e);
                }
                push_rows(&mut conn, &app)?;
            }
            // `module.prefs.changed`, and anything a newer host invents: nothing to do.
            _ => {}
        }
    }
    Ok(())
}
