//! The real `avada-image` binary against a fake host and a real directory of pictures.
//!
//! The host side is what `avada_core` does: a socketpair whose child end is handed to the
//! module in `AVADA_MODULE_FD`, `module.hello` / `host.hello` as the first two lines, then
//! JSON-RPC both ways. `host.fs.read` hits an actual temp directory, scoped to the
//! workspace root exactly as the host scopes it, and — the difference from the text
//! viewers — answers a real picture with `bytes_b64`, since a PNG is not UTF-8. Nothing
//! here touches the network or anything outside `std::env::temp_dir()`.
//!
//! The integration under test is the whole of what the built-in image view *was*, moved to
//! the far side of the wire. The host says a file was opened into the `image` surface
//! (`module.event` carrying a `doc.open`), and the module answers by reading the file
//! through `host.fs.read`, decoding it with the shared `avada-image-decode` crate — the
//! same decode the built-in view runs, so the two cannot sniff or fail differently — and
//! shipping the finished RGBA through `host.image.set`. A file it cannot decode rides back
//! as a caption in `SetImage::error`, never as a teardown; the last test pins the module's
//! `opens` list to exactly what that shared decoder accepts.
#![cfg(unix)]
#![allow(unsafe_code)] // one fcntl, to let the module inherit its socket

use std::io::{BufRead, BufReader, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use avada_module_sdk::contract::{methods, ModuleHello};
use avada_module_sdk::image::SetImage;
use avada_module_sdk::Capability;
use base64::Engine;
use serde_json::{json, Value};

const STEP: Duration = Duration::from_secs(15);

/// The module's manifest, parsed in-test so the `opens` assertion is against the real file
/// the binary embeds rather than a hand-copied list.
const MANIFEST: &str = include_str!("../avada.toml");

// ---------------------------------------------------------------- the workspace

struct TempDir(PathBuf);

impl TempDir {
    /// A workspace holding a real 3×2 PNG, a text file wearing a `.png` name, and a
    /// truncated PNG, canonicalised: on macOS `std::env::temp_dir()` is a symlink and the
    /// host answers with real paths, so the test must too or every path it sends would
    /// disagree with the root it scoped against.
    fn workspace(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "avada-image-e2e-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let png = png_bytes(3, 2);
        std::fs::write(dir.join("cat.png"), &png).unwrap();
        // A text file that happens to end in `.png`: the opener routed on the extension, so
        // the module still gets it, and the sniff — not the name — must reject it.
        std::fs::write(dir.join("liar.png"), b"i am not a picture\n").unwrap();
        // Half a real PNG: a genuine binary that is not a whole container.
        std::fs::write(dir.join("half.png"), &png[..png.len() / 2]).unwrap();
        TempDir(dir.canonicalize().unwrap())
    }

    fn join(&self, rel: &str) -> String {
        self.0.join(rel).display().to_string()
    }

    fn root(&self) -> String {
        self.0.display().to_string()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A `w × h` RGBA PNG built at test time — the same encoder `avada-image-decode`'s own
/// tests use, so the bytes on disk are a container the shared decoder genuinely reads.
fn png_bytes(w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut wr = enc.write_header().expect("png header");
        let px: Vec<u8> = (0..w * h)
            .flat_map(|i| [(i % 251) as u8, (i / 7 % 251) as u8, 64, 200])
            .collect();
        wr.write_image_data(&px).expect("png data");
    }
    out
}

/// Resolve a module-supplied path the way the host does: inside the workspace root or
/// refused.
fn scoped(root: &std::path::Path, path: &str) -> Result<PathBuf, Value> {
    let real = PathBuf::from(path)
        .canonicalize()
        .map_err(|e| json!({ "code": -32602, "message": format!("`{path}`: {e}") }))?;
    if !real.starts_with(root) {
        return Err(json!({
            "code": -32001,
            "message": format!("`{path}` is outside the workspace root"),
        }));
    }
    Ok(real)
}

/// `host.fs.read`: `text` when the file is UTF-8, `bytes_b64` (base64 STANDARD) when it is
/// not — which every real picture is. The module base64-decodes the second answer back to
/// the bytes it sniffs, so the fake host must encode it exactly as `avada_core` does.
fn fs_read(root: &std::path::Path, path: &str) -> Result<Value, Value> {
    let real = scoped(root, path)?;
    let bytes = std::fs::read(&real)
        .map_err(|e| json!({ "code": -32602, "message": format!("`{path}`: {e}") }))?;
    match String::from_utf8(bytes) {
        Ok(text) => Ok(json!({ "text": text })),
        Err(e) => Ok(json!({
            "bytes_b64": base64::engine::general_purpose::STANDARD.encode(e.as_bytes()),
        })),
    }
}

// ---------------------------------------------------------------- the fake host

/// The outcome of serving one line: a response to a call, a bare notification/request, or
/// the module closing the pipe (which a clean `module.shutdown` provokes).
enum Pumped {
    /// A line with no `method`: the module's response to one of *our* requests. We only
    /// count `host.*` messages, so nothing here reads the body.
    Response,
    Handled,
    Closed,
}

struct Host {
    child: Child,
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    /// Every `host.*` request or notification the module sent, in order.
    host_calls: Vec<(String, Value)>,
    ws: TempDir,
}

impl Host {
    fn spawn(tag: &str) -> Self {
        Self::spawn_with(tag, granted())
    }

    fn spawn_with(tag: &str, granted: Vec<Capability>) -> Self {
        let (host_end, child_end) = UnixStream::pair().unwrap();
        // The host clears CLOEXEC on the child end so the descriptor survives exec.
        let fd = child_end.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(flags >= 0);
        assert!(unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } >= 0);
        let ws = TempDir::workspace(tag);
        let child = Command::new(env!("CARGO_BIN_EXE_avada-image"))
            .env("AVADA_MODULE_FD", fd.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn module");
        drop(child_end);
        host_end.set_read_timeout(Some(STEP)).unwrap();
        let writer = host_end.try_clone().unwrap();
        let mut host = Host {
            child,
            reader: BufReader::new(host_end),
            writer,
            host_calls: vec![],
            ws,
        };
        let first = host.read_line().expect("module.hello, not EOF");
        let hello: ModuleHello = serde_json::from_str(&first).expect("module.hello");
        assert_eq!(hello.manifest.module.id.to_string(), "bshuler/avada-image");
        assert!(hello.contract_min <= 1 && 1 <= hello.contract_max);
        for m in methods::MODULE_REQUIRED_V1 {
            assert!(
                hello.methods.iter().any(|x| x == m),
                "module.hello does not list {m}"
            );
        }
        // An image draws once and answers nothing, so it serves none of the grid optionals.
        // If one ever appears here, the image surface has quietly grown a keymap and stopped
        // being the one-directional surface this test is written to guard.
        for m in methods::MODULE_OPTIONAL {
            assert!(
                !hello.methods.iter().any(|x| x == m),
                "an image pane must not serve the grid method {m}"
            );
        }
        let root = host.ws.root();
        host.write_line(&json!({
            "type": "host.hello",
            "contract_version": 1,
            "host_version": "0.0.0-test",
            "product": "Avada Terminal",
            "granted": granted,
            "methods": [
                methods::HOST_FS_READ, methods::HOST_EVENTS_SUBSCRIBE, methods::HOST_IMAGE_SET,
            ],
            "data_dir": root,
            "workspace": { "id": "ws1", "name": "Test workspace", "root": root },
        }));
        host
    }

    fn read_line(&mut self) -> Option<String> {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => None,
            Ok(_) => Some(line),
            Err(e) => panic!("no line from the module within {STEP:?}: {e}"),
        }
    }

    fn write_line(&mut self, v: &Value) {
        let mut s = v.to_string();
        s.push('\n');
        self.writer.write_all(s.as_bytes()).unwrap();
        self.writer.flush().unwrap();
    }

    fn pump(&mut self) -> Pumped {
        match self.read_line() {
            None => Pumped::Closed,
            Some(line) => self.handle(&line),
        }
    }

    /// Answer one module→host message. `host.fs.read` hits the real temp workspace;
    /// everything else (including `host.image.set`) gets `{}`. A line with no `method` is the
    /// module answering one of our requests and is neither answered nor counted.
    fn handle(&mut self, line: &str) -> Pumped {
        let v: Value = serde_json::from_str(line).expect("json line");
        let Some(method) = v.get("method").and_then(Value::as_str).map(str::to_string) else {
            return Pumped::Response;
        };
        let params = v.get("params").cloned().unwrap_or(Value::Null);
        if let Some(id) = v.get("id").cloned() {
            let result = match method.as_str() {
                methods::HOST_FS_READ => {
                    fs_read(&self.ws.0, params["path"].as_str().unwrap_or_default())
                }
                _ => Ok(json!({})),
            };
            let answer = match result {
                Ok(ok) => json!({ "jsonrpc": "2.0", "id": id, "result": ok }),
                Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": e }),
            };
            self.write_line(&answer);
        }
        self.host_calls.push((method, params));
        Pumped::Handled
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.write_line(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    /// The `doc.open` cue the opener emits after routing a file to this surface.
    fn open(&mut self, surface: &str, path: &str) {
        self.notify(
            methods::MODULE_EVENT,
            json!({ "kind": methods::events::DOC_OPEN, "payload": { "surface": surface, "path": path } }),
        );
    }

    /// Serve the module until it has sent `method` `n` times in total, returning the params
    /// of the nth.
    fn wait_for(&mut self, method: &str, n: usize) -> Value {
        let deadline = Instant::now() + STEP;
        while Instant::now() < deadline {
            let seen: Vec<&Value> = self
                .host_calls
                .iter()
                .filter(|(m, _)| m == method)
                .map(|(_, p)| p)
                .collect();
            if seen.len() >= n {
                return seen[n - 1].clone();
            }
            if let Pumped::Closed = self.pump() {
                panic!("module closed the pipe before sending {method} #{n}");
            }
        }
        panic!("module never sent {method} #{n}")
    }

    fn count(&self, method: &str) -> usize {
        self.host_calls.iter().filter(|(m, _)| m == method).count()
    }

    /// Serve the module until it closes the pipe, which a `module.shutdown` provokes. Proves
    /// a negative — that nothing was sent — without racing a timeout.
    fn run_to_shutdown(&mut self) {
        self.notify(methods::MODULE_SHUTDOWN, Value::Null);
        let deadline = Instant::now() + STEP;
        while Instant::now() < deadline {
            if let Pumped::Closed = self.pump() {
                return;
            }
        }
        panic!("module did not close the pipe after {:?}", methods::MODULE_SHUTDOWN);
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn granted() -> Vec<Capability> {
    vec![
        Capability::FsRead,
        Capability::UiPane,
        Capability::EventsSubscribe,
    ]
}

/// Read a `host.image.set` params back into the very type the module serialised, so the
/// assertions are against a `SetImage`, not against hand-spelled JSON that could drift.
fn set_image(params: &Value) -> SetImage {
    serde_json::from_value(params.clone()).expect("host.image.set params are a SetImage")
}

// ---------------------------------------------------------------- the tests

#[test]
fn it_subscribes_to_doc_open_when_it_may() {
    let mut host = Host::spawn("subscribe");
    let params = host.wait_for(methods::HOST_EVENTS_SUBSCRIBE, 1);
    let kinds = params["kinds"].as_array().expect("kinds is a list");
    assert!(
        kinds.iter().any(|k| k == methods::events::DOC_OPEN),
        "the image view subscribes to {}, got {params}",
        methods::events::DOC_OPEN
    );
}

#[test]
fn a_doc_open_reads_the_file_and_ships_the_pixels() {
    let mut host = Host::spawn("render");
    let path = host.ws.join("cat.png");
    host.open("image", &path);

    // The module reads through the host's cap, not its own file handle.
    let read = host.wait_for(methods::HOST_FS_READ, 1);
    assert_eq!(read["path"].as_str(), Some(path.as_str()));

    // Then it fills the surface it was cued with, with the picture and no error.
    let img = set_image(&host.wait_for(methods::HOST_IMAGE_SET, 1));
    assert_eq!(img.surface, "image");
    assert_eq!(img.name, "cat.png", "the caption names the file, not the path");
    assert_eq!((img.width, img.height), (3, 2), "the decoded dimensions");
    assert_eq!(img.format, "PNG", "the sniffed container, not the extension");
    assert!(img.error.is_empty(), "a good picture carries no error");

    // The RGBA the host will park as a texture: tightly packed, decodable, the right size.
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(img.bytes, bytes.len() as u64, "bytes is the encoded file size");
    let rgba = base64::engine::general_purpose::STANDARD
        .decode(&img.rgba_b64)
        .expect("rgba_b64 is valid base64");
    assert_eq!(
        rgba.len(),
        3 * 2 * 4,
        "width * height * 4 bytes of row-major RGBA"
    );
    // Anti-drift: the module's pixels *are* the shared decoder's pixels, byte for byte —
    // the same crate the app's built-in image view feeds its texture from.
    let decoded = avada_image_decode::decode(&bytes).expect("the shared decoder reads it");
    assert_eq!(rgba, decoded.rgba, "the module ships exactly what the crate decoded");
    assert_eq!((img.width, img.height), (decoded.width, decoded.height));
}

#[test]
fn a_text_file_wearing_a_png_name_is_a_caption_not_a_teardown() {
    let mut host = Host::spawn("liar");
    // The opener routed on the extension; the module got a UTF-8 file, so `host.fs.read`
    // answers with `text`. The sniff — not the name — must reject it.
    host.open("image", &host.ws.join("liar.png"));

    let img = set_image(&host.wait_for(methods::HOST_IMAGE_SET, 1));
    assert_eq!(img.surface, "image");
    assert!(!img.error.is_empty(), "an undecodable file explains itself");
    assert!(img.rgba_b64.is_empty(), "an error carries no pixels");
    assert_eq!((img.width, img.height), (0, 0), "and no dimensions");
    assert_eq!(img.name, "liar.png", "the caption still names the file");

    // And the module is still alive: a real picture opened next still ships.
    host.open("image", &host.ws.join("cat.png"));
    let good = set_image(&host.wait_for(methods::HOST_IMAGE_SET, 2));
    assert!(good.error.is_empty() && !good.rgba_b64.is_empty(), "it renders after the caption");
}

#[test]
fn a_truncated_picture_is_a_caption_not_a_panic() {
    let mut host = Host::spawn("half");
    // Half a real PNG is genuine binary, so `host.fs.read` answers with `bytes_b64`; the
    // decode fails on the truncation, and that failure must ride back as a caption.
    host.open("image", &host.ws.join("half.png"));
    let img = set_image(&host.wait_for(methods::HOST_IMAGE_SET, 1));
    assert!(!img.error.is_empty(), "a truncated container explains itself");
    assert!(img.rgba_b64.is_empty(), "and ships no pixels");
}

#[test]
fn a_missing_file_becomes_a_caption() {
    let mut host = Host::spawn("missing");
    host.open("image", &host.ws.join("nope.png"));
    let img = set_image(&host.wait_for(methods::HOST_IMAGE_SET, 1));
    assert_eq!(img.surface, "image");
    assert!(!img.error.is_empty(), "a read that failed names the failure");
    assert!(img.rgba_b64.is_empty(), "and there is no picture to ship");
}

#[test]
fn a_second_open_replaces_the_picture() {
    let mut host = Host::spawn("replace");
    // A caption first, then a real picture into the same surface: the second ship replaces
    // the first outright, the way `SetImage` replaces `SetImage`.
    host.open("image", &host.ws.join("liar.png"));
    let first = set_image(&host.wait_for(methods::HOST_IMAGE_SET, 1));
    assert!(!first.error.is_empty() && first.rgba_b64.is_empty());

    host.open("image", &host.ws.join("cat.png"));
    let second = set_image(&host.wait_for(methods::HOST_IMAGE_SET, 2));
    assert_eq!(second.surface, "image");
    assert!(second.error.is_empty() && !second.rgba_b64.is_empty(), "the picture replaces the caption");
}

#[test]
fn an_unknown_event_is_ignored() {
    let mut host = Host::spawn("unknown");
    // A kind the module does not handle must not make it read or ship anything.
    host.notify(
        methods::MODULE_EVENT,
        json!({ "kind": "something.else", "payload": { "surface": "image", "path": "x" } }),
    );
    host.run_to_shutdown();
    assert_eq!(host.count(methods::HOST_FS_READ), 0, "no file should have been read");
    assert_eq!(host.count(methods::HOST_IMAGE_SET), 0, "no picture should have been set");
}

#[test]
fn without_the_pane_cap_it_neither_reads_nor_ships() {
    // The host can grant events but withhold the pane: the module subscribes and hears the
    // open, but a picture with nowhere to draw reads nothing and ships nothing.
    let mut host = Host::spawn_with(
        "no-pane",
        vec![Capability::FsRead, Capability::EventsSubscribe],
    );
    host.wait_for(methods::HOST_EVENTS_SUBSCRIBE, 1);
    host.open("image", &host.ws.join("cat.png"));
    host.run_to_shutdown();
    assert_eq!(host.count(methods::HOST_FS_READ), 0, "no pane, so no read");
    assert_eq!(host.count(methods::HOST_IMAGE_SET), 0, "no pane, so no image.set");
}

#[test]
fn the_manifest_opens_exactly_what_the_shared_decoder_accepts() {
    // The anti-drift pin: the host routes a file here on the manifest's `opens` list, and
    // the module decodes it with `avada_image_decode`. If those two sets ever disagree, a
    // file would route to a pane that cannot read it (or a readable one would be left to the
    // text viewer). The e2e test is where the two crates meet, so it is where they are tied.
    let manifest = avada_module_sdk::manifest::Manifest::parse(MANIFEST).expect("manifest parses");
    let pane = manifest
        .contributions
        .iter()
        .find(|c| c.id == "image")
        .expect("an image contribution");
    assert!(!pane.opens.is_empty(), "the image pane claims some extensions");
    for ext in &pane.opens {
        assert!(
            avada_image_decode::is_image_ext(ext),
            "opens lists `{ext}`, which the shared decoder does not accept"
        );
    }
    // And every extension the decoder accepts is claimed, so nothing decodable is orphaned.
    for ext in ["png", "jpg", "jpeg", "gif", "webp", "bmp"] {
        assert!(
            pane.opens.iter().any(|o| o == ext),
            "the shared decoder accepts `{ext}` but the manifest does not open it"
        );
    }
}
