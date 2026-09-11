//! The real `avada-markdown` binary against a fake host and a real directory tree.
//!
//! The host side is what `avada_core` does: a socketpair whose child end is handed to the
//! module in `AVADA_MODULE_FD`, `module.hello` / `host.hello` as the first two lines, then
//! JSON-RPC both ways. `host.fs.read` hits an actual temp directory, scoped to the
//! workspace root exactly as the host scopes it, so a file this test says was opened is a
//! file on disk. Nothing here touches the network or anything outside `std::env::temp_dir()`.
//!
//! The integration under test is the whole of what a preview *is*: the host says a file was
//! opened into the `preview` surface (`module.event` carrying a `doc.open`), and the module
//! answers by reading the file through `host.fs.read` and handing back a whole [`Doc`]
//! through `host.doc.set`. The reader never types into it, so — unlike the editor — this
//! surface serves no grid methods, and the test asserts their absence.
#![cfg(unix)]
#![allow(unsafe_code)] // one fcntl, to let the module inherit its socket

use std::io::{BufRead, BufReader, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use avada_markdown_parse::parse_markdown;
use avada_module_sdk::contract::{methods, ModuleHello};
use avada_module_sdk::doc::{Block, Doc};
use avada_module_sdk::Capability;
use serde_json::{json, Value};

const STEP: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------- the workspace

struct TempDir(PathBuf);

impl TempDir {
    /// A workspace with a markdown file, canonicalised: on macOS `std::env::temp_dir()` is a
    /// symlink and the host answers with real paths, so the test must too or every path it
    /// sends would disagree with the root it scoped against.
    fn workspace(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "avada-markdown-e2e-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("README.md"),
            "# Title\n\nA paragraph with *emphasis*.\n\n- one\n- two\n\n```rust\nfn main() {}\n```\n",
        )
        .unwrap();
        std::fs::write(dir.join("logo.png"), [0x89u8, b'P', b'N', b'G', 0x00, 0xff]).unwrap();
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

/// Resolve a module-supplied path the way the host does: inside the workspace root or
/// refused.
fn scoped(root: &Path, path: &str) -> Result<PathBuf, Value> {
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

/// `host.fs.read`: text when the file is UTF-8, `bytes_b64` when it is not. The module
/// refuses a preview of the second answer, so the fake host must be able to give it.
fn fs_read(root: &Path, path: &str) -> Result<Value, Value> {
    let real = scoped(root, path)?;
    let bytes = std::fs::read(&real)
        .map_err(|e| json!({ "code": -32602, "message": format!("`{path}`: {e}") }))?;
    match String::from_utf8(bytes) {
        Ok(text) => Ok(json!({ "text": text })),
        Err(_) => Ok(json!({ "bytes_b64": "" })),
    }
}

// ---------------------------------------------------------------- the fake host

/// The outcome of serving one line: a response to a call, a bare notification, or the
/// module closing the pipe (which a clean `module.shutdown` provokes).
enum Pumped {
    /// A response to one of our calls (its id already matched by the caller loop). We only
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
        let child = Command::new(env!("CARGO_BIN_EXE_avada-markdown"))
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
        assert_eq!(hello.manifest.module.id.to_string(), "bshuler/avada-markdown");
        assert!(hello.contract_min <= 1 && 1 <= hello.contract_max);
        for m in methods::MODULE_REQUIRED_V1 {
            assert!(
                hello.methods.iter().any(|x| x == m),
                "module.hello does not list {m}"
            );
        }
        // A doc surface takes no keystroke back, so it serves none of the grid optionals.
        // If one ever appears here, the preview has quietly grown a keymap and stopped being
        // the one-directional document this test is written to guard.
        for m in methods::MODULE_OPTIONAL {
            assert!(
                !hello.methods.iter().any(|x| x == m),
                "a doc surface must not serve the grid method {m}"
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
                methods::HOST_FS_READ, methods::HOST_EVENTS_SUBSCRIBE, methods::HOST_DOC_SET,
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
    /// everything else gets `{}`.
    fn handle(&mut self, line: &str) -> Pumped {
        let v: Value = serde_json::from_str(line).expect("json line");
        let Some(method) = v.get("method").and_then(Value::as_str).map(str::to_string) else {
            let _ = v;
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

/// Read the `host.doc.set` params back into the very type the module serialised, so the
/// assertions are against a `Doc`, not against hand-spelled JSON that could drift from it.
fn doc(params: &Value) -> Doc {
    serde_json::from_value(params.clone()).expect("host.doc.set params are a Doc")
}

// ---------------------------------------------------------------- the tests

#[test]
fn it_subscribes_to_doc_open_when_it_may() {
    let mut host = Host::spawn("subscribe");
    let params = host.wait_for(methods::HOST_EVENTS_SUBSCRIBE, 1);
    let kinds = params["kinds"].as_array().expect("kinds is a list");
    assert!(
        kinds.iter().any(|k| k == methods::events::DOC_OPEN),
        "the preview subscribes to {}, got {params}",
        methods::events::DOC_OPEN
    );
}

#[test]
fn a_doc_open_reads_the_file_and_typesets_it() {
    let mut host = Host::spawn("render");
    let path = host.ws.join("README.md");
    host.open("preview", &path);

    // The module reads through the host's cap, not its own file handle.
    let read = host.wait_for(methods::HOST_FS_READ, 1);
    assert_eq!(read["path"].as_str(), Some(path.as_str()));

    // Then it hands back a whole document for the surface it was cued with.
    let set = host.wait_for(methods::HOST_DOC_SET, 1);
    let got = doc(&set);
    assert_eq!(got.surface, "preview");

    // Block-for-block the same parse the app's built-in preview runs: one crate, two
    // front-ends, and this is the assertion that keeps them from drifting.
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(got.blocks, parse_markdown(&text));
    // And the file genuinely exercised the parser rather than collapsing to one block.
    assert!(
        matches!(got.blocks.first(), Some(Block::Heading { level: 1, .. })),
        "expected a top-level heading first, got {:?}",
        got.blocks.first()
    );
}

#[test]
fn a_second_open_replaces_the_document() {
    let mut host = Host::spawn("replace");
    std::fs::write(host.ws.0.join("other.md"), "## Second\n").unwrap();

    host.open("preview", &host.ws.join("README.md"));
    let first = doc(&host.wait_for(methods::HOST_DOC_SET, 1));
    assert!(matches!(first.blocks.first(), Some(Block::Heading { level: 1, .. })));

    host.open("preview", &host.ws.join("other.md"));
    let second = doc(&host.wait_for(methods::HOST_DOC_SET, 2));
    assert_eq!(second.surface, "preview");
    assert_eq!(second.blocks, vec![Block::Heading { level: 2, text: "Second".into() }]);
}

#[test]
fn a_missing_file_becomes_a_notice_not_a_teardown() {
    let mut host = Host::spawn("missing");
    host.open("preview", &host.ws.join("nope.md"));

    // A read that fails is the reader's mistake, not a protocol fault: the module turns it
    // into a one-block document the reader sees in the pane.
    let set = doc(&host.wait_for(methods::HOST_DOC_SET, 1));
    assert_eq!(set.surface, "preview");
    assert!(
        matches!(set.blocks.as_slice(), [Block::Notice { .. }]),
        "a missing file is one notice block, got {:?}",
        set.blocks
    );

    // And the module is still alive: a real file opened next still renders.
    host.open("preview", &host.ws.join("README.md"));
    let good = doc(&host.wait_for(methods::HOST_DOC_SET, 2));
    assert!(matches!(good.blocks.first(), Some(Block::Heading { level: 1, .. })));
}

#[test]
fn a_non_text_file_is_named_not_parsed_as_mojibake() {
    let mut host = Host::spawn("binary");
    host.open("preview", &host.ws.join("logo.png"));
    let set = doc(&host.wait_for(methods::HOST_DOC_SET, 1));
    assert!(
        matches!(set.blocks.as_slice(), [Block::Notice { .. }]),
        "a binary file is one notice block, got {:?}",
        set.blocks
    );
}

#[test]
fn an_unknown_event_is_ignored() {
    let mut host = Host::spawn("unknown");
    // A kind the module does not handle must not make it read or draw anything.
    host.notify(
        methods::MODULE_EVENT,
        json!({ "kind": "something.else", "payload": { "surface": "preview", "path": "x" } }),
    );
    host.run_to_shutdown();
    assert_eq!(host.count(methods::HOST_FS_READ), 0, "no file should have been read");
    assert_eq!(host.count(methods::HOST_DOC_SET), 0, "no document should have been set");
}

#[test]
fn without_the_pane_cap_it_neither_reads_nor_draws() {
    // The host can grant events but withhold the pane: the module subscribes and hears the
    // open, but a preview with nowhere to draw reads nothing and sets nothing.
    let mut host = Host::spawn_with(
        "no-pane",
        vec![Capability::FsRead, Capability::EventsSubscribe],
    );
    host.wait_for(methods::HOST_EVENTS_SUBSCRIBE, 1);
    host.open("preview", &host.ws.join("README.md"));
    host.run_to_shutdown();
    assert_eq!(host.count(methods::HOST_FS_READ), 0, "no pane, so no read");
    assert_eq!(host.count(methods::HOST_DOC_SET), 0, "no pane, so no doc.set");
}
