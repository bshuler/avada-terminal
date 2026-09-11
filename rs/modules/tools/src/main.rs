//! `avada-tools`: the AI-CLI rail entries for Avada Terminal.
//!
//! A tier-1 module: it speaks the module contract over the socket in `AVADA_MODULE_FD`,
//! reads the tool catalogue and each tool's conversation history through the host's
//! control server, hands the host a flat row list per entry with `host.rows.set`, and
//! turns a click on a conversation into a pane that resumes it.
//!
//! It never reads a transcript itself. Every fact on every row came from the host, which
//! is the half of this feature that has to stay host-side — see the module doc on
//! `avada-core`'s `tool_sessions` for why.
//!
//! `avada-tools --manifest` prints the embedded `avada.toml` and exits.

mod api;
mod app;
mod rows;

/// The manifest this module presents in its hello (the repo's `avada.toml`).
pub const MANIFEST: &str = include_str!("../avada.toml");

fn main() {
    if std::env::args().any(|a| a == "--manifest") {
        print!("{MANIFEST}");
        return;
    }
    if let Err(e) = run() {
        eprintln!("avada-tools: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(unix))]
fn run() -> Result<(), avada_module_sdk::client::ClientError> {
    eprintln!("avada-tools: this module needs the Unix socket transport (AVADA_MODULE_FD)");
    Ok(())
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

    use api::{Api, Control, Offline};
    use app::{App, Outcome, COMMANDS, ORDER_BASE};

    let manifest = Manifest::parse(MANIFEST).map_err(|e| ClientError::Handshake(e.to_string()))?;
    let mut conn = client::from_env()?;
    let served = methods::MODULE_REQUIRED_V1
        .iter()
        .map(|m| m.to_string())
        .collect();
    let hello = conn.handshake(manifest, served)?;

    // Everything this module knows arrives over the control server, so a host that offered
    // none is a module that can only explain itself. It still registers its entries and
    // still draws — a rail entry that said nothing at all would look like a crash.
    let mut host: Box<dyn Api> =
        match Control::new(hello.control_url.as_deref(), hello.token.as_deref()) {
            Some(c) => Box::new(c),
            None => Box::new(Offline),
        };

    let mut app = App::new();

    fn register_rail<R: Read, W: Write>(
        conn: &mut Connection<R, W>,
        app: &App,
    ) -> Result<(), ClientError> {
        if !conn.has(Capability::UiRail) {
            return Ok(());
        }
        let entries: Vec<RailEntry> = app
            .state
            .entries
            .iter()
            .enumerate()
            .map(|(i, id)| RailEntry {
                label: app
                    .state
                    .tool(id)
                    .map(|t| t.name.clone())
                    .unwrap_or_else(|| id.clone()),
                id: id.clone(),
                icon: None,
                tier: UiTier::Data,
                module: None,
                // After Files, which claims 10 because it is where the built-in browser
                // was, and in the human's own starred order within that.
                order: ORDER_BASE + i as i32,
                component: None,
            })
            .collect();
        // Sent even when empty: `host.rail.register` replaces this module's whole set, so
        // an empty list is how an entry the human just un-starred goes away.
        conn.call(
            methods::HOST_RAIL_REGISTER,
            serde_json::to_value(RegisterRail { entries })
                .map_err(|e| ClientError::Handshake(e.to_string()))?,
        )?;
        Ok(())
    }

    fn push_rows<R: Read, W: Write>(
        conn: &mut Connection<R, W>,
        app: &App,
    ) -> Result<(), ClientError> {
        if !conn.has(Capability::UiRail) {
            return Ok(());
        }
        for entry in &app.state.entries {
            conn.call(
                methods::HOST_ROWS_SET,
                serde_json::to_value(SetRows {
                    target: avada_module_sdk::rail::RowTarget::Rail,
                    entry: entry.clone(),
                    rows: rows::rows(&app.state, entry),
                })
                .map_err(|e| ClientError::Handshake(e.to_string()))?,
            )?;
        }
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
        // The filter box arrives as an event. Without it the entries still list; they just
        // cannot be searched from the panel.
        conn.call(
            methods::HOST_EVENTS_SUBSCRIBE,
            json!({ "kinds": [methods::events::RAIL_QUERY] }),
        )?;
    }

    // First paint, before any activation: the rail has to have its entries the moment the
    // panel is drawn, not the first time somebody clicks where one should have been.
    app.activate(&mut host);
    let mut registered = app.state.entries.clone();
    register_rail(&mut conn, &app)?;
    push_rows(&mut conn, &app)?;

    while let Some(msg) = conn.recv()? {
        match msg {
            Message::Request(req) => {
                let outcome: Option<Result<Outcome, RpcError>> = match req.method.as_str() {
                    methods::MODULE_ACTIVATE => {
                        app.activate(&mut host);
                        Some(Ok(Outcome::default()))
                    }
                    methods::MODULE_DEACTIVATE => {
                        app.deactivate();
                        None
                    }
                    methods::MODULE_COMMAND_INVOKE => {
                        let id = req.params["id"].as_str().unwrap_or_default().to_string();
                        let args = req.params.get("args").cloned().unwrap_or(json!({}));
                        Some(app.command(&mut host, &id, &args))
                    }
                    methods::MODULE_ROW_ACTIVATE => {
                        let gesture = req.params["gesture"].as_str().unwrap_or("open").to_string();
                        let data = req.params.get("data").cloned().unwrap_or(Value::Null);
                        Some(app.row_activate(&mut host, &data, &gesture))
                    }
                    _ => None,
                };
                let (resp, spawn) = match (req.method.as_str(), outcome) {
                    (methods::MODULE_DEACTIVATE, _) => (req.ok(json!({})), None),
                    (_, Some(Ok(out))) => {
                        if let Some(text) = &out.toast {
                            toast(&mut conn, text, "info")?;
                        }
                        (req.ok(out.result), out.spawn)
                    }
                    (_, Some(Err(e))) => {
                        toast(&mut conn, &e.message, "error")?;
                        (req.err(e), None)
                    }
                    (_, None) => (
                        req.err(RpcError::new(
                            ErrorCode::MethodNotFound,
                            format!("tools does not serve {}", req.method),
                        )),
                        None,
                    ),
                };
                // Answer first. Opening a pane is a second, independent conversation with
                // the host, and it should not be left holding an unanswered request
                // through a process launch.
                conn.respond(resp)?;
                if let Some(spec) = spawn {
                    if !conn.has(Capability::PanesSpawn) {
                        toast(&mut conn, "Opening panes was not permitted", "error")?;
                    } else if let Err(e) = host.spawn(&spec) {
                        toast(&mut conn, &format!("Could not resume: {e}"), "error")?;
                    }
                }
                if registered != app.state.entries {
                    registered = app.state.entries.clone();
                    register_rail(&mut conn, &app)?;
                }
                push_rows(&mut conn, &app)?;
            }
            Message::Notification(n) if n.method == methods::MODULE_SHUTDOWN => return Ok(()),
            Message::Notification(n) if n.method == methods::MODULE_EVENT => {
                let kind = n.params["kind"].as_str().unwrap_or_default().to_string();
                let payload = n.params.get("payload").cloned().unwrap_or(Value::Null);
                app.event(&mut host, &kind, &payload);
                push_rows(&mut conn, &app)?;
            }
            // `module.prefs.changed`, and anything a newer host invents: nothing to do.
            _ => {}
        }
    }
    Ok(())
}
