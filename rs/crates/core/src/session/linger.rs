//! The retired daemon's **linger stub** (unix only; backlog B12).
//!
//! macOS attributes a process's TCC requests (Screen Recording, Accessibility, …) to its
//! *responsible* process, fixed at spawn time: for every shell a session daemon starts that
//! is the daemon's pid. When a live upgrade hands the ptys to a successor and the retired
//! daemon exits, each old shell is reparented to launchd and becomes responsible for itself —
//! a bare `zsh` with no TCC row that can neither inherit the grant nor prompt for one, so
//! `screencapture` in every pre-existing pane fails until the pane is reopened.
//!
//! The fix keeps the responsible pid alive. After the handoff the retired daemon does not
//! `exit`: it **`execv`s its own current image** — the attic copy the install script keeps
//! (`Avada.app/Contents/MacOS/avada` resolved through the vnode, so the renamed attic path,
//! never the freshly installed binary) — into this mode. `exec` keeps the pid, the parent /
//! child links, the launchd job and the TCC responsibility; it drops every thread (the pty
//! readers that would otherwise steal bytes from the successor), every descriptor marked
//! close-on-exec (we mark them all, so the pty masters, the socket and the flock all close
//! atomically at the exec boundary — the successor already holds its own dups), and skips
//! every destructor (so `portable-pty`'s writer drop can never type EOT at a shell).
//!
//! The stub then does one thing: `waitpid(-1)` until `ECHILD`, i.e. until the last shell it
//! is responsible for has gone, then exits 0. It holds no socket, no lock and no port, and
//! its argv contains neither `--session-daemon` (so the install script's daemon census,
//! the live-session guard and the daemon salt parser all ignore it) nor anything the GUI
//! could mistake for a daemon to talk to.
#![cfg(unix)]

use std::ffi::CString;
use std::io;
use std::path::PathBuf;

/// The hidden mode flag (see [`super::LINGER_FLAG`] for why it is spelled the way it is).
pub const FLAG: &str = super::LINGER_FLAG;

/// The executable image this process is *currently running*, resolved through the kernel
/// (`proc_pidpath` on macOS) rather than `std::env::current_exe`, which on macOS returns the
/// path string the process was launched with — after an install that string names the NEW
/// bundle, while the mapped image (and its code signature, which is what TCC checks) is the
/// one the install script moved into the attic.
#[tracing::instrument(level = "debug", ret)]
pub fn current_image() -> io::Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: `buf` is a writable buffer of exactly `buffersize` bytes; `proc_pidpath`
        // writes at most that many and returns the length, or <= 0 with errno set.
        let n = unsafe {
            libc::proc_pidpath(
                libc::getpid(),
                buf.as_mut_ptr().cast(),
                buf.len() as u32,
            )
        };
        if n <= 0 {
            return Err(io::Error::last_os_error());
        }
        buf.truncate(n as usize);
        use std::os::unix::ffi::OsStringExt;
        return Ok(PathBuf::from(std::ffi::OsString::from_vec(buf)));
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::env::current_exe()
    }
}

/// Mark every descriptor from `first` upward close-on-exec, so an `execv` closes them
/// atomically without disturbing the threads that may still be blocked on them (closing a
/// pty master under a reader thread is exactly what this avoids). Descriptors that are not
/// open are skipped. Bounded by the soft `RLIMIT_NOFILE`, capped so a runaway limit cannot
/// turn this into a long loop.
#[tracing::instrument(level = "debug", ret)]
pub fn set_cloexec_from(first: libc::c_int) -> usize {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is a valid, writable rlimit struct.
    let soft = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0 {
        limit.rlim_cur
    } else {
        1024
    };
    let last = soft.min(1 << 16) as libc::c_int;
    let mut marked = 0usize;
    for fd in first..last {
        // SAFETY: fcntl F_GETFD / F_SETFD on an arbitrary fd number is always safe; a closed
        // fd just reports EBADF.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags == -1 {
            continue;
        }
        if flags & libc::FD_CLOEXEC == 0
            && unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == 0
        {
            marked += 1;
        }
    }
    marked
}

/// Replace this process with the linger stub for `salt`. **Returns only on failure**; the
/// caller then falls back to a plain exit, which is exactly today's behaviour (the shells
/// survive, just without a responsible pid).
///
/// `argv[0]` is the resolved attic path, so the install script's attic prune (which keeps a
/// retired bundle while `pgrep -f "<attic>/"` finds a user) keeps the image alive for as
/// long as the stub runs.
#[tracing::instrument(level = "debug", ret)]
pub fn exec_into_linger(salt: &str) -> io::Error {
    let image = match current_image() {
        Ok(p) => p,
        Err(e) => return e,
    };
    use std::os::unix::ffi::OsStrExt;
    let argv = [
        CString::new(image.as_os_str().as_bytes()),
        CString::new(FLAG),
        CString::new(salt),
    ];
    let argv: Vec<CString> = match argv.into_iter().collect::<Result<_, _>>() {
        Ok(v) => v,
        Err(e) => return io::Error::new(io::ErrorKind::InvalidInput, e),
    };
    let mut ptrs: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
    ptrs.push(std::ptr::null());
    let marked = set_cloexec_from(3);
    tracing::info!(
        image = %image.display(),
        marked_cloexec = marked,
        pid = std::process::id(),
        "retired daemon exec'ing into the linger stub"
    );
    // SAFETY: `ptrs` is a NULL-terminated array of pointers into `argv`, which outlives the
    // call; `execv` only returns on failure.
    unsafe { libc::execv(ptrs[0], ptrs.as_ptr()) };
    io::Error::last_os_error()
}

/// One `waitpid(-1)` step as the stub sees it: a reaped child, a retry, or "no children".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    Reaped(libc::pid_t),
    Interrupted,
    NoChildren,
}

/// Block until the last child is reaped, counting them. `wait_any` is one blocking
/// `waitpid(-1)` call; [`reap_all`] supplies the real one.
#[tracing::instrument(level = "debug", ret, skip(wait_any))]
pub fn drain(mut wait_any: impl FnMut() -> Wait) -> usize {
    let mut reaped = 0;
    loop {
        match wait_any() {
            Wait::Reaped(_) => reaped += 1,
            Wait::Interrupted => continue,
            Wait::NoChildren => return reaped,
        }
    }
}

/// Reap every child of this process, blocking until `ECHILD`. Returns the number reaped.
/// Only the stub may call this: `waitpid(-1)` claims *any* child, which inside a process
/// that also runs ptys would steal their exit statuses.
#[tracing::instrument(level = "debug", ret)]
pub fn reap_all() -> usize {
    drain(|| {
        // SAFETY: waitpid with a NULL status pointer is permitted; no memory is written.
        let pid = unsafe { libc::waitpid(-1, std::ptr::null_mut(), 0) };
        if pid > 0 {
            Wait::Reaped(pid)
        } else if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            Wait::Interrupted
        } else {
            Wait::NoChildren
        }
    })
}

/// The stub's whole life: wait for the shells, then leave. Never returns.
///
/// (Not `#[tracing::instrument]`ed: the macro re-states the return type in a position where
/// `!` is still unstable, E0658.)
pub fn run(salt: &str) -> ! {
    let image = current_image()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|e| format!("<unknown: {e}>"));
    tracing::info!(
        pid = std::process::id(),
        salt,
        image,
        "linger stub started: holding TCC responsibility for the retired daemon's shells"
    );
    let reaped = reap_all();
    tracing::info!(reaped, "linger stub: last shell gone, exiting");
    std::process::exit(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_image_names_an_existing_executable() {
        let p = current_image().expect("resolves");
        assert!(p.is_absolute(), "{p:?}");
        assert!(p.exists(), "{p:?}");
    }

    #[test]
    fn set_cloexec_from_marks_an_inherited_descriptor() {
        // A plain dup is NOT close-on-exec — the same shape as portable-pty's writer dup.
        let f = std::fs::File::open("/dev/null").expect("/dev/null");
        use std::os::fd::AsRawFd;
        let fd = unsafe { libc::dup(f.as_raw_fd()) };
        assert!(fd >= 0);
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC, 0);
        let marked = set_cloexec_from(fd);
        assert!(marked >= 1, "at least our dup was marked: {marked}");
        assert_ne!(unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC, 0);
        // Idempotent: a second pass has nothing left to do for that fd.
        unsafe { libc::close(fd) };
    }

    #[test]
    fn drain_counts_reaped_children_and_retries_eintr() {
        let mut steps = vec![
            Wait::Reaped(10),
            Wait::Interrupted,
            Wait::Reaped(11),
            Wait::Interrupted,
            Wait::NoChildren,
        ]
        .into_iter();
        assert_eq!(drain(|| steps.next().expect("drain stops at NoChildren")), 2);
    }

    #[test]
    fn drain_with_no_children_returns_zero_immediately() {
        let mut calls = 0;
        assert_eq!(
            drain(|| {
                calls += 1;
                Wait::NoChildren
            }),
            0
        );
        assert_eq!(calls, 1);
    }

    #[test]
    fn the_flag_is_not_a_session_daemon_superstring() {
        // The install script counts daemons with `pgrep -f -- '--session-daemon'`, and the
        // live-session guard denies killing by the same token: a stub must match neither.
        assert!(!FLAG.contains("--session-daemon"));
    }
}
