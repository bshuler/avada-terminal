//! The real `avada-hyperpane` binary against a fake host.
//!
//! The host side is what `avada_core` does: a socketpair whose child end is handed to the
//! module in `AVADA_MODULE_FD`, `module.hello` / `host.hello` as the first two lines, then
//! JSON-RPC both ways. `host.panes.spawn` answers with a fresh pane id exactly as
//! `avada_core::module::rpc` does, so a test that passes here is a test that would pass
//! against the host. Nothing here touches the network or the user's disk outside
//! `std::env::temp_dir()`, and no real terminal is ever started: the point of proof is the
//! conversation, not the shell at the far end of it.
#![cfg(unix)]
#![allow(unsafe_code)] // one fcntl, to let the module inherit its socket

use std::io::{BufRead, BufReader, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use avada_module_sdk::contract::{methods, ModuleHello};
use avada_module_sdk::Capability;
use serde_json::{json, Value};

const STEP: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------- the data dir

struct TempDir(PathBuf);

impl TempDir {
    /// The module's private data directory, canonicalised: on macOS `std::env::temp_dir()`
    /// is a symlink and the host hands out real paths, so the test must too or the paths
    /// the module echoes back would not be the ones we sent.
    fn data(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "avada-hyperpane-e2e-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir.canonicalize().unwrap())
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

// ---------------------------------------------------------------- the fake host

struct Host {
    child: Child,
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
    /// Every `host.*` request or notification the module sent, in order.
    host_calls: Vec<(String, Value)>,
    /// Pane ids handed out so far, so a restart can be told from a re-open.
    panes: usize,
    dir: TempDir,
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
        let dir = TempDir::data(tag);
        let child = Command::new(env!("CARGO_BIN_EXE_avada-hyperpane"))
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
            panes: 0,
            dir,
        };
        let first = host.read_line();
        let hello: ModuleHello = serde_json::from_str(&first).expect("module.hello");
        assert_eq!(
            hello.manifest.module.id.to_string(),
            "bshuler/avada-hyperpane"
        );
        assert!(hello.contract_min <= 1 && 1 <= hello.contract_max);
        for m in methods::MODULE_REQUIRED_V1 {
            assert!(
                hello.methods.iter().any(|x| x == m),
                "module.hello does not list {m}"
            );
        }
        let root = host.dir.root();
        host.write_line(&json!({
            "type": "host.hello",
            "contract_version": 1,
            "host_version": "0.0.0-test",
            "product": "Avada Terminal",
            "granted": granted,
            "methods": [
                methods::HOST_RAIL_REGISTER, methods::HOST_ROWS_SET,
                methods::HOST_COMMAND_REGISTER, methods::HOST_TOAST,
                methods::HOST_PANES_SPAWN, methods::HOST_PANES_INPUT,
                methods::HOST_EVENTS_SUBSCRIBE,
            ],
            "data_dir": root,
            // A workspace is open, and the module must still start its shell in its own
            // data dir: the agent tab is the session that outlives the project.
            "workspace": { "id": "ws1", "name": "Test workspace", "root": "/somewhere/else" },
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

    /// Answer one module→host message. `host.panes.spawn` mints a fresh id the way the
    /// host mints a uuid; everything else gets `{}`. Notifications are only recorded.
    fn handle(&mut self, line: &str) -> Option<Value> {
        let v: Value = serde_json::from_str(line).expect("json line");
        let Some(method) = v.get("method").and_then(Value::as_str).map(str::to_string) else {
            return Some(v);
        };
        let params = v.get("params").cloned().unwrap_or(Value::Null);
        if let Some(id) = v.get("id").cloned() {
            let answer = match method.as_str() {
                methods::HOST_PANES_SPAWN => {
                    self.panes += 1;
                    let pane = format!("pane-{}", self.panes);
                    json!({ "jsonrpc": "2.0", "id": id, "result": { "pane_id": pane } })
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

    /// Invoke a command from the palette, the way the host does.
    fn command(&mut self, id: &str, args: Value) -> Value {
        self.ok(
            methods::MODULE_COMMAND_INVOKE,
            json!({ "id": id, "args": args }),
        )
    }

    fn command_err(&mut self, id: &str, args: Value) -> Value {
        self.err(
            methods::MODULE_COMMAND_INVOKE,
            json!({ "id": id, "args": args }),
        )
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
                "entry": "hyperpane",
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
        Capability::UiRail,
        Capability::UiPane,
        Capability::UiCommands,
        Capability::UiToast,
        Capability::EventsSubscribe,
        Capability::PanesSpawn,
        Capability::PanesInput,
        Capability::SkillsMaterialize,
    ]
}

fn row_by_id<'a>(rows: &'a [Value], id: &str) -> &'a Value {
    rows.iter()
        .find(|r| r["id"] == id)
        .unwrap_or_else(|| panic!("no row {id:?} in {rows:?}"))
}

// ---------------------------------------------------------------- the tests

#[test]
fn the_handshake_registers_the_rail_the_commands_and_the_events() {
    let mut host = Host::spawn("register");
    host.wait_for(methods::HOST_ROWS_SET, 1);

    let rail = host.last(methods::HOST_RAIL_REGISTER);
    let entries = rail["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["id"], "hyperpane");
    assert_eq!(entries[0]["label"], "Hyperpane");
    assert_eq!(entries[0]["tier"], 1);

    let commands = host.last(methods::HOST_COMMAND_REGISTER);
    let ids: Vec<&str> = commands["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["open", "restart", "send", "reveal"]);

    let subscribed = host.last(methods::HOST_EVENTS_SUBSCRIBE);
    let kinds: Vec<&str> = subscribed["kinds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k.as_str().unwrap())
        .collect();
    assert!(kinds.contains(&methods::events::RAIL_QUERY));

    host.shutdown();
}

#[test]
fn the_first_paint_shows_a_cold_session_and_the_data_dir_not_the_workspace() {
    let mut host = Host::spawn("first-paint");
    let params = host.wait_for(methods::HOST_ROWS_SET, 1);
    assert_eq!(params["entry"], "hyperpane");
    let rows = params["rows"].as_array().unwrap().clone();

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["id"], "session");
    assert_eq!(rows[0]["detail"], "not started");
    assert_eq!(
        rows[1]["detail"],
        host.dir.root(),
        "the shell lives in the module's own data dir, not in the open workspace"
    );
    assert_eq!(host.count(methods::HOST_PANES_SPAWN), 0, "nothing yet");

    host.shutdown();
}

#[test]
fn open_spawns_one_shell_in_the_data_dir_and_a_second_open_does_not() {
    let mut host = Host::spawn("open");
    host.wait_for(methods::HOST_ROWS_SET, 1);

    let result = host.command("open", json!({}));
    assert_eq!(result["spawned"], true);
    let spawn = host.wait_for(methods::HOST_PANES_SPAWN, 1);
    assert_eq!(spawn["kind"], "shell");
    assert_eq!(spawn["path"], host.dir.root());

    let rows = host.next_rows();
    let session = row_by_id(&rows, "session");
    assert_eq!(session["detail"], "running");
    assert_eq!(session["marks"], json!(["modified"]));

    let again = host.command("open", json!({}));
    assert_eq!(again["spawned"], false);
    assert_eq!(
        again["pane_id"], "pane-1",
        "the second open reports the pane it already has"
    );
    host.wait_for(methods::HOST_TOAST, 1);
    assert_eq!(
        host.count(methods::HOST_PANES_SPAWN),
        1,
        "a second terminal is never what was wanted"
    );

    host.shutdown();
}

#[test]
fn send_types_into_the_pane_and_refuses_when_there_is_none() {
    let mut host = Host::spawn("send");
    host.wait_for(methods::HOST_ROWS_SET, 1);

    let e = host.command_err("send", json!({ "text": "ls\r" }));
    assert_eq!(e["code"], -32600, "invalid request: there is no pane yet");
    assert_eq!(host.count(methods::HOST_PANES_INPUT), 0);

    host.command("open", json!({}));
    host.wait_for(methods::HOST_PANES_SPAWN, 1);

    let result = host.command("send", json!({ "text": "avada ctl status\r" }));
    assert_eq!(result["pane_id"], "pane-1");
    let input = host.wait_for(methods::HOST_PANES_INPUT, 1);
    assert_eq!(input["pane_id"], "pane-1");
    assert_eq!(
        input["text"], "avada ctl status\r",
        "the bytes reach the shell exactly as they were given"
    );

    // No text at all is the module's own error, and nothing is typed.
    let e = host.command_err("send", json!({}));
    assert_eq!(e["code"], -32602);
    assert_eq!(host.count(methods::HOST_PANES_INPUT), 1);

    host.shutdown();
}

#[test]
fn restart_spawns_a_fresh_shell_and_later_input_goes_to_the_new_one() {
    let mut host = Host::spawn("restart");
    host.wait_for(methods::HOST_ROWS_SET, 1);
    host.command("open", json!({}));
    host.wait_for(methods::HOST_PANES_SPAWN, 1);

    host.command("restart", json!({}));
    host.wait_for(methods::HOST_PANES_SPAWN, 2);
    assert_eq!(host.count(methods::HOST_PANES_SPAWN), 2);

    host.command("send", json!({ "text": "x" }));
    let input = host.wait_for(methods::HOST_PANES_INPUT, 1);
    assert_eq!(
        input["pane_id"], "pane-2",
        "a restart moves the module's attention to the pane it just opened"
    );

    host.shutdown();
}

#[test]
fn a_row_click_opens_an_alt_click_restarts_and_the_directory_row_reveals() {
    let mut host = Host::spawn("rows");
    let rows = host.wait_for(methods::HOST_ROWS_SET, 1)["rows"]
        .as_array()
        .unwrap()
        .clone();
    let session = row_by_id(&rows, "session").clone();
    let directory = row_by_id(&rows, "directory").clone();

    host.activate(&session, "open");
    let first = host.wait_for(methods::HOST_PANES_SPAWN, 1);
    assert_eq!(first["kind"], "shell");

    host.activate(&session, "alt");
    let second = host.wait_for(methods::HOST_PANES_SPAWN, 2);
    assert_eq!(second["kind"], "shell");

    host.activate(&directory, "open");
    let third = host.wait_for(methods::HOST_PANES_SPAWN, 3);
    assert_eq!(third["kind"], "file", "reveal is a file pane, not a shell");
    assert_eq!(third["path"], host.dir.root());

    // Revealing did not disturb which pane the module types into.
    host.command("send", json!({ "text": "x" }));
    let input = host.wait_for(methods::HOST_PANES_INPUT, 1);
    assert_eq!(input["pane_id"], "pane-2");

    host.shutdown();
}

#[test]
fn the_rail_query_narrows_the_rows_and_deactivate_drops_the_pane() {
    let mut host = Host::spawn("query");
    host.wait_for(methods::HOST_ROWS_SET, 1);

    host.notify(
        methods::MODULE_EVENT,
        json!({ "kind": methods::events::RAIL_QUERY, "payload": { "query": "no such thing" } }),
    );
    assert!(host.next_rows().is_empty(), "the shared filter box applies");

    host.notify(
        methods::MODULE_EVENT,
        json!({ "kind": methods::events::RAIL_QUERY, "payload": { "query": "" } }),
    );
    assert_eq!(host.next_rows().len(), 2);

    host.command("open", json!({}));
    host.wait_for(methods::HOST_PANES_SPAWN, 1);
    host.ok(methods::MODULE_DEACTIVATE, json!({}));

    // The pane may well outlive us; our claim on it does not. Typing into a stale id is
    // exactly the bug this refusal prevents.
    let e = host.command_err("send", json!({ "text": "x" }));
    assert_eq!(e["code"], -32600);
    assert_eq!(host.count(methods::HOST_PANES_INPUT), 0);

    host.shutdown();
}

#[test]
fn spawning_without_panes_input_still_opens_the_shell_but_types_nothing() {
    let denied = vec![
        Capability::UiRail,
        Capability::UiCommands,
        Capability::UiToast,
        Capability::EventsSubscribe,
        Capability::PanesSpawn,
    ];
    let mut host = Host::spawn_with("denied", denied);
    host.wait_for(methods::HOST_ROWS_SET, 1);

    host.command("open", json!({}));
    host.wait_for(methods::HOST_PANES_SPAWN, 1);

    // The module answers the request — it did decide to type — and then discovers it may
    // not. The human is told; the socket is not used for a call the host would refuse.
    let result = host.command("send", json!({ "text": "x" }));
    assert_eq!(result["pane_id"], "pane-1");
    host.wait_for(methods::HOST_TOAST, 1);
    assert_eq!(host.count(methods::HOST_PANES_INPUT), 0);

    host.shutdown();
}

#[test]
fn the_manifest_flag_prints_the_embedded_toml() {
    let out = Command::new(env!("CARGO_BIN_EXE_avada-hyperpane"))
        .arg("--manifest")
        .output()
        .expect("run --manifest");
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    let m = avada_module_sdk::manifest::Manifest::parse(&text).expect("valid manifest");
    assert_eq!(m.module.id.to_string(), "bshuler/avada-hyperpane");
    assert!(m.capabilities.contains(&Capability::PanesInput));
    assert!(m.capabilities.contains(&Capability::SkillsMaterialize));
    assert_eq!(m.skills.paths, ["skills"]);
}
