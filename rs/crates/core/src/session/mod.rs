//! Session subsystem. The pure `cwd` parser (done, Wave 0) plus the LIVE engine
//! (this wave): `pty` / `spawn` / `batcher` / `replay` / `screen`. Plus the session
//! **daemon** (M0): `proto` (the wire protocol) + `daemon` (a PTY-owning daemon over a
//! UDS / named pipe, with a loopback client) — both headless-testable, no Slint.
/// Adopting a pty master handed over by a predecessor daemon (unix only).
#[cfg(unix)]
pub mod adopt;
/// The `avada attach` client core (M2): protocol + detach-key + resize policy, with no
/// tty or stdio in it — the app crate supplies those, and M3's SSH channel will supply its own.
pub mod attach;
pub mod batcher;
/// This binary's build identity, carried in the daemon handshake so either side can be
/// upgraded (or rolled back) without dropping the sessions.
pub mod build_id;
/// The cross-process session claim registry (M7): who is hosting which uid right now.
pub mod claims;
/// The tmux **control-mode** (`-CC`) server surface (M4): a pure protocol encoder and state
/// machine that presents avada panes to iTerm2 and the mobile tmux clients as native
/// tmux panes. No I/O — the app crate and M3's SSH channel each supply their own transport.
pub mod control_mode;
pub mod cwd;
pub mod daemon;
pub mod daemon_client;
pub mod env;
/// Descriptor handoff for the daemon live upgrade (unix only).
#[cfg(unix)]
pub mod handoff;
pub mod launchd;
/// The retired daemon's post-handoff linger stub, which keeps the responsible pid of every
/// pre-upgrade shell alive for macOS TCC (unix only, B12).
#[cfg(unix)]
pub mod linger;

/// The linger stub's mode flag, on every platform (the stub itself is unix-only, but the
/// entry point parses argv everywhere). Deliberately *not* a superstring of
/// `--session-daemon`: anything that counts, kills or parses daemons by that token must
/// keep ignoring a stub.
pub const LINGER_FLAG: &str = "--daemon-linger";
pub mod namespace;
pub mod openurl;
pub mod osc133;
pub mod paste;
pub mod proto;
pub mod pty;
pub mod replay;
pub mod screen;
#[cfg(unix)]
pub mod signal_log;
pub mod spawn;
/// The blocking client transport the daemon protocol rides on (UDS / named pipe).
pub mod transport;
