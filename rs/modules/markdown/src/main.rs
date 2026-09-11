//! `avada-markdown`: a tier-5 document preview for Avada Terminal.
//!
//! This is a *panel type* that ships as a module. It claims `.md` and its kin in its
//! manifest; the host routes a matching file to its `preview` surface and cues it with a
//! `doc.open` naming the surface and the path. The module then does the whole of what a
//! preview is: read the file through `host.fs.read`, parse it with the shared
//! [`avada_markdown_parse`] crate — the same parse the app's built-in preview runs, so the
//! two cannot drift — and hand the host a [`Doc`] through `host.doc.set`. The host typesets
//! it; the reader only reads. **The module owns the text; the host owns the pixels.**
//!
//! `avada-markdown --manifest` prints the embedded `avada.toml` and exits.

/// The manifest this module presents in its hello (the repo's `avada.toml`).
pub const MANIFEST: &str = include_str!("../avada.toml");

fn main() {
    if std::env::args().any(|a| a == "--manifest") {
        print!("{MANIFEST}");
        return;
    }
    if let Err(e) = run() {
        eprintln!("avada-markdown: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(unix))]
fn run() -> Result<(), avada_module_sdk::client::ClientError> {
    eprintln!("avada-markdown: this module needs the Unix socket transport (AVADA_MODULE_FD)");
    Ok(())
}

#[cfg(unix)]
fn run() -> Result<(), avada_module_sdk::client::ClientError> {
    use avada_markdown_parse::parse_markdown;
    use avada_module_sdk::client::{self, ClientError, Connection};
    use avada_module_sdk::contract::{methods, Message};
    use avada_module_sdk::doc::Doc;
    use avada_module_sdk::manifest::Manifest;
    use avada_module_sdk::Capability;
    use serde_json::json;
    use std::io::{Read, Write};

    let manifest = Manifest::parse(MANIFEST).map_err(|e| ClientError::Handshake(e.to_string()))?;
    let mut conn = client::from_env()?;
    // A preview draws a document and takes no keys back, so it serves the required set and
    // none of the grid optionals: a tier-5 surface that answers no keystroke is a doc, not
    // a grid.
    let served: Vec<String> = methods::MODULE_REQUIRED_V1
        .iter()
        .map(|m| m.to_string())
        .collect();
    let _hello = conn.handshake(manifest, served)?;

    /// Read a file through the host, parse it, and replace the document on `surface`.
    ///
    /// Every way this can fail is something the reader did — a path outside the workspace,
    /// a missing file, a binary file — not a protocol fault, so each becomes a one-block
    /// [`Doc`] the reader sees in the pane rather than an error that tears the module down.
    fn render<R: Read, W: Write>(
        conn: &mut Connection<R, W>,
        surface: &str,
        path: &str,
    ) -> Result<(), ClientError> {
        if !conn.has(Capability::UiPane) {
            return Ok(());
        }
        let blocks = match read_text(conn, path) {
            Ok(text) => parse_markdown(&text),
            Err(notice) => vec![avada_module_sdk::doc::Block::Notice { text: notice }],
        };
        let doc = Doc::new(surface, blocks);
        let params =
            serde_json::to_value(&doc).map_err(|e| ClientError::Handshake(e.to_string()))?;
        // A refused `host.doc.set` means the pane is gone — news, not a failure. The module
        // keeps running and renders again the next time a file is opened into it.
        match conn.call(methods::HOST_DOC_SET, params) {
            Ok(_) | Err(ClientError::Rpc(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// The file's text, or the notice to show in its place. A non-UTF-8 file has no preview
    /// to render, so it is named rather than parsed as mojibake.
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

    if conn.has(Capability::EventsSubscribe) {
        // The whole of the integration: the host says a file was opened into this surface,
        // and the module reads it and typesets it. Modules cannot call each other, so the
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
            // A doc surface answers no request of its own; every required method is a
            // lifecycle acknowledgement the host only needs to see returned.
            Message::Request(req) => {
                let resp = req.ok(json!({}));
                conn.respond(resp)?;
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
                render(&mut conn, surface, path)?;
            }

            _ => {}
        }
    }
    Ok(())
}
