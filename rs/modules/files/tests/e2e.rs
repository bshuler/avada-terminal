//! The real `avada-files` binary against a fake host and a real directory tree.
//!
//! The host side is what `avada_core` does: a socketpair whose child end is handed to the
//! module in `AVADA_MODULE_FD`, `module.hello` / `host.hello` as the first two lines, then
//! JSON-RPC both ways. `host.fs.list` is answered from an actual temp directory, scoped to
//! the workspace root exactly as the host scopes it, so a test that passes here is a test
//! that would pass against the host. Nothing here touches the network or the user's disk
//! outside `std::env::temp_dir()`.
#![cfg(unix)]
#![allow(unsafe_code)] // one fcntl, to let the module inherit its socket

use std::io::{BufRead, BufReader, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use avada_module_sdk::contract::{methods, ModuleHello};
use avada_module_sdk::manifest::Manifest;
use avada_module_sdk::Capability;
use serde_json::{json, Value};

const STEP: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------- the workspace

struct TempDir(PathBuf);

impl TempDir {
    /// A workspace with a couple of levels, canonicalised: on macOS `std::env::temp_dir()`
    /// is a symlink, and the host answers with real paths, so the test must too or every
    /// row id would disagree with every path it sent.
    fn workspace(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "avada-files-e2e-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("src/util")).unwrap();
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        std::fs::write(dir.join("README.md"), "hello\n").unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("src/util/helper.rs"), "// helper\n").unwrap();
        std::fs::write(dir.join("docs/guide.md"), "# guide\n").unwrap();
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

/// `host.fs.list`, scoped the way `avada_core::module::rpc` scopes it: a path that does not
/// resolve inside the workspace root is `CapabilityDenied`, not an empty listing.
fn fs_list(root: &Path, path: &str) -> Result<Value, Value> {
    let real = PathBuf::from(path)
        .canonicalize()
        .map_err(|e| json!({ "code": -32602, "message": format!("`{path}`: {e}") }))?;
    if !real.starts_with(root) {
        return Err(json!({
            "code": -32001,
            "message": format!("`{path}` is outside the workspace root"),
        }));
    }
    let rd = std::fs::read_dir(&real)
        .map_err(|e| json!({ "code": -32602, "message": format!("`{path}`: {e}") }))?;
    let mut entries: Vec<(String, &'static str)> = Vec::new();
    for ent in rd.flatten() {
        let kind = match ent.file_type() {
            Ok(t) if t.is_symlink() => "symlink",
            Ok(t) if t.is_dir() => "dir",
            Ok(t) if t.is_file() => "file",
            _ => "other",
        };
        entries.push((ent.file_name().to_string_lossy().into_owned(), kind));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(json!({
        "entries": entries.into_iter()
            .map(|(name, kind)| json!({ "name": name, "kind": kind }))
            .collect::<Vec<Value>>(),
    }))
}

// ---------------------------------------------------------------- the fake host

struct Host {
    child: Child,
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
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
        let child = Command::new(env!("CARGO_BIN_EXE_avada-files"))
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
            ws,
        };
        let first = host.read_line();
        let hello: ModuleHello = serde_json::from_str(&first).expect("module.hello");
        assert_eq!(hello.manifest.module.id.to_string(), "bshuler/avada-files");
        assert!(hello.contract_min <= 1 && 1 <= hello.contract_max);
        for m in methods::MODULE_REQUIRED_V1 {
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
                methods::HOST_FS_LIST, methods::HOST_FS_READ,
                methods::HOST_PANES_SPAWN, methods::HOST_EVENTS_SUBSCRIBE,
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

    /// Answer one module→host message. `host.fs.list` gets the real directory, everything
    /// else gets `{}` (or a pane id); notifications are only recorded.
    fn handle(&mut self, line: &str) -> Option<Value> {
        let v: Value = serde_json::from_str(line).expect("json line");
        let Some(method) = v.get("method").and_then(Value::as_str).map(str::to_string) else {
            return Some(v);
        };
        let params = v.get("params").cloned().unwrap_or(Value::Null);
        if let Some(id) = v.get("id").cloned() {
            let answer = match method.as_str() {
                methods::HOST_FS_LIST => {
                    let path = params["path"].as_str().unwrap_or_default().to_string();
                    match fs_list(&self.ws.0, &path) {
                        Ok(ok) => json!({ "jsonrpc": "2.0", "id": id, "result": ok }),
                        Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": e }),
                    }
                }
                methods::HOST_PANES_SPAWN => {
                    json!({ "jsonrpc": "2.0", "id": id, "result": { "pane_id": "pane-1" } })
                }
                _ => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
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

    /// The rows of the next `host.rows.set` after whatever we just did.
    fn next_rows(&mut self) -> Vec<Value> {
        let n = self.count(methods::HOST_ROWS_SET) + 1;
        let params = self.wait_for(methods::HOST_ROWS_SET, n);
        params["rows"].as_array().cloned().unwrap_or_default()
    }

    /// Click a row: what the host sends when a human clicks one.
    fn activate(&mut self, row: &Value, gesture: &str) -> Value {
        self.ok(
            methods::MODULE_ROW_ACTIVATE,
            json!({
                "entry": "files",
                "row": row["id"],
                "data": row["data"],
                "gesture": gesture,
            }),
        )
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
        Capability::UiRail,
        Capability::UiPane,
        Capability::UiCommands,
        Capability::UiToast,
        Capability::EventsSubscribe,
        Capability::PanesSpawn,
    ]
}

fn labels(rows: &[Value]) -> Vec<String> {
    rows.iter()
        .map(|r| r["label"].as_str().unwrap_or_default().to_string())
        .collect()
}

fn row_by_label<'a>(rows: &'a [Value], label: &str) -> &'a Value {
    rows.iter()
        .find(|r| r["label"] == label)
        .unwrap_or_else(|| panic!("no row {label:?} in {}", labels(rows).join(", ")))
}

// ---------------------------------------------------------------- the tests

#[test]
fn the_handshake_registers_the_rail_the_commands_and_the_events() {
    let mut host = Host::spawn("register");
    host.wait_for(methods::HOST_ROWS_SET, 1);

    let rail = host.last(methods::HOST_RAIL_REGISTER);
    let entries = rail["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["id"], "files");
    assert_eq!(entries[0]["label"], "Files");
    assert_eq!(entries[0]["tier"], 1);

    let commands = host.last(methods::HOST_COMMAND_REGISTER);
    let ids: Vec<&str> = commands["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["up", "refresh", "reveal", "filter", "set-root"]);

    let subscribed = host.last(methods::HOST_EVENTS_SUBSCRIBE);
    let kinds: Vec<&str> = subscribed["kinds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k.as_str().unwrap())
        .collect();
    assert!(kinds.contains(&methods::events::RAIL_QUERY));
    assert!(kinds.contains(&methods::events::FILES_REVEAL));

    host.shutdown();
}

#[test]
fn the_first_paint_lists_the_workspace_root() {
    let mut host = Host::spawn("first-paint");
    let params = host.wait_for(methods::HOST_ROWS_SET, 1);
    assert_eq!(params["entry"], "files");
    let rows = params["rows"].as_array().unwrap().clone();

    assert_eq!(rows[0]["id"], "up", "the way out comes first");
    assert_eq!(rows[1]["id"], "root");
    // Directories before files, each sorted, exactly as the old built-in browser drew it.
    assert_eq!(
        labels(&rows)[2..],
        ["docs", "src", "README.md"],
        "a collapsed root shows only its own children"
    );
    let readme = row_by_label(&rows, "README.md");
    assert_eq!(readme["id"], host.ws.join("README.md"));
    assert_eq!(readme["data"]["kind"], "file");
    assert!(!readme["expandable"].as_bool().unwrap());

    host.shutdown();
}

#[test]
fn a_toggle_opens_a_directory_and_a_second_one_shuts_it() {
    let mut host = Host::spawn("toggle");
    let rows = host.wait_for(methods::HOST_ROWS_SET, 1)["rows"]
        .as_array()
        .unwrap()
        .clone();
    let src = row_by_label(&rows, "src").clone();

    host.activate(&src, "toggle");
    let opened = host.next_rows();
    assert_eq!(
        labels(&opened)[2..],
        ["docs", "src", "util", "main.rs", "README.md"]
    );
    assert_eq!(
        row_by_label(&opened, "util")["depth"],
        2,
        "a child sits one level under its directory"
    );
    assert!(row_by_label(&opened, "src")["expanded"].as_bool().unwrap());

    host.activate(&src, "toggle");
    let shut = host.next_rows();
    assert_eq!(labels(&shut)[2..], ["docs", "src", "README.md"]);

    host.shutdown();
}

#[test]
fn opening_a_directory_makes_it_the_root_and_up_comes_back() {
    let mut host = Host::spawn("descend");
    let rows = host.wait_for(methods::HOST_ROWS_SET, 1)["rows"]
        .as_array()
        .unwrap()
        .clone();
    let src = row_by_label(&rows, "src").clone();

    host.activate(&src, "open");
    let inside = host.next_rows();
    assert_eq!(inside[1]["label"], "src", "src is the root now");
    assert_eq!(inside[1]["detail"], host.ws.join("src"));
    assert_eq!(labels(&inside)[2..], ["util", "main.rs"]);

    // And back out: the directory we came from stays open.
    host.ok(
        methods::MODULE_COMMAND_INVOKE,
        json!({ "id": "up", "args": {} }),
    );
    let out = host.next_rows();
    let name = host.ws.0.file_name().unwrap().to_string_lossy().to_string();
    assert_eq!(out[1]["label"], name);
    assert!(row_by_label(&out, "src")["expanded"].as_bool().unwrap());

    host.shutdown();
}

#[test]
fn clicking_a_file_asks_the_host_for_a_pane_and_marks_the_row() {
    let mut host = Host::spawn("open-file");
    let rows = host.wait_for(methods::HOST_ROWS_SET, 1)["rows"]
        .as_array()
        .unwrap()
        .clone();
    let readme = row_by_label(&rows, "README.md").clone();

    host.activate(&readme, "open");
    host.wait_for(methods::HOST_PANES_SPAWN, 1);
    let spawn = host.last(methods::HOST_PANES_SPAWN);
    assert_eq!(spawn["kind"], "file");
    assert_eq!(spawn["path"], host.ws.join("README.md"));

    let after = host.next_rows();
    assert!(
        row_by_label(&after, "README.md")["marks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m == "selected"),
        "the row you opened is the row the host scrolls to"
    );

    host.shutdown();
}

#[test]
fn the_context_gesture_is_answered_at_once_and_opens_nothing() {
    let mut host = Host::spawn("context");
    let rows = host.wait_for(methods::HOST_ROWS_SET, 1)["rows"]
        .as_array()
        .unwrap()
        .clone();
    let readme = row_by_label(&rows, "README.md").clone();

    host.activate(&readme, "context");
    // Round-trip something else to prove no pane request is in flight behind it.
    host.ok(
        methods::MODULE_COMMAND_INVOKE,
        json!({ "id": "refresh", "args": {} }),
    );
    host.next_rows();
    assert_eq!(
        host.count(methods::HOST_PANES_SPAWN),
        0,
        "the menu is the host's; the module opens nothing"
    );

    host.shutdown();
}

#[test]
fn the_filter_event_replaces_the_tree_with_ranked_matches() {
    let mut host = Host::spawn("filter");
    host.wait_for(methods::HOST_ROWS_SET, 1);

    host.notify(
        methods::MODULE_EVENT,
        json!({
            "kind": methods::events::RAIL_QUERY,
            "payload": { "entry": "files", "query": "help" },
        }),
    );
    let found = host.next_rows();
    assert_eq!(
        labels(&found)[2..],
        ["helper.rs"],
        "the finder walks the whole tree, not just the open rows"
    );
    assert_eq!(
        row_by_label(&found, "helper.rs")["data"]["path"],
        host.ws.join("src/util/helper.rs")
    );

    // A query aimed at another entry is not ours to answer.
    host.notify(
        methods::MODULE_EVENT,
        json!({
            "kind": methods::events::RAIL_QUERY,
            "payload": { "entry": "git", "query": "README" },
        }),
    );
    let unchanged = host.next_rows();
    assert_eq!(labels(&unchanged)[2..], ["helper.rs"]);

    host.notify(
        methods::MODULE_EVENT,
        json!({
            "kind": methods::events::RAIL_QUERY,
            "payload": { "entry": "files", "query": "" },
        }),
    );
    let restored = host.next_rows();
    assert_eq!(labels(&restored)[2..], ["docs", "src", "README.md"]);

    host.shutdown();
}

#[test]
fn a_reveal_event_expands_the_way_down_and_selects_the_row() {
    let mut host = Host::spawn("reveal");
    host.wait_for(methods::HOST_ROWS_SET, 1);
    let target = host.ws.join("src/util/helper.rs");

    host.notify(
        methods::MODULE_EVENT,
        json!({
            "kind": methods::events::FILES_REVEAL,
            "payload": { "path": target, "line": 12, "col": 3 },
        }),
    );
    let rows = host.next_rows();
    assert_eq!(
        labels(&rows)[2..],
        ["docs", "src", "util", "helper.rs", "main.rs", "README.md"],
        "exactly the directories on the way down are open, and nothing else"
    );
    let hit = row_by_label(&rows, "helper.rs");
    assert_eq!(hit["id"], target);
    assert!(hit["marks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m == "selected"));

    host.shutdown();
}

#[test]
fn the_reveal_command_answers_with_what_it_did() {
    let mut host = Host::spawn("reveal-cmd");
    host.wait_for(methods::HOST_ROWS_SET, 1);
    let target = host.ws.join("docs/guide.md");

    let result = host.ok(
        methods::MODULE_COMMAND_INVOKE,
        json!({ "id": "reveal", "args": { "path": target, "line": 4 } }),
    );
    assert_eq!(result["path"], target);
    assert_eq!(result["line"], 4);
    assert_eq!(result["revealed"], true);

    let rows = host.next_rows();
    assert!(row_by_label(&rows, "guide.md")["marks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m == "selected"));

    host.shutdown();
}

#[test]
fn a_command_the_module_does_not_have_is_a_parameter_error() {
    let mut host = Host::spawn("bad-command");
    host.wait_for(methods::HOST_ROWS_SET, 1);
    let e = host.err(
        methods::MODULE_COMMAND_INVOKE,
        json!({ "id": "teleport", "args": {} }),
    );
    assert_eq!(e["code"], -32602);
    assert!(
        e["message"].as_str().unwrap().contains("teleport"),
        "the error names the command: {e}"
    );
    host.shutdown();
}

#[test]
fn a_directory_outside_the_workspace_is_a_row_saying_why_not_a_crash() {
    let mut host = Host::spawn("outside");
    let rows = host.wait_for(methods::HOST_ROWS_SET, 1)["rows"]
        .as_array()
        .unwrap()
        .clone();
    let up = rows[0].clone();
    assert_eq!(up["id"], "up");

    host.activate(&up, "open");
    let above = host.next_rows();
    // The host refuses to list outside the workspace, so the module says so in a row and
    // keeps running; the `..` row back down is still there.
    assert_eq!(above.len(), 3, "up row, root row, and one note: {above:?}");
    assert!(above[2]["data"].is_null(), "a note row cannot be clicked");
    assert!(!above[2]["label"].as_str().unwrap().is_empty());

    host.shutdown();
}

#[test]
fn without_the_rail_capability_the_module_still_runs_and_says_nothing() {
    let mut host = Host::spawn_with("no-rail", vec![Capability::FsRead]);
    // No rail, no commands, no subscription: the loop is still alive and answers.
    host.ok(
        methods::MODULE_COMMAND_INVOKE,
        json!({ "id": "refresh", "args": {} }),
    );
    assert_eq!(host.count(methods::HOST_RAIL_REGISTER), 0);
    assert_eq!(host.count(methods::HOST_ROWS_SET), 0);
    host.shutdown();
}

#[test]
fn the_manifest_flag_prints_a_manifest_the_sdk_can_parse() {
    let out = Command::new(env!("CARGO_BIN_EXE_avada-files"))
        .arg("--manifest")
        .output()
        .expect("run --manifest");
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    let manifest = Manifest::parse(&text).expect("the printed manifest parses");
    assert_eq!(manifest.module.id.to_string(), "bshuler/avada-files");
    assert!(manifest.capabilities.contains(&Capability::FsRead));
}
