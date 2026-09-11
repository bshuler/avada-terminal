//! The real `avada-workspace` binary against a fake host and a real directory tree.
//!
//! The host side is what `avada_core` does: a socketpair whose child end is handed to the
//! module in `AVADA_MODULE_FD`, `module.hello` / `host.hello` as the first two lines, then
//! JSON-RPC both ways. `host.fs.list`, `host.fs.read` and `host.fs.write` are answered from
//! an actual temp directory, scoped to the workspace root exactly as
//! `avada_core::module::rpc` scopes them, so a test that passes here is a test that would
//! pass against the host. Nothing here touches the network or the user's disk outside
//! `std::env::temp_dir()`.
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

/// The project file: one window, two tabs, four panes, one of which carries a command that
/// `host.panes.spawn` has no way to convey.
const PROJECT: &str = r#"{
  "format": "avada",
  "version": 1,
  "workspace": {
    "name": "repo",
    "groups": [
      {
        "title": "build",
        "panes": [
          { "label": "editor", "cwd": "src" },
          { "label": "server", "command": "cargo", "args": ["run"], "cwd": "." }
        ]
      },
      {
        "title": "logs",
        "panes": [
          { "label": "tail" },
          { "label": "docs", "cwd": "/tmp" }
        ]
      }
    ]
  }
}"#;

/// A saved workspace in the library, in the `panes` shorthand.
const SAVED: &str = r#"{
  "format": "avada",
  "version": 1,
  "workspace": {
    "name": "scratch",
    "panes": [
      { "label": "one" }
    ]
  }
}"#;

/// A set naming the saved workspace beside it.
const SET: &str = r#"{
  "format": "avada-set",
  "version": 1,
  "set": {
    "name": "morning",
    "members": [
      { "path": "saved.avada.json" }
    ]
  }
}"#;

struct TempDir(PathBuf);

impl TempDir {
    /// A workspace holding a project file, a library file, a set, a file that claims to be
    /// ours and is not parseable, and a `package.json` that must be ignored entirely.
    /// Canonicalised: on macOS `std::env::temp_dir()` is a symlink and the host answers
    /// with real paths, so the test must too or every row id would disagree with every
    /// path it sent.
    fn workspace(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "avada-workspace-e2e-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join(".avada")).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join(".avada/project.json"), PROJECT).unwrap();
        std::fs::write(dir.join("saved.avada.json"), SAVED).unwrap();
        std::fs::write(dir.join("morning.set.json"), SET).unwrap();
        std::fs::write(dir.join("torn.avada.json"), "{ not json").unwrap();
        std::fs::write(dir.join("package.json"), r#"{"name":"x","version":"1"}"#).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
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

fn denied(path: &str) -> Value {
    json!({
        "code": -32001,
        "message": format!("`{path}` is outside the workspace root"),
    })
}

fn bad_params(path: &str, e: std::io::Error) -> Value {
    json!({ "code": -32602, "message": format!("`{path}`: {e}") })
}

/// `host.fs.list`, scoped the way the host scopes it: a path that does not resolve inside
/// the workspace root is `CapabilityDenied`, not an empty listing.
fn fs_list(root: &Path, path: &str) -> Result<Value, Value> {
    let real = PathBuf::from(path)
        .canonicalize()
        .map_err(|e| bad_params(path, e))?;
    if !real.starts_with(root) {
        return Err(denied(path));
    }
    let rd = std::fs::read_dir(&real).map_err(|e| bad_params(path, e))?;
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

/// `host.fs.read`. A missing file is an error, which is how the module tells `.avada` from
/// `.hyperpanes` without a directory listing.
fn fs_read(root: &Path, path: &str) -> Result<Value, Value> {
    let real = PathBuf::from(path)
        .canonicalize()
        .map_err(|e| bad_params(path, e))?;
    if !real.starts_with(root) {
        return Err(denied(path));
    }
    let text = std::fs::read_to_string(&real).map_err(|e| bad_params(path, e))?;
    Ok(json!({ "text": text, "truncated": false }))
}

/// `host.fs.write`, scoped through the deepest existing ancestor exactly as the host's
/// `scoped_write` does.
fn fs_write(root: &Path, path: &str, text: &str) -> Result<Value, Value> {
    let target = PathBuf::from(path);
    let mut existing = target.as_path();
    while !existing.exists() {
        match existing.parent() {
            Some(p) => existing = p,
            None => return Err(denied(path)),
        }
    }
    let real = existing.canonicalize().map_err(|e| bad_params(path, e))?;
    if !real.starts_with(root) {
        return Err(denied(path));
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| bad_params(path, e))?;
    }
    std::fs::write(&target, text).map_err(|e| bad_params(path, e))?;
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
        let child = Command::new(env!("CARGO_BIN_EXE_avada-workspace"))
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
        assert_eq!(
            hello.manifest.module.id.to_string(),
            "bshuler/avada-workspace"
        );
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
                methods::HOST_FS_LIST, methods::HOST_FS_READ, methods::HOST_FS_WRITE,
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

    /// Answer one module→host message from the real temp directory; notifications are only
    /// recorded.
    fn handle(&mut self, line: &str) -> Option<Value> {
        let v: Value = serde_json::from_str(line).expect("json line");
        let Some(method) = v.get("method").and_then(Value::as_str).map(str::to_string) else {
            return Some(v);
        };
        let params = v.get("params").cloned().unwrap_or(Value::Null);
        if let Some(id) = v.get("id").cloned() {
            let path = params["path"].as_str().unwrap_or_default().to_string();
            let answered = match method.as_str() {
                methods::HOST_FS_LIST => Some(fs_list(&self.ws.0, &path)),
                methods::HOST_FS_READ => Some(fs_read(&self.ws.0, &path)),
                methods::HOST_FS_WRITE => Some(fs_write(
                    &self.ws.0,
                    &path,
                    params["text"].as_str().unwrap_or_default(),
                )),
                _ => None,
            };
            let answer = match answered {
                Some(Ok(ok)) => json!({ "jsonrpc": "2.0", "id": id, "result": ok }),
                Some(Err(e)) => json!({ "jsonrpc": "2.0", "id": id, "error": e }),
                None if method == methods::HOST_PANES_SPAWN => {
                    json!({ "jsonrpc": "2.0", "id": id, "result": { "pane_id": "pane-1" } })
                }
                None => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
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

    /// Every `host.panes.spawn` the module has sent, in order.
    fn spawns(&self) -> Vec<Value> {
        self.host_calls
            .iter()
            .filter(|(m, _)| m == methods::HOST_PANES_SPAWN)
            .map(|(_, p)| p.clone())
            .collect()
    }

    /// The rows of the next `host.rows.set` after whatever we just did.
    fn next_rows(&mut self) -> Vec<Value> {
        let n = self.count(methods::HOST_ROWS_SET) + 1;
        let params = self.wait_for(methods::HOST_ROWS_SET, n);
        params["rows"].as_array().cloned().unwrap_or_default()
    }

    fn first_rows(&mut self) -> Vec<Value> {
        self.wait_for(methods::HOST_ROWS_SET, 1)["rows"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    /// Click a row: what the host sends when a human clicks one.
    fn activate(&mut self, row: &Value, gesture: &str) -> Value {
        self.ok(
            methods::MODULE_ROW_ACTIVATE,
            json!({
                "entry": "workspace",
                "row": row["id"],
                "data": row["data"],
                "gesture": gesture,
            }),
        )
    }

    /// Click a row named by label, from the rows currently on screen.
    fn click(&mut self, rows: &[Value], label: &str, gesture: &str) -> Vec<Value> {
        let row = row_by_label(rows, label).clone();
        self.activate(&row, gesture);
        self.next_rows()
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

fn has_mark(row: &Value, mark: &str) -> bool {
    row["marks"]
        .as_array()
        .is_some_and(|m| m.iter().any(|x| x == mark))
}

// ---------------------------------------------------------------- the tests

#[test]
fn the_handshake_registers_the_rail_the_commands_and_the_events() {
    let mut host = Host::spawn("register");
    host.wait_for(methods::HOST_ROWS_SET, 1);

    let rail = host.last(methods::HOST_RAIL_REGISTER);
    let entries = rail["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["id"], "workspace");
    assert_eq!(entries[0]["label"], "Workspace");
    assert_eq!(entries[0]["tier"], 1);

    let commands = host.last(methods::HOST_COMMAND_REGISTER);
    let ids: Vec<&str> = commands["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["refresh", "filter", "reveal", "open-group", "note"]);

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
fn the_first_paint_sorts_the_workspace_into_its_three_sections() {
    let mut host = Host::spawn("first-paint");
    let params = host.wait_for(methods::HOST_ROWS_SET, 1);
    assert_eq!(params["entry"], "workspace");
    let rows = params["rows"].as_array().unwrap().clone();
    let seen = labels(&rows);

    // The three headers, in order, each already open.
    let heads: Vec<&String> = seen
        .iter()
        .filter(|l| ["PROJECT", "LIBRARY", "SETS"].contains(&l.as_str()))
        .collect();
    assert_eq!(heads, ["PROJECT", "LIBRARY", "SETS"]);

    assert!(
        seen.contains(&"repo".to_string()),
        "the project file: {seen:?}"
    );
    assert!(seen.contains(&"scratch".to_string()), "the library file");
    assert!(seen.contains(&"morning".to_string()), "the set");

    // `package.json` parses as an empty workspace and must be neither listed nor reported.
    assert!(!seen.iter().any(|l| l.contains("package")), "{seen:?}");
    // A file whose *name* claims to be ours and does not parse is reported, not swallowed.
    let torn = rows
        .iter()
        .find(|r| r["id"] == format!("broken:{}", host.ws.join("torn.avada.json")))
        .expect("the torn file is reported");
    assert!(has_mark(torn, "broken") && has_mark(torn, "note"));
    assert!(torn["data"].is_null(), "a note row cannot be clicked");

    host.shutdown();
}

#[test]
fn a_toggle_opens_a_workspace_down_to_its_panes_and_a_second_one_shuts_it() {
    let mut host = Host::spawn("toggle");
    let rows = host.first_rows();
    assert!(!labels(&rows).contains(&"build".to_string()), "starts shut");

    let opened = host.click(&rows, "repo", "toggle");
    let seen = labels(&opened);
    // One window, so the window level is drawn away: tabs sit directly under the file.
    assert!(seen.contains(&"build".to_string()) && seen.contains(&"logs".to_string()));
    assert_eq!(row_by_label(&opened, "build")["depth"], 2);
    assert!(row_by_label(&opened, "repo")["expanded"].as_bool().unwrap());

    let deeper = host.click(&opened, "build", "toggle");
    assert!(labels(&deeper).contains(&"editor".to_string()));
    assert_eq!(row_by_label(&deeper, "editor")["depth"], 3);
    assert_eq!(row_by_label(&deeper, "editor")["data"]["kind"], "pane");

    let shut = host.click(&deeper, "repo", "toggle");
    assert!(!labels(&shut).contains(&"build".to_string()));

    host.shutdown();
}

#[test]
fn opening_a_tab_asks_for_exactly_its_panes_and_says_what_it_could_not_restore() {
    let mut host = Host::spawn("open-tab");
    let rows = host.first_rows();
    let opened = host.click(&rows, "repo", "toggle");

    host.click(&opened, "build", "open");
    host.wait_for(methods::HOST_PANES_SPAWN, 2);
    let spawns = host.spawns();
    assert_eq!(spawns.len(), 2, "the `build` tab, and only it: {spawns:?}");
    // A relative saved cwd resolves against the directory of the file that saved it.
    assert_eq!(spawns[0]["path"], host.ws.join(".avada/src"));
    assert_eq!(spawns[0]["kind"], "terminal");

    // `host.panes.spawn` carries no command line, so the module says so rather than
    // silently opening a shell where a `cargo run` was saved.
    let toast = host.last(methods::HOST_TOAST);
    let text = toast["text"].as_str().unwrap();
    assert!(
        text.contains("1 saved command could not be restored"),
        "the toast names the loss: {text}"
    );

    host.shutdown();
}

#[test]
fn opening_a_single_pane_row_opens_that_pane_and_nothing_else() {
    let mut host = Host::spawn("open-pane");
    let rows = host.first_rows();
    let opened = host.click(&rows, "repo", "toggle");
    let tabs = host.click(&opened, "logs", "toggle");

    let after = host.click(&tabs, "tail", "open");
    host.wait_for(methods::HOST_PANES_SPAWN, 1);
    assert_eq!(host.count(methods::HOST_PANES_SPAWN), 1);
    assert!(
        has_mark(row_by_label(&after, "tail"), "selected"),
        "the row you opened is the row the host scrolls to"
    );

    host.shutdown();
}

#[test]
fn opening_a_set_opens_every_workspace_it_names() {
    let mut host = Host::spawn("open-set");
    let rows = host.first_rows();

    host.click(&rows, "morning", "open");
    host.wait_for(methods::HOST_PANES_SPAWN, 1);
    let spawns = host.spawns();
    assert_eq!(spawns.len(), 1, "`saved.avada.json` has one pane");

    host.shutdown();
}

#[test]
fn the_context_gesture_says_where_the_row_came_from_and_opens_nothing() {
    let mut host = Host::spawn("context");
    let rows = host.first_rows();
    let saved = row_by_label(&rows, "scratch").clone();

    host.activate(&saved, "context");
    host.wait_for(methods::HOST_TOAST, 1);
    assert_eq!(
        host.last(methods::HOST_TOAST)["text"],
        host.ws.join("saved.avada.json")
    );
    assert_eq!(
        host.count(methods::HOST_PANES_SPAWN),
        0,
        "the menu is the host's; the module opens nothing"
    );

    host.shutdown();
}

#[test]
fn the_filter_event_keeps_the_match_and_the_path_down_to_it() {
    let mut host = Host::spawn("filter");
    host.wait_for(methods::HOST_ROWS_SET, 1);

    host.notify(
        methods::MODULE_EVENT,
        json!({
            "kind": methods::events::RAIL_QUERY,
            "payload": { "entry": "workspace", "query": "scratch" },
        }),
    );
    let found = host.next_rows();
    let seen = labels(&found);
    assert!(seen.contains(&"scratch".to_string()));
    assert!(
        !seen.contains(&"repo".to_string()),
        "filtered out: {seen:?}"
    );
    assert!(
        seen.contains(&"PROJECT".to_string()),
        "the headers stay so the human can see which section matched"
    );

    // A query aimed at another entry is not ours to answer.
    host.notify(
        methods::MODULE_EVENT,
        json!({
            "kind": methods::events::RAIL_QUERY,
            "payload": { "entry": "git", "query": "repo" },
        }),
    );
    host.ok(
        methods::MODULE_COMMAND_INVOKE,
        json!({ "id": "refresh", "args": {} }),
    );
    let unchanged = host.next_rows();
    assert!(!labels(&unchanged).contains(&"repo".to_string()));

    host.notify(
        methods::MODULE_EVENT,
        json!({
            "kind": methods::events::RAIL_QUERY,
            "payload": { "entry": "workspace", "query": "" },
        }),
    );
    let restored = host.next_rows();
    assert!(labels(&restored).contains(&"repo".to_string()));

    host.shutdown();
}

#[test]
fn a_reveal_opens_the_way_down_and_selects_the_row() {
    let mut host = Host::spawn("reveal");
    host.wait_for(methods::HOST_ROWS_SET, 1);
    let target = host.ws.join("saved.avada.json");

    let result = host.ok(
        methods::MODULE_COMMAND_INVOKE,
        json!({ "id": "reveal", "args": { "path": target } }),
    );
    assert!(result.get("error").is_none(), "{result}");
    let rows = host.next_rows();
    assert!(has_mark(row_by_label(&rows, "scratch"), "selected"));

    // A reveal of somebody else's file is silence on the event path.
    host.notify(
        methods::MODULE_EVENT,
        json!({
            "kind": methods::events::FILES_REVEAL,
            "payload": { "path": host.ws.join("src/main.rs") },
        }),
    );
    // …but a complaint on the command path, where a human asked.
    host.ok(
        methods::MODULE_COMMAND_INVOKE,
        json!({ "id": "reveal", "args": { "path": host.ws.join("src/main.rs") } }),
    );
    host.wait_for(methods::HOST_TOAST, 1);
    let text = host.last(methods::HOST_TOAST);
    assert_eq!(text["level"], "error");
    assert!(text["text"].as_str().unwrap().contains("main.rs"));

    host.shutdown();
}

#[test]
fn a_note_is_written_back_through_the_host_and_leaves_the_rest_of_the_file_alone() {
    let mut host = Host::spawn("note");
    let rows = host.first_rows();
    let opened = host.click(&rows, "repo", "toggle");
    let tabs = host.click(&opened, "logs", "toggle");
    // Select the pane by opening it; `note` acts on the selection.
    host.click(&tabs, "tail", "open");

    host.ok(
        methods::MODULE_COMMAND_INVOKE,
        json!({ "id": "note", "args": { "text": "watch the deploy log" } }),
    );
    host.wait_for(methods::HOST_FS_WRITE, 1);
    let written = host.last(methods::HOST_FS_WRITE);
    assert_eq!(written["path"], host.ws.join(".avada/project.json"));

    let back = std::fs::read_to_string(host.ws.join(".avada/project.json")).unwrap();
    assert!(back.contains("watch the deploy log"));
    // Everything else survived the round trip, including the command the module cannot
    // itself restore into a pane.
    assert!(back.contains("\"cargo\"") && back.contains("\"repo\""));
    assert!(
        back.starts_with("{\n  \"format\": \"avada\","),
        "2-space pretty"
    );

    host.shutdown();
}

#[test]
fn a_command_the_module_does_not_have_says_so_without_failing_the_request() {
    let mut host = Host::spawn("bad-command");
    host.wait_for(methods::HOST_ROWS_SET, 1);
    // A palette typo is not a protocol error: the request succeeds and the human is told.
    host.ok(
        methods::MODULE_COMMAND_INVOKE,
        json!({ "id": "teleport", "args": {} }),
    );
    host.wait_for(methods::HOST_TOAST, 1);
    let toast = host.last(methods::HOST_TOAST);
    assert_eq!(toast["level"], "error");
    assert!(toast["text"].as_str().unwrap().contains("teleport"));

    // A *method* the module does not serve is a protocol error, because a host that sent
    // it has a bug the log should carry.
    let e = host.err("module.nonsense", json!({}));
    assert_eq!(e["code"], -32601);

    host.shutdown();
}

#[test]
fn activation_re_sweeps_and_repaints_rather_than_leaving_a_stale_rail() {
    let mut host = Host::spawn("activate");
    host.wait_for(methods::HOST_ROWS_SET, 1);
    // Something else wrote a workspace file while the entry was off screen.
    std::fs::write(
        host.ws.join("later.avada.json"),
        SAVED.replace("scratch", "later"),
    )
    .unwrap();

    let root = host.ws.root();
    host.ok(
        methods::MODULE_ACTIVATE,
        json!({ "entry": "workspace", "workspace": { "id": "ws1", "root": root } }),
    );
    let rows = host.next_rows();
    assert!(
        labels(&rows).contains(&"later".to_string()),
        "the moment the human looks is the moment the answer has to be current"
    );

    // Deactivation is answered and paints nothing.
    let before = host.count(methods::HOST_ROWS_SET);
    host.ok(methods::MODULE_DEACTIVATE, json!({ "entry": "workspace" }));
    assert_eq!(host.count(methods::HOST_ROWS_SET), before);

    host.shutdown();
}

#[test]
fn an_activate_with_no_workspace_empties_the_rail_instead_of_sweeping_the_cwd() {
    let mut host = Host::spawn("no-workspace");
    host.wait_for(methods::HOST_ROWS_SET, 1);
    let listings = host.count(methods::HOST_FS_LIST);

    host.ok(methods::MODULE_ACTIVATE, json!({ "entry": "workspace" }));
    let rows = host.next_rows();
    let seen = labels(&rows);
    assert!(!seen.contains(&"repo".to_string()), "{seen:?}");
    // Each section says what would fill it rather than going blank.
    assert!(rows
        .iter()
        .any(|r| r["id"] == "section:project:empty" && r["data"].is_null()));
    assert_eq!(
        host.count(methods::HOST_FS_LIST),
        listings,
        "with no root there is nothing to sweep, so not one directory was listed"
    );

    host.shutdown();
}

#[test]
fn a_stale_row_id_is_a_repaint_rather_than_a_wrong_pane() {
    let mut host = Host::spawn("stale");
    host.wait_for(methods::HOST_ROWS_SET, 1);
    host.ok(
        methods::MODULE_ROW_ACTIVATE,
        json!({
            "entry": "workspace",
            "row": "ws:/nowhere/gone.avada.json",
            "data": { "id": "ws:/nowhere/gone.avada.json", "kind": "workspace" },
            "gesture": "open",
        }),
    );
    host.next_rows();
    assert_eq!(
        host.count(methods::HOST_PANES_SPAWN),
        0,
        "guessing what a stale row meant is how a click opens the wrong pane"
    );
    host.shutdown();
}

#[test]
fn an_unknown_gesture_from_a_newer_host_is_read_as_a_plain_click() {
    let mut host = Host::spawn("gesture");
    let rows = host.first_rows();
    let saved = row_by_label(&rows, "scratch").clone();
    host.activate(&saved, "teleport");
    host.wait_for(methods::HOST_PANES_SPAWN, 1);
    host.shutdown();
}

#[test]
fn without_the_pane_capability_the_module_says_so_instead_of_failing_quietly() {
    let mut host = Host::spawn_with(
        "no-spawn",
        vec![
            Capability::FsRead,
            Capability::UiRail,
            Capability::UiToast,
            Capability::UiCommands,
        ],
    );
    let rows = host.first_rows();
    let saved = row_by_label(&rows, "scratch").clone();
    host.activate(&saved, "open");
    host.wait_for(methods::HOST_TOAST, 1);
    let toast = host.last(methods::HOST_TOAST);
    assert_eq!(toast["level"], "error");
    assert!(toast["text"].as_str().unwrap().contains("not permitted"));
    assert_eq!(host.count(methods::HOST_PANES_SPAWN), 0);
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
    let out = Command::new(env!("CARGO_BIN_EXE_avada-workspace"))
        .arg("--manifest")
        .output()
        .expect("run --manifest");
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    let manifest = Manifest::parse(&text).expect("the printed manifest parses");
    assert_eq!(manifest.module.id.to_string(), "bshuler/avada-workspace");
    assert!(manifest.capabilities.contains(&Capability::FsRead));
    assert!(manifest.capabilities.contains(&Capability::FsWrite));
}
