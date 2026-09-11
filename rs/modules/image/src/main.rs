//! `avada-image`: a tier-5 raster image viewer for Avada Terminal.
//!
//! This is a *panel type* that ships as a module. It claims the raster formats the shared
//! [`avada_image_decode`] crate can read; the host routes a matching file to its `image`
//! surface and cues it with a `doc.open` naming the surface and the path. The module then
//! reads the file through `host.fs.read`, decodes it with that same crate — the decode the
//! app's built-in image view runs, so the two cannot sniff or fail differently — and hands
//! the host the finished RGBA through `host.image.set`.
//!
//! An image is the deliberate exception to the rule that a module ships source and never
//! pixels: a picture has no source the host could re-render from. So **the module owns the
//! decode, the host owns the geometry** — the fit-never-upscale clamp, the zoom chord, the
//! caption and the checkerboard all stay in the host, drawn one step past where a document's
//! typesetting ends. A picture takes no keystrokes, so the surface serves only the required
//! set and answers no `module.image.*` — like a doc, the flow is one-directional.
//!
//! Every way the read or decode can fail — a path outside the workspace, a missing file, an
//! unsupported container, truncated bytes — is something the reader did, not a protocol
//! fault, so it rides [`SetImage::error`] as a caption the reader sees in the pane rather
//! than an error that tears the module down.
//!
//! `avada-image --manifest` prints the embedded `avada.toml` and exits.

/// The manifest this module presents in its hello (the repo's `avada.toml`).
pub const MANIFEST: &str = include_str!("../avada.toml");

fn main() {
    if std::env::args().any(|a| a == "--manifest") {
        print!("{MANIFEST}");
        return;
    }
    if let Err(e) = run() {
        eprintln!("avada-image: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(unix))]
fn run() -> Result<(), avada_module_sdk::client::ClientError> {
    eprintln!("avada-image: this module needs the Unix socket transport (AVADA_MODULE_FD)");
    Ok(())
}

#[cfg(unix)]
fn run() -> Result<(), avada_module_sdk::client::ClientError> {
    use avada_module_sdk::client::{self, ClientError, Connection};
    use avada_module_sdk::contract::{methods, Message};
    use avada_module_sdk::image::SetImage;
    use avada_module_sdk::Capability;
    use base64::Engine;
    use serde_json::json;
    use std::io::{Read, Write};
    use std::path::Path;

    /// The bare file name for the caption ("cat.png"), never the full path — the reader's
    /// pane titles itself with the crumb, and the caption only names the file.
    fn file_name(path: &str) -> String {
        Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(path)
            .to_string()
    }

    /// The file's raw bytes, or the notice to show in its place. `host.fs.read` answers
    /// `bytes_b64` for a non-UTF-8 file (which nearly every image is) and `text` for one
    /// that happens to be valid UTF-8 (a plausible BMP or a corrupt file); either way the
    /// bytes are what the decoder sniffs, so both answers are turned back into bytes here.
    fn read_bytes<R: Read, W: Write>(
        conn: &mut Connection<R, W>,
        path: &str,
    ) -> Result<Vec<u8>, String> {
        if !conn.has(Capability::FsRead) {
            return Err("Reading files was not permitted".into());
        }
        let value = match conn.call(methods::HOST_FS_READ, json!({ "path": path })) {
            Ok(v) => v,
            Err(ClientError::Rpc(e) | ClientError::Protocol(e)) => return Err(e.message),
            Err(e) => return Err(format!("Could not read {path}: {e}")),
        };
        if let Some(b64) = value["bytes_b64"].as_str() {
            return base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| format!("{}: {e}", file_name(path)));
        }
        if let Some(text) = value["text"].as_str() {
            return Ok(text.as_bytes().to_vec());
        }
        Err(format!("{} could not be read", file_name(path)))
    }

    /// Read `path`, decode it, and fill `surface` with the picture — or with the caption
    /// that says why there is none. Both outcomes are a [`SetImage`]: a decoded picture
    /// carries the RGBA and metadata, a failure carries `error` and no pixels, and the host
    /// tells them apart by which is set. The module base64-encodes the RGBA the [`decode`]
    /// crate handed back as a plain `Vec<u8>`; the host decodes it at the RPC boundary.
    ///
    /// [`decode`]: avada_image_decode::decode
    fn show<R: Read, W: Write>(
        conn: &mut Connection<R, W>,
        surface: &str,
        path: &str,
    ) -> Result<(), ClientError> {
        if !conn.has(Capability::UiPane) {
            return Ok(());
        }
        let name = file_name(path);
        let picture = match read_bytes(conn, path) {
            Ok(bytes) => match avada_image_decode::decode(&bytes) {
                Ok(d) => SetImage {
                    surface: surface.to_string(),
                    name,
                    width: d.width,
                    height: d.height,
                    format: d.format.to_string(),
                    bytes: d.bytes,
                    rgba_b64: base64::engine::general_purpose::STANDARD.encode(&d.rgba),
                    error: String::new(),
                },
                Err(why) => error_image(surface, &name, &why),
            },
            Err(why) => error_image(surface, &name, &why),
        };
        let params = serde_json::to_value(picture)
            .map_err(|e| ClientError::Handshake(e.to_string()))?;
        // A refused `host.image.set` means the pane is gone — news, not a failure. The
        // module keeps running and shows again the next time a file opens into it.
        match conn.call(methods::HOST_IMAGE_SET, params) {
            Ok(_) | Err(ClientError::Rpc(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// A [`SetImage`] that carries only a caption: no pixels, the reason in `error`. The
    /// host draws no picture and shows this text, exactly as the built-in view does for a
    /// file it could not decode.
    fn error_image(surface: &str, name: &str, why: &str) -> SetImage {
        SetImage {
            surface: surface.to_string(),
            name: name.to_string(),
            error: why.to_string(),
            ..Default::default()
        }
    }

    let manifest = avada_module_sdk::manifest::Manifest::parse(MANIFEST)
        .map_err(|e| ClientError::Handshake(e.to_string()))?;
    let mut conn = client::from_env()?;
    // A picture draws once and answers nothing: the surface serves the required set and none
    // of the grid optionals — a tier-5 surface that takes no keystroke is not a grid.
    let served: Vec<String> = methods::MODULE_REQUIRED_V1
        .iter()
        .map(|m| m.to_string())
        .collect();
    let _hello = conn.handshake(manifest, served)?;

    if conn.has(Capability::EventsSubscribe) {
        // The whole of the integration: the host says a file was opened into this surface,
        // and the module reads it, decodes it and ships the picture. Modules cannot call
        // each other, so the host's event bus is the only wire in.
        conn.call(
            methods::HOST_EVENTS_SUBSCRIBE,
            json!({ "kinds": [methods::events::DOC_OPEN] }),
        )?;
    }

    // ---- the loop ------------------------------------------------------------------

    while let Some(msg) = conn.recv()? {
        match msg {
            // Every required method is a lifecycle acknowledgement the host only needs to
            // see returned — a picture answers no row clicks and no keystrokes.
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
                show(&mut conn, surface, path)?;
            }

            _ => {}
        }
    }
    Ok(())
}
