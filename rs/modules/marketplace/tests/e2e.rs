//! The module binary against a fake host and a fake control server.
//!
//! The host side is what `avada_core` does: a socketpair whose child end is handed
//! to the module in `AVADA_MODULE_FD`, `module.hello` / `host.hello` as the first
//! lines, then JSON-RPC both ways. The control server is a std `TcpListener` that
//! answers the twelve `/marketplace/...` routes with canned JSON, records every
//! request it saw (method, path, body) and refuses a missing or wrong bearer.
//! Nothing here touches the network beyond loopback.
#![cfg(unix)]
#![allow(unsafe_code)] // one fcntl, to let the module inherit its socket

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use avada_module_sdk::contract::{methods, ModuleHello};
use avada_module_sdk::manifest::Manifest;
use avada_module_sdk::Capability;
use serde_json::{json, Value};

/// A per-run token like the host mints; a fixed test value, not a secret.
const TOKEN: &str = "module-run-token-for-tests";
const STEP: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------- fake control

/// `(method, path, body)` as the server saw it.
type Seen = Vec<(String, String, Value)>;

struct FakeControl {
    port: u16,
    seen: Arc<Mutex<Seen>>,
}

impl FakeControl {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().unwrap().port();
        let seen: Arc<Mutex<Seen>> = Arc::default();
        let record = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let record = record.clone();
                std::thread::spawn(move || serve(stream, record));
            }
        });
        FakeControl { port, seen }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn calls(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|(m, p, _)| format!("{m} {p}"))
            .collect()
    }

    fn body_of(&self, key: &str) -> Option<Value> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .find(|(m, p, _)| format!("{m} {p}") == key)
            .map(|(_, _, b)| b.clone())
    }

    fn clear(&self) {
        self.seen.lock().unwrap().clear();
    }
}

fn serve(mut stream: TcpStream, record: Arc<Mutex<Seen>>) {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut length = 0usize;
    let mut authorized = false;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
            break;
        }
        let (name, value) = h.split_once(':').unwrap_or((&h, ""));
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => length = value.trim().parse().unwrap_or(0),
            "authorization" => authorized = value.trim() == format!("Bearer {TOKEN}"),
            _ => {}
        }
    }
    let mut raw = vec![0u8; length];
    if length > 0 {
        reader.read_exact(&mut raw).unwrap();
    }
    let body: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
    let (status, answer) = if authorized {
        record
            .lock()
            .unwrap()
            .push((method.clone(), path.clone(), body.clone()));
        let seen = record.lock().unwrap();
        answer(&method, &path, &body, &seen)
    } else {
        (401, json!({ "error": "bad token" }))
    };
    let payload = answer.to_string();
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        match status {
            200 => "OK",
            202 => "Accepted",
            401 => "Unauthorized",
            _ => "Not Found",
        },
        payload.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(payload.as_bytes());
    let _ = stream.flush();
}

fn job(phase: &str) -> Value {
    let mut j = json!({
        "id": "j1", "module": "acme/avada-git", "tag": "v0.1.0", "kind": "manual",
        "phase": phase, "progress": null, "log_tail": ["cloning"], "started_at": 1
    });
    if phase == "done" {
        j["version"] = json!("0.1.0");
        j["finished_at"] = json!(2);
    }
    j
}

fn signin(status: &str) -> Value {
    json!({
        "id": "s1", "user_code": "ABCD-1234",
        "verification_uri": "https://github.com/login/device",
        "expires_at": 1, "interval": 5, "status": status
    })
}

/// The rights table for `acme/avada-files`, with `profile` chosen and `net` as the
/// user's override on `net.fetch`.
///
/// `effective` is the host's job, not the module's, so the fake computes only enough
/// of it for the pane to have something to render: a user override wins, otherwise
/// the profile's own value stands.
fn rights(profile: Option<&str>, net: Option<&str>) -> Value {
    json!({
        "module": "acme/avada-files",
        "version": "1.1.0",
        "workspace": "ws1",
        "profile": profile,
        "profiles": [
            { "name": "reader", "description": "read only",
              "values": { "fs.read": "always", "net.fetch": "never" } },
            { "name": "writer", "description": "read and write",
              "values": { "fs.read": "always", "net.fetch": "ask" } },
        ],
        "rows": [
            { "cap": "fs.read", "description": "read files", "accepted": true,
              "user": null, "workspace": null, "effective": "always" },
            { "cap": "net.fetch", "description": "reach the network", "accepted": true,
              "user": net, "workspace": null,
              "effective": net.unwrap_or(if profile == Some("writer") { "ask" } else { "never" }) },
        ],
    })
}

/// What `GET /marketplace/pins` should say, given everything this server was asked
/// before. Deriving it from the recorded history rather than a global keeps two
/// tests running at once from seeing each other's pins.
fn pins_now(seen: &Seen) -> Value {
    let last = seen
        .iter()
        .rev()
        .find_map(|(m, p, b)| match (m.as_str(), p.as_str()) {
            ("POST", "/marketplace/modules/acme/avada-files/pin") => {
                Some(b.get("version").cloned().unwrap_or(Value::Null))
            }
            ("POST", "/marketplace/modules/acme/avada-files/unpin") => Some(Value::Null),
            _ => None,
        });
    match last.unwrap_or_else(|| json!("1.1.0")) {
        Value::Null => json!({}),
        v => json!({ "acme/avada-files": v }),
    }
}

/// The routes, canned.
fn answer(method: &str, path: &str, body: &Value, seen: &Seen) -> (u16, Value) {
    let route = path.split('?').next().unwrap_or(path);
    match (method, route) {
        ("GET", "/marketplace/toolchain") => (
            200,
            json!({ "ready": true, "missing": [], "guide": null,
                    "toolchain": { "rustup": "1.27", "cargo": "1.89", "git": "2.45" },
                    "signed_in": false }),
        ),
        ("GET", "/marketplace/installed") => (
            200,
            json!({ "modules": [{
                "module": "acme/avada-files", "version": "1.2.0", "active": true,
                "kind": "manual", "tag": "v1.2.0", "commit": "abc", "sha256": "00",
                "accepted": ["ui.rail"], "enabled": { "ws1": true }, "broken": null
            }] }),
        ),
        ("GET", "/marketplace/search") => (
            200,
            json!({ "modules": [{
                "full_name": "acme/avada-git", "description": "Git rail",
                "html_url": "https://github.com/acme/avada-git", "stars": 5,
                "updated_at": "2026-09-01T00:00:00Z"
            }] }),
        ),
        ("GET", "/marketplace/modules/acme/avada-git") => (
            200,
            json!({ "repo": { "full_name": "acme/avada-git" }, "tags": [] }),
        ),
        ("POST", "/marketplace/install") => {
            if body.get("module").and_then(Value::as_str).is_none() {
                return (400, json!({ "error": "module is required" }));
            }
            (202, json!({ "job": job("fetch") }))
        }
        ("GET", "/marketplace/jobs") => (200, json!({ "jobs": [job("done")] })),
        ("GET", "/marketplace/jobs/j1") => (200, job("done")),
        ("POST", "/marketplace/modules/acme/avada-files/enable") => {
            let Some(ws) = body.get("workspace").and_then(Value::as_str) else {
                return (400, json!({ "error": "workspace is required" }));
            };
            (
                200,
                json!({ "module": "acme/avada-files", "enabled": { ws: true } }),
            )
        }
        ("POST", "/marketplace/modules/acme/avada-files/disable") => {
            let Some(ws) = body.get("workspace").and_then(Value::as_str) else {
                return (400, json!({ "error": "workspace is required" }));
            };
            (
                200,
                json!({ "module": "acme/avada-files", "enabled": { ws: false } }),
            )
        }
        ("DELETE", "/marketplace/modules/acme/avada-files/1.2.0") => (
            200,
            json!({ "ok": true, "module": "acme/avada-files", "version": "1.2.0" }),
        ),
        ("GET", "/marketplace/modules/acme/avada-files") => (
            200,
            json!({
                "module": "acme/avada-files",
                "repo": { "full_name": "acme/avada-files", "description": "A file browser",
                          "html_url": "https://github.com/acme/avada-files", "stars": 7,
                          "updated_at": "2026-09-01T00:00:00Z" },
                "tags": [{ "name": "1.2.0", "commit": "c2" },
                         { "name": "1.1.0", "commit": "c1" },
                         { "name": "1.0.0", "commit": "c0" }],
                "newest_tag": "1.2.0",
                "installed": ["1.1.0", "1.2.0"],
                "active": "1.1.0",
                "enabled": { "ws1": true }
            }),
        ),
        // A module nobody installed answers 404 here by design: "not installed" is
        // not the same fact as "installed with an empty table".
        ("GET", "/marketplace/modules/acme/avada-git/rights") => (
            404,
            json!({ "error": "module acme/avada-git is not installed" }),
        ),
        ("GET", "/marketplace/modules/acme/avada-files/rights") => {
            (200, rights(Some("reader"), None))
        }
        ("POST", "/marketplace/modules/acme/avada-files/rights") => {
            let Some(cap) = body.get("cap").and_then(Value::as_str) else {
                return (400, json!({ "error": "cap is required" }));
            };
            if cap != "net.fetch" {
                return (400, json!({ "error": format!("unknown capability {cap}") }));
            }
            (
                200,
                rights(Some("reader"), body.get("value").and_then(Value::as_str)),
            )
        }
        ("POST", "/marketplace/modules/acme/avada-files/profile") => (
            200,
            rights(body.get("profile").and_then(Value::as_str), None),
        ),
        ("POST", "/marketplace/modules/acme/avada-files/pin")
        | ("POST", "/marketplace/modules/acme/avada-files/unpin") => {
            if body.get("workspace").and_then(Value::as_str).is_none() {
                return (400, json!({ "error": "workspace is required" }));
            }
            (200, json!({ "ok": true }))
        }
        ("GET", "/marketplace/pins") => {
            (200, json!({ "workspace": "ws1", "pins": pins_now(seen) }))
        }
        ("POST", "/marketplace/signin") => (200, signin("pending")),
        ("GET", "/marketplace/signin/s1") => (200, signin("done")),
        _ => (
            404,
            json!({ "error": format!("no route {method} {route}") }),
        ),
    }
}

// ---------------------------------------------------------------- fake host

struct Host {
    child: Child,
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
    /// Every `host.*` request or notification the module sent, in order.
    host_calls: Vec<(String, Value)>,
    /// A method the host answers with `CapabilityDenied`, as it would for a
    /// capability the user never granted.
    deny: Option<&'static str>,
    _data: TempDir,
}

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "avada-marketplace-e2e-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn all_caps() -> Vec<Capability> {
    vec![
        Capability::UiRail,
        Capability::UiPane,
        Capability::PanesSpawn,
        Capability::UiCommands,
        Capability::UiToast,
        Capability::MarketplaceManage,
    ]
}

impl Host {
    /// Spawn the module with the socket in `AVADA_MODULE_FD` and complete the hellos.
    fn spawn(tag: &str, control: &FakeControl, granted: Vec<Capability>) -> Self {
        let (host_end, child_end) = UnixStream::pair().unwrap();
        // The host clears CLOEXEC on its child end so the descriptor survives exec.
        let fd = child_end.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(flags >= 0);
        assert!(unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } >= 0);
        let data = TempDir::new(tag);
        let child = Command::new(env!("CARGO_BIN_EXE_avada-marketplace"))
            .env("AVADA_MODULE_FD", fd.to_string())
            .env("AVADA_MODULE_DATA", &data.0)
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
            deny: None,
            _data: data,
        };
        let first = host.read_line();
        let hello: ModuleHello = serde_json::from_str(&first).expect("module.hello");
        assert_eq!(
            hello.manifest.module.id.to_string(),
            "bshuler/avada-marketplace"
        );
        assert!(hello.contract_min <= 1 && 1 <= hello.contract_max);
        for m in methods::MODULE_REQUIRED_V1 {
            assert!(
                hello.methods.iter().any(|x| x == m),
                "module.hello does not list {m}"
            );
        }
        host.write_line(&json!({
            "type": "host.hello",
            "contract_version": 1,
            "host_version": "0.0.0-test",
            "product": "Avada Terminal",
            "granted": granted,
            "methods": [methods::HOST_RAIL_REGISTER, methods::HOST_ROWS_SET,
                        methods::HOST_COMMAND_REGISTER, methods::HOST_TOAST,
                        methods::HOST_PANES_SPAWN],
            "data_dir": host._data.0.to_string_lossy(),
            "workspace": { "id": "ws1", "name": "Test workspace" },
            "token": TOKEN,
            "control_url": control.url(),
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

    /// Read one message. Module→host requests are answered `{}` and recorded;
    /// notifications are recorded; a response is returned.
    fn pump(&mut self) -> Option<Value> {
        let line = self.read_line();
        self.handle(&line)
    }

    fn handle(&mut self, line: &str) -> Option<Value> {
        let v: Value = serde_json::from_str(line).expect("json line");
        if let Some(method) = v.get("method").and_then(Value::as_str) {
            let method = method.to_string();
            let params = v.get("params").cloned().unwrap_or(Value::Null);
            if let Some(id) = v.get("id") {
                if self.deny == Some(method.as_str()) {
                    self.write_line(&json!({ "jsonrpc": "2.0", "id": id,
                        "error": { "code": -32001, "message": format!("`{method}` was not granted") } }));
                } else {
                    self.write_line(&json!({ "jsonrpc": "2.0", "id": id, "result": {} }));
                }
            }
            self.host_calls.push((method, params));
            None
        } else {
            Some(v)
        }
    }

    /// Send a request and wait for its response, serving the module meanwhile.
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

    fn rows_seen(&self, target: &str) -> usize {
        self.host_calls
            .iter()
            .filter(|(m, p)| m == methods::HOST_ROWS_SET && p["target"] == target)
            .count()
    }

    fn has_rows_for(&self, target: &str) -> bool {
        self.rows_seen(target) > 0
    }

    /// The newest push for one surface out of what the host has already read.
    fn last_rows(&self, target: &str) -> Value {
        self.host_calls
            .iter()
            .rev()
            .find(|(m, p)| m == methods::HOST_ROWS_SET && p["target"] == target)
            .map(|(_, p)| p.clone())
            .unwrap_or_else(|| panic!("module never pushed {target} rows"))
    }

    /// Serve the module until it repaints one surface, and return those rows.
    ///
    /// Rail and pane share an entry id — `target` is what tells them apart on the
    /// wire — and a repaint always trails the response that caused it, so a test
    /// that reads the last push it happens to have seen reads the wrong one.
    fn next_rows(&mut self, target: &str) -> Value {
        let before = self.rows_seen(target);
        let deadline = Instant::now() + STEP;
        while Instant::now() < deadline {
            if self.rows_seen(target) > before {
                return self
                    .host_calls
                    .iter()
                    .rev()
                    .find(|(m, p)| m == methods::HOST_ROWS_SET && p["target"] == target)
                    .map(|(_, p)| p.clone())
                    .unwrap();
            }
            let _ = self.pump();
        }
        panic!("module never repainted {target}")
    }

    fn row<'a>(rows: &'a Value, id: &str) -> &'a Value {
        rows["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == id)
            .unwrap_or_else(|| panic!("no row {id} in {rows}"))
    }

    /// `module.shutdown`, then keep serving whatever the module still sends (a
    /// `host.rows.set` it was waiting on, say) until it exits; 5 s is the contract.
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

/// A host with everything granted, past the first paint.
fn ready(tag: &str) -> (FakeControl, Host) {
    let control = FakeControl::start();
    let mut host = Host::spawn(tag, &control, all_caps());
    host.wait_for(methods::HOST_ROWS_SET, 1);
    (control, host)
}

// ---------------------------------------------------------------- tests

#[test]
fn manifest_flag_prints_the_embedded_manifest() {
    let out = Command::new(env!("CARGO_BIN_EXE_avada-marketplace"))
        .arg("--manifest")
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(text, std::fs::read_to_string("avada.toml").unwrap());
    let m = Manifest::parse(&text).expect("manifest parses");
    assert_eq!(m.module.id.to_string(), "bshuler/avada-marketplace");
    assert_eq!(m.tag(), format!("v{}", env!("CARGO_PKG_VERSION")));
    assert!(m.capabilities.contains(&Capability::MarketplaceManage));
    assert!(matches!(
        m.distribution.kind,
        avada_module_sdk::manifest::DistributionKind::Source
    ));
}

#[test]
fn handshake_registers_rail_commands_and_first_rows() {
    let (control, mut host) = ready("handshake");
    let rail = host.last(methods::HOST_RAIL_REGISTER);
    assert_eq!(rail["entries"][0]["id"], "marketplace");
    assert_eq!(rail["entries"][0]["tier"], 1);
    let commands = host.last(methods::HOST_COMMAND_REGISTER);
    let mut ids: Vec<&str> = commands["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    ids.sort_unstable();
    assert_eq!(
        ids,
        [
            "disable",
            "enable",
            "install",
            "job",
            "pane",
            "refresh",
            "search",
            "signin",
            "uninstall"
        ]
    );
    // The first paint came from the routes, with the bearer the hello carried.
    assert_eq!(
        control.calls(),
        ["GET /marketplace/toolchain", "GET /marketplace/installed"]
    );
    let rows = host.wait_for(methods::HOST_ROWS_SET, 1);
    assert_eq!(rows["entry"], "marketplace");
    assert!(Host::row(&rows, "toolchain")["detail"]
        .as_str()
        .unwrap()
        .contains("ready"));
    let installed = Host::row(&rows, "installed-0");
    assert_eq!(installed["label"], "acme/avada-files");
    assert_eq!(installed["data"]["action"], "disable");
    assert!(rows["rows"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["id"] != "notice"));
    host.shutdown();
}

#[test]
fn search_command_hits_the_route_and_lists_results() {
    let (control, mut host) = ready("search");
    control.clear();
    let result = host.ok(
        methods::MODULE_COMMAND_INVOKE,
        json!({ "id": "search", "args": { "q": "git rail" } }),
    );
    assert_eq!(result["results"], 1);
    assert_eq!(control.calls(), ["GET /marketplace/search?q=git%20rail"]);
    let rows = host.wait_for(methods::HOST_ROWS_SET, 2);
    let hit = Host::row(&rows, "result-0");
    assert_eq!(hit["label"], "acme/avada-git");
    assert_eq!(
        hit["data"],
        json!({ "action": "install", "module": "acme/avada-git" })
    );
    host.shutdown();
}

#[test]
fn install_command_posts_the_job_and_job_command_polls_it() {
    let (control, mut host) = ready("install");
    control.clear();
    let result = host.ok(
        methods::MODULE_COMMAND_INVOKE,
        json!({ "id": "install", "args": { "module": "acme/avada-git", "tag": "v0.1.0" } }),
    );
    assert_eq!(result["job"], "j1");
    assert_eq!(
        control.body_of("POST /marketplace/install").unwrap(),
        json!({ "module": "acme/avada-git", "tag": "v0.1.0", "workspace": "ws1" })
    );
    assert!(host.last(methods::HOST_TOAST)["text"]
        .as_str()
        .unwrap()
        .contains("installing acme/avada-git"));
    let rows = host.wait_for(methods::HOST_ROWS_SET, 2);
    assert_eq!(Host::row(&rows, "job-j1")["data"]["id"], "j1");

    control.clear();
    let result = host.ok(methods::MODULE_COMMAND_INVOKE, json!({ "id": "job" }));
    assert_eq!(result["phase"], "done");
    assert_eq!(result["version"], "0.1.0");
    assert_eq!(control.calls()[0], "GET /marketplace/jobs/j1");
    // A finished job refreshes the install store.
    assert!(control
        .calls()
        .contains(&"GET /marketplace/installed".to_string()));
    host.shutdown();
}

#[test]
fn enable_and_disable_commands_post_the_workspace() {
    let (control, mut host) = ready("enable");
    control.clear();
    host.ok(
        methods::MODULE_COMMAND_INVOKE,
        json!({ "id": "disable", "args": { "module": "acme/avada-files" } }),
    );
    assert_eq!(
        control.body_of("POST /marketplace/modules/acme/avada-files/disable"),
        Some(json!({ "workspace": "ws1" }))
    );
    let rows = host.wait_for(methods::HOST_ROWS_SET, 2);
    assert_eq!(Host::row(&rows, "installed-0")["data"]["action"], "enable");

    host.ok(
        methods::MODULE_COMMAND_INVOKE,
        json!({ "id": "enable", "args": { "module": "acme/avada-files" } }),
    );
    assert_eq!(
        control.body_of("POST /marketplace/modules/acme/avada-files/enable"),
        Some(json!({ "workspace": "ws1" }))
    );
    let rows = host.wait_for(methods::HOST_ROWS_SET, 3);
    assert_eq!(Host::row(&rows, "installed-0")["data"]["action"], "disable");
    host.shutdown();
}

#[test]
fn uninstall_command_deletes_the_active_version() {
    let (control, mut host) = ready("uninstall");
    control.clear();
    let result = host.ok(
        methods::MODULE_COMMAND_INVOKE,
        json!({ "id": "uninstall", "args": { "module": "acme/avada-files" } }),
    );
    assert_eq!(result["version"], "1.2.0");
    assert_eq!(
        control.calls(),
        ["DELETE /marketplace/modules/acme/avada-files/1.2.0"]
    );
    let rows = host.wait_for(methods::HOST_ROWS_SET, 2);
    assert_eq!(Host::row(&rows, "installed")["detail"], "nothing installed");
    host.shutdown();
}

#[test]
fn signin_command_starts_the_device_flow_then_polls_it() {
    let (control, mut host) = ready("signin");
    control.clear();
    let result = host.ok(methods::MODULE_COMMAND_INVOKE, json!({ "id": "signin" }));
    assert_eq!(result["user_code"], "ABCD-1234");
    assert!(host.last(methods::HOST_TOAST)["text"]
        .as_str()
        .unwrap()
        .contains("ABCD-1234"));
    let rows = host.wait_for(methods::HOST_ROWS_SET, 2);
    let github = Host::row(&rows, "github");
    assert_eq!(
        github["data"],
        json!({ "action": "signin.poll", "id": "s1" })
    );
    assert!(github["detail"].as_str().unwrap().contains("ABCD-1234"));

    let result = host.ok(methods::MODULE_COMMAND_INVOKE, json!({ "id": "signin" }));
    assert_eq!(result["status"], "done");
    assert_eq!(
        control.calls(),
        ["POST /marketplace/signin", "GET /marketplace/signin/s1"]
    );
    let rows = host.wait_for(methods::HOST_ROWS_SET, 3);
    assert_eq!(Host::row(&rows, "github")["detail"], "signed in");
    host.shutdown();
}

#[test]
fn refresh_command_and_activate_refetch_the_routes() {
    let (control, mut host) = ready("refresh");
    control.clear();
    host.ok(methods::MODULE_COMMAND_INVOKE, json!({ "id": "refresh" }));
    assert_eq!(
        control.calls(),
        ["GET /marketplace/toolchain", "GET /marketplace/installed"]
    );
    control.clear();
    host.ok(
        methods::MODULE_ACTIVATE,
        json!({ "workspace": { "id": "ws2", "name": "Other" } }),
    );
    assert_eq!(
        control.calls(),
        ["GET /marketplace/toolchain", "GET /marketplace/installed"]
    );
    // Enable now targets the new workspace.
    control.clear();
    host.ok(
        methods::MODULE_COMMAND_INVOKE,
        json!({ "id": "enable", "args": { "module": "acme/avada-files" } }),
    );
    assert_eq!(
        control.body_of("POST /marketplace/modules/acme/avada-files/enable"),
        Some(json!({ "workspace": "ws2" }))
    );
    host.ok(methods::MODULE_DEACTIVATE, json!({}));
    host.shutdown();
}

#[test]
fn row_activation_uses_the_row_payload() {
    let (control, mut host) = ready("rows");
    let rows = host.wait_for(methods::HOST_ROWS_SET, 1);
    let installed = Host::row(&rows, "installed-0").clone();
    control.clear();
    host.ok(
        methods::MODULE_ROW_ACTIVATE,
        json!({ "entry": "marketplace", "row": installed["id"], "data": installed["data"], "gesture": "open" }),
    );
    assert_eq!(
        control.calls(),
        ["POST /marketplace/modules/acme/avada-files/disable"]
    );
    control.clear();
    host.ok(
        methods::MODULE_ROW_ACTIVATE,
        json!({ "entry": "marketplace", "row": installed["id"], "data": installed["data"], "gesture": "context" }),
    );
    assert!(control.calls().is_empty(), "context does not act");
    host.ok(
        methods::MODULE_ROW_ACTIVATE,
        json!({ "entry": "marketplace", "row": "result-0",
                "data": { "action": "install", "module": "acme/avada-git" }, "gesture": "open" }),
    );
    assert_eq!(control.calls(), ["POST /marketplace/install"]);
    host.shutdown();
}

#[test]
fn bad_arguments_are_refused_before_any_route_is_called() {
    let (control, mut host) = ready("bad-args");
    control.clear();
    let err = host.err(
        methods::MODULE_COMMAND_INVOKE,
        json!({ "id": "install", "args": { "module": "not-a-module-id" } }),
    );
    assert_eq!(err["code"], -32602);
    let err = host.err(
        methods::MODULE_COMMAND_INVOKE,
        json!({ "id": "bogus", "args": {} }),
    );
    assert_eq!(err["code"], -32602);
    let err = host.err("module.something.else", json!({}));
    assert_eq!(err["code"], -32601);
    assert!(control.calls().is_empty());
    // Failures are toasted so the user hears about them without opening the rail.
    assert!(host.count(methods::HOST_TOAST) >= 2);
    host.shutdown();
}

#[test]
fn a_route_failure_becomes_the_notice_row_and_an_error() {
    let (control, mut host) = ready("notice");
    control.clear();
    let err = host.err(
        methods::MODULE_COMMAND_INVOKE,
        json!({ "id": "uninstall", "args": { "module": "acme/avada-files", "version": "9.9.9" } }),
    );
    assert_eq!(err["code"], -32602, "a 404 maps to invalid params");
    assert!(err["message"].as_str().unwrap().starts_with("404: "));
    let rows = host.wait_for(methods::HOST_ROWS_SET, 2);
    assert_eq!(rows["rows"][0]["id"], "notice");
    host.shutdown();
}

#[test]
fn without_marketplace_manage_no_route_is_ever_called() {
    let control = FakeControl::start();
    let mut host = Host::spawn(
        "no-cap",
        &control,
        vec![
            Capability::UiRail,
            Capability::UiCommands,
            Capability::UiToast,
        ],
    );
    let rows = host.wait_for(methods::HOST_ROWS_SET, 1);
    assert_eq!(rows["rows"].as_array().unwrap().len(), 1);
    assert_eq!(rows["rows"][0]["id"], "no-capability");
    let err = host.err(
        methods::MODULE_COMMAND_INVOKE,
        json!({ "id": "search", "args": { "q": "x" } }),
    );
    assert_eq!(err["code"], -32001);
    assert!(control.calls().is_empty());
    host.shutdown();
}

#[test]
fn without_ui_rail_nothing_is_registered_but_commands_still_work() {
    let control = FakeControl::start();
    let mut host = Host::spawn(
        "no-rail",
        &control,
        vec![Capability::UiCommands, Capability::MarketplaceManage],
    );
    host.wait_for(methods::HOST_COMMAND_REGISTER, 1);
    let result = host.ok(
        methods::MODULE_COMMAND_INVOKE,
        json!({ "id": "search", "args": { "q": "x" } }),
    );
    assert_eq!(result["results"], 1);
    assert_eq!(host.count(methods::HOST_RAIL_REGISTER), 0);
    assert_eq!(host.count(methods::HOST_ROWS_SET), 0);
    assert_eq!(host.count(methods::HOST_TOAST), 0, "no ui.toast, no toast");
    host.shutdown();
}

#[test]
fn shutdown_exits_within_five_seconds() {
    let (_control, host) = ready("shutdown");
    host.shutdown();
}

/// Open the pane on `acme/avada-files` and return the rows it painted.
fn open_pane(host: &mut Host) -> Value {
    host.ok(
        methods::MODULE_COMMAND_INVOKE,
        json!({ "id": "pane", "args": { "module": "acme/avada-files" } }),
    );
    host.wait_for(methods::HOST_PANES_SPAWN, 1);
    host.next_rows("pane")
}

#[test]
fn the_pane_command_spawns_a_module_pane_and_paints_the_focused_module() {
    let (control, mut host) = ready("pane-open");
    control.clear();
    let rows = open_pane(&mut host);

    let spawn = host.last(methods::HOST_PANES_SPAWN);
    assert_eq!(spawn["kind"], "module");
    assert_eq!(spawn["surface"], "marketplace");

    // Focusing reads the module, its rights and the workspace's pins — and nothing else.
    assert_eq!(
        control.calls(),
        [
            "GET /marketplace/modules/acme/avada-files",
            "GET /marketplace/modules/acme/avada-files/rights?workspace=ws1",
            "GET /marketplace/pins?workspace=ws1",
        ]
    );

    assert_eq!(rows["entry"], "marketplace");
    assert_eq!(Host::row(&rows, "back")["data"]["action"], "pane.index");
    // The version picker: installed-and-pinned, installed, and not installed.
    let version = |label: &str| {
        rows["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["label"] == label && r["id"].as_str().unwrap().starts_with("version-"))
            .unwrap_or_else(|| panic!("no version row {label} in {rows}"))
            .clone()
    };
    assert_eq!(version("1.1.0")["data"]["action"], "pane.unpin");
    assert_eq!(version("1.2.0")["data"]["action"], "pane.pin");
    assert_eq!(version("1.0.0")["data"]["action"], "pane.install");
    // The profiles UI and the per-capability table came from the rights route.
    assert_eq!(Host::row(&rows, "profile-0")["label"], "reader");
    assert_eq!(Host::row(&rows, "cap-1")["label"], "net.fetch");

    // The rail kept its own, narrower projection throughout.
    let rail = host.last_rows("rail");
    assert!(rail["rows"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| !r["id"].as_str().unwrap().starts_with("cap-")));
    host.shutdown();
}

#[test]
fn without_ui_pane_the_module_never_asks_for_a_pane_it_cannot_have() {
    // Two capabilities gate a pane; withholding either one is enough.
    let control = FakeControl::start();
    let caps: Vec<Capability> = all_caps()
        .into_iter()
        .filter(|c| *c != Capability::PanesSpawn)
        .collect();
    let mut host = Host::spawn("pane-nocap", &control, caps);
    host.wait_for(methods::HOST_ROWS_SET, 1);
    host.ok(
        methods::MODULE_COMMAND_INVOKE,
        json!({ "id": "pane", "args": {} }),
    );
    // One more rail paint proves the module got that far and still declined the pane.
    host.wait_for(methods::HOST_ROWS_SET, 2);
    assert_eq!(host.count(methods::HOST_PANES_SPAWN), 0);
    assert!(!host.has_rows_for("pane"), "pane rows without ui.pane");
    host.shutdown();
}

#[test]
fn pinning_from_the_pane_posts_the_workspace_and_the_next_paint_shows_it() {
    let (control, mut host) = ready("pane-pin");
    let rows = open_pane(&mut host);
    let target = rows["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["label"] == "1.2.0" && r["id"].as_str().unwrap().starts_with("version-"))
        .unwrap()
        .clone();
    control.clear();
    host.ok(
        methods::MODULE_ROW_ACTIVATE,
        json!({ "entry": "marketplace", "row": target["id"],
                "data": target["data"], "gesture": "open" }),
    );
    assert_eq!(
        control.calls(),
        [
            "POST /marketplace/modules/acme/avada-files/pin",
            "GET /marketplace/pins?workspace=ws1",
        ]
    );
    let body = control
        .body_of("POST /marketplace/modules/acme/avada-files/pin")
        .unwrap();
    assert_eq!(body["workspace"], "ws1");
    assert_eq!(body["version"], "1.2.0");

    // The re-read moved the pin, so the two rows swapped their offers.
    let rows = host.next_rows("pane");
    let by_label = |label: &str| {
        rows["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["label"] == label && r["id"].as_str().unwrap().starts_with("version-"))
            .unwrap()
            .clone()
    };
    assert_eq!(by_label("1.2.0")["data"]["action"], "pane.unpin");
    assert_eq!(by_label("1.1.0")["data"]["action"], "pane.pin");
    host.shutdown();
}

#[test]
fn cycling_a_right_from_the_pane_posts_the_next_value_and_redraws_the_table() {
    let (control, mut host) = ready("pane-right");
    let rows = open_pane(&mut host);
    let cap = Host::row(&rows, "cap-1").clone();
    // `net.fetch` has no user override yet, so the ring's first stop is `never`.
    assert_eq!(cap["data"]["value"], "never");
    control.clear();
    host.ok(
        methods::MODULE_ROW_ACTIVATE,
        json!({ "entry": "marketplace", "row": cap["id"],
                "data": cap["data"], "gesture": "open" }),
    );
    assert_eq!(
        control.calls(),
        ["POST /marketplace/modules/acme/avada-files/rights"]
    );
    let body = control
        .body_of("POST /marketplace/modules/acme/avada-files/rights")
        .unwrap();
    assert_eq!(body["cap"], "net.fetch");
    assert_eq!(body["value"], "never");

    // The answer is the new table, so the same row now offers the next stop along.
    let cap = Host::row(&host.next_rows("pane"), "cap-1").clone();
    assert!(cap["detail"].as_str().unwrap().starts_with("never"));
    assert_eq!(cap["data"]["value"], "always");
    host.shutdown();
}

#[test]
fn choosing_a_profile_from_the_pane_reaches_the_profile_route() {
    let (control, mut host) = ready("pane-profile");
    let rows = open_pane(&mut host);
    let writer = Host::row(&rows, "profile-1").clone();
    assert_eq!(writer["label"], "writer");
    control.clear();
    host.ok(
        methods::MODULE_ROW_ACTIVATE,
        json!({ "entry": "marketplace", "row": writer["id"],
                "data": writer["data"], "gesture": "open" }),
    );
    assert_eq!(
        control.calls(),
        ["POST /marketplace/modules/acme/avada-files/profile"]
    );
    assert_eq!(
        control
            .body_of("POST /marketplace/modules/acme/avada-files/profile")
            .unwrap()["profile"],
        "writer"
    );
    let rows = host.next_rows("pane");
    assert!(Host::row(&rows, "profile-1")["marks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m == "ok"));
    host.shutdown();
}

#[test]
fn a_module_that_is_not_installed_still_shows_its_versions() {
    // Its rights route answers 404 by design; that must not swallow the rest of
    // the pane or leave an error notice standing over it.
    let (control, mut host) = ready("pane-uninstalled");
    control.clear();
    host.ok(
        methods::MODULE_COMMAND_INVOKE,
        json!({ "id": "pane", "args": { "module": "acme/avada-git" } }),
    );
    let rows = host.next_rows("pane");
    assert_eq!(
        rows["rows"][0]["id"], "back",
        "no error notice on top: {rows}"
    );
    assert!(Host::row(&rows, "rights")["detail"]
        .as_str()
        .unwrap()
        .contains("install this module"));
    host.shutdown();
}

#[test]
fn going_back_from_a_module_returns_the_pane_to_the_index() {
    let (_control, mut host) = ready("pane-back");
    let rows = open_pane(&mut host);
    let back = Host::row(&rows, "back").clone();
    host.ok(
        methods::MODULE_ROW_ACTIVATE,
        json!({ "entry": "marketplace", "row": back["id"],
                "data": back["data"], "gesture": "open" }),
    );
    let rows = host.next_rows("pane");
    assert!(
        rows["rows"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["id"] != "back"),
        "still on the module: {rows}"
    );
    assert_eq!(
        Host::row(&rows, "installed-0")["data"]["action"],
        "pane.focus"
    );
    host.shutdown();
}

#[test]
fn a_refused_pane_is_reported_and_survived_rather_than_fatal() {
    // The host is the last word on capabilities: it can refuse a spawn the module
    // believed it was granted, and that must not take the module down with it.
    let (_control, mut host) = ready("pane-denied");
    host.deny = Some(methods::HOST_PANES_SPAWN);
    host.ok(
        methods::MODULE_COMMAND_INVOKE,
        json!({ "id": "pane", "args": {} }),
    );
    let toast = host.wait_for(methods::HOST_TOAST, 1);
    assert_eq!(toast["level"], "warn");
    assert!(toast["text"]
        .as_str()
        .unwrap()
        .contains("the pane could not open"));
    assert!(
        !host.has_rows_for("pane"),
        "painted a pane the host refused"
    );
    // Still alive and still serving the rail.
    host.deny = None;
    host.ok(
        methods::MODULE_COMMAND_INVOKE,
        json!({ "id": "refresh", "args": {} }),
    );
    host.shutdown();
}
