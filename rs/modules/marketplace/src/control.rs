//! The control-plane leg: one HTTP/1.1 exchange per call against the host's control
//! server (`HostHello::control_url`) with the per-run bearer token.
//!
//! No HTTP client crate: the control server is loopback `http://`, every answer is a
//! small JSON document, and `Connection: close` makes reading to EOF the whole
//! parser. The [`Control`] trait is what the rest of the module talks to, so tests
//! can swap in a canned answer set without a socket.

use std::fmt;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use serde_json::Value;

/// What went wrong with one exchange.
#[derive(Debug)]
pub enum ControlError {
    /// The host did not hand this module a control URL or token.
    Unavailable,
    /// `control_url` is not `http://host:port`.
    BadUrl(String),
    /// The socket failed.
    Io(std::io::Error),
    /// The answer was not HTTP or its body was not JSON.
    Malformed(String),
}

impl fmt::Display for ControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ControlError::Unavailable => {
                write!(f, "the host did not provide a control URL and token")
            }
            ControlError::BadUrl(u) => write!(f, "control URL is not http://host:port: {u}"),
            ControlError::Io(e) => write!(f, "control server: {e}"),
            ControlError::Malformed(m) => write!(f, "control server answer: {m}"),
        }
    }
}

impl std::error::Error for ControlError {}

impl From<std::io::Error> for ControlError {
    fn from(e: std::io::Error) -> Self {
        ControlError::Io(e)
    }
}

/// An HTTP status and the JSON body (`Value::Null` when the body was empty).
pub type Answer = (u16, Value);

/// One exchange with the control server.
pub trait Control {
    /// Send `method path` with an optional JSON body and return status + body.
    fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Answer, ControlError>;
}

/// The real thing: a TCP connection per call.
pub struct HttpControl {
    host: String,
    /// The per-run bearer. Never logged, never in `Debug`.
    token: String,
    timeout: Duration,
}

impl fmt::Debug for HttpControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpControl")
            .field("host", &self.host)
            .field("token", &"<redacted>")
            .finish()
    }
}

impl HttpControl {
    /// From the hello's `control_url` (`http://127.0.0.1:1234`) and `token`.
    pub fn new(control_url: &str, token: &str) -> Result<Self, ControlError> {
        let host = host_of(control_url)?;
        Ok(HttpControl {
            host,
            token: token.to_string(),
            timeout: Duration::from_secs(30),
        })
    }

    /// Both ends of the exchange give up after this long.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// `host:port` from `http://host:port[/...]`.
pub fn host_of(url: &str) -> Result<String, ControlError> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| ControlError::BadUrl(url.to_string()))?;
    let host = rest.split('/').next().unwrap_or("").trim_end_matches('/');
    if host.is_empty() || !host.contains(':') {
        return Err(ControlError::BadUrl(url.to_string()));
    }
    Ok(host.to_string())
}

impl Control for HttpControl {
    fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Answer, ControlError> {
        let addr = self
            .host
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| ControlError::BadUrl(self.host.clone()))?;
        let mut stream = TcpStream::connect_timeout(&addr, self.timeout)?;
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;
        let payload = body.map(|b| b.to_string()).unwrap_or_default();
        let mut req = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAuthorization: Bearer {}\r\nAccept: application/json\r\n",
            self.host, self.token
        );
        if body.is_some() {
            req.push_str(&format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                payload.len()
            ));
        }
        req.push_str("\r\n");
        req.push_str(&payload);
        stream.write_all(req.as_bytes())?;
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw)?;
        parse_response(&raw)
    }
}

/// Status line + headers + body → (status, JSON body). Chunked bodies are
/// de-chunked; anything else is taken to EOF (`Connection: close`).
pub fn parse_response(raw: &[u8]) -> Result<Answer, ControlError> {
    let text = String::from_utf8_lossy(raw);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| ControlError::Malformed("no header terminator".into()))?;
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| ControlError::Malformed("no status code".into()))?;
    let chunked = head.lines().any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });
    let body = if chunked {
        dechunk(body)
    } else {
        body.to_string()
    };
    let json = if body.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&body).unwrap_or_else(|_| Value::String(body.trim().to_string()))
    };
    Ok((status, json))
}

fn dechunk(body: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    while let Some((size_line, after)) = rest.split_once("\r\n") {
        let size = usize::from_str_radix(size_line.trim().split(';').next().unwrap_or("0"), 16)
            .unwrap_or(0);
        if size == 0 {
            break;
        }
        out.push_str(after.get(..size).unwrap_or(after));
        rest = after.get(size + 2..).unwrap_or("");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_of_accepts_only_plain_http_origins() {
        assert_eq!(host_of("http://127.0.0.1:4321").unwrap(), "127.0.0.1:4321");
        assert_eq!(host_of("http://127.0.0.1:4321/").unwrap(), "127.0.0.1:4321");
        assert!(host_of("https://127.0.0.1:4321").is_err());
        assert!(host_of("http://localhost").is_err());
        assert!(host_of("").is_err());
    }

    #[test]
    fn parse_response_reads_status_and_json_body() {
        let raw = b"HTTP/1.1 202 Accepted\r\nContent-Type: application/json\r\nContent-Length: 11\r\n\r\n{\"ok\":true}";
        let (status, body) = parse_response(raw).unwrap();
        assert_eq!(status, 202);
        assert_eq!(body["ok"], Value::Bool(true));
    }

    #[test]
    fn parse_response_dechunks() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n{\"a\":\r\n2\r\n1}\r\n0\r\n\r\n";
        let (status, body) = parse_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body["a"], 1);
    }

    #[test]
    fn parse_response_keeps_a_non_json_body_as_text() {
        let raw = b"HTTP/1.1 401 Unauthorized\r\n\r\nno bearer";
        let (status, body) = parse_response(raw).unwrap();
        assert_eq!(status, 401);
        assert_eq!(body, Value::String("no bearer".into()));
    }

    #[test]
    fn debug_never_shows_the_token() {
        let c = HttpControl::new("http://127.0.0.1:1", "secret-token").unwrap();
        assert!(!format!("{c:?}").contains("secret-token"));
    }
}
