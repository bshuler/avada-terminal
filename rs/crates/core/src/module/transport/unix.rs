//! Unix socketpair transport: `socketpair(AF_UNIX, SOCK_STREAM)` through `libc`, the
//! child's end inherited across `exec` with its number in `AVADA_MODULE_FD`.

use super::{ChildEnd, HostEnd};
use avada_module_sdk::client::ENV_FD;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::process::Command;

/// One connected pair. Both ends start close-on-exec; [`attach`] clears the flag on the
/// child's end alone, so a module inherits exactly one descriptor from the host.
#[allow(unsafe_code)] // raw socketpair + fd adoption; SAFETY notes inline
pub(super) fn socketpair() -> io::Result<(HostEnd, ChildEnd)> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` is a valid, writable two-element array; socketpair writes both slots
    // on success and touches nothing else.
    let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both descriptors were just created by socketpair and are owned by nobody
    // else; each is adopted exactly once.
    let host = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let child = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    set_cloexec(&host, true)?;
    set_cloexec(&child, true)?;
    Ok((
        HostEnd {
            stream: UnixStream::from(host),
        },
        ChildEnd { fd: child },
    ))
}

/// Mark the descriptor inheritable and tell the child where to find it.
pub(super) fn attach(fd: &OwnedFd, cmd: &mut Command) -> io::Result<()> {
    set_cloexec(fd, false)?;
    cmd.env(ENV_FD, fd.as_raw_fd().to_string());
    Ok(())
}

#[allow(unsafe_code)] // fcntl on a descriptor we own
fn set_cloexec(fd: &OwnedFd, on: bool) -> io::Result<()> {
    let raw = fd.as_raw_fd();
    // SAFETY: plain fcntl calls on a descriptor this process owns; no memory is passed.
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    let next = if on {
        flags | libc::FD_CLOEXEC
    } else {
        flags & !libc::FD_CLOEXEC
    };
    // SAFETY: as above.
    if unsafe { libc::fcntl(raw, libc::F_SETFD, next) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

impl ChildEnd {
    /// The child's end as a stream — what `avada_module_sdk::client::from_env` builds
    /// inside the module. Tests use it to play the module in-process.
    pub fn into_stream(self) -> UnixStream {
        UnixStream::from(self.fd)
    }

    /// Raw descriptor number, as the child will see it.
    pub fn raw_fd(&self) -> i32 {
        self.fd.as_raw_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(unsafe_code)]
    fn cloexec(fd: i32) -> bool {
        // SAFETY: read-only fcntl query on a descriptor the test owns.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        flags & libc::FD_CLOEXEC != 0
    }

    #[test]
    fn attach_clears_cloexec_on_the_child_end_only() {
        let (host, child) = socketpair().unwrap();
        assert!(cloexec(host.stream.as_raw_fd()));
        assert!(cloexec(child.raw_fd()));
        let mut cmd = Command::new("true");
        child.attach(&mut cmd).unwrap();
        assert!(!cloexec(child.raw_fd()));
        assert!(cloexec(host.stream.as_raw_fd()));
        let want = child.raw_fd().to_string();
        assert!(cmd
            .get_envs()
            .any(|(k, v)| k == ENV_FD && v.and_then(|v| v.to_str()) == Some(want.as_str())));
    }
}
