//! The byte pipe between the host and one module process, plus the line-delimited
//! JSON-RPC framing on top of it (mirrors `avada_module_sdk::client`, which is the other
//! end of the same wire).
//!
//! Unix: an `AF_UNIX`/`SOCK_STREAM` socketpair; the child inherits its end and finds the
//! descriptor number in `AVADA_MODULE_FD`. Windows: a named pipe, landing in track H7 —
//! until then [`pair`] returns an `io::Error` so the crate compiles and every caller sees
//! a typed refusal rather than a hang.

use avada_module_sdk::client::MAX_LINE;
use avada_module_sdk::contract::{ErrorCode, Message, RpcError};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::Command;

#[cfg(unix)]
pub mod unix;

/// The host's end of the pipe. Split it with [`HostEnd::split`] once the child is running.
pub struct HostEnd {
    #[cfg(unix)]
    stream: std::os::unix::net::UnixStream,
    #[cfg(not(unix))]
    _never: std::convert::Infallible,
}

/// The child's end. [`ChildEnd::attach`] wires it into a `Command`; drop it right after
/// spawning so the host does not keep the module's side open.
pub struct ChildEnd {
    #[cfg(unix)]
    fd: std::os::fd::OwnedFd,
    #[cfg(not(unix))]
    _never: std::convert::Infallible,
}

/// Create one connected pair.
#[cfg(unix)]
pub fn pair() -> io::Result<(HostEnd, ChildEnd)> {
    unix::socketpair()
}

/// Windows: the named-pipe transport lands in track H7. Until then every spawn is refused
/// here, before any process starts.
#[cfg(not(unix))]
pub fn pair() -> io::Result<(HostEnd, ChildEnd)> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "named-pipe transport lands in track H7",
    ))
}

impl ChildEnd {
    /// Hand the descriptor to the child: keep it open across `exec` and publish its number
    /// in the SDK's environment variable.
    pub fn attach(&self, cmd: &mut Command) -> io::Result<()> {
        #[cfg(unix)]
        {
            unix::attach(&self.fd, cmd)
        }
        #[cfg(not(unix))]
        {
            let _ = cmd;
            match self._never {}
        }
    }
}

impl HostEnd {
    /// Split into a framed reader, a framed writer and a closer that can end the
    /// conversation from any thread (the reader then sees EOF).
    pub fn split(self) -> io::Result<(LineReader, LineWriter, Closer)> {
        #[cfg(unix)]
        {
            let writer = self.stream.try_clone()?;
            let closer_stream = self.stream.try_clone()?;
            let closer = Closer(Box::new(move || {
                let _ = closer_stream.shutdown(std::net::Shutdown::Both);
            }));
            Ok((
                LineReader::new(Box::new(self.stream)),
                LineWriter::new(Box::new(writer)),
                closer,
            ))
        }
        #[cfg(not(unix))]
        {
            match self._never {}
        }
    }
}

/// Ends the connection; safe to call more than once and from any thread.
pub struct Closer(Box<dyn Fn() + Send + Sync>);

impl Closer {
    /// Shut the pipe down in both directions.
    pub fn close(&self) {
        (self.0)()
    }
}

impl std::fmt::Debug for Closer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Closer")
    }
}

/// What can go wrong reading a frame.
#[derive(Debug)]
pub enum FrameError {
    /// The pipe broke.
    Io(io::Error),
    /// A line longer than [`MAX_LINE`]; the peer is misbehaving.
    LineTooLong,
    /// A line that is not a JSON-RPC 2.0 message.
    Protocol(RpcError),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::Io(e) => write!(f, "io: {e}"),
            FrameError::LineTooLong => write!(f, "line longer than {MAX_LINE} bytes"),
            FrameError::Protocol(e) => write!(f, "protocol: {e}"),
        }
    }
}
impl std::error::Error for FrameError {}
impl From<io::Error> for FrameError {
    fn from(e: io::Error) -> Self {
        FrameError::Io(e)
    }
}

/// Reads `\n`-terminated lines, refusing anything over [`MAX_LINE`].
pub struct LineReader {
    inner: BufReader<Box<dyn Read + Send>>,
}

impl LineReader {
    /// Wrap any byte source.
    pub fn new(reader: Box<dyn Read + Send>) -> Self {
        LineReader {
            inner: BufReader::new(reader),
        }
    }

    /// One line without its terminator; `Ok(None)` at EOF.
    pub fn read_line(&mut self) -> Result<Option<String>, FrameError> {
        let mut buf = Vec::new();
        loop {
            let available = self.inner.fill_buf()?;
            if available.is_empty() {
                if buf.is_empty() {
                    return Ok(None);
                }
                break;
            }
            let (used, done) = match available.iter().position(|b| *b == b'\n') {
                Some(i) => {
                    buf.extend_from_slice(&available[..i]);
                    (i + 1, true)
                }
                None => {
                    buf.extend_from_slice(available);
                    (available.len(), false)
                }
            };
            self.inner.consume(used);
            if buf.len() > MAX_LINE {
                return Err(FrameError::LineTooLong);
            }
            if done {
                break;
            }
        }
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
        String::from_utf8(buf).map(Some).map_err(|e| {
            FrameError::Protocol(RpcError::new(
                ErrorCode::ParseError,
                format!("line is not UTF-8: {e}"),
            ))
        })
    }

    /// The next JSON-RPC message; `Ok(None)` at EOF.
    pub fn read_message(&mut self) -> Result<Option<Message>, FrameError> {
        match self.read_line()? {
            None => Ok(None),
            Some(line) => Message::parse(&line)
                .map(Some)
                .map_err(FrameError::Protocol),
        }
    }
}

/// Writes one line per message, flushing each.
pub struct LineWriter {
    inner: Box<dyn Write + Send>,
}

impl LineWriter {
    /// Wrap any byte sink.
    pub fn new(writer: Box<dyn Write + Send>) -> Self {
        LineWriter { inner: writer }
    }

    /// Write `line` plus `\n`. Lines must not contain a newline themselves; serde_json
    /// never emits one.
    pub fn write_line(&mut self, line: &str) -> io::Result<()> {
        if line.len() > MAX_LINE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "line longer than MAX_LINE",
            ));
        }
        self.inner.write_all(line.as_bytes())?;
        self.inner.write_all(b"\n")?;
        self.inner.flush()
    }

    /// Write one message.
    pub fn write_message(&mut self, msg: &Message) -> io::Result<()> {
        self.write_line(&msg.to_line())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use avada_module_sdk::contract::{Notification, Request};
    use std::io::Cursor;

    #[test]
    fn reads_lines_and_messages_until_eof() {
        let req = Message::Request(Request::new(
            1,
            "host.toast",
            serde_json::json!({"text": "hi"}),
        ));
        let note =
            Message::Notification(Notification::new("module.event", serde_json::Value::Null));
        let bytes = format!("{}\n{}\r\n", req.to_line(), note.to_line());
        let mut r = LineReader::new(Box::new(Cursor::new(bytes.into_bytes())));
        assert_eq!(r.read_message().unwrap(), Some(req));
        assert_eq!(r.read_message().unwrap(), Some(note));
        assert_eq!(r.read_message().unwrap(), None);
    }

    #[test]
    fn last_line_without_newline_still_counts() {
        let mut r = LineReader::new(Box::new(Cursor::new(b"abc".to_vec())));
        assert_eq!(r.read_line().unwrap().as_deref(), Some("abc"));
        assert_eq!(r.read_line().unwrap(), None);
    }

    #[test]
    fn garbage_is_a_protocol_error_not_a_panic() {
        let mut r = LineReader::new(Box::new(Cursor::new(b"{\"nope\":1}\n".to_vec())));
        assert!(matches!(r.read_message(), Err(FrameError::Protocol(_))));
    }

    #[test]
    fn writer_appends_newline_and_refuses_oversize() {
        struct Sink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl Write for Sink {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let sink = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut w = LineWriter::new(Box::new(Sink(sink.clone())));
        w.write_line("x").unwrap();
        assert_eq!(&*sink.lock().unwrap(), b"x\n");
        let big = "y".repeat(MAX_LINE + 1);
        assert!(w.write_line(&big).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn socketpair_round_trips_and_closer_ends_it() {
        let (host, child) = pair().unwrap();
        let (mut reader, _writer, closer) = host.split().unwrap();
        // Write on the child's end directly, as a module would after `from_env`.
        let mut child_stream = child.into_stream();
        child_stream
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"module.event\"}\n")
            .unwrap();
        let msg = reader.read_message().unwrap().unwrap();
        assert!(matches!(msg, Message::Notification(n) if n.method == "module.event"));
        closer.close();
        assert!(matches!(
            reader.read_message(),
            Ok(None) | Err(FrameError::Io(_))
        ));
    }
}
