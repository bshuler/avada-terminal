//! `avada-operating-model`: a module with no behaviour of its own.
//!
//! Everything it ships is files: an always-on rule and two role playbooks under `skills/`,
//! which the host materializes into agent instruction files. The binary exists because
//! the module contract wants a running process; it handshakes, answers the lifecycle
//! calls, and returns when the host says `module.shutdown`.
//!
//! `avada-operating-model --manifest` prints the embedded `avada.toml` and exits.

/// The manifest this module presents in its hello (the repo's `avada.toml`).
pub const MANIFEST: &str = include_str!("../avada.toml");

fn main() {
    if std::env::args().any(|a| a == "--manifest") {
        print!("{MANIFEST}");
        return;
    }
    if let Err(e) = run() {
        eprintln!("avada-operating-model: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(unix))]
fn run() -> Result<(), avada_module_sdk::client::ClientError> {
    eprintln!("avada-operating-model: this module needs the Unix socket transport (AVADA_MODULE_FD)");
    Ok(())
}

#[cfg(unix)]
fn run() -> Result<(), avada_module_sdk::client::ClientError> {
    use avada_module_sdk::client::{self, ClientError};
    use avada_module_sdk::contract::methods;
    use avada_module_sdk::contract::{ErrorCode, Message, RpcError};
    use avada_module_sdk::manifest::Manifest;
    use serde_json::json;

    let manifest = Manifest::parse(MANIFEST).map_err(|e| ClientError::Handshake(e.to_string()))?;
    let mut conn = client::from_env()?;
    let served = methods::MODULE_REQUIRED_V1
        .iter()
        .map(|m| m.to_string())
        .collect();
    conn.handshake(manifest, served)?;

    while let Some(msg) = conn.recv()? {
        match msg {
            Message::Request(req) => {
                let resp = match req.method.as_str() {
                    methods::MODULE_ACTIVATE => req.ok(json!({ "activated": true })),
                    methods::MODULE_DEACTIVATE => req.ok(json!({})),
                    _ => req.err(RpcError::new(
                        ErrorCode::MethodNotFound,
                        format!("avada-operating-model does not serve {}", req.method),
                    )),
                };
                conn.respond(resp)?;
            }
            Message::Notification(n) if n.method == methods::MODULE_SHUTDOWN => return Ok(()),
            _ => {}
        }
    }
    Ok(())
}
