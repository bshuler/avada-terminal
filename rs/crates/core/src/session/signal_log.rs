//! Make a signal death leave a trace.
//!
//! The 2026-10-09 handoff loss (`docs/incidents/2026-10-09-b3/`) was a daemon killed by a
//! SIGTERM it never logged: no handler, so no line, no panic, no crash report — the log
//! simply stopped. Reconstructing who killed it took the unified log and an hour. This
//! installs `SA_SIGINFO` handlers for the terminating signals that append one line —
//! signal, sender pid/uid, time — to the role's log file, then re-raise with the default
//! action restored, so the process still dies exactly as before.
//!
//! The handler uses only async-signal-safe calls (`open`, `write`, `close`, `getpid`,
//! `time`, `raise`), formats into a stack buffer, and re-opens the log by path on every
//! hit so log rotation cannot leave it writing into a renamed file. The path is stored in
//! a `OnceLock` before any handler is installed; reading it is a plain atomic load.

use std::ffi::CString;
use std::path::Path;
use std::sync::OnceLock;

static LOG_PATH: OnceLock<CString> = OnceLock::new();

/// Install the handlers (SIGTERM, SIGHUP, SIGINT) for this process. A second call is a
/// no-op; a path with an interior NUL just leaves the handlers uninstalled.
#[tracing::instrument(level = "debug")]
pub fn install(log_path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = CString::new(log_path.as_os_str().as_bytes()) else {
        return;
    };
    if LOG_PATH.set(c).is_err() {
        return;
    }
    for sig in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
        // SAFETY: a zeroed sigaction is a valid starting value; the handler below is
        // async-signal-safe; SA_RESETHAND means a second delivery takes the default action.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            // Through the fn-pointer type, not the fn item: clippy `function_casts_as_integer`.
            let h: extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) = handler;
            sa.sa_sigaction = h as usize;
            sa.sa_flags = libc::SA_SIGINFO | libc::SA_RESETHAND;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }
}

fn name(sig: libc::c_int) -> &'static str {
    match sig {
        libc::SIGTERM => "SIGTERM",
        libc::SIGHUP => "SIGHUP",
        libc::SIGINT => "SIGINT",
        _ => "signal",
    }
}

/// A no-allocation line builder for use inside the handler.
struct Line {
    buf: [u8; 256],
    len: usize,
}

impl Line {
    fn push(&mut self, s: &str) {
        for &b in s.as_bytes() {
            if self.len < self.buf.len() {
                self.buf[self.len] = b;
                self.len += 1;
            }
        }
    }
    fn num(&mut self, n: i64) {
        if n < 0 {
            self.push("-");
        }
        let mut digits = [0u8; 20];
        let mut i = digits.len();
        let mut v = n.unsigned_abs();
        loop {
            i -= 1;
            digits[i] = b'0' + (v % 10) as u8;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        for &d in &digits[i..] {
            if self.len < self.buf.len() {
                self.buf[self.len] = d;
                self.len += 1;
            }
        }
    }
}

#[cfg(target_os = "macos")]
unsafe fn sender(info: *mut libc::siginfo_t) -> (i64, i64) {
    if info.is_null() {
        return (-1, -1);
    }
    (i64::from((*info).si_pid), i64::from((*info).si_uid))
}

#[cfg(all(unix, not(target_os = "macos")))]
unsafe fn sender(info: *mut libc::siginfo_t) -> (i64, i64) {
    if info.is_null() {
        return (-1, -1);
    }
    (i64::from((*info).si_pid()), i64::from((*info).si_uid()))
}

extern "C" fn handler(sig: libc::c_int, info: *mut libc::siginfo_t, _ctx: *mut libc::c_void) {
    let mut line = Line {
        buf: [0u8; 256],
        len: 0,
    };
    // SAFETY: every call here is async-signal-safe and `info` is either null or the
    // siginfo the kernel handed us.
    unsafe {
        let (spid, suid) = sender(info);
        line.push("[signal] pid=");
        line.num(i64::from(libc::getpid()));
        line.push(" got ");
        line.push(name(sig));
        line.push(" (");
        line.num(i64::from(sig));
        line.push(") from pid=");
        line.num(spid);
        line.push(" uid=");
        line.num(suid);
        line.push(" epoch=");
        line.num(libc::time(std::ptr::null_mut()) as i64);
        line.push("; exiting on the default action\n");
        if let Some(p) = LOG_PATH.get() {
            let fd = libc::open(
                p.as_ptr(),
                libc::O_WRONLY | libc::O_APPEND | libc::O_CREAT | libc::O_CLOEXEC,
                0o600,
            );
            if fd >= 0 {
                let _ = libc::write(fd, line.buf.as_ptr().cast(), line.len);
                libc::close(fd);
            }
        }
        // SA_RESETHAND already restored the default disposition: this terminates.
        libc::raise(sig);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_formats_numbers_without_allocation() {
        let mut l = Line {
            buf: [0u8; 256],
            len: 0,
        };
        l.push("a=");
        l.num(0);
        l.push(" b=");
        l.num(-42);
        l.push(" c=");
        l.num(1_760_040_046);
        assert_eq!(
            std::str::from_utf8(&l.buf[..l.len]).unwrap(),
            "a=0 b=-42 c=1760040046"
        );
    }

    #[test]
    fn line_truncates_instead_of_overflowing() {
        let mut l = Line {
            buf: [0u8; 256],
            len: 0,
        };
        for _ in 0..100 {
            l.push("0123456789");
        }
        assert_eq!(l.len, 256);
    }
}
