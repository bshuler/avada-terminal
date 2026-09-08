//! `hello`: the smallest module that speaks the whole contract.
//!
//! Run by the host with the socket in `AVADA_MODULE_FD`, it
//!
//! 1. handshakes ([`Connection::handshake`], which negotiates the contract version),
//! 2. registers one rail entry (`host.rail.register`) and a tier-1 row list
//!    (`host.rows.set`), plus one command when `ui.commands` was granted,
//! 3. answers `module.activate`, `module.deactivate`, `module.command.invoke` and
//!    `module.row.activate`,
//! 4. returns from `main` when `module.shutdown` arrives (or the pipe closes).
//!
//! `hello --manifest` prints the embedded `avada.toml` and exits, so an installer or a
//! test can record exactly the manifest the module will present.
//!
//! The `crash` command exits the process with status 3 without answering; the host's
//! integration test uses it to watch the supervisor restart the module.

/// The manifest this module presents in its hello.
///
/// Top-level keys come before the first table header (TOML rule).
pub const MANIFEST: &str = r#"capabilities = ["ui.rail", "ui.commands"]

[module]
id = "avada/hello"
name = "Hello"
version = "0.1.0"
description = "The smallest module: one rail entry, a few rows, one command"
publisher = "Avada"
contract = "^1"

[distribution]
kind = "source"

[[contributions]]
kind = "rail"
id = "hello"
tier = 1
label = "Hello"

[[contributions]]
kind = "command"
id = "greet"
tier = 1
label = "Say hello"
"#;

fn main() {
    if std::env::args().any(|a| a == "--manifest") {
        print!("{MANIFEST}");
        return;
    }
    if let Err(e) = run() {
        eprintln!("hello: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(unix))]
fn run() -> Result<(), avada_module_sdk::client::ClientError> {
    eprintln!("hello: this example needs the Unix socket transport (AVADA_MODULE_FD)");
    Ok(())
}

#[cfg(unix)]
fn run() -> Result<(), avada_module_sdk::client::ClientError> {
    use avada_module_sdk::client::{self, ClientError};
    use avada_module_sdk::contract::methods;
    use avada_module_sdk::contract::{ErrorCode, Message, RpcError};
    use avada_module_sdk::manifest::{Manifest, UiTier};
    use avada_module_sdk::rail::{RailEntry, RegisterRail, Row, SetRows};
    use avada_module_sdk::Capability;
    use serde_json::json;

    let manifest = Manifest::parse(MANIFEST).map_err(|e| ClientError::Handshake(e.to_string()))?;
    let mut conn = client::from_env()?;
    let served = methods::MODULE_REQUIRED_V1
        .iter()
        .map(|m| m.to_string())
        .collect();
    let hello = conn.handshake(manifest, served)?;
    let workspace = hello
        .workspace
        .as_ref()
        .map(|w| w.name.clone())
        .unwrap_or_else(|| "no workspace".to_string());

    if conn.has(Capability::UiRail) {
        conn.call(
            methods::HOST_RAIL_REGISTER,
            serde_json::to_value(RegisterRail {
                entries: vec![RailEntry {
                    id: "hello".into(),
                    label: "Hello".into(),
                    icon: None,
                    tier: UiTier::Data,
                    module: None,
                    order: 0,
                    component: None,
                }],
            })
            .map_err(|e| ClientError::Handshake(e.to_string()))?,
        )?;
        let row = |id: &str, label: &str, detail: String| Row {
            id: id.into(),
            label: label.into(),
            detail,
            depth: 0,
            expandable: false,
            expanded: false,
            icon: None,
            marks: vec![],
            data: json!({ "id": id }),
        };
        conn.call(
            methods::HOST_ROWS_SET,
            serde_json::to_value(SetRows {
                entry: "hello".into(),
                rows: vec![
                    row("greeting", "Hello, world", String::new()),
                    row(
                        "host",
                        "Host",
                        format!("{} {}", hello.product, hello.host_version),
                    ),
                    row("workspace", "Workspace", workspace),
                ],
            })
            .map_err(|e| ClientError::Handshake(e.to_string()))?,
        )?;
    }

    if conn.has(Capability::UiCommands) {
        conn.call(
            methods::HOST_COMMAND_REGISTER,
            json!({ "commands": [{ "id": "greet", "label": "Say hello" }] }),
        )?;
    }

    let mut greetings: u64 = 0;
    while let Some(msg) = conn.recv()? {
        match msg {
            Message::Request(req) => {
                let resp = match req.method.as_str() {
                    methods::MODULE_ACTIVATE => {
                        let ws = req.params["workspace"]["id"].clone();
                        req.ok(json!({ "activated": true, "workspace": ws }))
                    }
                    methods::MODULE_DEACTIVATE => req.ok(json!({})),
                    methods::MODULE_COMMAND_INVOKE => {
                        match req.params["id"].as_str() {
                            Some("greet") => {
                                greetings += 1;
                                let who = req.params["args"]["name"]
                                    .as_str()
                                    .unwrap_or("world")
                                    .to_string();
                                req.ok(json!({ "greeting": format!("Hello, {who}"), "count": greetings }))
                            }
                            // Test hook: die without answering so the host sees a crash.
                            Some("crash") => std::process::exit(3),
                            other => req.err(RpcError::new(
                                ErrorCode::InvalidParams,
                                format!("unknown command {other:?}"),
                            )),
                        }
                    }
                    methods::MODULE_ROW_ACTIVATE => req.ok(json!({ "row": req.params["row"] })),
                    _ => req.err(RpcError::new(
                        ErrorCode::MethodNotFound,
                        format!("hello does not serve {}", req.method),
                    )),
                };
                conn.respond(resp)?;
            }
            Message::Notification(n) if n.method == methods::MODULE_SHUTDOWN => return Ok(()),
            // `module.event`, `module.prefs.changed`: nothing to do.
            _ => {}
        }
    }
    Ok(())
}
