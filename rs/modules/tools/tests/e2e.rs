//! The real `avada-tools` binary against a fake host: a socketpair for the module
//! contract, and a loopback HTTP server for the control routes.
//!
//! This module is unusual among the tier-1 modules in that almost nothing it draws comes
//! down the pipe. The rail entries, the rows, the resume — all of it comes from
//! `GET /tools`, `GET /tools/{tool}/sessions`, `GET /settings` and `POST /command` on the
//! host's own control server, which the host names in `host.hello`. So the fake host here
//! is two fakes: the socketpair that `avada_core::module` would drive, and a small HTTP
//! responder standing in for `avada_core::control`. Both answer the way the real ones do,
//! including the 503 a headless host gives `GET /settings` and the 400 `POST /command`
//! gives a body with no `windowId`.
//!
//! Nothing here touches the network beyond 127.0.0.1, and nothing touches the user's disk.
#![cfg(unix)]
#![allow(unsafe_code)] // one fcntl, to let the module inherit its socket

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use avada_module_sdk::contract::{methods, ModuleHello};
use avada_module_sdk::Capability;
use serde_json::{json, Value};

const STEP: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------- the control server

/// Everything the fake control server will say, and everything it was asked.
#[derive(Debug)]
struct Sim {
    /// The `GET /tools` body, or the status to fail it with.
    tools: Result<Vec<Value>, u16>,
    /// `GET /tools/{tool}/sessions` per tool id. A missing tool is a 404.
    sessions: BTreeMap<String, Vec<Value>>,
    /// `toolFavorites`, or `None` for a host with no GUI (which answers 503).
    favourites: Option<Vec<String>>,
    /// Every request, as `(verb, path, body)`.
    seen: Vec<(String, String, Value)>,
    /// Whether `GET /state` has a window to offer.
    windows: bool,
}

impl Sim {
    /// The three readable tools and one unreadable one, the way `GET /tools` answers.
    fn ready() -> Self {
        Sim {
            tools: Ok(vec![
                tool("claude", "Claude Code", "#d97757", true),
                tool("cursor-agent", "Cursor", "#6b7280", true),
                tool("copilot", "Copilot", "#22c55e", true),
                tool("aider", "Aider", "#8b5cf6", false),
            ]),
            sessions: BTreeMap::new(),
            favourites: None,
            seen: vec![],
            windows: true,
        }
    }

    fn count(&self, verb: &str, path: &str) -> usize {
        self.seen
            .iter()
            .filter(|(v, p, _)| v == verb && p == path)
            .count()
    }

    fn last(&self, verb: &str, path: &str) -> Value {
        self.seen
            .iter()
            .rev()
            .find(|(v, p, _)| v == verb && p == path)
            .map(|(_, _, b)| b.clone())
            .unwrap_or_else(|| panic!("the module never sent {verb} {path}"))
    }
}

fn tool(id: &str, name: &str, brand: &str, has_history: bool) -> Value {
    json!({
        "id": id, "name": name, "brand": brand,
        "hasHistory": has_history, "path": format!("/usr/local/bin/{id}"),
        "source": "wellKnown",
    })
}

fn session(id: &str, project: &str, summary: &str) -> Value {
    json!({
        "id": id, "project": project, "projectExact": true,
        "branch": "main", "startedAt": 0, "summary": summary,
        "messageCount": 4,
        "resume": { "command": "claude", "args": ["--resume", id], "cwd": project },
    })
}

/// A control server on 127.0.0.1, answering the five routes this module uses.
struct Control {
    addr: SocketAddr,
    sim: Arc<Mutex<Sim>>,
}

impl Control {
    fn start(sim: Sim) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let sim = Arc::new(Mutex::new(sim));
        let shared = Arc::clone(&sim);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                serve(stream, &shared);
            }
        });
        Control { addr, sim }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn sim(&self) -> std::sync::MutexGuard<'_, Sim> {
        self.sim.lock().unwrap()
    }
}

/// One request/response, `Connection: close` the way the module sends it.
fn serve(mut stream: TcpStream, sim: &Arc<Mutex<Sim>>) {
    stream.set_read_timeout(Some(STEP)).ok();
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(0) | Err(_) => return,
            Ok(_) => head.push(byte[0]),
        }
    }
    let text = String::from_utf8_lossy(&head).into_owned();
    let mut lines = text.lines();
    let request = lines.next().unwrap_or_default().to_string();
    let mut parts = request.split_whitespace();
    let verb = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    // The host refuses an unauthenticated caller; assert the module always presents its
    // token, since a module that forgot would silently get 401s in production.
    assert!(
        text.contains("Authorization: Bearer test-token"),
        "the module sent no bearer token: {text}"
    );
    let len: usize = text
        .lines()
        .find_map(|l| l.strip_prefix("Content-Length: "))
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    if len > 0 && stream.read_exact(&mut body).is_err() {
        return;
    }
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);

    let mut sim = sim.lock().unwrap();
    sim.seen.push((verb.clone(), path.clone(), body.clone()));
    let (status, payload): (u16, Value) = match (verb.as_str(), path.as_str()) {
        ("GET", "/tools") => match &sim.tools {
            Ok(t) => (200, json!({ "tools": t })),
            Err(code) => (
                *code,
                json!({ "error": "the catalogue is not yours to read" }),
            ),
        },
        ("GET", "/settings") => match &sim.favourites {
            // A host with no GUI attached: exactly what `avada_core` answers.
            None => (
                503,
                json!({ "error": "settings unavailable (no GUI attached)" }),
            ),
            Some(f) => (200, json!({ "settings": { "toolFavorites": f } })),
        },
        ("GET", "/state") => {
            let windows = if sim.windows {
                json!([{ "windowId": 7, "panes": [] }])
            } else {
                json!([])
            };
            (
                200,
                json!({ "windows": windows, "speech": {}, "dictation": {} }),
            )
        }
        ("POST", "/command") => {
            if body.get("windowId").is_none() {
                (
                    400,
                    json!({ "error": "command needs a paneId or windowId" }),
                )
            } else {
                (200, json!({ "result": "pane-1" }))
            }
        }
        ("GET", p) if p.starts_with("/tools/") && p.ends_with("/sessions") => {
            let id = p
                .trim_start_matches("/tools/")
                .trim_end_matches("/sessions");
            match sim.sessions.get(id) {
                Some(s) => (200, json!({ "sessions": s })),
                None => (404, json!({ "error": format!("no tool `{id}`") })),
            }
        }
        _ => (404, json!({ "error": format!("no route {verb} {path}") })),
    };
    drop(sim);

    let text = payload.to_string();
    let resp = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{text}",
        text.len()
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

// ---------------------------------------------------------------- the fake host

struct Host {
    child: Child,
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
    /// Every `host.*` request or notification the module sent, in order.
    host_calls: Vec<(String, Value)>,
    control: Control,
}

impl Host {
    fn spawn(sim: Sim) -> Self {
        Self::spawn_with(sim, granted(), true)
    }

    /// `wired: false` withholds `control_url`, which is what a host with no control server
    /// running actually sends.
    fn spawn_with(sim: Sim, granted: Vec<Capability>, wired: bool) -> Self {
        let control = Control::start(sim);
        let (host_end, child_end) = UnixStream::pair().unwrap();
        // The host clears CLOEXEC on the child end so the descriptor survives exec.
        let fd = child_end.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(flags >= 0);
        assert!(unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } >= 0);
        let child = Command::new(env!("CARGO_BIN_EXE_avada-tools"))
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
            control,
        };
        let first = host.read_line();
        let hello: ModuleHello = serde_json::from_str(&first).expect("module.hello");
        assert_eq!(hello.manifest.module.id.to_string(), "bshuler/avada-tools");
        assert!(hello.contract_min <= 1 && 1 <= hello.contract_max);
        for m in methods::MODULE_REQUIRED_V1 {
            assert!(
                hello.methods.iter().any(|x| x == m),
                "module.hello does not list {m}"
            );
        }
        let mut greeting = json!({
            "type": "host.hello",
            "contract_version": 1,
            "host_version": "0.0.0-test",
            "product": "Avada Terminal",
            "granted": granted,
            "methods": [
                methods::HOST_RAIL_REGISTER, methods::HOST_ROWS_SET,
                methods::HOST_COMMAND_REGISTER, methods::HOST_TOAST,
                methods::HOST_EVENTS_SUBSCRIBE,
            ],
            "data_dir": std::env::temp_dir().display().to_string(),
            "token": "test-token",
        });
        if wired {
            greeting["control_url"] = json!(host.control.url());
        }
        host.write_line(&greeting);
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

    /// Answer one module→host message. Everything this module asks of the pipe is a
    /// one-way instruction, so `{}` is the whole answer; the interesting half is HTTP.
    fn handle(&mut self, line: &str) -> Option<Value> {
        let v: Value = serde_json::from_str(line).expect("json line");
        let Some(method) = v.get("method").and_then(Value::as_str).map(str::to_string) else {
            return Some(v);
        };
        let params = v.get("params").cloned().unwrap_or(Value::Null);
        if let Some(id) = v.get("id").cloned() {
            self.write_line(&json!({ "jsonrpc": "2.0", "id": id, "result": {} }));
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

    /// Serve the module until the control server has been asked `verb path` `n` times.
    fn wait_for_http(&mut self, verb: &str, path: &str, n: usize) {
        let deadline = Instant::now() + STEP;
        while Instant::now() < deadline {
            if self.control.sim().count(verb, path) >= n {
                return;
            }
            let _ = self.pump();
        }
        panic!("the module never sent {verb} {path} #{n}")
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

    /// The rows of the most recent `host.rows.set` for one entry.
    fn rows(&mut self, entry: &str) -> Vec<Value> {
        self.host_calls
            .iter()
            .rev()
            .find(|(m, p)| m == methods::HOST_ROWS_SET && p["entry"] == entry)
            .map(|(_, p)| p["rows"].as_array().cloned().unwrap_or_default())
            .unwrap_or_else(|| panic!("module never set rows for {entry:?}"))
    }

    /// Wait until every entry has had a fresh row push, so `rows()` is not last round's.
    fn settle(&mut self) {
        let want = self.count(methods::HOST_ROWS_SET) + 1;
        self.wait_for(methods::HOST_ROWS_SET, want);
        // Drain whatever else is already queued behind it.
        let deadline = Instant::now() + Duration::from_millis(400);
        self.reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        while Instant::now() < deadline {
            let mut line = String::new();
            match self.reader.read_line(&mut line) {
                Ok(n) if n > 0 => {
                    self.handle(&line);
                }
                _ => break,
            }
        }
        self.reader.get_ref().set_read_timeout(Some(STEP)).unwrap();
    }

    /// Click a row: what the host sends when a human clicks one.
    fn activate(&mut self, entry: &str, row: &Value, gesture: &str) -> Value {
        self.ok(
            methods::MODULE_ROW_ACTIVATE,
            json!({
                "entry": entry,
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
        Capability::SettingsRead,
        Capability::FsReadAny,
        Capability::WorkspaceRead,
        Capability::WorkspaceWrite,
        Capability::PanesSpawn,
        Capability::UiRail,
        Capability::UiCommands,
        Capability::UiToast,
        Capability::EventsSubscribe,
    ]
}

fn ids(rows: &[Value]) -> Vec<String> {
    rows.iter()
        .map(|r| r["id"].as_str().unwrap_or_default().to_string())
        .collect()
}

fn labels(rows: &[Value]) -> Vec<String> {
    rows.iter()
        .map(|r| r["label"].as_str().unwrap_or_default().to_string())
        .collect()
}

fn row_by_id<'a>(rows: &'a [Value], id: &str) -> &'a Value {
    rows.iter()
        .find(|r| r["id"] == id)
        .unwrap_or_else(|| panic!("no row {id:?} in {}", ids(rows).join(", ")))
}

// ---------------------------------------------------------------- the tests

#[test]
fn the_handshake_registers_one_entry_per_readable_tool_with_its_commands_and_events() {
    let mut host = Host::spawn(Sim::ready());
    host.wait_for(methods::HOST_ROWS_SET, 1);

    let rail = host.last(methods::HOST_RAIL_REGISTER);
    let entries = rail["entries"].as_array().unwrap();
    let got: Vec<(&str, &str, i64)> = entries
        .iter()
        .map(|e| {
            (
                e["id"].as_str().unwrap(),
                e["label"].as_str().unwrap(),
                e["order"].as_i64().unwrap(),
            )
        })
        .collect();
    // `aider` has no transcript reader, so it can never have an entry, and the orders sit
    // after Files' 10.
    assert_eq!(
        got,
        [
            ("claude", "Claude Code", 20),
            ("cursor-agent", "Cursor", 21),
            ("copilot", "Copilot", 22),
        ]
    );
    assert!(entries.iter().all(|e| e["tier"] == 1));

    let commands = host.last(methods::HOST_COMMAND_REGISTER);
    let ids: Vec<&str> = commands["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["refresh", "filter", "resume"]);

    let subscribed = host.last(methods::HOST_EVENTS_SUBSCRIBE);
    let kinds: Vec<&str> = subscribed["kinds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k.as_str().unwrap())
        .collect();
    assert_eq!(kinds, [methods::events::RAIL_QUERY]);

    host.shutdown();
}

#[test]
fn the_stars_choose_the_entries_and_their_order() {
    let mut sim = Sim::ready();
    sim.favourites = Some(vec!["copilot".into(), "claude".into()]);
    let mut host = Host::spawn(sim);
    host.wait_for(methods::HOST_ROWS_SET, 1);

    let rail = host.last(methods::HOST_RAIL_REGISTER);
    let got: Vec<&str> = rail["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap())
        .collect();
    assert_eq!(got, ["copilot", "claude"]);
    host.shutdown();
}

#[test]
fn the_conversations_group_under_their_project_directory() {
    let mut sim = Sim::ready();
    sim.favourites = Some(vec!["claude".into()]);
    sim.sessions.insert(
        "claude".into(),
        vec![
            session("a1", "/Users/x/code/hyperpanes", "Fix the rail"),
            session("a2", "/Users/x/code/hyperpanes", "Add a test"),
            session("b1", "/Users/x/code/avada-files", "Tree rows"),
        ],
    );
    let mut host = Host::spawn(sim);
    host.wait_for(methods::HOST_ROWS_SET, 1);

    let rows = host.rows("claude");
    assert_eq!(
        labels(&rows),
        [
            "hyperpanes",
            "Fix the rail",
            "Add a test",
            "avada-files",
            "Tree rows",
        ]
    );
    let depths: Vec<i64> = rows.iter().map(|r| r["depth"].as_i64().unwrap()).collect();
    assert_eq!(depths, [0, 1, 1, 0, 1]);
    assert_eq!(
        rows[0]["detail"],
        "/Users/x/code/hyperpanes · 2 conversations"
    );
    // Every clickable row carries what a click needs; a heading carries its project.
    assert_eq!(rows[0]["data"]["kind"], "project");
    assert_eq!(
        rows[1]["data"],
        json!({ "kind": "session", "tool": "claude", "session": "a1" })
    );
    host.shutdown();
}

#[test]
fn folding_a_project_hides_only_its_own_conversations() {
    let mut sim = Sim::ready();
    sim.favourites = Some(vec!["claude".into()]);
    sim.sessions.insert(
        "claude".into(),
        vec![
            session("a1", "/Users/x/code/one", "First"),
            session("b1", "/Users/x/code/two", "Second"),
        ],
    );
    let mut host = Host::spawn(sim);
    host.wait_for(methods::HOST_ROWS_SET, 1);

    let rows = host.rows("claude");
    let heading = row_by_id(&rows, "p:/Users/x/code/one").clone();
    host.activate("claude", &heading, "open");
    host.settle();

    let rows = host.rows("claude");
    assert_eq!(labels(&rows), ["one", "two", "Second"]);
    assert_eq!(rows[0]["expanded"], false);
    host.shutdown();
}

#[test]
fn a_filter_event_narrows_the_entry_it_names_and_nothing_else() {
    let mut sim = Sim::ready();
    sim.favourites = Some(vec!["claude".into(), "copilot".into()]);
    sim.sessions.insert(
        "claude".into(),
        vec![
            session("a1", "/Users/x/code/one", "Fix the rail"),
            session("b1", "/Users/x/code/two", "Add a test"),
        ],
    );
    sim.sessions.insert(
        "copilot".into(),
        vec![session("c1", "/Users/x/code/one", "Fix the rail")],
    );
    let mut host = Host::spawn(sim);
    host.wait_for(methods::HOST_ROWS_SET, 2);

    host.write_line(&json!({
        "jsonrpc": "2.0",
        "method": methods::MODULE_EVENT,
        "params": { "kind": methods::events::RAIL_QUERY, "payload": { "entry": "claude", "query": "test" } },
    }));
    host.settle();

    assert_eq!(labels(&host.rows("claude")), ["two", "Add a test"]);
    assert_eq!(labels(&host.rows("copilot")), ["one", "Fix the rail"]);
    host.shutdown();
}

#[test]
fn opening_a_conversation_asks_the_control_server_for_the_pane_that_resumes_it() {
    let mut sim = Sim::ready();
    sim.favourites = Some(vec!["claude".into()]);
    sim.sessions.insert(
        "claude".into(),
        vec![session("a1", "/Users/x/code/one", "Fix the rail")],
    );
    let mut host = Host::spawn(sim);
    host.wait_for(methods::HOST_ROWS_SET, 1);

    let rows = host.rows("claude");
    let row = row_by_id(&rows, "s:a1").clone();
    host.activate("claude", &row, "open");
    host.wait_for_http("POST", "/command", 1);

    let sim = host.control.sim();
    // A `newPane` is a 400 without a window, so the module asks for one first.
    assert!(
        sim.count("GET", "/state") >= 1,
        "no GET /state before the pane"
    );
    let body = sim.last("POST", "/command");
    assert_eq!(body["type"], "newPane");
    assert_eq!(body["windowId"], 7);
    assert_eq!(body["pane"]["command"], "claude");
    assert_eq!(body["pane"]["args"], json!(["--resume", "a1"]));
    assert_eq!(body["pane"]["cwd"], "/Users/x/code/one");
    assert_eq!(body["pane"]["label"], "Fix the rail");
    assert_eq!(body["pane"]["color"], "#d97757");
    drop(sim);
    host.shutdown();
}

#[test]
fn a_toggle_selects_a_conversation_rather_than_opening_one() {
    let mut sim = Sim::ready();
    sim.favourites = Some(vec!["claude".into()]);
    sim.sessions.insert(
        "claude".into(),
        vec![session("a1", "/Users/x/code/one", "Fix the rail")],
    );
    let mut host = Host::spawn(sim);
    host.wait_for(methods::HOST_ROWS_SET, 1);

    let rows = host.rows("claude");
    let row = row_by_id(&rows, "s:a1").clone();
    host.activate("claude", &row, "toggle");
    host.settle();

    assert_eq!(host.control.sim().count("POST", "/command"), 0);
    let marks = host.rows("claude");
    let marks = row_by_id(&marks, "s:a1")["marks"].clone();
    assert_eq!(marks, json!(["selected"]));
    host.shutdown();
}

#[test]
fn a_conversation_the_host_blocked_is_a_toast_and_never_a_pane() {
    let mut sim = Sim::ready();
    sim.favourites = Some(vec!["claude".into()]);
    let mut blocked = session("a1", "/Users/x/code/one", "Fix the rail");
    blocked["resume"] = Value::Null;
    blocked["blocked"] = json!("the project directory is gone");
    sim.sessions.insert("claude".into(), vec![blocked]);
    let mut host = Host::spawn(sim);
    host.wait_for(methods::HOST_ROWS_SET, 1);

    let rows = host.rows("claude");
    let row = row_by_id(&rows, "s:a1").clone();
    host.activate("claude", &row, "open");
    let toast = host.wait_for(methods::HOST_TOAST, 1);

    assert_eq!(toast["text"], "the project directory is gone");
    assert_eq!(host.control.sim().count("POST", "/command"), 0);
    host.shutdown();
}

#[test]
fn a_host_that_offered_no_control_server_says_so_instead_of_looking_empty() {
    let mut host = Host::spawn_with(Sim::ready(), granted(), false);
    host.wait_for(methods::HOST_ROWS_SET, 1);

    // With no catalogue there is nothing to star, so the module falls back to the three
    // tools its manifest contributes and each one explains itself.
    let rows = host.rows("claude");
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0]["label"]
            .as_str()
            .unwrap()
            .contains("this host offered no control server"),
        "{rows:?}"
    );
    assert_eq!(rows[0]["data"], Value::Null);
    assert_eq!(host.control.sim().seen.len(), 0);
    host.shutdown();
}

#[test]
fn a_refused_history_shows_the_hosts_own_reason_under_that_entry_alone() {
    let mut sim = Sim::ready();
    sim.favourites = Some(vec!["claude".into(), "copilot".into()]);
    // `copilot` has an entry in `sessions`; `claude` does not, so it 404s.
    sim.sessions.insert(
        "copilot".into(),
        vec![session("c1", "/Users/x/code/one", "Fine")],
    );
    let mut host = Host::spawn(sim);
    host.wait_for(methods::HOST_ROWS_SET, 2);

    let rows = host.rows("claude");
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0]["label"]
            .as_str()
            .unwrap()
            .contains("no tool `claude`"),
        "{rows:?}"
    );
    assert_eq!(labels(&host.rows("copilot")), ["one", "Fine"]);
    host.shutdown();
}

#[test]
fn a_refused_catalogue_keeps_the_entries_it_already_had() {
    let mut sim = Sim::ready();
    sim.favourites = Some(vec!["claude".into()]);
    sim.sessions.insert(
        "claude".into(),
        vec![session("a1", "/Users/x/code/one", "Fix the rail")],
    );
    let mut host = Host::spawn(sim);
    host.wait_for(methods::HOST_ROWS_SET, 1);
    assert_eq!(labels(&host.rows("claude")), ["one", "Fix the rail"]);

    host.control.sim().tools = Err(403);
    host.ok(methods::MODULE_COMMAND_INVOKE, json!({ "id": "refresh" }));
    host.settle();

    // The entry survives — a refusal now does not make the tool stop existing — and the
    // rows are still there, because the history route still answers.
    let rail = host.last(methods::HOST_RAIL_REGISTER);
    assert_eq!(rail["entries"].as_array().unwrap().len(), 1);
    assert_eq!(labels(&host.rows("claude")), ["one", "Fix the rail"]);
    host.shutdown();
}

#[test]
fn losing_a_star_re_registers_the_rail_without_that_entry() {
    let mut sim = Sim::ready();
    sim.favourites = Some(vec!["claude".into(), "copilot".into()]);
    sim.sessions.insert("claude".into(), vec![]);
    sim.sessions.insert("copilot".into(), vec![]);
    let mut host = Host::spawn(sim);
    host.wait_for(methods::HOST_ROWS_SET, 2);
    assert_eq!(host.count(methods::HOST_RAIL_REGISTER), 1);

    host.control.sim().favourites = Some(vec!["copilot".into()]);
    host.ok(methods::MODULE_COMMAND_INVOKE, json!({ "id": "refresh" }));
    let rail = host.wait_for(methods::HOST_RAIL_REGISTER, 2);

    let got: Vec<&str> = rail["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap())
        .collect();
    assert_eq!(got, ["copilot"]);
    host.shutdown();
}

#[test]
fn looking_away_and_back_re_reads_the_history() {
    let mut sim = Sim::ready();
    sim.favourites = Some(vec!["claude".into()]);
    sim.sessions.insert("claude".into(), vec![]);
    let mut host = Host::spawn(sim);
    host.wait_for(methods::HOST_ROWS_SET, 1);
    let before = host.control.sim().count("GET", "/tools/claude/sessions");

    host.ok(methods::MODULE_DEACTIVATE, json!({}));
    host.ok(methods::MODULE_ACTIVATE, json!({}));
    host.wait_for_http("GET", "/tools/claude/sessions", before + 1);
    host.shutdown();
}

#[test]
fn a_command_this_module_does_not_have_is_an_error_and_not_a_crash() {
    let mut host = Host::spawn(Sim::ready());
    host.wait_for(methods::HOST_ROWS_SET, 1);

    let e = host.err(methods::MODULE_COMMAND_INVOKE, json!({ "id": "teleport" }));
    assert_eq!(e["code"], -32602);
    assert!(e["message"].as_str().unwrap().contains("teleport"));

    let e = host.err("module.teleport", json!({}));
    assert_eq!(
        e["code"], -32601,
        "an unknown method is MethodNotFound: {e}"
    );
    host.shutdown();
}

#[test]
fn a_newer_host_can_announce_an_event_this_module_has_never_heard_of() {
    let mut host = Host::spawn(Sim::ready());
    host.wait_for(methods::HOST_ROWS_SET, 1);
    host.write_line(&json!({
        "jsonrpc": "2.0",
        "method": methods::MODULE_EVENT,
        "params": { "kind": "something.new", "payload": { "x": 1 } },
    }));
    // Still answering afterwards.
    host.ok(methods::MODULE_ACTIVATE, json!({}));
    host.shutdown();
}
