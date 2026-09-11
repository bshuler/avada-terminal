//! Windows named-pipe transport (track H7): one `\\.\pipe\avada-module-<pid>-<random>`
//! instance per module, created by the host with an owner-only DACL
//! (`persistence::acl_windows`), the child told its path in `AVADA_MODULE_PIPE`.
//!
//! Shape, mirroring the Unix socketpair:
//!
//! * [`create_pair`] makes the pipe (`FILE_FLAG_FIRST_PIPE_INSTANCE`, one instance,
//!   `PIPE_REJECT_REMOTE_CLIENTS`, byte mode, overlapped). Nothing blocks here.
//! * `ChildEnd::attach` only publishes the path in the environment; the child opens it
//!   by name (`CreateFileW` — what `std::fs::OpenOptions::new().read(true).write(true)`
//!   does), so no handle is inherited.
//! * `HostEnd::split` hands out a reader, a writer and a closer. The first read or
//!   write waits for the module to connect (`ConnectNamedPipe`), so `split` itself never
//!   hangs on a child that dies before opening the pipe.
//! * Every read and write is an overlapped operation waiting on two events: its own
//!   completion and a shared, manual-reset *shutdown* event. The closer sets that event,
//!   cancels the pending I/O and disconnects the pipe, so a blocked reader sees EOF and
//!   the module sees its end break — the `shutdown(Both)` of the Unix path.
//!
//! The pipe-name derivation, the random suffix and the environment plumbing are pure and
//! tested on every OS; the Win32 half is `#[cfg(windows)]`.

#![cfg_attr(not(windows), allow(dead_code))]

use avada_module_sdk::client::ENV_PIPE;
use std::process::Command;

/// Prefix every module pipe name carries, under the local pipe namespace.
pub const PIPE_PREFIX: &str = r"\\.\pipe\avada-module-";

/// Bytes of randomness in the suffix (hex-encoded, so twice as many characters).
const SUFFIX_BYTES: usize = 16;

/// The pipe name for host `pid` and a [`random_suffix`]:
/// `\\.\pipe\avada-module-<pid>-<suffix>`.
pub fn pipe_name(pid: u32, suffix: &str) -> String {
    format!("{PIPE_PREFIX}{pid}-{suffix}")
}

/// 128 bits from the OS RNG as lowercase hex — unguessable, so nobody can pre-create the
/// name (and `FILE_FLAG_FIRST_PIPE_INSTANCE` refuses the create if somebody did).
pub fn random_suffix() -> String {
    use rand::Rng;
    let mut bytes = [0u8; SUFFIX_BYTES];
    rand::rng().fill_bytes(&mut bytes);
    let mut out = String::with_capacity(SUFFIX_BYTES * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// True for a name this module would have produced for `pid`.
pub fn is_module_pipe_name(name: &str, pid: u32) -> bool {
    let Some(rest) = name.strip_prefix(PIPE_PREFIX) else {
        return false;
    };
    let Some((p, suffix)) = rest.split_once('-') else {
        return false;
    };
    p == pid.to_string()
        && suffix.len() == SUFFIX_BYTES * 2
        && suffix
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Tell the child where the pipe is. The only thing `attach` has to do on Windows.
pub fn attach_env(name: &str, cmd: &mut Command) {
    cmd.env(ENV_PIPE, name);
}

#[cfg(windows)]
pub use win::{create_pair, split, ChildPipe, HostPipe};

#[cfg(windows)]
mod win {
    use super::super::{ChildEnd, Closer, HostEnd, LineReader, LineWriter};
    use super::{pipe_name, random_suffix};
    use crate::persistence::acl_windows::OwnerOnly;
    use std::io::{self, Read, Write};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use windows::core::{HRESULT, HSTRING};
    use windows::Win32::Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, ERROR_BROKEN_PIPE, ERROR_HANDLE_EOF, ERROR_IO_PENDING,
        ERROR_NO_DATA, ERROR_OPERATION_ABORTED, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED,
        ERROR_PIPE_NOT_CONNECTED, HANDLE, WAIT_EVENT, WAIT_OBJECT_0, WIN32_ERROR,
    };
    use windows::Win32::Storage::FileSystem::{
        ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED,
        PIPE_ACCESS_DUPLEX,
    };
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
        PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
    };
    use windows::Win32::System::Threading::{
        CreateEventW, SetEvent, WaitForMultipleObjects, INFINITE,
    };
    use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

    /// Pipe buffer hint in each direction. Advisory; lines up to `MAX_LINE` still flow.
    const BUFFER_SIZE: u32 = 64 * 1024;

    /// Fresh names tried when a create fails because the name is taken.
    const CREATE_ATTEMPTS: u32 = 3;

    /// Owning wrapper for a kernel handle. `Send + Sync`: a HANDLE is a kernel index and
    /// every call made through it here is documented thread-safe.
    struct Handle(HANDLE);
    #[allow(unsafe_code)]
    unsafe impl Send for Handle {}
    #[allow(unsafe_code)]
    unsafe impl Sync for Handle {}
    impl Drop for Handle {
        #[allow(unsafe_code)]
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                // SAFETY: we own the handle and nothing uses it after drop.
                let _ = unsafe { CloseHandle(self.0) };
            }
        }
    }

    /// A manual-reset event: stays signalled once set, so a waiter that arrives late
    /// still wakes.
    #[allow(unsafe_code)]
    fn manual_reset_event() -> io::Result<Handle> {
        // SAFETY: no attributes, no name; the returned handle is owned by `Handle`.
        let h = unsafe { CreateEventW(None, true, false, None) }?;
        Ok(Handle(h))
    }

    /// Auto-reset event for one overlapped operation at a time.
    #[allow(unsafe_code)]
    fn auto_reset_event() -> io::Result<Handle> {
        // SAFETY: as above.
        let h = unsafe { CreateEventW(None, false, false, None) }?;
        Ok(Handle(h))
    }

    fn is_win32(e: &windows::core::Error, code: WIN32_ERROR) -> bool {
        e.code() == HRESULT::from_win32(code.0)
    }

    /// Errors that mean "the other side is gone" and read as EOF.
    fn is_eof(e: &windows::core::Error) -> bool {
        [
            ERROR_BROKEN_PIPE,
            ERROR_PIPE_NOT_CONNECTED,
            ERROR_NO_DATA,
            ERROR_OPERATION_ABORTED,
            ERROR_HANDLE_EOF,
        ]
        .iter()
        .any(|c| is_win32(e, *c))
    }

    fn broken() -> io::Error {
        io::Error::new(io::ErrorKind::BrokenPipe, "module pipe closed")
    }

    /// What the host side keeps: the server handle plus the shutdown machinery.
    struct Shared {
        pipe: Handle,
        shutdown: Handle,
        /// Serialises the one-time `ConnectNamedPipe`.
        connect: Mutex<()>,
        connected: AtomicBool,
        closed: AtomicBool,
    }

    /// Outcome of waiting on one overlapped operation.
    enum Outcome {
        /// Completed; the byte count.
        Done(u32),
        /// The closer fired first; the operation was cancelled and reaped.
        Closed,
    }

    impl Shared {
        /// Wait for `ov` (already submitted) or the shutdown event, whichever comes first.
        /// On return the operation is finished either way, so `ov` may be dropped.
        #[allow(unsafe_code)]
        fn wait(&self, ov: &mut OVERLAPPED) -> io::Result<Outcome> {
            // SAFETY: both handles are live events owned by `self` / the caller.
            let which =
                unsafe { WaitForMultipleObjects(&[ov.hEvent, self.shutdown.0], false, INFINITE) };
            if which == WAIT_OBJECT_0 {
                let mut n = 0u32;
                // SAFETY: the operation signalled its event, so its OVERLAPPED is
                // complete and `n` is a valid out-pointer.
                match unsafe { GetOverlappedResult(self.pipe.0, ov, &mut n, false) } {
                    Ok(()) => Ok(Outcome::Done(n)),
                    Err(e) if is_eof(&e) => Ok(Outcome::Closed),
                    Err(e) => Err(e.into()),
                }
            } else {
                self.reap(ov);
                if which == WAIT_EVENT(WAIT_OBJECT_0.0 + 1) {
                    Ok(Outcome::Closed)
                } else {
                    Err(windows::core::Error::from_thread().into())
                }
            }
        }

        /// Cancel `ov` and wait until the kernel has let go of it.
        #[allow(unsafe_code)]
        fn reap(&self, ov: &mut OVERLAPPED) {
            let mut n = 0u32;
            // SAFETY: `ov` is the OVERLAPPED of an operation submitted on `pipe`; the
            // blocking GetOverlappedResult guarantees it is finished before we return.
            unsafe {
                let _ = CancelIoEx(self.pipe.0, Some(ov));
                let _ = GetOverlappedResult(self.pipe.0, ov, &mut n, true);
            }
        }

        /// Wait for the module to open its end (once). Fails with `BrokenPipe` if the
        /// closer fires first.
        #[allow(unsafe_code)]
        fn ensure_connected(&self) -> io::Result<()> {
            if self.connected.load(Ordering::Acquire) {
                return Ok(());
            }
            let _guard = self.connect.lock().unwrap_or_else(|e| e.into_inner());
            if self.connected.load(Ordering::Acquire) {
                return Ok(());
            }
            if self.closed.load(Ordering::Acquire) {
                return Err(broken());
            }
            let event = auto_reset_event()?;
            let mut ov = OVERLAPPED {
                hEvent: event.0,
                ..Default::default()
            };
            // SAFETY: `ov` outlives the operation — every path below either sees it
            // complete or reaps it.
            let submitted = unsafe { ConnectNamedPipe(self.pipe.0, Some(&mut ov)) };
            let outcome = match submitted {
                Ok(()) => self.wait(&mut ov)?,
                Err(e) if is_win32(&e, ERROR_IO_PENDING) => self.wait(&mut ov)?,
                Err(e) if is_win32(&e, ERROR_PIPE_CONNECTED) => Outcome::Done(0),
                Err(e) => return Err(e.into()),
            };
            match outcome {
                Outcome::Done(_) => {
                    self.connected.store(true, Ordering::Release);
                    Ok(())
                }
                Outcome::Closed => Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "pipe closed before the module connected",
                )),
            }
        }

        /// The closer: wake every waiter, cancel what is pending, drop the client.
        #[allow(unsafe_code)]
        fn close(&self) {
            self.closed.store(true, Ordering::Release);
            // SAFETY: all three handles are owned by `self` and valid until drop; each
            // call is documented safe from any thread and idempotent here.
            unsafe {
                let _ = SetEvent(self.shutdown.0);
                let _ = CancelIoEx(self.pipe.0, None);
                let _ = DisconnectNamedPipe(self.pipe.0);
            }
        }
    }

    /// The host's half, held inside `HostEnd` until `split`.
    pub struct HostPipe {
        shared: Arc<Shared>,
        name: String,
    }

    /// The child's half: just the name. Dropping it releases nothing — the child opens
    /// the pipe by name — which is exactly the "host does not keep the module's side open"
    /// property `spawn` relies on.
    pub struct ChildPipe {
        name: String,
    }

    impl ChildPipe {
        /// The `\\.\pipe\...` path the child receives in `AVADA_MODULE_PIPE`.
        pub fn name(&self) -> &str {
            &self.name
        }

        /// Open the child's end in-process, as a module would from `AVADA_MODULE_PIPE`:
        /// a plain read+write `CreateFileW` on the path. Tests use it to play the module.
        pub fn connect(&self) -> io::Result<std::fs::File> {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&self.name)
        }
    }

    /// Create the pipe with the owner-only DACL. Retries with a fresh name if the one we
    /// picked is somehow taken (`ERROR_ACCESS_DENIED` from `FIRST_PIPE_INSTANCE`, or busy).
    #[allow(unsafe_code)] // CreateNamedPipeW; SAFETY note inline
    pub fn create_pair() -> io::Result<(HostEnd, ChildEnd)> {
        let owner = OwnerOnly::for_current_user()?;
        let attrs = owner.security_attributes();
        let pid = std::process::id();
        let mut last = None;
        for _ in 0..CREATE_ATTEMPTS {
            let name = pipe_name(pid, &random_suffix());
            let wide = HSTRING::from(name.as_str());
            // SAFETY: `wide` is NUL-terminated; `attrs` points at `owner`'s descriptor,
            // which lives until the end of this function — the kernel copies the DACL at
            // create time.
            let handle = unsafe {
                CreateNamedPipeW(
                    &wide,
                    PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                    1,
                    BUFFER_SIZE,
                    BUFFER_SIZE,
                    0,
                    Some(&attrs),
                )
            };
            if handle.is_invalid() {
                let e = windows::core::Error::from_thread();
                let taken = is_win32(&e, ERROR_ACCESS_DENIED) || is_win32(&e, ERROR_PIPE_BUSY);
                last = Some(io::Error::from(e));
                if taken {
                    continue;
                }
                break;
            }
            let shared = Arc::new(Shared {
                pipe: Handle(handle),
                shutdown: manual_reset_event()?,
                connect: Mutex::new(()),
                connected: AtomicBool::new(false),
                closed: AtomicBool::new(false),
            });
            return Ok((
                HostEnd {
                    inner: HostPipe {
                        shared,
                        name: name.clone(),
                    },
                },
                ChildEnd {
                    inner: ChildPipe { name },
                },
            ));
        }
        Err(last.unwrap_or_else(|| io::Error::other("CreateNamedPipeW failed")))
    }

    impl HostPipe {
        /// The pipe path (diagnostics / tests).
        pub fn name(&self) -> &str {
            &self.name
        }
    }

    /// Split into the framed reader, the framed writer and the closer.
    pub fn split(pipe: HostPipe) -> io::Result<(LineReader, LineWriter, Closer)> {
        let reader = PipeReader {
            shared: pipe.shared.clone(),
            event: auto_reset_event()?,
        };
        let writer = PipeWriter {
            shared: pipe.shared.clone(),
            event: auto_reset_event()?,
        };
        let shared = pipe.shared;
        let closer = Closer(Box::new(move || shared.close()));
        Ok((
            LineReader::new(Box::new(reader)),
            LineWriter::new(Box::new(writer)),
            closer,
        ))
    }

    struct PipeReader {
        shared: Arc<Shared>,
        event: Handle,
    }

    impl Read for PipeReader {
        #[allow(unsafe_code)] // overlapped ReadFile; SAFETY note inline
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            if self.shared.closed.load(Ordering::Acquire) {
                return Ok(0);
            }
            if self.shared.ensure_connected().is_err() {
                return Ok(0);
            }
            let mut ov = OVERLAPPED {
                hEvent: self.event.0,
                ..Default::default()
            };
            // SAFETY: `buf` and `ov` outlive the operation — `wait` returns only once it
            // has completed or been reaped.
            let submitted = unsafe { ReadFile(self.shared.pipe.0, Some(buf), None, Some(&mut ov)) };
            let outcome = match submitted {
                Ok(()) => self.shared.wait(&mut ov)?,
                Err(e) if is_win32(&e, ERROR_IO_PENDING) => self.shared.wait(&mut ov)?,
                Err(e) if is_eof(&e) => return Ok(0),
                Err(e) => return Err(e.into()),
            };
            match outcome {
                Outcome::Done(n) => Ok(n as usize),
                Outcome::Closed => Ok(0),
            }
        }
    }

    struct PipeWriter {
        shared: Arc<Shared>,
        event: Handle,
    }

    impl Write for PipeWriter {
        #[allow(unsafe_code)] // overlapped WriteFile; SAFETY note inline
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            if self.shared.closed.load(Ordering::Acquire) {
                return Err(broken());
            }
            self.shared.ensure_connected()?;
            let mut ov = OVERLAPPED {
                hEvent: self.event.0,
                ..Default::default()
            };
            // SAFETY: as in `read`.
            let submitted =
                unsafe { WriteFile(self.shared.pipe.0, Some(buf), None, Some(&mut ov)) };
            let outcome = match submitted {
                Ok(()) => self.shared.wait(&mut ov)?,
                Err(e) if is_win32(&e, ERROR_IO_PENDING) => self.shared.wait(&mut ov)?,
                Err(e) if is_eof(&e) => return Err(broken()),
                Err(e) => return Err(e.into()),
            };
            match outcome {
                Outcome::Done(n) => Ok(n as usize),
                Outcome::Closed => Err(broken()),
            }
        }

        /// Byte-mode pipes hand every write to the kernel immediately; nothing to flush.
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl ChildEnd {
        /// The pipe path the child will find in `AVADA_MODULE_PIPE`.
        pub fn pipe_name(&self) -> &str {
            self.inner.name()
        }

        /// Open the child's end in-process — the module's side, for tests.
        pub fn connect(&self) -> io::Result<std::fs::File> {
            self.inner.connect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_name_is_local_prefixed_and_carries_pid_and_suffix() {
        let name = pipe_name(4242, "00ff");
        assert_eq!(name, r"\\.\pipe\avada-module-4242-00ff");
        assert!(name.starts_with(r"\\.\pipe\"));
    }

    #[test]
    fn random_suffix_is_32_lowercase_hex_and_fresh_each_time() {
        let a = random_suffix();
        let b = random_suffix();
        assert_eq!(a.len(), 32);
        assert!(a
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_ne!(a, b);
    }

    #[test]
    fn recognises_only_names_it_would_have_made() {
        let pid = std::process::id();
        let good = pipe_name(pid, &random_suffix());
        assert!(is_module_pipe_name(&good, pid));
        assert!(!is_module_pipe_name(&good, pid.wrapping_add(1)));
        assert!(!is_module_pipe_name(&pipe_name(pid, "short"), pid));
        assert!(!is_module_pipe_name(&pipe_name(pid, &"G".repeat(32)), pid));
        assert!(!is_module_pipe_name(r"\\.\pipe\other", pid));
        assert!(!is_module_pipe_name(
            r"\\server\pipe\avada-module-1-00",
            pid
        ));
    }

    #[test]
    fn attach_env_publishes_the_pipe_path() {
        let mut cmd = Command::new("true");
        attach_env(r"\\.\pipe\avada-module-1-abc", &mut cmd);
        assert!(cmd.get_envs().any(|(k, v)| {
            k == ENV_PIPE && v.and_then(|v| v.to_str()) == Some(r"\\.\pipe\avada-module-1-abc")
        }));
    }
}

#[cfg(all(test, windows))]
mod win_tests {
    use super::super::{pair, FrameError};
    use super::*;
    use avada_module_sdk::contract::Message;
    use std::io::{BufRead, BufReader, Write};
    use std::time::Duration;

    #[test]
    fn attach_sets_env_and_pipe_name_is_ours() {
        let (_host, child) = pair().unwrap();
        assert!(is_module_pipe_name(child.pipe_name(), std::process::id()));
        let mut cmd = Command::new("cmd");
        child.attach(&mut cmd).unwrap();
        let want = child.pipe_name().to_string();
        assert!(cmd
            .get_envs()
            .any(|(k, v)| k == ENV_PIPE && v.and_then(|v| v.to_str()) == Some(want.as_str())));
    }

    #[test]
    fn round_trips_both_ways_and_closer_ends_it() {
        let (host, child) = pair().unwrap();
        let (mut reader, mut writer, closer) = host.split().unwrap();
        // The module reports through a channel so the wait below has a deadline; a plain
        // `join` would sit forever if the close never reached the module's read.
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut stream = child.connect().unwrap();
            stream
                .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"module.event\"}\n")
                .unwrap();
            let mut lines = BufReader::new(stream);
            let mut line = String::new();
            lines.read_line(&mut line).unwrap();
            // Tell the host the line arrived before it disconnects: DisconnectNamedPipe
            // discards whatever the client has not read yet.
            lines
                .get_mut()
                .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"module.ack\"}\n")
                .unwrap();
            // After the closer fires the module's next read is EOF or a broken pipe.
            let mut rest = String::new();
            let after = lines.read_line(&mut rest);
            let _ = done_tx.send((line, matches!(after, Ok(0) | Err(_))));
        });
        let msg = reader.read_message().unwrap().unwrap();
        assert!(matches!(msg, Message::Notification(n) if n.method == "module.event"));
        writer
            .write_line("{\"jsonrpc\":\"2.0\",\"method\":\"host.ping\"}")
            .unwrap();
        let ack = reader.read_message().unwrap().unwrap();
        assert!(matches!(ack, Message::Notification(n) if n.method == "module.ack"));
        closer.close();
        assert!(matches!(
            reader.read_message(),
            Ok(None) | Err(FrameError::Io(_))
        ));
        assert!(writer.write_line("x").is_err());
        let (line, eof) = done_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the module sees the close within 10s");
        assert_eq!(
            line.trim(),
            "{\"jsonrpc\":\"2.0\",\"method\":\"host.ping\"}"
        );
        assert!(eof);
    }

    #[test]
    fn closer_unblocks_a_reader_waiting_for_a_module_that_never_connects() {
        let (host, _child) = pair().unwrap();
        let (mut reader, _writer, closer) = host.split().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(reader.read_message().map(|m| m.is_none()));
        });
        assert!(
            rx.try_recv().is_err(),
            "reader must block until the closer fires"
        );
        closer.close();
        closer.close();
        let seen = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("reader woke up");
        assert!(matches!(seen, Ok(true) | Err(FrameError::Io(_))));
    }

    #[test]
    fn second_client_is_refused_on_a_single_instance_pipe() {
        let (host, child) = pair().unwrap();
        let (mut reader, _writer, closer) = host.split().unwrap();
        let first = child.connect().unwrap();
        assert!(child.connect().is_err(), "one instance, one client");
        drop(first);
        closer.close();
        assert!(matches!(
            reader.read_message(),
            Ok(None) | Err(FrameError::Io(_))
        ));
    }
}
