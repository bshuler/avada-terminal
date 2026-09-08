//! The module side of the socket: framing, handshake, and a blocking request loop
//! small enough that a module needs no async runtime.
//!
//! The host spawns the module binary with one inherited descriptor:
//! * Unix: a `socketpair` end at fd 3, named in [`ENV_FD`].
//! * Windows: a named pipe whose path is in [`ENV_PIPE`].
//!
//! A module calls [`Connection::from_env`], then [`Connection::handshake`], then
//! alternates [`Connection::call`] / [`Connection::next`] as it likes. Everything
//! is a line of JSON terminated by `\n`; a line longer than [`MAX_LINE`] bytes is
//! a protocol error.

use crate::caps::Capability;
use crate::contract::{
    self, ErrorCode, HelloKind, HostHello, Id, Message, ModuleHello, Notification, Request,
    Response, RpcError, CONTRACT_VERSION,
};
use crate::manifest::Manifest;
use serde_json::Value;
use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Read, Write};

/// Unix: the inherited socket's file descriptor number.
pub const ENV_FD: &str = "AVADA_MODULE_FD";
/// Windows: the named pipe path.
pub const ENV_PIPE: &str = "AVADA_MODULE_PIPE";
/// The module's private data directory (also in the host hello).
pub const ENV_DATA_DIR: &str = "AVADA_MODULE_DATA";
/// Longest line either side will accept.
pub const MAX_LINE: usize = 16 * 1024 * 1024;

/// A framed connection over any byte stream.
pub struct Connection<R: Read, W: Write> {
    reader: BufReader<R>,
    writer: W,
    next_id: u64,
    /// Requests from the peer that arrived while waiting for a response.
    pending: VecDeque<Message>,
    /// What the host granted; set by [`Connection::handshake`].
    granted: Vec<Capability>,
    contract_version: u32,
}

/// Framing and protocol failures.
#[derive(Debug)]
pub enum ClientError {
    /// The socket.
    Io(io::Error),
    /// The peer sent something malformed.
    Protocol(RpcError),
    /// A request was answered with an error.
    Rpc(RpcError),
    /// The peer closed the stream.
    Closed,
    /// A line exceeded [`MAX_LINE`].
    LineTooLong,
    /// The hello did not negotiate.
    Handshake(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Io(e) => write!(f, "io: {e}"),
            ClientError::Protocol(e) => write!(f, "protocol: {e}"),
            ClientError::Rpc(e) => write!(f, "rpc: {e}"),
            ClientError::Closed => f.write_str("peer closed the connection"),
            ClientError::LineTooLong => write!(f, "line longer than {MAX_LINE} bytes"),
            ClientError::Handshake(s) => write!(f, "handshake: {s}"),
        }
    }
}
impl std::error::Error for ClientError {}
impl From<io::Error> for ClientError {
    fn from(e: io::Error) -> Self {
        ClientError::Io(e)
    }
}

/// Read one `\n`-terminated line, enforcing [`MAX_LINE`]. `Ok(None)` on EOF.
pub fn read_line<R: BufRead>(r: &mut R) -> Result<Option<String>, ClientError> {
    let mut buf = Vec::new();
    loop {
        let chunk = r.fill_buf()?;
        if chunk.is_empty() {
            return if buf.is_empty() {
                Ok(None)
            } else {
                Ok(Some(finish(buf)?))
            };
        }
        match chunk.iter().position(|&b| b == b'\n') {
            Some(i) => {
                buf.extend_from_slice(&chunk[..i]);
                r.consume(i + 1);
                if buf.len() > MAX_LINE {
                    return Err(ClientError::LineTooLong);
                }
                return Ok(Some(finish(buf)?));
            }
            None => {
                buf.extend_from_slice(chunk);
                let n = chunk.len();
                r.consume(n);
                if buf.len() > MAX_LINE {
                    return Err(ClientError::LineTooLong);
                }
            }
        }
    }
}

fn finish(mut buf: Vec<u8>) -> Result<String, ClientError> {
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
    String::from_utf8(buf)
        .map_err(|e| ClientError::Protocol(RpcError::new(ErrorCode::ParseError, e.to_string())))
}

/// Write one line.
pub fn write_line<W: Write>(w: &mut W, line: &str) -> Result<(), ClientError> {
    if line.len() > MAX_LINE {
        return Err(ClientError::LineTooLong);
    }
    w.write_all(line.as_bytes())?;
    w.write_all(b"\n")?;
    w.flush()?;
    Ok(())
}

impl<R: Read, W: Write> Connection<R, W> {
    /// Wrap a stream pair.
    pub fn new(reader: R, writer: W) -> Self {
        Connection {
            reader: BufReader::new(reader),
            writer,
            next_id: 1,
            pending: VecDeque::new(),
            granted: Vec::new(),
            contract_version: 0,
        }
    }

    /// Send the module hello and read the host's answer.
    pub fn handshake(
        &mut self,
        manifest: Manifest,
        methods: Vec<String>,
    ) -> Result<HostHello, ClientError> {
        let hello = ModuleHello {
            kind: HelloKind::Module,
            manifest,
            contract_min: CONTRACT_VERSION,
            contract_max: CONTRACT_VERSION,
            methods,
            sdk_version: env!("CARGO_PKG_VERSION").to_string(),
        };
        write_line(
            &mut self.writer,
            &serde_json::to_string(&hello).expect("hello serializes"),
        )?;
        let line = read_line(&mut self.reader)?.ok_or(ClientError::Closed)?;
        let host: HostHello = serde_json::from_str(&line)
            .map_err(|e| ClientError::Handshake(format!("host hello does not parse: {e}")))?;
        if host.kind != HelloKind::Host {
            return Err(ClientError::Handshake(
                "first line was not host.hello".into(),
            ));
        }
        if contract::negotiate(
            CONTRACT_VERSION,
            CONTRACT_VERSION,
            host.contract_version,
            host.contract_version,
        )
        .is_none()
        {
            return Err(ClientError::Handshake(format!(
                "host speaks contract {} and this SDK speaks {CONTRACT_VERSION}",
                host.contract_version
            )));
        }
        self.granted = host.granted.clone();
        self.contract_version = host.contract_version;
        Ok(host)
    }

    /// Capabilities the host granted.
    pub fn granted(&self) -> &[Capability] {
        &self.granted
    }

    /// Negotiated version (0 before handshake).
    pub fn contract_version(&self) -> u32 {
        self.contract_version
    }

    /// Whether a capability was granted.
    pub fn has(&self, cap: Capability) -> bool {
        self.granted.contains(&cap)
    }

    /// Send a request and block for its response. Requests and notifications from the
    /// peer that arrive meanwhile are queued for [`Connection::next`].
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, ClientError> {
        if let Some(cap) = contract::methods::required_capability(method) {
            if !self.has(cap) {
                return Err(ClientError::Rpc(RpcError::new(
                    ErrorCode::CapabilityDenied,
                    format!("`{cap}` was not granted at install"),
                )));
            }
        }
        let id = self.next_id;
        self.next_id += 1;
        let req = Request::new(id, method, params);
        write_line(&mut self.writer, &Message::Request(req).to_line())?;
        loop {
            let msg = self.read_message()?;
            match msg {
                Message::Response(resp) if resp.id == Id::Num(id) => {
                    return match (resp.result, resp.error) {
                        (_, Some(e)) => Err(ClientError::Rpc(e)),
                        (Some(v), None) => Ok(v),
                        (None, None) => Ok(Value::Null),
                    };
                }
                other => self.pending.push_back(other),
            }
        }
    }

    /// Send a notification.
    pub fn notify(&mut self, method: &str, params: Value) -> Result<(), ClientError> {
        write_line(
            &mut self.writer,
            &Message::Notification(Notification::new(method, params)).to_line(),
        )
    }

    /// Answer a request from the peer.
    pub fn respond(&mut self, resp: Response) -> Result<(), ClientError> {
        write_line(&mut self.writer, &Message::Response(resp).to_line())
    }

    /// The next message from the peer (queued first, then the stream). `Ok(None)` on EOF.
    pub fn recv(&mut self) -> Result<Option<Message>, ClientError> {
        if let Some(m) = self.pending.pop_front() {
            return Ok(Some(m));
        }
        match self.read_message() {
            Ok(m) => Ok(Some(m)),
            Err(ClientError::Closed) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn read_message(&mut self) -> Result<Message, ClientError> {
        let line = read_line(&mut self.reader)?.ok_or(ClientError::Closed)?;
        Message::parse(&line).map_err(ClientError::Protocol)
    }
}

/// Open the connection the host handed this process: an inherited socketpair end on
/// Unix (`AVADA_MODULE_FD`), a named pipe on Windows (`AVADA_MODULE_PIPE`).
#[cfg(unix)]
#[allow(unsafe_code)] // the one place a raw fd enters the SDK; see SAFETY below
pub fn from_env(
) -> Result<Connection<std::os::unix::net::UnixStream, std::os::unix::net::UnixStream>, ClientError>
{
    use std::os::unix::io::FromRawFd;
    let fd: i32 = std::env::var(ENV_FD)
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| {
            ClientError::Handshake(format!(
                "{ENV_FD} is not set; was this binary launched by the host?"
            ))
        })?;
    // SAFETY: the host created this descriptor for us and nothing else in this
    // process owns it; the env var is the contract that says so.
    let stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };
    let writer = stream.try_clone()?;
    Ok(Connection::new(stream, writer))
}

/// Windows: the host listens on a named pipe and puts its path in `AVADA_MODULE_PIPE`;
/// opening it read+write is the connect. One handle is duplicated so the reader and
/// writer halves can live on different threads, as on Unix.
#[cfg(windows)]
pub fn from_env() -> Result<Connection<std::fs::File, std::fs::File>, ClientError> {
    let path = std::env::var(ENV_PIPE).map_err(|_| {
        ClientError::Handshake(format!(
            "{ENV_PIPE} is not set; was this binary launched by the host?"
        ))
    })?;
    let stream = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)?;
    let writer = stream.try_clone()?;
    Ok(Connection::new(stream, writer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::methods;
    use serde_json::json;
    use std::io::Cursor;

    fn manifest() -> Manifest {
        Manifest::parse(crate::manifest::tests::EXAMPLE).unwrap()
    }

    fn host_hello(granted: Vec<Capability>) -> String {
        serde_json::to_string(&HostHello {
            kind: HelloKind::Host,
            contract_version: CONTRACT_VERSION,
            host_version: "0.0.37".into(),
            product: crate::PRODUCT_NAME.into(),
            granted,
            methods: methods::HOST_REQUIRED_V1
                .iter()
                .map(|s| s.to_string())
                .collect(),
            data_dir: "/tmp/d".into(),
            workspace: None,
            token: None,
        })
        .unwrap()
    }

    #[test]
    fn read_line_handles_eof_crlf_and_limits() {
        let mut r = BufReader::new(Cursor::new(b"a\r\nb\nc".to_vec()));
        assert_eq!(read_line(&mut r).unwrap().as_deref(), Some("a"));
        assert_eq!(read_line(&mut r).unwrap().as_deref(), Some("b"));
        assert_eq!(read_line(&mut r).unwrap().as_deref(), Some("c"));
        assert_eq!(read_line(&mut r).unwrap(), None);
        let mut r = BufReader::new(Cursor::new(vec![0xff, b'\n']));
        assert!(matches!(read_line(&mut r), Err(ClientError::Protocol(_))));
        let mut w = Vec::new();
        assert!(matches!(
            write_line(&mut w, &"x".repeat(MAX_LINE + 1)),
            Err(ClientError::LineTooLong)
        ));
    }

    #[test]
    fn handshake_sends_module_hello_and_records_grants() {
        let input = format!("{}\n", host_hello(vec![Capability::UiToast]));
        let mut out = Vec::new();
        let mut c = Connection::new(Cursor::new(input.into_bytes()), &mut out);
        let host = c.handshake(manifest(), vec![]).unwrap();
        assert_eq!(host.product, "Avada Terminal");
        assert!(c.has(Capability::UiToast) && !c.has(Capability::FsRead));
        assert_eq!(c.contract_version(), CONTRACT_VERSION);
        let sent = String::from_utf8(out).unwrap();
        let hello: ModuleHello = serde_json::from_str(sent.trim_end()).unwrap();
        assert_eq!(hello.kind, HelloKind::Module);
        assert_eq!(hello.manifest, manifest());
        assert_eq!((hello.contract_min, hello.contract_max), (1, 1));
    }

    #[test]
    fn handshake_refuses_wrong_version_or_wrong_line() {
        let mut h: Value = serde_json::from_str(&host_hello(vec![])).unwrap();
        h["contract_version"] = json!(99);
        let mut out = Vec::new();
        let mut c = Connection::new(Cursor::new(format!("{h}\n").into_bytes()), &mut out);
        assert!(matches!(
            c.handshake(manifest(), vec![]),
            Err(ClientError::Handshake(_))
        ));
        let mut out = Vec::new();
        let mut c = Connection::new(
            Cursor::new(b"{\"type\":\"module.hello\"}\n".to_vec()),
            &mut out,
        );
        assert!(matches!(
            c.handshake(manifest(), vec![]),
            Err(ClientError::Handshake(_))
        ));
        let mut out = Vec::new();
        let mut c = Connection::new(Cursor::new(Vec::new()), &mut out);
        assert!(matches!(
            c.handshake(manifest(), vec![]),
            Err(ClientError::Closed)
        ));
    }

    #[test]
    fn call_matches_ids_and_queues_interleaved_traffic() {
        // Host: hello, then an activate request, then the toast response, then EOF.
        let activate = Message::Request(Request::new(
            1,
            methods::MODULE_ACTIVATE,
            json!({"workspace": {"id": "w", "name": "W"}}),
        ))
        .to_line();
        let toast_ok = Message::Response(Response {
            jsonrpc: Default::default(),
            id: Id::Num(1),
            result: Some(json!({"shown": true})),
            error: None,
        })
        .to_line();
        let input = format!(
            "{}\n{activate}\n{toast_ok}\n",
            host_hello(vec![Capability::UiToast])
        );
        let mut out = Vec::new();
        let mut c = Connection::new(Cursor::new(input.into_bytes()), &mut out);
        c.handshake(manifest(), vec![]).unwrap();
        let r = c.call(methods::HOST_TOAST, json!({"text": "hi"})).unwrap();
        assert_eq!(r["shown"], true);
        match c.recv().unwrap() {
            Some(Message::Request(r)) => assert_eq!(r.method, methods::MODULE_ACTIVATE),
            other => panic!("{other:?}"),
        }
        assert!(c.recv().unwrap().is_none(), "EOF is Ok(None)");
    }

    #[test]
    fn call_without_grant_fails_locally_and_writes_nothing() {
        let input = format!("{}\n", host_hello(vec![]));
        let mut c = Connection::new(Cursor::new(input.into_bytes()), Vec::new());
        c.handshake(manifest(), vec![]).unwrap();
        let before = c.writer.len();
        match c.call(methods::HOST_FS_READ, json!({"path": "/x"})) {
            Err(ClientError::Rpc(e)) => assert_eq!(e.kind(), ErrorCode::CapabilityDenied),
            other => panic!("{other:?}"),
        }
        assert_eq!(c.writer.len(), before, "nothing was sent to the host");
    }

    #[test]
    fn rpc_errors_surface_and_protocol_junk_is_typed() {
        let err = Message::Response(Response {
            jsonrpc: Default::default(),
            id: Id::Num(1),
            result: None,
            error: Some(RpcError::new(ErrorCode::UserDenied, "no")),
        })
        .to_line();
        let input = format!("{}\n{err}\n", host_hello(vec![Capability::UiToast]));
        let mut out = Vec::new();
        let mut c = Connection::new(Cursor::new(input.into_bytes()), &mut out);
        c.handshake(manifest(), vec![]).unwrap();
        match c.call(methods::HOST_TOAST, json!({})) {
            Err(ClientError::Rpc(e)) => assert_eq!(e.kind(), ErrorCode::UserDenied),
            other => panic!("{other:?}"),
        }
        let input = format!("{}\nnot json\n", host_hello(vec![]));
        let mut out = Vec::new();
        let mut c = Connection::new(Cursor::new(input.into_bytes()), &mut out);
        c.handshake(manifest(), vec![]).unwrap();
        assert!(matches!(c.recv(), Err(ClientError::Protocol(_))));
    }

    #[cfg(unix)]
    #[test]
    fn from_env_without_the_variable_is_a_clear_error() {
        // The variable is never set in the test process; do not set it (it would leak
        // across parallel tests). The error must name the variable.
        match from_env() {
            Err(ClientError::Handshake(s)) => assert!(s.contains(ENV_FD)),
            other => panic!("{:?}", other.map(|_| ())),
        }
    }
}
