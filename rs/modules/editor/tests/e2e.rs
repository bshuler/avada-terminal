//! The real `avada-editor` binary against a fake host and a real directory tree.
//!
//! The host side is what `avada_core` does: a socketpair whose child end is handed to the
//! module in `AVADA_MODULE_FD`, `module.hello` / `host.hello` as the first two lines, then
//! JSON-RPC both ways. `host.fs.read` and `host.fs.write` hit an actual temp directory,
//! scoped to the workspace root exactly as the host scopes it, so a file this test says
//! was saved is a file on disk. Nothing here touches the network or anything outside
//! `std::env::temp_dir()`.
//!
//! The keystrokes are sent the way the host sends them — already resolved against the
//! keymap, with `action` set when a binding claimed the chord and `text` set for a plain
//! printable character. That is the contract the module was written against, so a test
//! that spells a key any other way would be testing a host that does not exist.
#![cfg(unix)]
#![allow(unsafe_code)] // one fcntl, to let the module inherit its socket

use std::io::{BufRead, BufReader, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use avada_module_sdk::contract::{methods, ModuleHello};
use avada_module_sdk::Capability;
use serde_json::{json, Value};

const STEP: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------- the workspace

struct TempDir(PathBuf);

impl TempDir {
    /// A workspace with a file or two, canonicalised: on macOS `std::env::temp_dir()` is a
    /// symlink and the host answers with real paths, so the test must too or every path it
    /// sends would disagree with the root it scoped against.
    fn workspace(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "avada-editor-e2e-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("README.md"), "hello\nworld\n").unwrap();
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
/// refused. `parent`-relative so a write to a file that does not exist yet still resolves.
fn scoped(root: &Path, path: &str) -> Result<PathBuf, Value> {
    let p = PathBuf::from(path);
    let real = match p.canonicalize() {
        Ok(r) => r,
        Err(_) => {
            let parent = p.parent().unwrap_or(Path::new("/"));
            let base = parent
                .canonicalize()
                .map_err(|e| json!({ "code": -32602, "message": format!("`{path}`: {e}") }))?;
            base.join(p.file_name().unwrap_or_default())
        }
    };
    if !real.starts_with(root) {
        return Err(json!({
            "code": -32001,
            "message": format!("`{path}` is outside the workspace root"),
        }));
    }
    Ok(real)
}

/// `host.fs.read`: text when the file is UTF-8, `bytes_b64` when it is not. The module has
/// to cope with the second answer, so the fake host must be able to give it.
fn fs_read(root: &Path, path: &str) -> Result<Value, Value> {
    let real = scoped(root, path)?;
    let bytes = std::fs::read(&real)
        .map_err(|e| json!({ "code": -32602, "message": format!("`{path}`: {e}") }))?;
    match String::from_utf8(bytes.clone()) {
        Ok(text) => Ok(json!({ "text": text })),
        // Only the shape matters here; the module refuses on `text` being absent.
        Err(_) => Ok(json!({ "bytes_b64": "" })),
    }
}

/// `host.fs.write`: whole file, text only.
fn fs_write(root: &Path, params: &Value) -> Result<Value, Value> {
    let path = params["path"].as_str().unwrap_or_default();
    let real = scoped(root, path)?;
    let text = params["text"]
        .as_str()
        .ok_or_else(|| json!({ "code": -32602, "message": "host.fs.write needs `text`" }))?;
    std::fs::write(&real, text)
        .map_err(|e| json!({ "code": -32602, "message": format!("`{path}`: {e}") }))?;
    Ok(json!({}))
}

// ---------------------------------------------------------------- the fake host

struct Host {
    child: Child,
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
    /// Every `host.*` request or notification the module sent, in order.
    host_calls: Vec<(String, Value)>,
    /// What `host.prefs.get` answers.
    prefs: Value,
    /// Whether `host.grid.set` is accepted. A host with no pane open for the surface
    /// refuses it with `InvalidParams`, which is the module's only liveness signal.
    pane_open: bool,
    ws: TempDir,
}

impl Host {
    fn spawn(tag: &str) -> Self {
        Self::spawn_with(tag, granted(), json!({}))
    }

    fn spawn_with(tag: &str, granted: Vec<Capability>, prefs: Value) -> Self {
        let (host_end, child_end) = UnixStream::pair().unwrap();
        // The host clears CLOEXEC on the child end so the descriptor survives exec.
        let fd = child_end.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(flags >= 0);
        assert!(unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } >= 0);
        let ws = TempDir::workspace(tag);
        let child = Command::new(env!("CARGO_BIN_EXE_avada-editor"))
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
            next_id: 1,
            host_calls: vec![],
            prefs,
            pane_open: false,
            ws,
        };
        let first = host.read_line();
        let hello: ModuleHello = serde_json::from_str(&first).expect("module.hello");
        assert_eq!(hello.manifest.module.id.to_string(), "bshuler/avada-editor");
        assert!(hello.contract_min <= 1 && 1 <= hello.contract_max);
        for m in methods::MODULE_REQUIRED_V1 {
            assert!(
                hello.methods.iter().any(|x| x == m),
                "module.hello does not list {m}"
            );
        }
        // A tier-5 contribution that did not serve these would be a picture, not a pane.
        for m in [methods::MODULE_GRID_KEY, methods::MODULE_GRID_RESIZE] {
            assert!(
                hello.methods.iter().any(|x| x == m),
                "module.hello does not list {m}"
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
                methods::HOST_RAIL_REGISTER, methods::HOST_ROWS_SET,
                methods::HOST_COMMAND_REGISTER, methods::HOST_TOAST,
                methods::HOST_FS_READ, methods::HOST_FS_WRITE,
                methods::HOST_PANES_SPAWN, methods::HOST_EVENTS_SUBSCRIBE,
                methods::HOST_GRID_SET, methods::HOST_KEYMAP_DECLARE,
                methods::HOST_PREFS_DECLARE, methods::HOST_PREFS_GET,
            ],
            "data_dir": root,
            "workspace": { "id": "ws1", "name": "Test workspace", "root": root },
        }));
        host
    }

    fn read_line(&mut self) -> String {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => panic!("module closed the pipe"),
            Ok(_) => line,
            Err(e) => panic!("no line from the module within {STEP:?}: {e}"),
        }
    }

    fn write_line(&mut self, v: &Value) {
        let mut s = v.to_string();
        s.push('\n');
        self.writer.write_all(s.as_bytes()).unwrap();
        self.writer.flush().unwrap();
    }

    fn pump(&mut self) -> Option<Value> {
        let line = self.read_line();
        self.handle(&line)
    }

    /// Answer one module→host message. The filesystem calls hit the real temp workspace;
    /// everything else gets `{}` or the one field its caller reads.
    fn handle(&mut self, line: &str) -> Option<Value> {
        let v: Value = serde_json::from_str(line).expect("json line");
        let Some(method) = v.get("method").and_then(Value::as_str).map(str::to_string) else {
            return Some(v);
        };
        let params = v.get("params").cloned().unwrap_or(Value::Null);
        if let Some(id) = v.get("id").cloned() {
            let result = match method.as_str() {
                methods::HOST_FS_READ => {
                    fs_read(&self.ws.0, params["path"].as_str().unwrap_or_default())
                }
                methods::HOST_FS_WRITE => fs_write(&self.ws.0, &params),
                methods::HOST_PANES_SPAWN => {
                    self.pane_open = true;
                    Ok(json!({ "pane_id": "pane-1" }))
                }
                methods::HOST_PREFS_GET => Ok(json!({ "values": self.prefs })),
                methods::HOST_GRID_SET if !self.pane_open => Err(json!({
                    "code": -32602,
                    "message": "no pane is open for surface `editor`",
                })),
                _ => Ok(json!({})),
            };
            let answer = match result {
                Ok(ok) => json!({ "jsonrpc": "2.0", "id": id, "result": ok }),
                Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": e }),
            };
            self.write_line(&answer);
        }
        self.host_calls.push((method, params));
        None
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.write_line(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        let deadline = Instant::now() + STEP;
        while Instant::now() < deadline {
            if let Some(resp) = self.pump() {
                assert_eq!(resp["id"], id, "response out of order: {resp}");
                return resp;
            }
        }
        panic!("no response to {method} within {STEP:?}")
    }

    fn ok(&mut self, method: &str, params: Value) -> Value {
        let resp = self.call(method, params);
        assert!(
            resp.get("error").is_none(),
            "{method} failed: {}",
            resp["error"]
        );
        resp["result"].clone()
    }

    fn err(&mut self, method: &str, params: Value) -> Value {
        let resp = self.call(method, params);
        assert!(
            resp.get("result").is_none(),
            "{method} unexpectedly succeeded: {}",
            resp["result"]
        );
        resp["error"].clone()
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.write_line(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    /// Serve the module until it has sent `method` `n` times in total.
    fn wait_for(&mut self, method: &str, n: usize) -> Value {
        let deadline = Instant::now() + STEP;
        while Instant::now() < deadline {
            let seen: Vec<&(String, Value)> = self
                .host_calls
                .iter()
                .filter(|(m, _)| m == method)
                .collect();
            if seen.len() >= n {
                return seen[n - 1].1.clone();
            }
            let _ = self.pump();
        }
        panic!("module never sent {method} #{n}")
    }

    fn count(&self, method: &str) -> usize {
        self.host_calls.iter().filter(|(m, _)| m == method).count()
    }

    fn last(&self, method: &str) -> Value {
        self.host_calls
            .iter()
            .rev()
            .find(|(m, _)| m == method)
            .map(|(_, p)| p.clone())
            .unwrap_or_else(|| panic!("module never sent {method}"))
    }

    /// The next frame the module paints after whatever we just did. A repaint always
    /// trails the message that caused it, so the count is snapshotted and then waited on.
    fn next_frame(&mut self) -> Value {
        let n = self.count(methods::HOST_GRID_SET) + 1;
        self.wait_for(methods::HOST_GRID_SET, n)
    }

    /// The most recent frame already taken, for after a helper that took its own.
    fn frame(&self) -> Value {
        self.last(methods::HOST_GRID_SET)
    }

    /// The next buffer list after whatever we just did.
    fn next_rows(&mut self) -> Vec<Value> {
        let n = self.count(methods::HOST_ROWS_SET) + 1;
        let params = self.wait_for(methods::HOST_ROWS_SET, n);
        params["rows"].as_array().cloned().unwrap_or_default()
    }

    /// A keystroke, spelled the way the host spells one: the chord already resolved.
    fn key(&mut self, key: &str, action: Option<&str>, text: Option<&str>) {
        self.notify(
            methods::MODULE_GRID_KEY,
            json!({ "surface": "editor", "key": key, "action": action, "text": text }),
        );
    }

    /// Type a run of plain printable characters, as the host would deliver them, taking
    /// the repaint each one causes. Every key produces exactly one frame, so leaving them
    /// unread would make the next assertion read a paint from several keystrokes ago.
    fn typing(&mut self, s: &str) {
        for c in s.chars() {
            let ch = c.to_string();
            self.key(&ch, None, Some(&ch));
            self.next_frame();
        }
    }

    fn shutdown(mut self) {
        self.write_line(&json!({ "jsonrpc": "2.0", "method": methods::MODULE_SHUTDOWN }));
        self.reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                assert!(status.success(), "module exited with {status}");
                return;
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("module did not exit within 5 s of module.shutdown");
            }
            let mut line = String::new();
            if let Ok(n) = self.reader.read_line(&mut line) {
                if n > 0 {
                    self.handle(&line);
                }
            }
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn granted() -> Vec<Capability> {
    vec![
        Capability::FsRead,
        Capability::FsWrite,
        Capability::UiRail,
        Capability::UiPane,
        Capability::UiCommands,
        Capability::UiPrefs,
        Capability::UiToast,
        Capability::EventsSubscribe,
        Capability::PanesSpawn,
    ]
}

/// The text of one rendered line, gutter included.
fn line_text(frame: &Value, i: usize) -> String {
    frame["lines"][i]["spans"]
        .as_array()
        .map(|spans| {
            spans
                .iter()
                .map(|s| s["text"].as_str().unwrap_or_default())
                .collect::<String>()
        })
        .unwrap_or_default()
}

/// Open a file through the command the palette offers, and take the pane with it.
fn open(host: &mut Host, rel: &str) {
    let path = host.ws.join(rel);
    host.ok(
        methods::MODULE_COMMAND_INVOKE,
        json!({ "id": "open", "args": { "path": path } }),
    );
}

// ---------------------------------------------------------------- the tests

#[test]
fn it_registers_a_rail_entry_a_keymap_a_prefs_page_and_its_commands() {
    let mut host = Host::spawn("register");
    // The rail entry is what the module is called on the left panel; the rows under it
    // are the open buffers, and there is one from the start: the scratch buffer.
    let rail = host.wait_for(methods::HOST_RAIL_REGISTER, 1);
    assert_eq!(rail["entries"][0]["id"], "editor");
    assert_eq!(rail["entries"][0]["tier"], 1);

    let keymap = host.wait_for(methods::HOST_KEYMAP_DECLARE, 1);
    assert_eq!(keymap["surface"], "editor");
    let presets: Vec<&str> = keymap["presets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(presets, ["helix", "vim", "basic"]);
    assert_eq!(keymap["default_preset"], "helix");
    assert!(!keymap["actions"].as_array().unwrap().is_empty());

    let prefs = host.wait_for(methods::HOST_PREFS_DECLARE, 1);
    assert_eq!(prefs["page"]["fields"][0]["key"], "start_mode");

    let cmds = host.wait_for(methods::HOST_COMMAND_REGISTER, 1);
    let ids: Vec<&str> = cmds["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["open", "save", "close", "next", "prev"]);

    let subs = host.wait_for(methods::HOST_EVENTS_SUBSCRIBE, 1);
    assert_eq!(subs["kinds"][0], methods::events::FILES_REVEAL);

    let rows = host.next_rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["label"], "[scratch]");
    host.shutdown();
}

#[test]
fn opening_a_file_spawns_a_pane_and_paints_it() {
    let mut host = Host::spawn("open");
    host.wait_for(methods::HOST_RAIL_REGISTER, 1);
    open(&mut host, "README.md");
    let spawn = host.wait_for(methods::HOST_PANES_SPAWN, 1);
    // No `command` field: `host.panes.spawn` is deliberately not a way to run a process.
    assert_eq!(spawn["kind"], "module");
    assert_eq!(spawn["surface"], "editor");

    let frame = host.next_frame();
    assert_eq!(frame["surface"], "editor");
    assert!(line_text(&frame, 0).ends_with("hello"), "{frame}");
    assert!(line_text(&frame, 1).ends_with("world"));
    assert!(
        frame["status"].as_str().unwrap().starts_with("README.md"),
        "the host draws the status line from what the module puts here"
    );
    // The scratch buffer was clean and unnamed, so the file took its place rather than
    // leaving an empty buffer behind forever.
    let rows = host.next_rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["label"], "README.md");
    host.shutdown();
}

#[test]
fn a_resize_is_what_tells_the_module_how_much_of_the_file_fits() {
    let mut host = Host::spawn("resize");
    open(&mut host, "README.md");
    host.next_frame();
    host.notify(
        methods::MODULE_GRID_RESIZE,
        json!({ "surface": "editor", "cols": 40, "rows": 6 }),
    );
    let frame = host.next_frame();
    // The host draws its own 22px status bar without taking the row back off the count it
    // reported, so the module reserves the last row itself: six cells, five lines of text.
    assert_eq!(frame["rows"], 5);
    assert_eq!(frame["cols"], 40);
    assert_eq!(frame["lines"].as_array().unwrap().len(), 5);
    host.shutdown();
}

#[test]
fn typing_only_reaches_the_file_in_insert_mode() {
    let mut host = Host::spawn("modality");
    open(&mut host, "README.md");
    host.next_frame();

    // `q` is not bound in the Helix preset, so it arrives with text and no action. In
    // normal mode that must not land in the file — this is the whole point of the mode.
    host.typing("q");
    let frame = host.frame();
    assert!(line_text(&frame, 0).ends_with("hello"), "{frame}");

    // `i` is bound: the host sends both the action and the text, and the action wins.
    host.key("i", Some("mode.insert"), Some("i"));
    let frame = host.next_frame();
    assert!(line_text(&frame, 0).ends_with("hello"), "{frame}");
    assert!(frame["status"].as_str().unwrap().contains("INS"));
    assert_eq!(frame["cursor"]["shape"], "bar");

    // Now the same keystroke types.
    host.typing("q");
    let frame = host.frame();
    assert!(line_text(&frame, 0).ends_with("qhello"), "{frame}");
    host.shutdown();
}

#[test]
fn escape_leaves_insert_mode_and_a_named_key_still_works_inside_it() {
    let mut host = Host::spawn("escape");
    open(&mut host, "README.md");
    host.next_frame();
    host.key("i", Some("mode.insert"), Some("i"));
    host.next_frame();
    // Named keys carry no text, so they keep working in insert mode by the same rule.
    host.key("enter", Some("edit.newline"), None);
    let frame = host.next_frame();
    assert!(line_text(&frame, 1).ends_with("hello"), "{frame}");
    host.key("escape", Some("mode.normal"), None);
    let frame = host.next_frame();
    assert!(frame["status"].as_str().unwrap().contains("NOR"));
    assert_eq!(frame["cursor"]["shape"], "block");
    host.shutdown();
}

#[test]
fn saving_writes_the_file_and_clears_the_modified_mark() {
    let mut host = Host::spawn("save");
    open(&mut host, "README.md");
    host.next_frame();
    host.key("i", Some("mode.insert"), Some("i"));
    host.next_frame();
    host.typing("X");
    let rows = host.next_rows();
    assert_eq!(rows[0]["marks"][0], "modified");

    host.key("ctrl+s", Some("file.save"), None);
    let write = host.wait_for(methods::HOST_FS_WRITE, 1);
    assert_eq!(write["text"], "Xhello\nworld\n");
    // Whole-file, text-only: there is no patch form of `host.fs.write` on purpose.
    assert!(write.get("bytes_b64").is_none());
    assert_eq!(
        std::fs::read_to_string(host.ws.join("README.md")).unwrap(),
        "Xhello\nworld\n"
    );
    let rows = host.next_rows();
    assert!(
        rows[0]["marks"].as_array().unwrap().is_empty(),
        "a saved buffer is not modified any more"
    );
    host.shutdown();
}

#[test]
fn undo_and_redo_walk_the_edit_back_and_forward() {
    let mut host = Host::spawn("undo");
    open(&mut host, "README.md");
    host.next_frame();
    host.key("i", Some("mode.insert"), Some("i"));
    host.next_frame();
    host.typing("abc");
    host.key("escape", Some("mode.normal"), None);
    let frame = host.next_frame();
    assert!(line_text(&frame, 0).ends_with("abchello"), "{frame}");

    host.key("u", Some("edit.undo"), Some("u"));
    let frame = host.next_frame();
    assert!(
        line_text(&frame, 0).ends_with("hello") && !line_text(&frame, 0).contains("abc"),
        "one undo takes the whole typed run: {frame}"
    );
    host.key("U", Some("edit.redo"), Some("U"));
    let frame = host.next_frame();
    assert!(line_text(&frame, 0).ends_with("abchello"), "{frame}");
    host.shutdown();
}

#[test]
fn a_files_reveal_event_opens_the_file_at_the_line_it_names() {
    let mut host = Host::spawn("reveal");
    host.wait_for(methods::HOST_EVENTS_SUBSCRIBE, 1);
    let path = host.ws.join("README.md");
    // This is the whole of the Files→Editor integration: modules may only call `host.*`,
    // so Files says what happened and the host fans it out. Nobody names the editor.
    host.notify(
        methods::MODULE_EVENT,
        json!({
            "kind": methods::events::FILES_REVEAL,
            "payload": { "path": path, "line": 2, "col": 3 },
        }),
    );
    let frame = host.next_frame();
    assert!(line_text(&frame, 1).ends_with("world"), "{frame}");
    // 1-based on the wire, as in `file:line:col` everywhere a human writes one.
    assert_eq!(frame["cursor"]["line"], 1);
    assert!(frame["status"].as_str().unwrap().contains("2:3"), "{frame}");
    host.shutdown();
}

#[test]
fn several_buffers_can_be_open_and_the_rail_switches_between_them() {
    let mut host = Host::spawn("buffers");
    open(&mut host, "README.md");
    host.next_frame();
    open(&mut host, "src/main.rs");
    let frame = host.next_frame();
    assert!(line_text(&frame, 0).ends_with("fn main() {}"), "{frame}");

    let rows = host.next_rows();
    let labels: Vec<&str> = rows.iter().map(|r| r["label"].as_str().unwrap()).collect();
    assert_eq!(labels, ["README.md", "main.rs"]);

    // Clicking the other buffer in the rail is how you go back to it.
    host.ok(
        methods::MODULE_ROW_ACTIVATE,
        json!({
            "entry": "editor",
            "row": rows[0]["id"],
            "data": rows[0]["data"],
            "gesture": "open",
        }),
    );
    let frame = host.next_frame();
    assert!(line_text(&frame, 0).ends_with("hello"), "{frame}");

    // And so is the command, which wraps.
    host.ok(methods::MODULE_COMMAND_INVOKE, json!({ "id": "next" }));
    let frame = host.next_frame();
    assert!(line_text(&frame, 0).ends_with("fn main() {}"), "{frame}");
    host.ok(methods::MODULE_COMMAND_INVOKE, json!({ "id": "prev" }));
    let frame = host.next_frame();
    assert!(line_text(&frame, 0).ends_with("hello"), "{frame}");
    host.shutdown();
}

#[test]
fn closing_the_last_buffer_leaves_a_scratch_behind_rather_than_nothing() {
    let mut host = Host::spawn("close");
    open(&mut host, "README.md");
    host.next_frame();
    host.ok(methods::MODULE_COMMAND_INVOKE, json!({ "id": "close" }));
    let rows = host.next_rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["label"], "[scratch]");
    host.shutdown();
}

#[test]
fn the_start_mode_preference_decides_which_mode_a_file_opens_in() {
    let mut host = Host::spawn_with("prefs", granted(), json!({ "start_mode": "insert" }));
    host.wait_for(methods::HOST_PREFS_GET, 1);
    open(&mut host, "README.md");
    let frame = host.next_frame();
    assert!(
        frame["status"].as_str().unwrap().contains("INS"),
        "the modeless keymap has no key that leaves normal mode, so this is the way in"
    );
    // And it follows a live change, without a restart.
    host.ok(
        methods::MODULE_PREFS_CHANGED,
        json!({ "values": { "start_mode": "normal" } }),
    );
    open(&mut host, "src/main.rs");
    let frame = host.next_frame();
    assert!(frame["status"].as_str().unwrap().contains("NOR"), "{frame}");
    host.shutdown();
}

#[test]
fn a_file_the_host_will_not_hand_over_becomes_a_toast_not_a_crash() {
    let mut host = Host::spawn("refused");
    host.wait_for(methods::HOST_RAIL_REGISTER, 1);
    // Outside the workspace: the host refuses, and the module has nothing to say but so.
    host.ok(
        methods::MODULE_COMMAND_INVOKE,
        json!({ "id": "open", "args": { "path": "/etc/hosts" } }),
    );
    let toast = host.wait_for(methods::HOST_TOAST, 1);
    assert_eq!(toast["level"], "error");
    assert!(toast["text"].as_str().unwrap().contains("outside"));

    // Binary: the host answers `bytes_b64`, and an editor that opened it would offer to
    // write the bytes back mangled.
    let png = host.ws.join("logo.png");
    host.ok(
        methods::MODULE_COMMAND_INVOKE,
        json!({ "id": "open", "args": { "path": png } }),
    );
    let toast = host.wait_for(methods::HOST_TOAST, 2);
    assert!(toast["text"].as_str().unwrap().contains("not a text file"));
    host.shutdown();
}

#[test]
fn open_without_a_path_is_an_error_and_an_unknown_command_is_method_not_found() {
    let mut host = Host::spawn("badcalls");
    let e = host.err(methods::MODULE_COMMAND_INVOKE, json!({ "id": "open" }));
    assert_eq!(e["code"], -32602);
    let e = host.err(methods::MODULE_COMMAND_INVOKE, json!({ "id": "fly" }));
    assert_eq!(e["code"], -32601);
    let e = host.err("module.nonsense", json!({}));
    assert_eq!(e["code"], -32601);
    host.shutdown();
}

#[test]
fn a_module_that_was_granted_nothing_still_answers_the_required_methods() {
    // The host hides the UI of a module it refused; the module must not fall over, and
    // must not call anything it was not granted.
    let mut host = Host::spawn_with("ungranted", vec![], json!({}));
    host.ok(methods::MODULE_ACTIVATE, json!({}));
    host.ok(methods::MODULE_COMMAND_INVOKE, json!({ "id": "save" }));
    let toast_or_none = host.count(methods::HOST_TOAST);
    assert_eq!(toast_or_none, 0, "no toast without `ui.toast`");
    assert_eq!(host.count(methods::HOST_RAIL_REGISTER), 0);
    assert_eq!(host.count(methods::HOST_GRID_SET), 0);
    host.shutdown();
}

#[test]
fn a_grid_key_for_another_surface_is_not_this_modules_business() {
    let mut host = Host::spawn("surface");
    open(&mut host, "README.md");
    host.next_frame();
    let before = host.count(methods::HOST_GRID_SET);
    host.key("x", Some("edit.delete.line"), None);
    host.notify(
        methods::MODULE_GRID_KEY,
        json!({ "surface": "somebody-else", "key": "x", "action": "edit.delete.line" }),
    );
    // One repaint for the key that was ours, and the file lost exactly one line.
    let frame = host.next_frame();
    assert_eq!(host.count(methods::HOST_GRID_SET), before + 1);
    assert!(line_text(&frame, 0).ends_with("world"), "{frame}");
    host.shutdown();
}

#[test]
fn the_manifest_flag_prints_the_embedded_manifest_and_exits() {
    let out = Command::new(env!("CARGO_BIN_EXE_avada-editor"))
        .arg("--manifest")
        .output()
        .expect("run --manifest");
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    let m = avada_module_sdk::manifest::Manifest::parse(&text).expect("manifest parses");
    assert_eq!(m.module.id.to_string(), "bshuler/avada-editor");
    assert!(m.capabilities.contains(&Capability::UiPane));
    // The `_any` variants are what let a module out of the workspace; this one never asks.
    assert!(!m.capabilities.contains(&Capability::FsReadAny));
    assert!(!m.capabilities.contains(&Capability::FsWriteAny));
}

#[test]
fn a_lost_pane_stops_the_painting_and_a_keystroke_brings_it_back() {
    let mut host = Host::spawn("lostpane");
    open(&mut host, "README.md");
    host.next_frame();
    // The user closed the pane. The host has no way to say so; a refused `host.grid.set`
    // is the whole of the signal, and the module must stop painting into nothing.
    host.pane_open = false;
    host.key("l", Some("move.right"), Some("l"));
    host.next_frame();
    let quiet = host.count(methods::HOST_GRID_SET);
    host.key("l", Some("move.right"), Some("l"));
    host.next_rows();
    assert_eq!(
        host.count(methods::HOST_GRID_SET),
        quiet,
        "no pane, no frames"
    );
    // A key can only arrive from a pane, so one is proof the belief was stale.
    host.pane_open = true;
    host.key("l", Some("move.right"), Some("l"));
    let frame = host.next_frame();
    assert_eq!(frame["surface"], "editor");
    host.shutdown();
}

#[test]
fn a_buffer_with_no_file_behind_it_says_so_rather_than_writing_somewhere() {
    let mut host = Host::spawn("scratchsave");
    host.wait_for(methods::HOST_RAIL_REGISTER, 1);
    host.ok(methods::MODULE_COMMAND_INVOKE, json!({ "id": "save" }));
    let toast = host.wait_for(methods::HOST_TOAST, 1);
    assert_eq!(toast["level"], "error");
    assert!(toast["text"].as_str().unwrap().contains("no file"));
    assert_eq!(host.count(methods::HOST_FS_WRITE), 0);
    host.shutdown();
}

#[test]
fn the_deactivate_and_route_methods_are_answered_even_though_nothing_is_routed() {
    let mut host = Host::spawn("required");
    for m in [
        methods::MODULE_ACTIVATE,
        methods::MODULE_DEACTIVATE,
        methods::MODULE_ROUTE_INVOKE,
        methods::MODULE_EVENT,
        methods::MODULE_PREFS_CHANGED,
    ] {
        host.ok(m, json!({}));
    }
    let _ = host.last(methods::HOST_RAIL_REGISTER);
    host.shutdown();
}
