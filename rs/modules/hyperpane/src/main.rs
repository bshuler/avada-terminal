//! `avada-hyperpane`: the always-on agent tab for Avada Terminal.
//!
//! A shell-tier module. It speaks the module contract over the socket in `AVADA_MODULE_FD`,
//! registers a rail entry and four commands, opens one terminal pane in its own data
//! directory with `host.panes.spawn`, and then drives that pane with `host.panes.input`.
//! It touches no filesystem capability at all: the only directory it cares about is the
//! private data dir the host names in the hello, and nothing in the contract guards that.
//!
//! What makes the tab worth installing is not the pane but `skills/hyperpane/SKILL.md`,
//! which the host materialises into the agent's rules because this manifest declares
//! `[skills] paths` and the `skills.materialize` capability. There is no runtime method
//! for that — materialisation happens around the module, not through it.
//!
//! `avada-hyperpane --manifest` prints the embedded `avada.toml` and exits.

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
        eprintln!("avada-hyperpane: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(unix))]
fn run() -> Result<(), avada_module_sdk::client::ClientError> {
    eprintln!("avada-hyperpane: this module needs the Unix socket transport (AVADA_MODULE_FD)");
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
    use std::path::PathBuf;

    use app::{Action, App, Outcome, COMMANDS};

    let manifest = Manifest::parse(MANIFEST).map_err(|e| ClientError::Handshake(e.to_string()))?;
    let mut conn = client::from_env()?;
    let served = methods::MODULE_REQUIRED_V1
        .iter()
        .map(|m| m.to_string())
        .collect();
    let hello = conn.handshake(manifest, served)?;

    let mut app = App::new();
    // The data dir, not the workspace. The agent tab is *not* per-project: it is the one
    // session that follows the human between workspaces, so its shell starts in the
    // module's own durable directory and stays there when the workspace changes.
    app.set_dir(PathBuf::from(&hello.data_dir));

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

    /// Carry out the one side effect an outcome asked for. Split out so the request arm
    /// and the event arm cannot disagree about which capability guards which call.
    fn perform<R: Read, W: Write>(
        conn: &mut Connection<R, W>,
        app: &mut App,
        action: Action,
    ) -> Result<(), ClientError> {
        match action {
            // `Restart` never reaches here: `App::open` turns it into a forget plus an
            // `Open`, so the spawn path has exactly one shape.
            Action::Open | Action::Restart => {
                if !conn.has(Capability::PanesSpawn) {
                    toast(conn, "Opening panes was not permitted", "error")?;
                    return Ok(());
                }
                let dir = app.state.dir.display().to_string();
                let v = conn.call(
                    methods::HOST_PANES_SPAWN,
                    json!({ "kind": "shell", "path": dir }),
                )?;
                match v["pane_id"].as_str() {
                    Some(id) => app.pane_opened(id.to_string()),
                    // A host that spawned without naming the pane leaves us unable to type
                    // into it. Say so rather than remembering a pane id we do not have.
                    None => toast(conn, "The host opened a pane it did not name", "error")?,
                }
            }
            Action::Reveal => {
                if !conn.has(Capability::PanesSpawn) {
                    toast(conn, "Opening panes was not permitted", "error")?;
                    return Ok(());
                }
                let dir = app.state.dir.display().to_string();
                conn.call(
                    methods::HOST_PANES_SPAWN,
                    json!({ "kind": "file", "path": dir }),
                )?;
            }
            Action::Input(text) => {
                if !conn.has(Capability::PanesInput) {
                    toast(conn, "Typing into panes was not permitted", "error")?;
                    return Ok(());
                }
                let Some(pane_id) = app.state.pane.clone() else {
                    return Ok(());
                };
                conn.call(
                    methods::HOST_PANES_INPUT,
                    json!({ "pane_id": pane_id, "text": text }),
                )?;
            }
        }
        Ok(())
    }

    if conn.has(Capability::UiRail) {
        conn.call(
            methods::HOST_RAIL_REGISTER,
            serde_json::to_value(RegisterRail {
                entries: vec![RailEntry {
                    id: rows::ENTRY.into(),
                    label: "Hyperpane".into(),
                    icon: None,
                    tier: UiTier::Data,
                    module: None,
                    // Below Files (10): the agent tab is reached from the keyboard far more
                    // often than from the rail, and its rail slot is a status light more
                    // than a destination.
                    order: 40,
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
        conn.call(
            methods::HOST_EVENTS_SUBSCRIBE,
            json!({ "kinds": [methods::events::RAIL_QUERY] }),
        )?;
    }

    // First paint before any activation: the hello alone says everything the rows show.
    push_rows(&mut conn, &app)?;

    while let Some(msg) = conn.recv()? {
        match msg {
            Message::Request(req) => {
                let outcome: Option<Result<Outcome, RpcError>> = match req.method.as_str() {
                    methods::MODULE_ACTIVATE => Some(Ok(Outcome::default())),
                    methods::MODULE_DEACTIVATE => {
                        app.deactivate();
                        None
                    }
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
                let (resp, action) = match (req.method.as_str(), outcome) {
                    (methods::MODULE_DEACTIVATE, _) => (req.ok(json!({})), None),
                    (_, Some(Ok(out))) => {
                        if let Some(text) = &out.toast {
                            toast(&mut conn, text, "info")?;
                        }
                        (req.ok(out.result), out.action)
                    }
                    (_, Some(Err(e))) => {
                        toast(&mut conn, &e.message, "error")?;
                        (req.err(e), None)
                    }
                    (_, None) => (
                        req.err(RpcError::new(
                            ErrorCode::MethodNotFound,
                            format!("hyperpane does not serve {}", req.method),
                        )),
                        None,
                    ),
                };
                // Answer first. Spawning a pane and typing into it are further
                // conversations, and the host must not be left holding this request
                // through either of them.
                conn.respond(resp)?;
                if let Some(action) = action {
                    perform(&mut conn, &mut app, action)?;
                }
                push_rows(&mut conn, &app)?;
            }
            Message::Notification(n) if n.method == methods::MODULE_SHUTDOWN => return Ok(()),
            Message::Notification(n) if n.method == methods::MODULE_EVENT => {
                let kind = n.params["kind"].as_str().unwrap_or_default().to_string();
                let payload = n.params.get("payload").cloned().unwrap_or(Value::Null);
                app.event(&kind, &payload);
                push_rows(&mut conn, &app)?;
            }
            // `module.prefs.changed`, and anything a newer host invents: nothing to do.
            _ => {}
        }
    }
    Ok(())
}
