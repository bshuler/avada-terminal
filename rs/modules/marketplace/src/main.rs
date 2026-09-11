//! `avada-marketplace`: the Marketplace rail entry for Avada Terminal.
//!
//! A tier-1 module: it speaks the module contract over the socket in
//! `AVADA_MODULE_FD`, draws its rows from the host's `/marketplace/...` control
//! routes (reached over HTTP with the per-run token from `host.hello`) and turns
//! commands and row activations into those routes. It never talks to GitHub itself.
//!
//! `avada-marketplace --manifest` prints the embedded `avada.toml` and exits.

mod app;
mod control;
mod model;
mod rows;

/// The manifest this module presents in its hello (the repo's `avada.toml`).
pub const MANIFEST: &str = include_str!("../avada.toml");

fn main() {
    if std::env::args().any(|a| a == "--manifest") {
        print!("{MANIFEST}");
        return;
    }
    if let Err(e) = run() {
        eprintln!("avada-marketplace: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(unix))]
fn run() -> Result<(), avada_module_sdk::client::ClientError> {
    eprintln!("avada-marketplace: this module needs the Unix socket transport (AVADA_MODULE_FD)");
    Ok(())
}

#[cfg(unix)]
fn run() -> Result<(), avada_module_sdk::client::ClientError> {
    use avada_module_sdk::client::{self, ClientError, Connection};
    use avada_module_sdk::contract::methods;
    use avada_module_sdk::contract::{ErrorCode, Message, RpcError};
    use avada_module_sdk::manifest::{Manifest, UiTier};
    use avada_module_sdk::rail::{RailEntry, RegisterRail, RowTarget, SetRows};
    use avada_module_sdk::Capability;
    use serde_json::{json, Value};
    use std::io::{Read, Write};

    use app::{App, Outcome, COMMANDS, PANE};
    use control::HttpControl;

    let manifest = Manifest::parse(MANIFEST).map_err(|e| ClientError::Handshake(e.to_string()))?;
    let mut conn = client::from_env()?;
    let served = methods::MODULE_REQUIRED_V1
        .iter()
        .map(|m| m.to_string())
        .collect();
    let hello = conn.handshake(manifest, served)?;

    // The token is only ever handed to the HTTP client; it is never logged.
    let control = match (&hello.control_url, &hello.token) {
        // Installs run as background jobs, so no route blocks long; 30 s is generous.
        (Some(url), Some(token)) => HttpControl::new(url, token)
            .ok()
            .map(|c| c.with_timeout(std::time::Duration::from_secs(30))),
        _ => None,
    };
    let control_available = control.is_some();
    let control = control.unwrap_or_else(|| HttpControl::new("http://127.0.0.1:1", "").unwrap());
    let mut app = App::new(
        control,
        conn.has(Capability::MarketplaceManage),
        control_available,
    );
    app.set_workspace(hello.workspace.as_ref().map(|w| w.id.clone()));

    fn set_rows<R: Read, W: Write>(
        conn: &mut Connection<R, W>,
        target: RowTarget,
        rows: Vec<avada_module_sdk::rail::Row>,
    ) -> Result<(), ClientError> {
        conn.call(
            methods::HOST_ROWS_SET,
            serde_json::to_value(SetRows {
                entry: rows::ENTRY.into(),
                target,
                rows,
            })
            .map_err(|e| ClientError::Handshake(e.to_string()))?,
        )?;
        Ok(())
    }

    /// Repaint every surface this module currently owns.
    ///
    /// The pane is only painted once the host has actually spawned it: `host.rows.set`
    /// rejects rows for a surface that was never opened, so pushing them earlier would
    /// turn every repaint into an error.
    fn push_rows<R: Read, W: Write>(
        conn: &mut Connection<R, W>,
        app: &App<HttpControl>,
    ) -> Result<(), ClientError> {
        if conn.has(Capability::UiRail) {
            set_rows(conn, RowTarget::Rail, rows::rows(&app.state))?;
        }
        if app.state.pane_open && conn.has(Capability::UiPane) {
            set_rows(conn, RowTarget::Pane, rows::pane_rows(&app.state))?;
        }
        Ok(())
    }

    /// Ask the host to open this module's pane. Returns whether it is now open.
    /// Ask the host to open this module's pane. Returns whether it is now open.
    ///
    /// Two capabilities gate a pane, not one: `panes.spawn` opens it and `ui.pane`
    /// is what `host.rows.set` checks before it will paint into it. A user who
    /// granted only one of them gets a refusal from the host, and a refusal is a
    /// normal answer here — the module says so and carries on rather than dying on
    /// a permission it was never promised.
    fn spawn_pane<R: Read, W: Write>(conn: &mut Connection<R, W>) -> Result<bool, ClientError> {
        if !conn.has(Capability::UiPane) || !conn.has(Capability::PanesSpawn) {
            return Ok(false);
        }
        match conn.call(
            methods::HOST_PANES_SPAWN,
            json!({ "kind": "module", "surface": PANE }),
        ) {
            Ok(_) => Ok(true),
            Err(ClientError::Rpc(e)) => {
                toast(
                    conn,
                    &format!("the pane could not open: {}", e.message),
                    "warn",
                )?;
                Ok(false)
            }
            Err(e) => Err(e),
        }
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
                    label: "Marketplace".into(),
                    icon: None,
                    tier: UiTier::Data,
                    module: None,
                    order: 90,
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

    // First paint: whatever the routes say right now; a failure shows as the notice row.
    let _ = app.refresh();
    push_rows(&mut conn, &app)?;

    while let Some(msg) = conn.recv()? {
        match msg {
            Message::Request(req) => {
                let outcome: Option<Result<Outcome, RpcError>> = match req.method.as_str() {
                    methods::MODULE_ACTIVATE => {
                        let ws = req.params["workspace"]["id"].as_str().map(str::to_string);
                        app.set_workspace(ws);
                        Some(app.refresh().map(|_| Outcome::default()))
                    }
                    methods::MODULE_DEACTIVATE => None,
                    methods::MODULE_COMMAND_INVOKE => {
                        let id = req.params["id"].as_str().unwrap_or_default().to_string();
                        let args = req.params.get("args").cloned().unwrap_or(json!({}));
                        Some(app.command(&id, &args))
                    }
                    methods::MODULE_ROW_ACTIVATE => {
                        let gesture = req.params["gesture"].as_str().unwrap_or("open").to_string();
                        let data = req.params.get("data").cloned().unwrap_or(Value::Null);
                        Some(app.row_activate(&data, &gesture))
                    }
                    _ => None,
                };
                let resp = match (req.method.as_str(), outcome) {
                    (methods::MODULE_DEACTIVATE, _) => req.ok(json!({})),
                    (_, Some(Ok(out))) => {
                        if out.spawn_pane && !spawn_pane(&mut conn)? {
                            // The pane can never appear, so drop the state flag
                            // again rather than repainting a ghost.
                            app.state.pane_open = false;
                        }
                        if let Some(text) = &out.toast {
                            toast(&mut conn, text, "info")?;
                        }
                        req.ok(out.result)
                    }
                    (_, Some(Err(e))) => {
                        toast(&mut conn, &e.message, "error")?;
                        req.err(e)
                    }
                    (_, None) => req.err(RpcError::new(
                        ErrorCode::MethodNotFound,
                        format!("marketplace does not serve {}", req.method),
                    )),
                };
                conn.respond(resp)?;
                push_rows(&mut conn, &app)?;
            }
            Message::Notification(n) if n.method == methods::MODULE_SHUTDOWN => return Ok(()),
            // `module.event`, `module.prefs.changed`: nothing to do.
            _ => {}
        }
    }
    Ok(())
}
