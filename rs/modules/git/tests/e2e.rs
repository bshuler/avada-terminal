//! The real `avada-git` binary against a fake host and a real git repository.
//!
//! The host side is what `avada_core` does: a socketpair whose child end is handed to the
//! module in `AVADA_MODULE_FD`, `module.hello` / `host.hello` as the first two lines, then
//! JSON-RPC both ways. `host.git.status` and `host.git.commit` are answered by running git
//! against an actual temp repository and shaping the answer exactly as
//! `avada_core::git` does, so a test that passes here is a test that would pass against
//! the host. Nothing here touches the network or the user's disk outside
//! `std::env::temp_dir()`.
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
const HOST_GIT_STATUS: &str = "host.git.status";
const HOST_GIT_COMMIT: &str = "host.git.commit";
const GIT_COMMIT_EVENT: &str = "git.commit";

// ---------------------------------------------------------------- the repository

struct TempRepo(PathBuf);

impl TempRepo {
    /// A repository with one commit behind it and something in each of the three sections,
    /// canonicalised: on macOS `std::env::temp_dir()` is a symlink and the host answers
    /// with real paths, so the test must too or every row id would disagree with its path.
    fn new(tag: &str) -> Option<Self> {
        let dir = std::env::temp_dir().join(format!(
            "avada-git-e2e-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let repo = TempRepo(dir.canonicalize().unwrap());
        if !repo.git(&["init", "-b", "main"]).0 {
            return None; // no git on this machine
        }
        repo.git(&["config", "user.email", "t@example.com"]);
        repo.git(&["config", "user.name", "T"]);
        repo.git(&["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.0.join("README.md"), "hello\n").unwrap();
        std::fs::write(repo.0.join("src/main.rs"), "fn main() {}\n").unwrap();
        repo.git(&["add", "-A"]);
        repo.git(&["commit", "-q", "-m", "the first commit"]);

        // One file in each section, which is the whole of what the deleted panel drew.
        std::fs::write(repo.0.join("src/main.rs"), "fn main() { /* x */ }\n").unwrap();
        repo.git(&["add", "src/main.rs"]);
        std::fs::write(repo.0.join("README.md"), "hello again\n").unwrap();
        std::fs::write(repo.0.join("notes.txt"), "scratch\n").unwrap();
        Some(repo)
    }

    /// Run git in the repository. `GIT_CONFIG_*` are pinned so the developer's own global
    /// config — a signing key, a default branch, a status format — cannot change what this
    /// test sees.
    fn git(&self, args: &[&str]) -> (bool, String) {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.0)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git runs");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    }

    fn root(&self) -> String {
        self.0.display().to_string()
    }

    fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"]).1.trim().to_string()
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn split(path: &str) -> (String, String) {
    match path.rsplit_once('/') {
        Some((d, n)) => (d.to_string(), n.to_string()),
        None => (String::new(), path.to_string()),
    }
}

/// `host.git.status`, in the shape `avada_core::module::rpc` answers it. Porcelain v1 here
/// rather than v2: the host's own parser is v2 and is tested in the host — what this test
/// needs is a real repository's real answer in the host's wire shape.
fn git_status(repo: &TempRepo, path: &str) -> Result<Value, Value> {
    let real = PathBuf::from(path)
        .canonicalize()
        .map_err(|e| json!({ "code": -32602, "message": format!("`{path}`: {e}") }))?;
    if !real.starts_with(&repo.0) {
        return Err(json!({
            "code": -32001,
            "message": format!("`{path}` is outside the workspace root"),
        }));
    }
    let (ok, out) = repo.git(&["status", "--porcelain"]);
    if !ok {
        return Ok(json!({
            "repo": false, "root": null, "branch": "", "upstream": null,
            "ahead": 0, "behind": 0, "summary": "", "rows": []
        }));
    }
    let mut rows = Vec::new();
    for line in out.lines() {
        if line.len() < 4 {
            continue;
        }
        let (x, y) = (line.chars().next().unwrap(), line.chars().nth(1).unwrap());
        let p = line[3..].to_string();
        let mut push = |code: char, section: &str| {
            let (detail, label) = split(&p);
            rows.push(json!({
                "path": p, "label": label, "detail": detail,
                "code": code.to_string(), "section": section,
            }));
        };
        if x == '?' {
            push('?', "untracked");
            continue;
        }
        if x != ' ' {
            push(x, "staged");
        }
        if y != ' ' {
            push(y, "changed");
        }
    }
    let branch = repo
        .git(&["rev-parse", "--abbrev-ref", "HEAD"])
        .1
        .trim()
        .to_string();
    Ok(json!({
        "repo": true,
        "root": repo.root(),
        "branch": branch,
        "upstream": null,
        "ahead": 0,
        "behind": 0,
        "summary": branch,
        "rows": rows,
    }))
}

/// `host.git.commit`, in the shape the host answers it: an unresolvable revision is
/// `found: false`, not an error.
fn git_commit(repo: &TempRepo, rev: &str) -> Value {
    let (ok, meta) = repo.git(&[
        "show",
        "--no-patch",
        "--format=%H%n%h%n%s%n%an%n%ad",
        "--date=short",
        rev,
    ]);
    if !ok {
        return json!({ "found": false });
    }
    let m: Vec<&str> = meta.lines().collect();
    let (_, names) = repo.git(&["show", "--name-status", "--format=", rev]);
    let files: Vec<Value> = names
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(code, path)| {
            let (detail, label) = split(path);
            json!({
                "path": path, "label": label, "detail": detail,
                "code": code.chars().next().unwrap_or('M').to_string(),
            })
        })
        .collect();
    json!({
        "found": true,
        "root": repo.root(),
        "hash": m.first().copied().unwrap_or_default(),
        "short": m.get(1).copied().unwrap_or_default(),
        "subject": m.get(2).copied().unwrap_or_default(),
        "author": m.get(3).copied().unwrap_or_default(),
        "date": m.get(4).copied().unwrap_or_default(),
        "files": files,
    })
}

// ---------------------------------------------------------------- the fake host

struct Host {
    child: Child,
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
    /// Every `host.*` request or notification the module sent, in order.
    host_calls: Vec<(String, Value)>,
    repo: TempRepo,
}

impl Host {
    fn spawn(tag: &str) -> Option<Self> {
        Self::spawn_with(tag, granted())
    }

    fn spawn_with(tag: &str, granted: Vec<Capability>) -> Option<Self> {
        let repo = TempRepo::new(tag)?;
        let (host_end, child_end) = UnixStream::pair().unwrap();
        // The host clears CLOEXEC on the child end so the descriptor survives exec.
        let fd = child_end.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(flags >= 0);
        assert!(unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } >= 0);
        let child = Command::new(env!("CARGO_BIN_EXE_avada-git"))
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
            repo,
        };
        let first = host.read_line();
        let hello: ModuleHello = serde_json::from_str(&first).expect("module.hello");
        assert_eq!(hello.manifest.module.id.to_string(), "bshuler/avada-git");
        assert!(hello.contract_min <= 1 && 1 <= hello.contract_max);
        for m in methods::MODULE_REQUIRED_V1 {
            assert!(
                hello.methods.iter().any(|x| x == m),
                "module.hello does not list {m}"
            );
        }
        let root = host.repo.root();
        host.write_line(&json!({
            "type": "host.hello",
            "contract_version": 1,
            "host_version": "0.0.0-test",
            "product": "Avada Terminal",
            "granted": granted,
            "methods": [
                methods::HOST_RAIL_REGISTER, methods::HOST_ROWS_SET,
                methods::HOST_COMMAND_REGISTER, methods::HOST_TOAST,
                methods::HOST_PANES_SPAWN, methods::HOST_EVENTS_SUBSCRIBE,
                HOST_GIT_STATUS, HOST_GIT_COMMIT,
            ],
            "data_dir": root,
            "workspace": { "id": "ws1", "name": "Test workspace", "root": root },
        }));
        Some(host)
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

    /// Answer one module→host message. The two git methods get the real repository,
    /// everything else gets `{}` (or a pane id); notifications are only recorded.
    fn handle(&mut self, line: &str) -> Option<Value> {
        let v: Value = serde_json::from_str(line).expect("json line");
        let Some(method) = v.get("method").and_then(Value::as_str).map(str::to_string) else {
            return Some(v);
        };
        let params = v.get("params").cloned().unwrap_or(Value::Null);
        if let Some(id) = v.get("id").cloned() {
            let answer = match method.as_str() {
                HOST_GIT_STATUS => {
                    let path = params["path"]
                        .as_str()
                        .unwrap_or(&self.repo.root())
                        .to_string();
                    match git_status(&self.repo, &path) {
                        Ok(ok) => json!({ "jsonrpc": "2.0", "id": id, "result": ok }),
                        Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": e }),
                    }
                }
                HOST_GIT_COMMIT => {
                    let rev = params["rev"].as_str().unwrap_or_default().to_string();
                    let result = git_commit(&self.repo, &rev);
                    json!({ "jsonrpc": "2.0", "id": id, "result": result })
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
                "entry": "git",
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
        Capability::GitRead,
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

/// `None` when git is not installed: the harness cannot make a repository, and a machine
/// without git is not a failing module.
macro_rules! host {
    ($tag:expr) => {
        match Host::spawn($tag) {
            Some(h) => h,
            None => return,
        }
    };
}

// ---------------------------------------------------------------- the tests

#[test]
fn the_handshake_registers_the_rail_the_commands_and_the_events() {
    let mut host = host!("register");
    host.wait_for(methods::HOST_ROWS_SET, 1);

    let rail = host.last(methods::HOST_RAIL_REGISTER);
    let entries = rail["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["id"], "git");
    assert_eq!(entries[0]["label"], "Git");
    assert_eq!(entries[0]["tier"], 1);

    let commands = host.last(methods::HOST_COMMAND_REGISTER);
    let ids: Vec<&str> = commands["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["refresh", "show-commit", "working-tree", "filter"]);

    let subscribed = host.last(methods::HOST_EVENTS_SUBSCRIBE);
    let kinds: Vec<&str> = subscribed["kinds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k.as_str().unwrap())
        .collect();
    assert!(kinds.contains(&methods::events::RAIL_QUERY));
    assert!(
        kinds.contains(&GIT_COMMIT_EVENT),
        "without this the commit link goes nowhere"
    );

    host.shutdown();
}

#[test]
fn the_working_tree_arrives_as_sections_of_absolute_paths_that_state_their_origin() {
    let mut host = host!("tree");
    let rows = host.next_rows();
    let root = host.repo.root();

    let seen = labels(&rows);
    for want in [
        "Staged",
        "Changed",
        "Untracked",
        "main.rs",
        "README.md",
        "notes.txt",
    ] {
        assert!(seen.contains(&want.to_string()), "no {want} in {seen:?}");
    }
    assert_eq!(seen[0], "main", "the branch is the head row");

    let staged = seen.iter().position(|l| l == "Staged").unwrap();
    let changed = seen.iter().position(|l| l == "Changed").unwrap();
    let untracked = seen.iter().position(|l| l == "Untracked").unwrap();
    assert!(
        staged < changed && changed < untracked,
        "reading order: {seen:?}"
    );

    let file = row_by_label(&rows, "notes.txt");
    assert_eq!(file["data"]["path"], json!(format!("{root}/notes.txt")));
    assert_eq!(file["data"]["code"], json!("?"));
    assert_eq!(
        file["data"]["git"],
        json!({ "root": root, "rev": null, "short": null }),
        "a null rev is how a row says `the working tree`"
    );

    // The heading carries no path, which is what stops the host drawing a file menu on it.
    let head = row_by_label(&rows, "Staged");
    assert!(head["data"].get("path").is_none());

    // The repository row IS the root: right-clicking it is the whole-tree diff.
    assert_eq!(rows[0]["data"]["path"], json!(root));

    host.shutdown();
}

#[test]
fn clicking_a_changed_file_asks_the_host_to_open_that_file() {
    let mut host = host!("open");
    let rows = host.next_rows();
    let row = row_by_label(&rows, "README.md").clone();
    let before = host.count(methods::HOST_PANES_SPAWN);

    host.activate(&row, "open");
    host.wait_for(methods::HOST_PANES_SPAWN, before + 1);
    let spawn = host.last(methods::HOST_PANES_SPAWN);
    assert_eq!(spawn["kind"], "file");
    assert_eq!(
        spawn["path"],
        json!(format!("{}/README.md", host.repo.root()))
    );

    host.shutdown();
}

#[test]
fn a_right_click_spawns_nothing_because_the_host_owns_that_menu() {
    let mut host = host!("context");
    let rows = host.next_rows();
    let row = row_by_label(&rows, "README.md").clone();
    let before = host.count(methods::HOST_PANES_SPAWN);

    host.activate(&row, "context");
    // Round-trip something else so the module has had every chance to spawn a pane.
    host.ok(methods::MODULE_COMMAND_INVOKE, json!({ "id": "refresh" }));
    assert_eq!(
        host.count(methods::HOST_PANES_SPAWN),
        before,
        "a right-click must not open a pane; the host draws the menu"
    );

    host.shutdown();
}

#[test]
fn a_commit_event_switches_the_panel_and_labels_every_row_with_the_revision() {
    let mut host = host!("commit");
    host.next_rows();
    let hash = host.repo.head();
    let root = host.repo.root();

    host.notify(
        methods::MODULE_EVENT,
        json!({
            "kind": GIT_COMMIT_EVENT,
            "payload": { "root": root, "rev": hash, "short": &hash[..7] },
        }),
    );
    let rows = host.next_rows();
    let seen = labels(&rows);
    assert!(
        seen[0].contains("the first commit"),
        "the subject is the head row: {seen:?}"
    );
    let file = row_by_label(&rows, "main.rs");
    assert_eq!(
        file["data"]["git"],
        json!({ "root": root, "rev": hash, "short": &hash[..7] }),
        "this is the fact that lets the HOST offer `Show Diff in {}`",
        &hash[..7]
    );

    // And back again, to a freshly read working tree.
    host.ok(
        methods::MODULE_COMMAND_INVOKE,
        json!({ "id": "working-tree" }),
    );
    let rows = host.next_rows();
    assert_eq!(labels(&rows)[0], "main");
    assert_eq!(
        row_by_label(&rows, "notes.txt")["data"]["git"]["rev"],
        json!(null)
    );

    host.shutdown();
}

#[test]
fn a_revision_that_does_not_resolve_says_so_instead_of_showing_the_wrong_tree() {
    let mut host = host!("missing");
    host.next_rows();

    let result = host.ok(
        methods::MODULE_COMMAND_INVOKE,
        json!({ "id": "show-commit", "args": { "rev": "0123456789abcdef0123456789abcdef01234567" } }),
    );
    assert_eq!(result["found"], json!(false));
    let rows = host.next_rows();
    assert_eq!(labels(&rows).len(), 1);
    assert!(
        labels(&rows)[0].starts_with("No commit"),
        "{:?}",
        labels(&rows)
    );

    // A blank revision is refused rather than guessed — `HEAD` would be the wrong commit.
    let e = host.err(
        methods::MODULE_COMMAND_INVOKE,
        json!({ "id": "show-commit", "args": { "rev": "" } }),
    );
    assert_eq!(e["code"], json!(-32602));

    host.shutdown();
}

#[test]
fn the_filter_narrows_the_rows_without_asking_git_again() {
    let mut host = host!("filter");
    host.next_rows();
    let before = host.count(HOST_GIT_STATUS);

    host.notify(
        methods::MODULE_EVENT,
        json!({ "kind": methods::events::RAIL_QUERY, "payload": { "query": "notes" } }),
    );
    let rows = host.next_rows();
    let seen = labels(&rows);
    assert!(seen.contains(&"notes.txt".to_string()), "{seen:?}");
    assert!(!seen.contains(&"README.md".to_string()), "{seen:?}");
    assert_eq!(
        host.count(HOST_GIT_STATUS),
        before,
        "typing in the filter box must not run git per keystroke"
    );

    host.shutdown();
}

#[test]
fn without_git_read_the_panel_says_so_instead_of_going_quietly_blank() {
    let ungranted: Vec<Capability> = granted()
        .into_iter()
        .filter(|c| *c != Capability::GitRead)
        .collect();
    let Some(mut host) = Host::spawn_with("denied", ungranted) else {
        return;
    };
    let rows = host.next_rows();
    assert_eq!(labels(&rows).len(), 1);
    assert!(
        labels(&rows)[0].contains("git.read"),
        "the row must name the capability that is missing: {:?}",
        labels(&rows)
    );
    assert_eq!(
        host.count(HOST_GIT_STATUS),
        0,
        "a module must not call a method it was not granted"
    );

    host.shutdown();
}

#[test]
fn a_path_outside_the_workspace_is_a_row_not_a_crash() {
    let mut host = host!("scope");
    host.next_rows();

    // The host refuses; the module has to keep running and say what happened.
    host.notify(
        methods::MODULE_EVENT,
        json!({ "kind": "some.future.kind", "payload": {} }),
    );
    host.ok(
        methods::MODULE_ACTIVATE,
        json!({ "entry": "git", "workspace": { "root": "/definitely/not/here" } }),
    );
    let rows = host.next_rows();
    assert_eq!(labels(&rows).len(), 1, "one row, and it explains itself");

    host.shutdown();
}
