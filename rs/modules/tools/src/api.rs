//! The host's control server, as this module sees it.
//!
//! Everything the tool entries are drawn from is a fact the host already owns: which AI
//! CLIs exist and where this machine has them (`GET /tools`), what conversations each one
//! can resume (`GET /tools/{tool}/sessions`), which tools the human starred
//! (`GET /settings`), and where to put a pane (`GET /state`, then `POST /command`).
//!
//! The transport is a plain loopback `TcpStream` speaking HTTP/1.1 by hand, not an HTTP
//! crate and not `net.fetch`. Both of those would be wrong for different reasons: a crate
//! would pull an async runtime and a TLS stack into a process whose entire network life is
//! one connection to `127.0.0.1`, and `net.fetch` gates the *host's* fetching on the
//! module's behalf, which is not what is happening here — the module is a client of the
//! host's own API, authenticated by the per-run bearer token in the hello.
//!
//! [`Api`] exists so the state machine in [`crate::app`] can be tested without a socket.

use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// How long any one control-server call may take.
///
/// Generous on purpose: a cold transcript store is a few thousand files and the host reads
/// it on a blocking thread. The module has nothing else to do while it waits — the host
/// queues anything that arrives on the socket meanwhile — and a timeout that fired early
/// would show an empty entry for a tool that has plenty of history.
pub const TIMEOUT: Duration = Duration::from_secs(60);

/// One AI CLI, out of `GET /tools`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    /// The registry id (`claude`, `cursor-agent`, …). Also the rail entry id.
    pub id: String,
    /// Its human name (`Claude Code`).
    pub name: String,
    /// `#rrggbb`, the tool's own brand colour.
    #[serde(default)]
    pub brand: String,
    /// Whether Avada has a transcript reader for it. A tool without one can never have a
    /// rail entry: there would be nothing to put in it.
    #[serde(default)]
    pub has_history: bool,
    /// Where this machine has the binary, if anywhere.
    #[serde(default)]
    pub path: Option<String>,
    /// How it was found: `override`, `path`, or `wellKnown`.
    #[serde(default)]
    pub source: Option<String>,
}

impl Tool {
    /// Whether the binary was found. An absent one still lists its history — the
    /// transcripts are on disk either way — but every row is blocked.
    pub fn installed(&self) -> bool {
        self.path.is_some()
    }
}

/// What resuming one conversation would run.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
pub struct Resume {
    /// The absolute path to the tool's binary.
    pub command: String,
    /// Its arguments, already in the right order.
    pub args: Vec<String>,
    /// The directory it must run in — the project the conversation happened in.
    pub cwd: String,
}

/// One resumable conversation, out of `GET /tools/{tool}/sessions`.
///
/// Note what is *not* here: the transcript. The host reads the whole conversation to build
/// this row and then sends a summary, a first line and a count. A rail module has no
/// business holding the text of somebody's conversations, and not sending it also keeps a
/// 200-session answer down to something a pipe can carry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    /// The tool's own resume key.
    pub id: String,
    /// The project directory the conversation happened in.
    pub project: String,
    /// Whether that directory is the real one or a guess decoded from a store path.
    #[serde(default)]
    pub project_exact: bool,
    /// The git branch it was on, when the transcript recorded one.
    #[serde(default)]
    pub branch: Option<String>,
    /// Epoch milliseconds of the first message.
    #[serde(default)]
    pub started_at: Option<u64>,
    /// The tool's own summary of the conversation, when it wrote one.
    #[serde(default)]
    pub summary: String,
    /// The first thing the human typed.
    #[serde(default)]
    pub first_user: String,
    /// How many messages the transcript holds.
    #[serde(default)]
    pub message_count: usize,
    /// `Some` when a click can resume it.
    #[serde(default)]
    pub resume: Option<Resume>,
    /// Why it cannot be resumed, when it cannot. Exactly one of this and `resume` is set.
    #[serde(default)]
    pub blocked: Option<String>,
    /// The Claude Desktop id holding the same conversation, when there is one.
    #[serde(default)]
    pub desktop: Option<String>,
}

/// The pane a resume asks the host for.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PaneSpec {
    /// Binary to run.
    pub command: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// Where to run it.
    pub cwd: String,
    /// The pane's title.
    pub label: String,
    /// Its second line.
    pub subtitle: String,
    /// The pane's frame colour — the tool's brand, so a resumed Claude pane looks like one.
    pub color: String,
}

/// Everything this module asks the host for.
pub trait Api {
    /// Every tool the host knows, and where this machine has it.
    fn tools(&mut self) -> Result<Vec<Tool>, String>;
    /// One tool's resumable conversations.
    fn sessions(&mut self, tool: &str) -> Result<Vec<Session>, String>;
    /// The tool ids the human starred, in their chosen order.
    ///
    /// `Ok(None)` means the host could not say — a headless host has no preferences at all,
    /// which is different from a human who starred nothing.
    fn favourites(&mut self) -> Result<Option<Vec<String>>, String>;
    /// Open a pane running `spec`; the pane id comes back.
    fn spawn(&mut self, spec: &PaneSpec) -> Result<String, String>;

    /// Now, in epoch milliseconds.
    ///
    /// The clock is here rather than in the state machine because "3h ago" is the one
    /// piece of a row that changes without anything happening, and a test that cannot
    /// pin the clock cannot assert on a detail line at all.
    fn now_ms(&mut self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or_default()
    }
}

/// So `main` can hold one of two clients — a live one or a dark one — behind a single
/// type while the state machine still takes a plain `&mut impl Api`.
impl Api for Box<dyn Api> {
    fn tools(&mut self) -> Result<Vec<Tool>, String> {
        (**self).tools()
    }
    fn sessions(&mut self, tool: &str) -> Result<Vec<Session>, String> {
        (**self).sessions(tool)
    }
    fn favourites(&mut self) -> Result<Option<Vec<String>>, String> {
        (**self).favourites()
    }
    fn spawn(&mut self, spec: &PaneSpec) -> Result<String, String> {
        (**self).spawn(spec)
    }
    fn now_ms(&mut self) -> u64 {
        (**self).now_ms()
    }
}

// ------------------------------------------------------------------ the real client

/// The host's control server over loopback HTTP.
#[derive(Debug, Clone)]
pub struct Control {
    /// `host:port` from the hello's `control_url`.
    authority: String,
    /// The per-run bearer token from the hello.
    token: String,
}

impl Control {
    /// Build a client from the two hello fields. `None` when the host offered no control
    /// server — an older host, or one that started without one.
    pub fn new(control_url: Option<&str>, token: Option<&str>) -> Option<Self> {
        let url = control_url?;
        let token = token?;
        let rest = url
            .strip_prefix("http://")
            .or_else(|| url.strip_prefix("https://"))?;
        let authority = rest.trim_end_matches('/').split('/').next()?.to_string();
        if authority.is_empty() {
            return None;
        }
        Some(Control {
            authority,
            token: token.to_string(),
        })
    }

    /// One request/response, over one connection. `Connection: close` rather than keep-alive
    /// because it makes "the body ends at EOF" true, and a module makes a handful of calls a
    /// minute — there is no pool worth keeping.
    fn request(&self, verb: &str, path: &str, body: Option<&Value>) -> Result<Value, String> {
        let mut sock = TcpStream::connect(&self.authority)
            .map_err(|e| format!("cannot reach the control server: {e}"))?;
        sock.set_read_timeout(Some(TIMEOUT)).ok();
        sock.set_write_timeout(Some(TIMEOUT)).ok();
        let payload = body.map(|b| b.to_string()).unwrap_or_default();
        let mut head = format!(
            "{verb} {path} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\n\
             Accept: application/json\r\nConnection: close\r\n",
            self.authority, self.token
        );
        if body.is_some() {
            head.push_str(&format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                payload.len()
            ));
        }
        head.push_str("\r\n");
        head.push_str(&payload);
        sock.write_all(head.as_bytes())
            .and_then(|()| sock.flush())
            .map_err(|e| format!("cannot send {verb} {path}: {e}"))?;
        let mut raw = Vec::new();
        sock.read_to_end(&mut raw)
            .map_err(|e| format!("no answer to {verb} {path}: {e}"))?;
        parse_response(&raw, verb, path)
    }
}

/// Split an HTTP/1.1 response into a status and a JSON body, turning a non-2xx into the
/// host's own `error` string.
fn parse_response(raw: &[u8], verb: &str, path: &str) -> Result<Value, String> {
    let text = String::from_utf8_lossy(raw);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| format!("{verb} {path}: truncated response"))?;
    let status: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("{verb} {path}: no status line"))?;
    let value: Value = serde_json::from_str(body.trim()).unwrap_or(Value::Null);
    if (200..300).contains(&status) {
        return Ok(value);
    }
    // The host's own words. It says "capability" and names the one it wanted, which is the
    // single most useful thing a module operator can be told, so it is not paraphrased.
    let why = value
        .get("error")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| body.trim().chars().take(200).collect());
    Err(format!(
        "{path}: HTTP {status}{}{why}",
        if why.is_empty() { "" } else { " — " }
    ))
}

impl Api for Control {
    fn tools(&mut self) -> Result<Vec<Tool>, String> {
        let v = self.request("GET", "/tools", None)?;
        serde_json::from_value(v["tools"].clone()).map_err(|e| format!("/tools: {e}"))
    }

    fn sessions(&mut self, tool: &str) -> Result<Vec<Session>, String> {
        let v = self.request("GET", &format!("/tools/{tool}/sessions"), None)?;
        serde_json::from_value(v["sessions"].clone()).map_err(|e| format!("/tools/{tool}: {e}"))
    }

    fn favourites(&mut self) -> Result<Option<Vec<String>>, String> {
        // A host with no GUI answers 503 here. That is not a failure worth showing anyone:
        // it means there are no preferences to read, and the caller falls back.
        let v = match self.request("GET", "/settings", None) {
            Ok(v) => v,
            Err(e) if e.contains("HTTP 503") => return Ok(None),
            Err(e) => return Err(e),
        };
        Ok(v["settings"]["toolFavorites"].as_array().map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        }))
    }

    fn spawn(&mut self, spec: &PaneSpec) -> Result<String, String> {
        // `newPane` needs a window to put the pane in and the module has never been told
        // one, so it asks. The first window is the right default: it is the one the human
        // is looking at when there is only one, and the panel that shows this rail lives
        // in a window either way.
        let state = self.request("GET", "/state", None)?;
        let window = state["windows"]
            .as_array()
            .and_then(|w| w.first())
            .and_then(|w| w["windowId"].as_i64())
            .ok_or_else(|| "no window to open a pane in".to_string())?;
        let body = json!({
            "type": "newPane",
            "windowId": window,
            "pane": {
                "command": spec.command,
                "args": spec.args,
                "cwd": spec.cwd,
                "label": spec.label,
                "subtitle": spec.subtitle,
                "color": spec.color,
            },
        });
        let v = self.request("POST", "/command", Some(&body))?;
        Ok(v["result"]
            .as_str()
            .or_else(|| v.as_str())
            .unwrap_or_default()
            .to_string())
    }
}

/// The client for a host that offered no control server: every call fails with the same
/// sentence, and the entry says so instead of looking empty.
#[derive(Debug, Default, Clone, Copy)]
pub struct Offline;

/// What [`Offline`] answers, and what the rows will read.
pub const NO_CONTROL: &str = "this host offered no control server";

impl Api for Offline {
    fn tools(&mut self) -> Result<Vec<Tool>, String> {
        Err(NO_CONTROL.into())
    }
    fn sessions(&mut self, _tool: &str) -> Result<Vec<Session>, String> {
        Err(NO_CONTROL.into())
    }
    fn favourites(&mut self) -> Result<Option<Vec<String>>, String> {
        Err(NO_CONTROL.into())
    }
    fn spawn(&mut self, _spec: &PaneSpec) -> Result<String, String> {
        Err(NO_CONTROL.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_control_url_becomes_an_authority_and_anything_else_becomes_nothing() {
        let c = Control::new(Some("http://127.0.0.1:8899"), Some("tok")).unwrap();
        assert_eq!(c.authority, "127.0.0.1:8899");
        assert_eq!(c.token, "tok");
        // A trailing path is not part of the authority.
        let c = Control::new(Some("http://127.0.0.1:8899/"), Some("tok")).unwrap();
        assert_eq!(c.authority, "127.0.0.1:8899");
        // A hello without one of the two fields is not half a client.
        assert!(Control::new(None, Some("tok")).is_none());
        assert!(Control::new(Some("http://127.0.0.1:1"), None).is_none());
        assert!(Control::new(Some("127.0.0.1:8899"), Some("tok")).is_none());
        assert!(Control::new(Some("http://"), Some("tok")).is_none());
    }

    #[test]
    fn a_2xx_yields_the_body_and_a_4xx_yields_the_hosts_own_words() {
        let ok = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"tools\":[]}";
        assert_eq!(
            parse_response(ok, "GET", "/tools").unwrap()["tools"],
            json!([])
        );

        let denied = b"HTTP/1.1 403 Forbidden\r\n\r\n{\"error\":\"capability\",\"capability\":\"fs.read_any\"}";
        let e = parse_response(denied, "GET", "/tools/claude/sessions").unwrap_err();
        assert!(e.contains("403"), "{e}");
        assert!(e.contains("capability"), "{e}");
    }

    #[test]
    fn a_body_that_is_not_json_is_still_reported_rather_than_swallowed() {
        let odd = b"HTTP/1.1 500 Internal Server Error\r\n\r\nsomething broke";
        let e = parse_response(odd, "GET", "/tools").unwrap_err();
        assert!(e.contains("500") && e.contains("something broke"), "{e}");
        // A 2xx whose body is not JSON is a null value, not an error: the caller's
        // `from_value` will say what was missing, naming the field.
        let empty = b"HTTP/1.1 204 No Content\r\n\r\n";
        assert!(parse_response(empty, "GET", "/x").unwrap().is_null());
    }

    #[test]
    fn a_response_with_no_header_break_is_an_error_not_a_panic() {
        assert!(parse_response(b"HTTP/1.1 200 OK", "GET", "/tools").is_err());
        assert!(parse_response(b"", "GET", "/tools").is_err());
        assert!(parse_response(b"garbage\r\n\r\n{}", "GET", "/tools").is_err());
    }

    #[test]
    fn an_offline_host_fails_every_call_with_one_sentence() {
        let mut o = Offline;
        assert_eq!(o.tools().unwrap_err(), NO_CONTROL);
        assert_eq!(o.sessions("claude").unwrap_err(), NO_CONTROL);
        assert_eq!(o.favourites().unwrap_err(), NO_CONTROL);
        assert_eq!(o.spawn(&PaneSpec::default()).unwrap_err(), NO_CONTROL);
    }

    #[test]
    fn a_session_deserialises_from_what_the_host_actually_sends() {
        let v = json!({
            "id": "abc", "project": "/w/p", "projectExact": true,
            "branch": "main", "startedAt": 1_700_000_000_000u64,
            "summary": "Fix the parser", "firstUser": "hello", "messageCount": 4,
            "resume": { "command": "/usr/bin/claude", "args": ["--resume", "abc"], "cwd": "/w/p" },
        });
        let s: Session = serde_json::from_value(v).unwrap();
        assert_eq!(s.message_count, 4);
        assert!(s.project_exact);
        assert_eq!(s.resume.unwrap().args, ["--resume", "abc"]);
        assert!(s.blocked.is_none());
        // Every field the host omits has a default, so a newer host that stops sending an
        // optional does not stop this module from drawing a row.
        let bare: Session = serde_json::from_value(json!({ "id": "x", "project": "/p" })).unwrap();
        assert_eq!(bare.summary, "");
        assert!(bare.started_at.is_none());
    }
}
