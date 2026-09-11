//! The real `avada-datatree` binary against a fake host and a real directory tree.
//!
//! The host side is what `avada_core` does: a socketpair whose child end is handed to the
//! module in `AVADA_MODULE_FD`, `module.hello` / `host.hello` as the first two lines, then
//! JSON-RPC both ways. `host.fs.read` hits an actual temp directory, scoped to the
//! workspace root exactly as the host scopes it, so a file this test says was opened is a
//! file on disk. Nothing here touches the network or anything outside `std::env::temp_dir()`.
//!
//! The integration under test is the whole of what the built-in JSON view *was*, and one
//! thing it never was: interactive. The host says a file was opened into the `datatree`
//! surface (`module.event` carrying a `doc.open`), and the module answers by reading the
//! file through `host.fs.read` and filling the pane with a tree of foldable rows through
//! `host.rows.set { target: "pane" }`. Then a row click comes back as a `module.row.activate`
//! request, and the module re-ships the rows with that node's disclosure flipped — the fold
//! is the module's, so this test drives it end to end.
#![cfg(unix)]
#![allow(unsafe_code)] // one fcntl, to let the module inherit its socket

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use avada_json_parse::{flatten, Flattened, LineKind};
use avada_module_sdk::contract::{methods, ModuleHello};
use avada_module_sdk::rail::{Row, RowTarget, SetRows};
use avada_module_sdk::Capability;
use serde_json::{json, Value};

const STEP: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------- the workspace

struct TempDir(PathBuf);

impl TempDir {
    /// A workspace with a nested JSON, a JSONL stream, and a binary file, canonicalised: on
    /// macOS `std::env::temp_dir()` is a symlink and the host answers with real paths, so
    /// the test must too or every path it sends would disagree with the root it scoped
    /// against.
    fn workspace(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "avada-datatree-e2e-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // Top-level scalars and one array container that opens by default (depth 0 is below
        // the collapse threshold), so the tree genuinely folds rather than being flat.
        std::fs::write(
            dir.join("data.json"),
            r#"{"name": "bolt", "qty": 4, "tags": ["a", "b"]}"#,
        )
        .unwrap();
        std::fs::write(dir.join("stream.jsonl"), "{\"a\": 1}\n{\"b\": 2}\n").unwrap();
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

/// `host.fs.read`: text when the file is UTF-8, `bytes_b64` when it is not. The module
/// makes no tree of the second answer, so the fake host must be able to give it.
fn fs_read(root: &std::path::Path, path: &str) -> Result<Value, Value> {
    let real = scoped(root, path)?;
    let bytes = std::fs::read(&real)
        .map_err(|e| json!({ "code": -32602, "message": format!("`{path}`: {e}") }))?;
    match String::from_utf8(bytes) {
        Ok(text) => Ok(json!({ "text": text })),
        Err(_) => Ok(json!({ "bytes_b64": "" })),
    }
}

// ---------------------------------------------------------------- the fake host

/// The outcome of serving one line: a response to a call, a bare notification/request, or
/// the module closing the pipe (which a clean `module.shutdown` provokes).
enum Pumped {
    /// A line with no `method`: the module's response to one of *our* requests (its id is
    /// the caller's problem, not ours). We only count `host.*` messages, so nothing here
    /// reads the body.
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
    /// Our own id space for the requests the host sends the module (row activations).
    next_id: i64,
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
        let child = Command::new(env!("CARGO_BIN_EXE_avada-datatree"))
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
            next_id: 1,
            ws,
        };
        let first = host.read_line().expect("module.hello, not EOF");
        let hello: ModuleHello = serde_json::from_str(&first).expect("module.hello");
        assert_eq!(hello.manifest.module.id.to_string(), "bshuler/avada-datatree");
        assert!(hello.contract_min <= 1 && 1 <= hello.contract_max);
        for m in methods::MODULE_REQUIRED_V1 {
            assert!(
                hello.methods.iter().any(|x| x == m),
                "module.hello does not list {m}"
            );
        }
        // A rows pane takes clicks, not keystrokes, so it serves none of the grid optionals.
        // If one ever appears here, the tree view has quietly grown a keymap and stopped
        // being the click-only surface this test is written to guard.
        for m in methods::MODULE_OPTIONAL {
            assert!(
                !hello.methods.iter().any(|x| x == m),
                "a rows pane must not serve the grid method {m}"
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
                methods::HOST_FS_READ, methods::HOST_EVENTS_SUBSCRIBE, methods::HOST_ROWS_SET,
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
    /// everything else (including `host.rows.set`) gets `{}`. A line with no `method` is the
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

    /// A row click the host echoes back as a request. `gesture` is `"toggle"` for a
    /// container's disclosure and `"open"` for a leaf, the exact strings the app's row
    /// handler chooses by whether the row is expandable.
    fn activate_row(&mut self, surface: &str, row: &str, gesture: &str) {
        let id = self.next_id;
        self.next_id += 1;
        self.write_line(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": methods::MODULE_ROW_ACTIVATE,
            "params": { "entry": surface, "target": "pane", "row": row, "gesture": gesture },
        }));
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

/// Read the `host.rows.set` params back into the very type the module serialised, so the
/// assertions are against a `SetRows`, not against hand-spelled JSON that could drift from it.
fn set_rows(params: &Value) -> SetRows {
    serde_json::from_value(params.clone()).expect("host.rows.set params are a SetRows")
}

/// Assert the module's rows *are* the shared flatten's lines — the anti-drift check that
/// keeps the module and the app's built-in tree view row-for-row identical. `jsonl` and
/// `flipped` must be the state the module rendered at.
fn assert_parity(rows: &[Row], flat: &Flattened) {
    assert_eq!(flat.cut, 0, "this fixture is small enough to have no cut tail");
    assert_eq!(rows.len(), flat.lines.len(), "one row per flattened line");
    for (row, line) in rows.iter().zip(&flat.lines) {
        assert_eq!(row.id, line.path, "the row id is the line's stable path");
        assert_eq!(row.label, line.key, "the row label is the line's key");
        assert_eq!(i32::from(row.depth), line.depth, "indent is the line depth");
        match &line.kind {
            LineKind::Container { open, kids } => {
                assert!(row.expandable, "a container folds");
                assert_eq!(row.expanded, *open, "disclosure mirrors the flatten");
                assert_eq!(&row.detail, kids, "a container's detail is its child count");
            }
            LineKind::Scalar { value, .. } => {
                assert!(!row.expandable, "a scalar has nothing to fold");
                assert_eq!(&row.detail, value, "a scalar's detail is its value");
            }
        }
    }
}

// ---------------------------------------------------------------- the tests

#[test]
fn it_subscribes_to_doc_open_when_it_may() {
    let mut host = Host::spawn("subscribe");
    let params = host.wait_for(methods::HOST_EVENTS_SUBSCRIBE, 1);
    let kinds = params["kinds"].as_array().expect("kinds is a list");
    assert!(
        kinds.iter().any(|k| k == methods::events::DOC_OPEN),
        "the tree view subscribes to {}, got {params}",
        methods::events::DOC_OPEN
    );
}

#[test]
fn a_doc_open_reads_the_file_and_fills_the_pane() {
    let mut host = Host::spawn("render");
    let path = host.ws.join("data.json");
    host.open("datatree", &path);

    // The module reads through the host's cap, not its own file handle.
    let read = host.wait_for(methods::HOST_FS_READ, 1);
    assert_eq!(read["path"].as_str(), Some(path.as_str()));

    // Then it fills the pane it was cued with — a pane target, not the rail.
    let set = set_rows(&host.wait_for(methods::HOST_ROWS_SET, 1));
    assert_eq!(set.entry, "datatree");
    assert_eq!(set.target, RowTarget::Pane);

    // Row-for-row the same flatten the app's built-in tree view runs.
    let text = std::fs::read_to_string(&path).unwrap();
    let flat = flatten(&text, false, &BTreeSet::new()).unwrap();
    assert_parity(&set.rows, &flat);
    // The fixture genuinely exercised the tree: `tags` is an open container with its two
    // items beneath it.
    let tags = set
        .rows
        .iter()
        .find(|r| r.id == "$.tags")
        .expect("a tags row");
    assert!(tags.expandable && tags.expanded, "tags opens by default");
    assert!(
        set.rows.iter().any(|r| r.id == "$.tags[0]"),
        "the open container's items are present, got {:?}",
        set.rows.iter().map(|r| &r.id).collect::<Vec<_>>()
    );
}

#[test]
fn a_toggle_folds_the_node_and_re_ships() {
    let mut host = Host::spawn("toggle");
    let path = host.ws.join("data.json");
    host.open("datatree", &path);
    let first = set_rows(&host.wait_for(methods::HOST_ROWS_SET, 1));
    assert!(
        first.rows.iter().any(|r| r.id == "$.tags[0]"),
        "tags is open before the toggle"
    );

    // The click the app forwards for a container: collapse `$.tags`.
    host.activate_row("datatree", "$.tags", "toggle");
    let second = set_rows(&host.wait_for(methods::HOST_ROWS_SET, 2));

    // The container stays, now closed, and its subtree is gone — but nothing after it.
    let tags = second.rows.iter().find(|r| r.id == "$.tags").expect("tags row");
    assert!(tags.expandable && !tags.expanded, "the toggle collapsed tags");
    assert!(
        !second.rows.iter().any(|r| r.id.starts_with("$.tags[")),
        "a collapsed container hides its items, got {:?}",
        second.rows.iter().map(|r| &r.id).collect::<Vec<_>>()
    );
    // And it is the module's own fold: the re-ship equals a fresh flatten with that node flipped.
    let text = std::fs::read_to_string(&path).unwrap();
    let flipped: BTreeSet<String> = ["$.tags".to_string()].into_iter().collect();
    assert_parity(&second.rows, &flatten(&text, false, &flipped).unwrap());

    // Toggling the same node again re-opens it: the fold set is XOR, not one-way.
    host.activate_row("datatree", "$.tags", "toggle");
    let third = set_rows(&host.wait_for(methods::HOST_ROWS_SET, 3));
    assert!(
        third.rows.iter().any(|r| r.id == "$.tags[0]"),
        "a second toggle re-opens the container"
    );
}

#[test]
fn a_leaf_open_gesture_changes_nothing() {
    let mut host = Host::spawn("leaf");
    host.open("datatree", &host.ws.join("data.json"));
    host.wait_for(methods::HOST_ROWS_SET, 1);

    // A scalar has nothing to fold, so the `open` the app sends for a leaf is acknowledged
    // and dropped — no re-ship. `name` is a top-level string.
    host.activate_row("datatree", "$.name", "open");
    host.run_to_shutdown();
    assert_eq!(
        host.count(methods::HOST_ROWS_SET),
        1,
        "a leaf click must not re-ship the tree"
    );
}

#[test]
fn jsonl_is_one_record_per_line() {
    let mut host = Host::spawn("jsonl");
    let path = host.ws.join("stream.jsonl");
    host.open("datatree", &path);
    let set = set_rows(&host.wait_for(methods::HOST_ROWS_SET, 1));

    // The `.jsonl` extension makes the module read one document per line, so the two
    // records are two roots — the same as `flatten(text, jsonl = true, …)`.
    let text = std::fs::read_to_string(&path).unwrap();
    assert_parity(&set.rows, &flatten(&text, true, &BTreeSet::new()).unwrap());
    assert_eq!(set.rows[0].id, "$1", "the first record is the first line");
    assert!(
        set.rows.iter().any(|r| r.id == "$2"),
        "the second line is a second root, got {:?}",
        set.rows.iter().map(|r| &r.id).collect::<Vec<_>>()
    );
}

#[test]
fn a_second_open_replaces_the_tree_and_resets_the_fold() {
    let mut host = Host::spawn("replace");

    // Open data.json and collapse tags, so there is a fold to carry over — or not.
    host.open("datatree", &host.ws.join("data.json"));
    host.wait_for(methods::HOST_ROWS_SET, 1);
    host.activate_row("datatree", "$.tags", "toggle");
    let folded = set_rows(&host.wait_for(methods::HOST_ROWS_SET, 2));
    assert!(!folded.rows.iter().any(|r| r.id == "$.tags[0]"), "tags is folded");

    // A different file into the same pane starts fresh: the new tree is the JSONL stream,
    // and — the point of the test — the previous file's fold does not touch it.
    host.open("datatree", &host.ws.join("stream.jsonl"));
    let second = set_rows(&host.wait_for(methods::HOST_ROWS_SET, 3));
    assert_eq!(second.entry, "datatree");
    let text = std::fs::read_to_string(host.ws.join("stream.jsonl")).unwrap();
    assert_parity(&second.rows, &flatten(&text, true, &BTreeSet::new()).unwrap());
}

#[test]
fn a_missing_file_becomes_a_notice_row_not_a_teardown() {
    let mut host = Host::spawn("missing");
    host.open("datatree", &host.ws.join("nope.json"));

    // A read that fails is the reader's mistake, not a protocol fault: the module turns it
    // into a single un-foldable row the reader sees in the pane.
    let set = set_rows(&host.wait_for(methods::HOST_ROWS_SET, 1));
    assert_eq!(set.entry, "datatree");
    assert_eq!(set.rows.len(), 1, "a missing file is one notice row, got {:?}", set.rows);
    assert!(!set.rows[0].expandable, "the notice row does not fold");
    assert!(!set.rows[0].label.is_empty(), "the notice row names the failure");

    // And the module is still alive: a real file opened next still renders.
    host.open("datatree", &host.ws.join("data.json"));
    let good = set_rows(&host.wait_for(methods::HOST_ROWS_SET, 2));
    assert!(good.rows.iter().any(|r| r.id == "$.name"), "the tree renders after the notice");
}

#[test]
fn a_non_text_file_is_named_not_parsed_as_mojibake() {
    let mut host = Host::spawn("binary");
    host.open("datatree", &host.ws.join("logo.png"));
    let set = set_rows(&host.wait_for(methods::HOST_ROWS_SET, 1));
    assert_eq!(set.rows.len(), 1, "a binary file is one notice row, got {:?}", set.rows);
    assert!(!set.rows[0].expandable, "the notice row does not fold");
}

#[test]
fn an_unknown_event_is_ignored() {
    let mut host = Host::spawn("unknown");
    // A kind the module does not handle must not make it read or draw anything.
    host.notify(
        methods::MODULE_EVENT,
        json!({ "kind": "something.else", "payload": { "surface": "datatree", "path": "x" } }),
    );
    host.run_to_shutdown();
    assert_eq!(host.count(methods::HOST_FS_READ), 0, "no file should have been read");
    assert_eq!(host.count(methods::HOST_ROWS_SET), 0, "no rows should have been set");
}

#[test]
fn without_the_pane_cap_it_neither_reads_nor_draws() {
    // The host can grant events but withhold the pane: the module subscribes and hears the
    // open, but a tree with nowhere to draw reads nothing and sets nothing.
    let mut host = Host::spawn_with(
        "no-pane",
        vec![Capability::FsRead, Capability::EventsSubscribe],
    );
    host.wait_for(methods::HOST_EVENTS_SUBSCRIBE, 1);
    host.open("datatree", &host.ws.join("data.json"));
    host.run_to_shutdown();
    assert_eq!(host.count(methods::HOST_FS_READ), 0, "no pane, so no read");
    assert_eq!(host.count(methods::HOST_ROWS_SET), 0, "no pane, so no rows.set");
}
