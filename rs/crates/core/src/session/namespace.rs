//! Is this process still inside a live macOS login session?
//!
//! The daemon is `setsid`'d away from the GUI that spawned it, so it outlives the GUI — and,
//! it turns out, the login session itself. When macOS tears a GUI session down (a logout,
//! a WindowServer crash, a fast-user-switch gone wrong) every process in it keeps a Mach
//! bootstrap port that now leads nowhere. Nothing crashes: the daemon keeps serving, the
//! panes keep drawing. But every child it spawns inherits the dead port, so anything that
//! needs a launchd service fails — `getpwuid` (opendirectoryd), `launchctl`, `open`,
//! `codesign`, crashpad, and with them ssh-through-1Password. That is the 2026-10-05
//! incident: a terminal that looks healthy and can authenticate to nothing.
//!
//! [`namespace_ok`] asks the bootstrap server for the very service `getpwuid` uses. It is
//! a direct Mach round-trip — not `getpwuid` itself, whose answer libinfo may serve from a
//! process-local cache long after the session died. The daemon reports the result in its
//! `Hello`; a client that is itself healthy then takes the sessions over onto a daemon it
//! spawns (see `daemon_client`), and the GUI restarts every pane, since the shells still
//! hold the dead port even after their pty masters have moved.

/// The launchd service `getpwuid` and friends reach opendirectoryd through. Losing it is
/// exactly the failure that broke the panes, so it is the one to ask about.
#[cfg(target_os = "macos")]
const PROBE_SERVICE: &[u8] = b"com.apple.system.opendirectoryd.libinfo\0";

/// `Some(true)` when this process's bootstrap namespace still answers, `Some(false)` when it
/// does not, `None` when the question has no answer here (not macOS). Computed fresh on
/// every call — a session can die under a long-lived process at any moment, so a cached
/// verdict would be the very bug this exists to catch.
#[cfg(target_os = "macos")]
#[tracing::instrument(level = "debug", ret)]
pub fn namespace_ok() -> Option<bool> {
    type MachPort = libc::c_uint;
    type KernReturn = libc::c_int;
    extern "C" {
        static bootstrap_port: MachPort;
        fn bootstrap_look_up(
            bp: MachPort,
            name: *const libc::c_char,
            sp: *mut MachPort,
        ) -> KernReturn;
        fn mach_task_self() -> MachPort;
        fn mach_port_deallocate(task: MachPort, name: MachPort) -> KernReturn;
    }
    let mut port: MachPort = 0;
    // SAFETY: `bootstrap_port` is libSystem's per-process global, set at exec; the name is
    // NUL-terminated; `port` is a valid out-pointer. A send right we receive is released
    // below so a probe per `Hello` leaks nothing.
    let kr = unsafe { bootstrap_look_up(bootstrap_port, PROBE_SERVICE.as_ptr().cast(), &mut port) };
    if kr == 0 {
        if port != 0 {
            // SAFETY: `port` is a send right this call just handed us.
            unsafe { mach_port_deallocate(mach_task_self(), port) };
        }
        return Some(true);
    }
    tracing::warn!(
        kern_return = kr,
        "bootstrap look-up failed: this process's login session is gone"
    );
    Some(false)
}

/// Not macOS: there is no bootstrap namespace to lose, so there is nothing to report.
#[cfg(not(target_os = "macos"))]
pub fn namespace_ok() -> Option<bool> {
    None
}

/// Whether a client should take a daemon's sessions over because the daemon's login
/// session is dead. Only on a definite `Some(false)` from the daemon — an older daemon
/// (`None`) is unknown, and unknown never forces anything — and only when the client is
/// itself definitely healthy: a successor spawned from a dead client would inherit the
/// same dead port, and the takeover would just move the problem while looping.
pub fn should_escape(daemon: Option<bool>, ours: Option<bool>) -> bool {
    daemon == Some(false) && ours == Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_dead_daemon_and_a_live_client_escape() {
        assert!(should_escape(Some(false), Some(true)));
        // A daemon that is fine, or that cannot say, is left alone.
        assert!(!should_escape(Some(true), Some(true)));
        assert!(!should_escape(None, Some(true)));
        // A client that is dead itself (or cannot tell) must not spawn a successor: it
        // would be born into the same dead namespace.
        assert!(!should_escape(Some(false), Some(false)));
        assert!(!should_escape(Some(false), None));
    }

    // The test runner lives in a real login session; if this ever fails on a dev machine
    // the probe is wrong, not the machine.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_live_session_answers_the_probe() {
        assert_eq!(namespace_ok(), Some(true));
    }
}
