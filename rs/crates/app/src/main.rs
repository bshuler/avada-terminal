//! `avada` — the native Slint GUI (Phase 4, Wave 1: **multi-window**).
//!
//! This file is now a thin **bootstrap**: it owns the Tokio runtime + the one shared
//! [`session_manager::SessionManager`], creates the app-level [`app::App`] window
//! registry, spawns the first window, and starts the single 8 ms pump timer that drives
//! every window. All the interesting logic lives in the modules:
//!
//! * [`app`]      — the **window registry** + central event drain + per-window wiring;
//! * [`state`]    — one window's workspace state (tabs/panes/layout/zoom) and its
//!   mutate-then-resync API (**Seam #1**);
//! * [`command`]  — the `Command` enum + `dispatch` (**Seam #2**);
//! * [`paneview`] — resync (State → Slint models) + the per-window render pump;
//! * [`theme`]    — palette, layout metadata, font loading;
//! * [`window`]   — Win32 frameless / fullscreen glue (per window).
//!
//! The `.slint` views carry an empty overlay slot (**Seam #3**) for Wave-2 panels.
//! See `ARCHITECTURE.md`. PTYs are owned centrally; a window only references pane uids,
//! so a pane can be re-hosted in any window (replay-primed, no PTY restart).

#![cfg_attr(windows, windows_subsystem = "windows")]

mod ai;
mod app;
mod attach_cli;
mod command;
mod contextmenu;
mod control_cli;
mod control_host;
mod control_mode_cli;
mod crash;
mod csv;
mod ctl_cli;
mod datatree;
mod devices;
mod drag;
mod filedrop;
mod glow;
mod gridpane;
mod highlight;
mod history_scan;
mod imagepane;
mod keybindings;
mod leftpanel;
mod loops;
mod mermaid;
mod module_runtime;
mod module_ui;
mod pair;
mod palette;
mod paneview;
mod prefs;
mod sidebar;
/// The embedded SSH server (mux backend M3): attach to a live pane from a phone.
mod ssh;
mod state;
mod tetris;
mod theme;
mod uitest;
mod update;
mod viewpane;
mod window;
mod winit_hooks;
mod worker;

use std::sync::Arc;
use std::time::Duration;

use avada_core::session_manager::{SessionEvent, SessionManager};

use slint::platform::Key;
use slint::SharedString;
use tokio::sync::mpsc::unbounded_channel;

use app::{App, PendingSeed};
use command::{dispatch, Command};
use state::State;

slint::include_modules!();

/// Set while the GUI is relaunching itself (app menu → Restart, or a scope-1 `restartApp`
/// control command). The quit path at the bottom of `main` must NOT tear the session daemon
/// down in that case: the point of a GUI restart is that the panes keep running while a new
/// build re-attaches to them, so the `keep_alive = false` preference — which means "don't
/// leave terminals running after I *quit*" — does not apply to a restart.
pub static GUI_RESTARTING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// The log-file role for this invocation (`avada-<role>.log`, see
/// `avada_core::logging`): the GUI, the session daemon, the crash dialog and the
/// pipeable/worker command lines each get their own file so a `ctl` burst cannot roll the
/// GUI's log out from under a bug report.
#[tracing::instrument(level = "debug", ret)]
fn log_role(argv: &[String]) -> &'static str {
    if session_daemon_salt(argv).is_some() {
        return "daemon";
    }
    if argv.iter().any(|a| a == "--crash-report") {
        return "crash";
    }
    match argv.get(1).map(String::as_str) {
        Some("worker") => "worker",
        Some(_) if pipeable_cli(argv) || wants_kill_daemon(argv) => "cli",
        Some("--control-mode") => "cli",
        _ => "app",
    }
}

/// Parse the `--session-daemon <salt>` mode flag out of `argv`, returning the salt when
/// present. Accepts both `--session-daemon <salt>` and `--session-daemon=<salt>`. Returns
/// `None` for a normal GUI launch. Kept here (not in core) so `main` stays the entry and
/// core owns the daemon logic.
#[tracing::instrument(level = "debug", ret)]
fn session_daemon_salt(argv: &[String]) -> Option<String> {
    let mut it = argv.iter();
    while let Some(arg) = it.next() {
        if let Some(rest) = arg.strip_prefix("--session-daemon") {
            if let Some(inline) = rest.strip_prefix('=') {
                return Some(inline.to_string());
            }
            if rest.is_empty() {
                // `--session-daemon <salt>` — the salt is the next argv token.
                return it.next().cloned();
            }
        }
    }
    None
}

/// Whether `argv` carries the `--kill-daemon` flag (the M3 lifecycle entry: connect to the
/// running session daemon, tell it to shut down its sessions + exit, then return). A bare
/// flag — the salt is the user-data dir (the same key the daemon's discovery uses), resolved
/// in `main`. No-op if no daemon is running.
#[tracing::instrument(level = "debug", ret)]
fn wants_kill_daemon(argv: &[String]) -> bool {
    argv.iter().any(|a| a == "--kill-daemon")
}

/// Lightweight perf instrumentation for the Wave-2 perf track (Task 17). Enabled by setting
/// `AVADA_PERFLOG` to a file path (or `1` / empty for a default temp path), so the
/// startup-latency (#2) and scroll-region-throughput (#1) work can be measured before/after
/// without an external profiler. Completely inert (one `OnceLock` load) when the env var is
/// unset, so it costs nothing in normal runs. Single UI thread, so the tick aggregates live
/// in a `thread_local`.
pub(crate) mod perf {
    use std::cell::RefCell;
    use std::io::Write;
    use std::sync::OnceLock;
    use std::time::Instant;

    static START: OnceLock<Instant> = OnceLock::new();
    static PATH: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();

    /// Capture t0 + resolve the log path. Call once at the very top of `main`.
    #[tracing::instrument(level = "debug", ret)]
    pub fn init() {
        let _ = START.get_or_init(Instant::now);
        let _ = PATH.get_or_init(|| {
            avada_core::compat::env_var_os("AVADA_PERFLOG").map(|v| {
                let s = v.to_string_lossy();
                if s.is_empty() || s == "1" {
                    std::env::temp_dir().join("avada-perf.log")
                } else {
                    std::path::PathBuf::from(s.as_ref())
                }
            })
        });
    }

    /// Whether perf logging is on (cheap — a resolved `OnceLock` load).
    #[inline]
    #[tracing::instrument(level = "debug", ret)]
    pub fn enabled() -> bool {
        matches!(PATH.get(), Some(Some(_)))
    }

    /// Milliseconds since [`init`].
    #[tracing::instrument(level = "debug", ret)]
    pub fn elapsed_ms() -> f64 {
        START
            .get()
            .map(|s| s.elapsed().as_secs_f64() * 1000.0)
            .unwrap_or(0.0)
    }

    #[tracing::instrument(level = "debug", ret)]
    fn write_line(line: &str) {
        if let Some(Some(p)) = PATH.get() {
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
            {
                let _ = writeln!(f, "{line}");
            }
        }
    }

    /// Record a one-off timestamped milestone (used for the startup path, #2).
    #[tracing::instrument(level = "debug", ret)]
    pub fn mark(label: &str) {
        if !enabled() {
            return;
        }
        write_line(&format!("[+{:.1}ms] {label}", elapsed_ms()));
    }

    struct TickStats {
        ticks: u64,
        events: u64,
        bytes: u64,
        renders: u64,
        drain_ns: u128,
        render_ns: u128,
        tick_ns: u128,
        window: Option<Instant>,
    }
    impl TickStats {
        const fn zero() -> Self {
            TickStats {
                ticks: 0,
                events: 0,
                bytes: 0,
                renders: 0,
                drain_ns: 0,
                render_ns: 0,
                tick_ns: 0,
                window: None,
            }
        }
    }
    thread_local! {
        static TICK: RefCell<TickStats> = const { RefCell::new(TickStats::zero()) };
    }

    /// Accumulate one tick's work; flush a `[tick/s]` summary ~once a second while there is
    /// activity. `drain_ns` covers the session-event drain+feed, `render_ns` the per-window
    /// render pump, `tick_ns` the whole tick — so the summary shows the app's busy fraction
    /// (is the app the throughput bottleneck, or is it idle waiting on the pty?).
    #[tracing::instrument(level = "debug", ret)]
    pub fn tick(
        events: u64,
        bytes: u64,
        renders: u64,
        drain_ns: u128,
        render_ns: u128,
        tick_ns: u128,
    ) {
        if !enabled() {
            return;
        }
        TICK.with(|t| {
            let mut t = t.borrow_mut();
            if t.window.is_none() {
                t.window = Some(Instant::now());
            }
            t.ticks += 1;
            t.events += events;
            t.bytes += bytes;
            t.renders += renders;
            t.drain_ns += drain_ns;
            t.render_ns += render_ns;
            t.tick_ns += tick_ns;
            let elapsed = t.window.map(|w| w.elapsed()).unwrap_or_default();
            if elapsed.as_millis() >= 1000 && (t.events > 0 || t.renders > 0) {
                let secs = elapsed.as_secs_f64().max(1e-6);
                write_line(&format!(
                    "[tick/s] ticks={} events={} bytes={} ({:.2} MB/s) renders={} drain={:.1}ms/s render={:.1}ms/s busy={:.1}ms/s",
                    t.ticks,
                    t.events,
                    t.bytes,
                    (t.bytes as f64 / 1e6) / secs,
                    t.renders,
                    t.drain_ns as f64 / 1e6 / secs,
                    t.render_ns as f64 / 1e6 / secs,
                    t.tick_ns as f64 / 1e6 / secs,
                ));
                *t = TickStats { window: Some(Instant::now()), ..TickStats::zero() };
            }
        });
    }
}

/// `--help`/`--version` classification, checked before ANY other mode (crash-report,
/// session-daemon, single-instance gate, …) so `avada --help` always prints to stdout
/// and exits, instead of silently forwarding to a running primary instance or falling through
/// to the GUI launch path.
#[derive(Debug, PartialEq, Eq)]
enum InfoMode {
    Help,
    Version,
}

/// Classifies `argv` as a `--help`/`--version` request, or `None` for anything else (including
/// a bare GUI launch or a subcommand like `worker`/`pair`). Only looks at `argv[1]` — the first
/// CLI arg after the program path — so e.g. `avada -c "echo --help"` isn't misclassified.
#[tracing::instrument(level = "debug", ret)]
fn cli_info_mode(argv: &[String]) -> Option<InfoMode> {
    match argv.get(1).map(String::as_str) {
        Some("--help") | Some("-h") | Some("help") => Some(InfoMode::Help),
        Some("--version") | Some("-V") => Some(InfoMode::Version),
        _ => None,
    }
}

const USAGE: &str = "\
avada — tiled terminal workspace with AI-pane orchestration

USAGE:
    avada                    Launch the GUI (resumes the last session, or a workspace
                                   file / -c command passed on the command line)
    avada worker --queue <name> [--worker <id>] [--count N] [--worktree --base <committish>]
                       [--retry-window <secs>] [--nack-delay <ms>] -- <cmd> [args...]
                                   Drain a work queue by running <cmd> per claimed task
    avada pair [--device <label>] [--ttl <30d|12h|90m|<ms>>]
                                   Mint a per-device token and print pairing URLs + a QR code
    avada devices             List paired mobile devices
    avada revoke <label>      Revoke a paired device by label
    avada attach [<pane>] [--resize] [--detach-key <key>]
                                   Render a live pane in THIS terminal (--list to see them)
    avada ssh <status|enable|disable|authorize|keys|revoke|serve>
                                   Manage the embedded SSH server, so a phone running Termius,
                                   Blink or plain ssh can attach to a pane (off by default,
                                   loopback-only, public-key auth; `ssh --help` for details)
    avada control-mode [--session-name <n>] [--resize] [--no-dcs]
                                   Serve the panes over tmux control mode (`tmux -CC`), for
                                   iTerm2 and the mobile tmux clients

FLAGS:
    --kill-daemon                  Shut down the running session daemon for this install, then exit
    -h, --help                     Print this help and exit
    -V, --version                  Print the version and exit

    --session-daemon <salt>        (internal) run the headless session daemon
    --crash-report <path>          (internal) show the crash-recovery dialog for a log
";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // t0 for the perf log (#2 startup) — must be the very first thing so every mark is
    // relative to process entry. Inert unless `AVADA_PERFLOG` is set.
    perf::init();
    perf::mark("main: enter");

    // Pin the launch directory before ANY mode below can `set_current_dir` (the workspace
    // resolver, a tool spawn, the crash reporter). `state::resolve_new_pane_cwd` uses it as
    // the fallback that makes `cd project && avada` open its terminals in `project`
    // instead of `$HOME`, and that only means anything if it is read at process entry.
    state::pin_launch_dir();

    // `--help`/`--version`: handled before ANY other mode, including the single-instance gate
    // (~line 358 pre-fix), which would otherwise silently forward these to a running primary
    // and exit 0 without printing anything (the original defect).
    {
        let argv: Vec<String> = std::env::args().collect();
        match cli_info_mode(&argv) {
            Some(InfoMode::Help) => {
                print!("{USAGE}");
                return Ok(());
            }
            Some(InfoMode::Version) => {
                println!("avada {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            None => {}
        }

        // First launch after the rename: copy the `hyperpanes` app-support directory into
        // the `avada` one (once, marker-guarded, never moving — the old install may still
        // be running). Before `prefs::load` so the copied settings are the ones we boot
        // with; logged once logging exists.
        let migration = avada_core::compat::migrate_user_data();

        // Logging: one subscriber per process, file per role, level from the persisted
        // `logLevel` setting unless `AVADA_LOG` / `AVADA_DEBUG` override it.
        let level = prefs::load().log_level;
        avada_core::logging::init(log_role(&argv), &level);
        tracing::debug!(argv = ?argv, "process start");
        migration.log();
    }

    // Crash-reporter mode: a fresh process spawned by the panic hook (or by the next launch when a
    // crash went unacknowledged) to show the recovery dialog. Handle it before ANY app init or the
    // single-instance gate — it only needs a Tokio runtime for rfd's portal backend, and it never
    // installs the panic hook below (so a panic in the reporter can't recurse).
    {
        let args: Vec<String> = std::env::args().collect();
        if let Some(i) = args.iter().position(|a| a == "--crash-report") {
            let log = args
                .get(i + 1)
                .map(std::path::PathBuf::from)
                .unwrap_or_else(crash::default_log_path);
            let rt = tokio::runtime::Runtime::new()?;
            let _guard = rt.enter();
            let outcome = crash::run_report(&log);
            crash::clear_marker();
            if matches!(outcome, crash::Outcome::Relaunch) {
                crash::relaunch();
            }
            return Ok(());
        }
    }

    // Capture any panic to a crash log (the windowed subsystem has no console), then pop a crash
    // reporter from a fresh process (this one is unwinding) — see `crate::crash`.
    std::panic::set_hook(Box::new(|info| {
        use std::io::Write;
        tracing::error!(panic = %info, "panic");
        let path = std::env::temp_dir().join("avada-crash.log");
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = writeln!(f, "PANIC: {info}");
            let bt = std::backtrace::Backtrace::force_capture();
            let _ = writeln!(f, "{bt}");
        }
        crate::crash::write_marker(&path);
        // Guard against recursion if the reporter itself panics (it sets this env on its child).
        if avada_core::compat::env_var_os("AVADA_CRASH_CHILD").is_none() {
            if let Ok(exe) = std::env::current_exe() {
                let _ = std::process::Command::new(exe)
                    .arg("--crash-report")
                    .arg(&path)
                    .env("AVADA_CRASH_CHILD", "1")
                    .spawn();
            }
        }
    }));

    // Session-daemon mode (`--session-daemon <salt>`): run the headless PTY-owning daemon
    // and return — no GUI, no fonts, no window. The daemon owns the sessions so they
    // survive a GUI crash (session-daemon-plan M0); the logic lives entirely in core, this
    // is just the entry. It builds its own Tokio runtime and blocks until the daemon exits.
    let argv0: Vec<String> = std::env::args().collect();
    if let Some(salt) = session_daemon_salt(&argv0) {
        tracing::debug!("session-daemon: starting for salt {salt:?}");
        // The SSH front door (M3) rides along with the daemon: the daemon process is the one
        // that outlives every GUI window, and the server is just another client of its
        // socket. Off unless the user enabled it, and never fatal to the daemon.
        ssh::spawn_with_daemon(&salt);
        avada_core::session::daemon::run(&salt)?;
        return Ok(());
    }

    // `--kill-daemon` mode (M3): connect to the running session daemon for THIS user-data dir
    // and ask it to kill its sessions + exit, then return without launching a GUI. A no-op if
    // none is running. The salt is the user-data dir — the same key `new_daemon` and the
    // daemon's discovery use — so we kill the daemon that THIS install/dev build would attach
    // to (an isolated instance has its own).
    if wants_kill_daemon(&argv0) {
        let salt = avada_core::persistence::paths::user_data_dir()
            .to_string_lossy()
            .into_owned();
        match avada_core::session::daemon::kill_daemon(&salt) {
            Ok(true) => tracing::debug!("kill-daemon: shut the running daemon down"),
            Ok(false) => tracing::debug!("kill-daemon: no daemon was running (no-op)"),
            Err(e) => tracing::debug!("kill-daemon: error {e}"),
        }
        return Ok(());
    }

    // Headless worker mode (`worker --queue <q> -- <cmd>`): drain a work queue by running a
    // child command per claimed task, acking/nacking on its exit, until the queue empties —
    // then return without launching a GUI. Worker runner MVP (#10); the loop lives in `worker`.
    if worker::wants_worker(&argv0) {
        return worker::run(&argv0);
    }

    // Everything below that is a plain command line — `pair`, `devices`, `revoke`, `attach`,
    // `ctl` — writes its answer to stdout, and whoever asked is entitled to stop reading it:
    // `avada ctl panes | head -3` is an ordinary thing to type. Rust starts every process
    // with SIGPIPE ignored so a closed pipe arrives as an `io::Error` rather than a signal, and
    // `println!` has nowhere to put that error but a panic — so the most routine shell idiom
    // there is ends in a backtrace and a crash dialog, for a condition every other Unix tool
    // treats as "the reader has what it wanted". Restore the default disposition, but only for
    // those modes: the GUI, the session daemon and the worker keep the ignore, because there a
    // broken pipe is a thing to handle and dying on one would take live terminals with it.
    if pipeable_cli(&argv0) {
        restore_default_sigpipe();
    }

    // `pair` mode: mint a per-device token, print mobile-app pairing URLs + a terminal QR, then
    // return without launching a GUI (docs/mobile-client-plan.md).
    if pair::wants_pair(&argv0) {
        return pair::run(&argv0).map_err(Into::into);
    }

    // `devices` / `revoke <label>`: list or drop paired mobile clients via the control API.
    if devices::wants_devices(&argv0) {
        return devices::run_list().map_err(Into::into);
    }
    if devices::wants_revoke(&argv0) {
        return devices::run_revoke(&argv0).map_err(Into::into);
    }

    // `attach`: render a live pane into this terminal — the tmux-client half of the mux
    // backend (docs/mux-backend-plan.md M2). Talks to the running daemon over the same
    // salted socket the GUI uses and returns without launching a GUI.
    if attach_cli::wants_attach(&argv0) {
        return attach_cli::run(&argv0).map_err(Into::into);
    }

    // `ssh`: manage the embedded SSH server that lets a phone attach to a pane with no
    // avada software on it (docs/mux-backend-plan.md M3).
    if ssh::wants_ssh(&argv0) {
        return ssh::run(&argv0).map_err(Into::into);
    }

    // `control-mode`: speak the SERVER half of tmux's control protocol on stdio, so iTerm2
    // and the mobile tmux clients see avada panes as tmux panes (M4). Like `attach`,
    // a pure daemon client — no GUI, no single-instance gate.
    if control_mode_cli::wants_control_mode(&argv0) {
        return control_mode_cli::run(&argv0).map_err(Into::into);
    }

    // `ctl <verb>`: the workspace's own command line over the running control API — the tool
    // surface the always-on Hyperpane tab hands its agent, and a plain shell command anywhere
    // else. A client of the HTTP server, so like `attach` it launches no GUI.
    if ctl_cli::wants_ctl(&argv0) {
        return ctl_cli::run(&argv0).map_err(Into::into);
    }
    if ctl_cli::wants_schema_cli(&argv0) {
        return ctl_cli::run_schema(&argv0).map_err(Into::into);
    }

    // Extract the baked-in OFL fonts (Fira Code / JetBrains Mono) so they always resolve.
    crate::prefs::init_bundled_fonts();

    let rt = tokio::runtime::Runtime::new()?;
    let _guard = rt.enter();

    // Single-instance gate (replaces Electron `requestSingleInstanceLock`). Salted by the
    // userData dir so an isolated instance (temp APPDATA / XDG dirs) or a differently-housed
    // dev build never collides with the installed app, exactly like Electron keyed its lock
    // off the userData path. A second launch forwards `{argv, cwd}` to the primary and exits;
    // the primary drains hand-offs in `App::tick` and routes them (attach / new window).
    let argv: Vec<String> = std::env::args().collect();
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".to_string());
    let salt = avada_core::persistence::paths::user_data_dir()
        .to_string_lossy()
        .into_owned();
    let mut handoff_primary = None;
    match avada_core::single_instance::acquire(&salt) {
        Ok(avada_core::single_instance::Instance::Secondary(sec)) => {
            tracing::debug!("single-instance: secondary, forwarding argv");
            let msg = avada_core::single_instance::HandoffMessage { argv, cwd };
            let fwd = rt.block_on(async move { sec.forward(&msg).await });
            tracing::debug!("single-instance: forward -> {fwd:?}");
            // Don't wait for the runtime's worker threads on the way out — the hand-off
            // is flushed; exit like Electron's second instance does.
            drop(_guard);
            rt.shutdown_timeout(Duration::from_secs(2));
            fwd?;
            return Ok(());
        }
        Ok(avada_core::single_instance::Instance::Primary(primary)) => {
            tracing::debug!("single-instance: primary, serving hand-offs");
            handoff_primary = Some(primary);
        }
        Err(e) => {
            // Gate unavailable on this platform/setup → run standalone.
            tracing::debug!("single-instance: gate unavailable ({e})");
        }
    }

    let (etx, erx) = unbounded_channel::<SessionEvent>();
    // Backend selection (session-daemon-plan M4 — daemon DEFAULT-ON everywhere): sessions run in
    // the PTY-owning daemon so they survive a GUI crash. Opt OUT with AVADA_SESSION_DAEMON=0
    // (forces today's in-process path); opt IN on any platform with =1. Default-on everywhere:
    // unix rides a UDS, Windows a named pipe, and on Windows the ConPTYs additionally live in a
    // pty-host process so a daemon upgrade never touches them. Selected ONCE here — the GUI's
    // `Arc<SessionManager>` and every call site are backend-agnostic. The daemon is keyed by the SAME `salt` (the user-data dir) as the
    // single-instance gate above, so a dev/isolated instance gets its own daemon. A connect/spawn
    // failure falls back to in-process rather than blocking launch — the daemon is an enhancement,
    // never a hard dependency.
    let want_daemon = match avada_core::compat::env_var("AVADA_SESSION_DAEMON").as_deref() {
        Some("1") => true,
        Some("0") => false,
        _ => true,
    };
    let mgr = Arc::new(if want_daemon {
        match SessionManager::new_daemon(etx.clone(), &salt) {
            Ok(m) => {
                tracing::debug!("session-backend: daemon");
                m
            }
            Err(e) => {
                // LOUD, not debug-gated: in-process ptys die with the window, so this is the
                // one failure that silently revokes session survival across a restart. The
                // usual cause is a socket path over SUN_LEN (a long TMPDIR), which is
                // fixable — but only by someone who has been told.
                eprintln!(
                    "avada: session daemon unavailable ({e}); running ptys in-process \
                     — terminals will NOT survive restarting the app"
                );
                tracing::debug!(
                    "session-backend: daemon unavailable ({e}); falling back to in-process"
                );
                SessionManager::new(etx)
            }
        }
    } else {
        SessionManager::new(etx)
    });

    // Auto-register the Claude SessionStart/SessionEnd hook (best-effort, idempotent) so the
    // pane→conversation marker is written reliably — backing claude-resume and the goals
    // system's marker-gated delivery without the user hand-editing settings.json. Only touches
    // existing Claude config dirs; a missing bundled hook or any write error is a silent no-op.
    if let Some(hook) = avada_core::claude_hook::bundled_hook_path() {
        avada_core::claude_hook::ensure_registered(&hook);
    }

    // The app owns the window registry + the shared session stream.
    let application = App::new(mgr.clone(), erx);

    // Primary: accept hand-offs on a background task; `App::tick` drains the channel on the
    // UI thread (the handler runs on the tokio runtime and must not touch UI state).
    if let Some(primary) = handoff_primary {
        let (htx, hrx) = std::sync::mpsc::channel();
        application.set_handoff_rx(hrx);
        rt.spawn(async move {
            let _ = primary
                .run_server(move |msg| {
                    let _ = htx.send(msg);
                })
                .await;
        });
    }

    // Wire the launch seed: `avada -c "<cmd>" --shell … --cwd … --name …` (or a
    // positional workspace `.avada`/`.json`) seeds the first window from that spec;
    // a bare launch falls back to the LAST SESSION (`last-workspace.json`, written when
    // the final window closes — see `app::persist_last_session`), so tabs/layout/per-pane
    // zoom survive a plain relaunch (#14). A first-ever launch (no last-session file)
    // stays an empty shell pane.
    let seed = match avada_core::workspace::launch::resolve_launch_workspace(&argv, &cwd) {
        Some(file) => PendingSeed::Workspace(Box::new(file)),
        None => PendingSeed::EmptyTab,
    };
    // Next-launch crash detection: if a previous run crashed and its instant reporter never ran
    // (or was killed), surface the dialog now from a separate process. Primary only — a secondary
    // already returned above. The instant reporter clears the marker once shown, so this won't
    // double-fire after a normal crash + dismiss.
    if let Some(log) = crash::pending() {
        crash::clear_marker();
        if let Ok(exe) = std::env::current_exe() {
            let _ = std::process::Command::new(exe)
                .arg("--crash-report")
                .arg(&log)
                .env("AVADA_CRASH_CHILD", "1")
                .spawn();
        }
    }

    perf::mark("main: spawn_window begin");
    application.spawn_window(seed);
    perf::mark("main: spawn_window done");

    // If auto-update is on, do a quiet GitHub-releases check on startup. This runs on a
    // background thread inside `check`, so it never blocks startup; an offline/failed check
    // is silently skipped, and an available update only surfaces a hint in Preferences →
    // General (never auto-downloads/-installs). Reads the persisted setting directly so we
    // don't depend on a window's state being seeded yet.
    if prefs::load().auto_update {
        application.update.check(true);
    }

    // One shared pump timer drives every window (drain → render → reap). The interval is
    // ADAPTIVE (#3): it starts at the fast cadence and `App::tick` slows it to the idle
    // cadence after a stretch with no work, waking back to fast on input/output. The closure
    // holds a `Weak` so storing the `Timer` inside the `App` (so `tick` can re-interval it)
    // doesn't create a strong reference cycle.
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(app::TICK_FAST_MS),
        {
            let weak = std::rc::Rc::downgrade(&application);
            move || {
                if let Some(app) = weak.upgrade() {
                    app.tick();
                }
            }
        },
    );
    application.set_timer(timer); // App owns the timer for the whole loop + adjusts its interval

    slint::run_event_loop()?;

    // The frame watcher debounces its writes, so a quit inside that window would drop the
    // very last move or resize — the one the human just made. Commit it now, while the
    // loop is down but the process is still alive.
    window::flush_geometry();

    // Quit-vs-keep-alive (session-daemon-plan M3). Read the persisted preference fresh (it
    // may have been toggled this session) — default ON: "keep terminals running in the
    // background when Avada closes".
    //
    //  * Daemon backend + keep-alive ON  → LEAVE the daemon (and its sessions) running, so a
    //    relaunch re-attaches the survivors (the whole point of the daemon). We do NOT call
    //    `kill_all` here, which would defeat persistence.
    //  * Daemon backend + keep-alive OFF → ask the daemon to shut down (kill its sessions +
    //    exit), so nothing lingers after an explicit quit.
    //  * In-process backend (either way) → the PTYs are our children and die with us; the
    //    keep-alive preference is INERT, and `kill_all` is the historical clean teardown.
    let keep_alive = prefs::load().keep_alive;
    match quit_action(
        mgr.is_daemon(),
        keep_alive,
        GUI_RESTARTING.load(std::sync::atomic::Ordering::SeqCst),
    ) {
        QuitAction::LeaveRunning => tracing::debug!(
            "quit: keep-alive ON (or GUI restart) — leaving the daemon + sessions running"
        ),
        QuitAction::ShutdownDaemon => {
            tracing::debug!("quit: keep-alive OFF — shutting the daemon down");
            mgr.shutdown_daemon();
        }
        QuitAction::KillChildren => mgr.kill_all(),
    }
    Ok(())
}

/// What quitting should do to the terminals this GUI was showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QuitAction {
    /// Walk away. The daemon and its sessions outlive us, and the next launch re-attaches.
    LeaveRunning,
    /// Ask the daemon to kill its sessions and exit, so an explicit quit leaves nothing.
    ShutdownDaemon,
    /// No daemon: the ptys are our own children and want the historical clean teardown.
    KillChildren,
}

/// Decide [`QuitAction`] from the three facts that bear on it.
///
/// Lifted out of the quit path so it can be tested at all. The effects it chooses between —
/// killing a process tree, telling a daemon to exit — only happen while a real GUI is shutting
/// down, which is exactly the moment no test can be present for; the *decision*, meanwhile, is
/// three booleans and has been wrong before.
///
/// `restarting` is the subtle one. `keep_alive = false` means "don't leave terminals running
/// after I **quit**", and a GUI restart is not a quit: the whole point of one is that the panes
/// survive while a new build re-attaches to them. So a restart overrides the preference rather
/// than obeying it. `keep_alive` is inert without a daemon, because there is nothing
/// out-of-process to keep alive.
#[tracing::instrument(level = "debug", ret)]
pub(crate) fn quit_action(is_daemon: bool, keep_alive: bool, restarting: bool) -> QuitAction {
    match (is_daemon, keep_alive || restarting) {
        (false, _) => QuitAction::KillChildren,
        (true, true) => QuitAction::LeaveRunning,
        (true, false) => QuitAction::ShutdownDaemon,
    }
}

#[cfg(test)]
mod quit_action_tests {
    use super::{quit_action, QuitAction};

    #[test]
    fn a_restart_never_takes_the_terminals_down_with_it() {
        // The case this seam exists for: `keep_alive = false` plus a restart used to be one
        // `||` away from killing every pane the restart was supposed to preserve.
        assert_eq!(
            quit_action(true, false, true),
            QuitAction::LeaveRunning,
            "a GUI restart is not a quit"
        );
        assert_eq!(quit_action(true, true, true), QuitAction::LeaveRunning);
    }

    #[test]
    fn an_explicit_quit_obeys_the_preference() {
        assert_eq!(quit_action(true, true, false), QuitAction::LeaveRunning);
        assert_eq!(quit_action(true, false, false), QuitAction::ShutdownDaemon);
    }

    #[test]
    fn without_a_daemon_the_preference_is_inert() {
        // Nothing out-of-process to keep alive: the ptys are our children either way, and a
        // `LeaveRunning` here would orphan them rather than persist them.
        for keep_alive in [true, false] {
            for restarting in [true, false] {
                assert_eq!(
                    quit_action(false, keep_alive, restarting),
                    QuitAction::KillChildren,
                    "keep_alive={keep_alive} restarting={restarting}"
                );
            }
        }
    }
}

/// Seed a richer workspace (2 tabs, several panes, non-default layouts) so a
/// screenshot exercises the Wave-1 surface. Gated by `AVADA_DEMO`.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) fn demo_seed(st: &mut State, mgr: &SessionManager) {
    use avada_core::layout::presets::Layout;
    // tab 0: 3 panes in main-stack (shows the main divider + focus ring)
    dispatch(st, Command::NewPane, mgr);
    dispatch(st, Command::NewPane, mgr);
    dispatch(st, Command::SetLayout(Layout::MainStack), mgr);
    // tab 1: 2 panes in columns (a vertical divider)
    dispatch(st, Command::NewTab, mgr);
    dispatch(st, Command::NewPane, mgr);
    dispatch(st, Command::SetLayout(Layout::Columns), mgr);
    // land on tab 0
    dispatch(st, Command::SwitchTab(0), mgr);
}

/// Whether `text` is the Slint special key `k`.
#[tracing::instrument(level = "debug", ret)]
pub(crate) fn is_key(text: &str, k: Key) -> bool {
    let s: SharedString = k.into();
    text == s.as_str()
}

/// The modifier that means **control to the pty**.
///
/// Slint swaps Command and Control on macOS (`i_slint_core::input`), so `KeyMsg.control` is the
/// Command key there and `KeyMsg.meta` is the physical Control key. App chords deliberately ride
/// the swapped slot — Cmd+Shift+P opens the palette on a Mac, which is what a Mac user expects.
/// A terminal is the one place the swap is wrong: Ctrl+C has to interrupt the foreground process
/// and Cmd+C has to copy, and the swap gave both the other one's job. So the bytes written to the
/// pty take their control modifier from the *physical* Control key, and Cmd is left free to be
/// the app's modifier (`pane.copy`, `pane.paste`).
#[tracing::instrument(level = "debug", ret)]
pub(crate) fn pty_ctrl(msg: &KeyMsg) -> bool {
    if cfg!(target_os = "macos") {
        msg.meta
    } else {
        msg.control
    }
}

/// Whether a key event should reach the shell at all. Drops bare modifiers
/// (Slint reports Shift/Ctrl/Alt/Meta as low control codepoints), F-keys, and
/// other special keys Slint delivers as control/private-use codepoints that
/// `encode_key` would otherwise pass through as garbage bytes.
#[tracing::instrument(level = "debug", ret)]
pub(crate) fn forwardable(text: &str) -> bool {
    // Special keys we explicitly translate to terminal sequences (encode_key).
    const ALLOWED: [Key; 14] = [
        Key::UpArrow,
        Key::DownArrow,
        Key::LeftArrow,
        Key::RightArrow,
        Key::Home,
        Key::End,
        Key::PageUp,
        Key::PageDown,
        Key::Delete,
        Key::Return,
        Key::Backspace,
        Key::Tab,
        // Shift+Tab arrives as Backtab (U+0019, a C0 control char) — without this entry
        // the control-char filter below ate it and Shift+Tab never reached the pty.
        Key::Backtab,
        Key::Escape,
    ];
    if ALLOWED.iter().any(|k| {
        let s: SharedString = (*k).into();
        text == s.as_str()
    }) {
        return true;
    }
    // Otherwise only forward normal printable text. Bare modifiers (U+0010..0012)
    // and other control chars, DEL (U+007F), and private-use special keys
    // (U+E000..F8FF: F-keys, Insert, Menu, …) are dropped.
    text.chars().next().is_some_and(|c| {
        let u = c as u32;
        u >= 0x20 && u != 0x7f && !(0xe000..=0xf8ff).contains(&u)
    })
}

/// Translate a key event's text into a [`keybindings::KeyTok`] (the modifier-agnostic key
/// token, the native port of the renderer's normalised `e.key`). Arrows / F11 / Tab / Enter /
/// Escape map to their named tokens; every other printable key becomes a [`KeyTok::Char`]
/// (letters, digits, and symbols like `=`/`-`/`0`) lower-cased so a chord matches regardless
/// of Shift. With Ctrl held Slint reports a control char (Ctrl+A = U+0001 … Ctrl+Z = U+001A),
/// so map that back to its letter. Shared by the router and the keybindings editor's capture.
#[tracing::instrument(level = "debug", ret)]
pub(crate) fn key_tok_from_text(text: &str, control: bool) -> Option<keybindings::KeyTok> {
    use keybindings::KeyTok;
    // Named keys first — these must win before the Ctrl control-char remap (e.g. Ctrl+Tab
    // arrives as U+0009 which would otherwise look like Ctrl+I).
    if is_key(text, Key::LeftArrow) {
        return Some(KeyTok::Left);
    }
    if is_key(text, Key::RightArrow) {
        return Some(KeyTok::Right);
    }
    if is_key(text, Key::UpArrow) {
        return Some(KeyTok::Up);
    }
    if is_key(text, Key::DownArrow) {
        return Some(KeyTok::Down);
    }
    if is_key(text, Key::F11) {
        return Some(KeyTok::F11);
    }
    if is_key(text, Key::Tab) {
        return Some(KeyTok::Tab);
    }
    if is_key(text, Key::Return) {
        return Some(KeyTok::Enter);
    }
    if is_key(text, Key::Escape) {
        return Some(KeyTok::Escape);
    }
    let c = text.chars().next()?;
    let u = c as u32;
    // Slint's NAMED MODIFIER keys are C0 control codepoints (key_codes.rs): Shift=U+0010,
    // Control=U+0011, Alt=U+0012, AltGr=U+0013, CapsLock=U+0014, ShiftR=U+0015,
    // ControlR=U+0016, Meta=U+0017, MetaR=U+0018. They must NEVER reach the Ctrl
    // control-char remap below: pressing the bare Shift key while Ctrl was already down
    // delivered U+0010 with ctrl+shift modifiers, which remapped to 'p' — a phantom
    // Ctrl+Shift+P that popped the command palette on every Ctrl+Shift press. Real letter
    // keys arrive as their literal character on this backend (live-traced: Ctrl+Shift+C =
    // "C"), so dropping the modifier codepoints loses nothing.
    if (0x10..=0x18).contains(&u) {
        return None;
    }
    // Remap a control codepoint back to a letter only when Ctrl is actually held (Ctrl+A =
    // U+0001 … Ctrl+Z = U+001A). The named-key checks above already consumed Tab/Enter/Esc.
    if control && (1..=26).contains(&u) {
        return Some(KeyTok::Char((b'a' + (u as u8) - 1) as char));
    }
    if c == ' ' {
        return Some(KeyTok::Space);
    }
    // On many keyboard layouts "+" is Shift+"=", so a Ctrl++ chord arrives with the literal
    // "+" text. Normalize it to "=" so it resolves to the zoom-in binding (Ctrl+=) the same as
    // the unshifted key (match_chord is also Shift-tolerant for "=").
    if c == '+' {
        return Some(KeyTok::Char('='));
    }
    let lc = c.to_ascii_lowercase();
    let lu = lc as u32;
    // Any other printable, non-control character is a Char token.
    if lu >= 0x20 && lu != 0x7f && !(0xe000..=0xf8ff).contains(&lu) {
        Some(KeyTok::Char(lc))
    } else {
        None
    }
}

#[tracing::instrument(level = "debug", ret)]
fn key_tok(msg: &KeyMsg) -> Option<keybindings::KeyTok> {
    key_tok_from_text(&msg.text, msg.control)
}

/// Resolve a key event to a bound [`Command`] via the user's keymap (overrides win over
/// defaults — see [`keybindings::Keymap::match_chord`]).
#[tracing::instrument(level = "debug", ret, skip(keymap))]
pub(crate) fn route_chord(keymap: &keybindings::Keymap, msg: &KeyMsg) -> Option<Command> {
    let tok = key_tok(msg)?;
    keymap.match_chord(msg.control, msg.alt, msg.shift, tok)
}

/// Translate a key event into the chord + typed text a tier-5 module surface expects.
///
/// Three things separate this from [`route_chord`], and each is a decision about who owns
/// the keyboard:
///
/// * **The control modifier is the physical one** ([`pty_ctrl`]). A module's presets are
///   written in editor vocabulary (`C-s`, `C-w`), which everywhere but a Mac's Slint
///   remapping means the Control key. Cmd stays the app's modifier, so Cmd+Shift+P still
///   opens the palette while an editor pane has focus.
/// * **More keys count.** `key_tok` only knows the keys a *binding* can name; an editor
///   also needs Backspace, Delete, Home, End and the page keys, which the pty path
///   otherwise encodes to escape sequences no module would see.
/// * **Shift is only spelled out when it is not already in the character.** Shift+A is
///   `shift+a` (so a preset can write `A`), but Shift+1 is `!` — the character carries it,
///   and `shift+!` would match nothing anyone would write.
///
/// The second half of the pair is the text the key would type, `None` for anything that
/// types nothing. A module gets it whether or not the chord is bound, because the most
/// common key in a text editor is the one with no binding at all.
#[tracing::instrument(level = "debug", ret)]
pub(crate) fn grid_chord(msg: &KeyMsg) -> Option<(String, Option<String>)> {
    // Named keys an editor needs that no app binding can name, checked before `key_tok`
    // so they never fall through to its printable-character branch.
    const EXTRA: [(Key, &str); 6] = [
        (Key::Backspace, "backspace"),
        (Key::Delete, "delete"),
        (Key::Home, "home"),
        (Key::End, "end"),
        (Key::PageUp, "pageup"),
        (Key::PageDown, "pagedown"),
    ];
    let named = EXTRA
        .iter()
        .find(|(k, _)| is_key(&msg.text, *k))
        .map(|(_, n)| (*n).to_string());
    let letter = named.is_none()
        && msg.text.chars().count() == 1
        && msg.text.chars().all(|c| c.is_ascii_alphabetic());
    let key = match named {
        Some(n) => n,
        None => key_tok(msg)?.token(),
    };
    // A one-character key that is not a letter already differs when Shift is held, so
    // saying "shift" as well would name a chord no preset spells.
    let spell_shift = msg.shift && (key.chars().count() > 1 || letter);
    let ctrl = pty_ctrl(msg);
    let mut chord = String::new();
    for (on, name) in [(ctrl, "ctrl"), (msg.alt, "alt"), (spell_shift, "shift")] {
        if on {
            chord.push_str(name);
            chord.push('+');
        }
    }
    chord.push_str(&key);
    let text = avada_terminal_widget::keys::is_printable(&msg.text, ctrl, msg.alt)
        .then(|| msg.text.to_string());
    Some((chord, text))
}

/// Translate a key event into a palette command while the palette overlay is open
/// (`query` is the current `state.palette_query`; the key router calls this before any
/// pty forwarding). The palette's query is **controller-owned**, not a focused Slint
/// `TextInput`: the old input grabbed focus with a one-shot `init => focus()` on the
/// freshly created overlay, which doesn't reliably land in Slint (the in-pane search box
/// hit the same thing — see widget.slint), so typed keys leaked into the shell underneath
/// and dismissing the palette could leave nothing focused (keyboard dead, Ctrl+Shift+P
/// included, until a click). Routing the keys here keeps the terminal `FocusScope` focused
/// the whole time, so the palette needs no focus hand-off in either direction.
/// `None` = swallow (no key reaches the pty while the palette is open).
#[tracing::instrument(level = "debug", ret)]
pub(crate) fn palette_key(query: &str, msg: &KeyMsg) -> Option<Command> {
    if is_key(&msg.text, Key::UpArrow) {
        return Some(Command::PaletteNav(-1));
    }
    if is_key(&msg.text, Key::DownArrow) {
        return Some(Command::PaletteNav(1));
    }
    if is_key(&msg.text, Key::Return) {
        return Some(Command::PaletteActivate);
    }
    if is_key(&msg.text, Key::Escape) {
        return Some(Command::CloseOverlay);
    }
    if is_key(&msg.text, Key::Backspace) {
        let mut q = query.to_string();
        q.pop();
        return Some(Command::PaletteQuery(q));
    }
    // Modifier chords are not query text (Ctrl+Shift+… is handled before this; the rest
    // are swallowed so e.g. Ctrl+V can't dump a control char into the shell).
    if msg.control || msg.alt {
        return None;
    }
    // Ordinary printable text extends the query (same printable test as `forwardable`).
    let c = msg.text.chars().next()?;
    let u = c as u32;
    if u >= 0x20 && u != 0x7f && !(0xe000..=0xf8ff).contains(&u) {
        return Some(Command::PaletteQuery(format!("{query}{}", msg.text)));
    }
    None
}

/// Translate a FORWARDED key event from the New-goal box into a command. The goal field is a
/// real Slint `TextInput` that owns text editing (typing, Left/Right/Up/Down/Home/End cursor
/// motion, Backspace) natively; it forwards ONLY the navigation/options/submit/dismiss keys here.
/// `field` is the focused field (0 = text, 1..=4 = chips), `menu_open` whether the focused
/// field's option list is showing.
///
/// Layout: Ctrl+O reveals/hides the option chips; Tab / Shift+Tab cycle chip focus (each chip
/// auto-shows its dropdown); Cmd+H opens the goal-history list (or cycles it forward if already
/// open); ↓/↑ move an already-open list (chips apply live) but never open one themselves; Enter
/// picks a history row, else submits; Esc collapses the options/list, then closes the box; Ctrl+V
/// pastes an image attachment (or text). Left/Right/Up/Down on the bare text field never reach
/// here — the TextInput moves its cursor with them.
#[tracing::instrument(level = "debug", ret)]
pub(crate) fn goal_key(field: usize, menu_open: bool, msg: &KeyMsg) -> Option<Command> {
    // Ctrl+O toggles the option chips (Slint may deliver the letter or the control char).
    if msg.control && !msg.alt && (msg.text == "o" || msg.text == "O" || msg.text == "\u{0f}") {
        return Some(Command::GoalToggleOptions);
    }
    // Ctrl+V pastes: an image on the clipboard becomes an attachment, otherwise text.
    if msg.control && !msg.alt && (msg.text == "v" || msg.text == "V" || msg.text == "\u{16}") {
        return Some(Command::GoalPasteClipboard);
    }
    // Cmd+H opens the goal-history list, or cycles it forward if already open.
    if msg.meta && !msg.control && !msg.alt && (msg.text == "h" || msg.text == "H") {
        return Some(if menu_open {
            Command::GoalMenuNav(1)
        } else {
            Command::GoalMenu(true)
        });
    }
    if is_key(&msg.text, Key::Escape) {
        return Some(if menu_open || field != 0 {
            Command::GoalCollapse
        } else {
            Command::CloseOverlay
        });
    }
    if is_key(&msg.text, Key::Tab) {
        return Some(Command::GoalNav(if msg.shift { -1 } else { 1 }));
    }
    // With a chip focused (options open), Left/Right move between the category chips. On the text
    // field they never reach here — the TextInput moves its cursor with them.
    if field != 0 {
        if is_key(&msg.text, Key::LeftArrow) {
            return Some(Command::GoalNav(-1));
        }
        if is_key(&msg.text, Key::RightArrow) {
            return Some(Command::GoalNav(1));
        }
    }
    if is_key(&msg.text, Key::DownArrow) {
        // A closed list on the bare text field (field == 0) is native caret movement — the
        // TextInput shouldn't forward it here at all, but guard anyway for a stray event.
        return if menu_open {
            Some(Command::GoalMenuNav(1))
        } else if field != 0 {
            Some(Command::GoalMenu(true))
        } else {
            None
        };
    }
    if is_key(&msg.text, Key::UpArrow) {
        return menu_open.then_some(Command::GoalMenuNav(-1));
    }
    if is_key(&msg.text, Key::Return) {
        // On a history row (text field, list open) → pick it. On a chip (options open) → confirm
        // the live-applied selection and collapse the options. On the bare text field → submit.
        return Some(if field == 0 && menu_open {
            Command::GoalMenuPick
        } else if field != 0 {
            Command::GoalCollapse
        } else {
            Command::GoalSubmit
        });
    }
    None
}

/// Whether this argv selects a command line whose whole job is to print an answer and exit —
/// the modes where a closed stdout means the reader is satisfied, not that something broke.
///
/// One predicate rather than a check at each entry point, so the SIGPIPE decision is made in a
/// single place and can be tested without spawning a process. Deliberately excludes the modes
/// that outlive their output: the session daemon, the worker, and the GUI.
#[tracing::instrument(level = "debug", ret)]
fn pipeable_cli(argv: &[String]) -> bool {
    ctl_cli::wants_ctl(argv)
        || ctl_cli::wants_schema_cli(argv)
        || pair::wants_pair(argv)
        || devices::wants_devices(argv)
        || devices::wants_revoke(argv)
        || attach_cli::wants_attach(argv)
}

/// Put SIGPIPE back to its default disposition, so writing to a closed pipe ends this process
/// the way it ends `cat` or `ls` — silently, with the conventional status — instead of
/// returning an error that `println!` escalates into a panic.
#[cfg(unix)]
#[tracing::instrument(level = "debug", ret)]
fn restore_default_sigpipe() {
    // SAFETY: `signal(2)` with `SIG_DFL` only writes this process's disposition for one
    // signal. Called once, on the main thread, before any of these CLIs has written a byte.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

/// No-op off unix: there is no SIGPIPE, and a closed pipe already surfaces as an ordinary
/// write error.
#[cfg(not(unix))]
#[tracing::instrument(level = "debug", ret)]
fn restore_default_sigpipe() {}

#[cfg(test)]
mod tests {
    // `avada ctl panes | head -3` used to end in a panic and a crash dialog: Rust
    // ignores SIGPIPE, so the closed pipe came back to `println!` as an error it could only
    // panic on. The disposition is restored for the print-and-exit CLIs and nothing else —
    // the daemon in particular must keep ignoring it, since a broken socket write there is a
    // condition to handle and not a reason to take the user's terminals down.
    #[test]
    fn only_the_print_and_exit_command_lines_die_on_a_closed_pipe() {
        let argv = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        for yes in [
            vec!["avada", "ctl", "panes"],
            vec!["avada", "pair"],
            vec!["avada", "devices"],
            vec!["avada", "revoke", "phone"],
            vec!["avada", "attach", "pane-1"],
        ] {
            assert!(
                super::pipeable_cli(&argv(&yes)),
                "{yes:?} is a pipeable CLI"
            );
        }

        for no in [
            vec!["avada"],
            vec!["avada", "--session-daemon", "/tmp/salt"],
            vec!["avada", "worker", "--queue", "q"],
            vec!["avada", "--kill-daemon"],
        ] {
            assert!(
                !super::pipeable_cli(&argv(&no)),
                "{no:?} keeps SIGPIPE ignored"
            );
        }
    }

    use super::*;

    fn msg(text: &str, ctrl: bool, alt: bool, shift: bool) -> KeyMsg {
        KeyMsg {
            text: text.into(),
            control: ctrl,
            alt,
            shift,
            meta: false,
        }
    }

    fn msg_meta(text: &str) -> KeyMsg {
        KeyMsg {
            text: text.into(),
            control: false,
            alt: false,
            shift: false,
            meta: true,
        }
    }

    // ---- pty_ctrl: which physical key means "control" to the shell ----

    #[test]
    fn the_pty_takes_its_control_from_the_physical_control_key() {
        // On macOS Slint hands us Command in `control` and physical Control in `meta`; the pty
        // wants the one the user thinks of as Ctrl. Everywhere else the two agree.
        let physical_ctrl = msg_meta("c");
        let command = msg("c", true, false, false);
        if cfg!(target_os = "macos") {
            assert!(pty_ctrl(&physical_ctrl), "Ctrl+C must still interrupt");
            assert!(
                !pty_ctrl(&command),
                "Cmd+C is the app's copy, not an interrupt"
            );
        } else {
            assert!(pty_ctrl(&command));
            assert!(!pty_ctrl(&physical_ctrl));
        }
    }

    #[test]
    fn an_unmodified_key_is_never_a_control_key() {
        assert!(!pty_ctrl(&msg("c", false, false, false)));
        assert!(!pty_ctrl(&msg("c", false, true, true)));
    }

    fn keymap() -> keybindings::Keymap {
        // No user overrides → the compiled-in defaults (Ctrl+Shift+P → palette).
        keybindings::Keymap::default_for_tests()
    }

    // ---- --session-daemon mode parsing ----

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn session_daemon_salt_parses_space_and_eq_forms() {
        assert_eq!(
            session_daemon_salt(&argv(&["avada", "--session-daemon", "/data/dir"])),
            Some("/data/dir".to_string())
        );
        assert_eq!(
            session_daemon_salt(&argv(&["avada", "--session-daemon=/data/dir"])),
            Some("/data/dir".to_string())
        );
    }

    // The Windows pty-host is spawned as a daemon whose salt carries a `\u{1}pty-host`
    // marker, so the flag must pass a salt through byte-for-byte — no trimming, no splitting
    // on anything but the argv boundary the OS already drew.
    #[test]
    fn session_daemon_salt_passes_the_pty_host_marker_through_verbatim() {
        let marked = "/data/dir\u{1}pty-host";
        assert_eq!(
            session_daemon_salt(&argv(&["avada", "--session-daemon", marked])),
            Some(marked.to_string())
        );
        assert_eq!(
            session_daemon_salt(&argv(&["avada", &format!("--session-daemon={marked}")])),
            Some(marked.to_string())
        );
    }

    #[test]
    fn session_daemon_salt_is_none_for_a_normal_launch() {
        assert_eq!(session_daemon_salt(&argv(&["avada"])), None);
        assert_eq!(
            session_daemon_salt(&argv(&["avada", "-c", "ls", "--cwd", "/tmp"])),
            None
        );
        // A bare flag with no following salt yields None (nothing to run a daemon for).
        assert_eq!(
            session_daemon_salt(&argv(&["avada", "--session-daemon"])),
            None
        );
    }

    // ---- --kill-daemon flag parsing (M3) ----

    #[test]
    fn kill_daemon_flag_is_detected() {
        assert!(wants_kill_daemon(&argv(&["avada", "--kill-daemon"])));
        // Tolerates other args around it.
        assert!(wants_kill_daemon(&argv(&[
            "avada",
            "--foo",
            "--kill-daemon",
            "bar"
        ])));
    }

    #[test]
    fn kill_daemon_flag_is_absent_on_a_normal_launch() {
        assert!(!wants_kill_daemon(&argv(&["avada"])));
        assert!(!wants_kill_daemon(&argv(&["avada", "-c", "ls"])));
        // Not confused by the daemon-RUN flag.
        assert!(!wants_kill_daemon(&argv(&[
            "avada",
            "--session-daemon",
            "/data"
        ])));
    }

    // ---- --help / --version classification ----

    #[test]
    fn cli_info_mode_detects_help() {
        for flag in ["--help", "-h", "help"] {
            assert_eq!(
                cli_info_mode(&argv(&["avada", flag])),
                Some(InfoMode::Help),
                "flag {flag:?} not classified as Help"
            );
        }
    }

    #[test]
    fn cli_info_mode_detects_version() {
        for flag in ["--version", "-V"] {
            assert_eq!(
                cli_info_mode(&argv(&["avada", flag])),
                Some(InfoMode::Version),
                "flag {flag:?} not classified as Version"
            );
        }
    }

    #[test]
    fn cli_info_mode_is_none_for_a_normal_launch() {
        assert_eq!(cli_info_mode(&argv(&["avada"])), None);
        assert_eq!(
            cli_info_mode(&argv(&["avada", "-c", "ls", "--cwd", "/tmp"])),
            None
        );
        assert_eq!(
            cli_info_mode(&argv(&["avada", "worker", "--queue", "q"])),
            None
        );
        assert_eq!(cli_info_mode(&argv(&["avada", "pair"])), None);
        assert_eq!(cli_info_mode(&argv(&["avada", "--kill-daemon"])), None);
        // Only argv[1] is checked, not flags/args elsewhere on the line.
        assert_eq!(cli_info_mode(&argv(&["avada", "-c", "echo --help"])), None);
    }

    // ---- Ctrl+Shift+P → palette, pinned at the ROUTER level (the full text→tok→chord
    // path a live key event takes), for both encodings Slint can deliver the key in.

    #[test]
    fn bare_modifier_presses_route_no_chord() {
        // Slint named modifiers are C0 codepoints (Shift=U+0010 … MetaR=U+0018). U+0010 used
        // to remap to 'p' under the Ctrl control-char rule, so pressing the bare Shift key
        // with Ctrl already down was a phantom Ctrl+Shift+P that popped the palette (live-
        // reproduced; the old test here pinned that phantom as "the control-char encoding").
        for u in 0x10u32..=0x18 {
            let text = char::from_u32(u).unwrap().to_string();
            let cmd = route_chord(&keymap(), &msg(&text, true, false, true));
            assert!(cmd.is_none(), "modifier codepoint {u:#x} routed {cmd:?}");
        }
    }

    #[test]
    fn ctrl_shift_p_opens_palette_letter_encoding() {
        // Real letter keys arrive as their literal character (live-traced: Ctrl+Shift+C =
        // "C"): shifted = "P", plain = "p".
        for text in ["P", "p"] {
            let cmd = route_chord(&keymap(), &msg(text, true, false, true));
            assert!(
                matches!(cmd, Some(Command::PaletteOpen)),
                "text {text:?} got {cmd:?}"
            );
        }
    }

    #[test]
    fn ctrl_p_without_shift_is_not_the_palette() {
        for text in ["P", "p"] {
            assert!(route_chord(&keymap(), &msg(text, true, false, false)).is_none());
        }
    }

    #[test]
    fn the_copy_chord_copies_not_palette() {
        // The chord that surfaced the phantom: the copy chord must copy — and the bare-Shift
        // press on the way to it (previous test) must not open the palette first. Shifted
        // everywhere but macOS, where copy is a bare Cmd+C.
        let shift = !cfg!(target_os = "macos");
        for text in ["C", "c"] {
            let cmd = route_chord(&keymap(), &msg(text, true, false, shift));
            assert!(
                matches!(cmd, Some(Command::CopyFocused)),
                "text {text:?} got {cmd:?}"
            );
        }
    }

    // ---- palette_key: the app-side keyboard while the palette overlay is open ----

    #[test]
    fn palette_key_edits_query() {
        // Printable text appends; Backspace pops (and is a no-op edit on empty).
        assert!(matches!(
            palette_key("la", &msg("y", false, false, false)),
            Some(Command::PaletteQuery(q)) if q == "lay"
        ));
        let bs: slint::SharedString = Key::Backspace.into();
        assert!(matches!(
            palette_key("lay", &msg(bs.as_str(), false, false, false)),
            Some(Command::PaletteQuery(q)) if q == "la"
        ));
        assert!(matches!(
            palette_key("", &msg(bs.as_str(), false, false, false)),
            Some(Command::PaletteQuery(q)) if q.is_empty()
        ));
    }

    #[test]
    fn palette_key_navigates_activates_dismisses() {
        let up: slint::SharedString = Key::UpArrow.into();
        let down: slint::SharedString = Key::DownArrow.into();
        let enter: slint::SharedString = Key::Return.into();
        let esc: slint::SharedString = Key::Escape.into();
        assert!(matches!(
            palette_key("", &msg(up.as_str(), false, false, false)),
            Some(Command::PaletteNav(-1))
        ));
        assert!(matches!(
            palette_key("", &msg(down.as_str(), false, false, false)),
            Some(Command::PaletteNav(1))
        ));
        assert!(matches!(
            palette_key("", &msg(enter.as_str(), false, false, false)),
            Some(Command::PaletteActivate)
        ));
        assert!(matches!(
            palette_key("", &msg(esc.as_str(), false, false, false)),
            Some(Command::CloseOverlay)
        ));
    }

    #[test]
    fn palette_key_swallows_chords_and_control_chars() {
        // Ctrl+V (control char 0x16) must not become query text — and must not reach the
        // pty either (the caller swallows on None).
        assert!(palette_key("q", &msg("\u{16}", true, false, false)).is_none());
        // Alt+letter is a chord, not text.
        assert!(palette_key("q", &msg("x", false, true, false)).is_none());
        // Bare modifier presses carry control/private-use text — swallowed.
        assert!(palette_key("q", &msg("\u{11}", false, false, false)).is_none());
    }

    // ---- goal_key(field, menu_open, msg): only the forwarded nav/options/submit/dismiss keys
    // (the TextInput owns text + cursor; Left/Right never reach here). ----

    #[test]
    fn goal_key_options_toggle_and_tab() {
        let tab: slint::SharedString = Key::Tab.into();
        // Ctrl+O reveals/hides the option chips (letter or control-char form).
        assert!(matches!(
            goal_key(0, false, &msg("o", true, false, false)),
            Some(Command::GoalToggleOptions)
        ));
        assert!(matches!(
            goal_key(0, false, &msg("\u{0f}", true, false, false)),
            Some(Command::GoalToggleOptions)
        ));
        // Tab / Shift+Tab move field focus (reveal-then-cycle handled in state).
        assert!(matches!(
            goal_key(0, false, &msg(tab.as_str(), false, false, false)),
            Some(Command::GoalNav(1))
        ));
        assert!(matches!(
            goal_key(1, false, &msg(tab.as_str(), false, false, true)),
            Some(Command::GoalNav(-1))
        ));
        // With a chip focused, Left/Right also move between categories; on the text field they
        // don't reach goal_key (native cursor) — so they're not nav there.
        let left: slint::SharedString = Key::LeftArrow.into();
        let right: slint::SharedString = Key::RightArrow.into();
        assert!(matches!(
            goal_key(2, false, &msg(right.as_str(), false, false, false)),
            Some(Command::GoalNav(1))
        ));
        assert!(matches!(
            goal_key(2, false, &msg(left.as_str(), false, false, false)),
            Some(Command::GoalNav(-1))
        ));
        assert!(goal_key(0, false, &msg(left.as_str(), false, false, false)).is_none());
    }

    #[test]
    fn goal_key_paste_menu_submit_dismiss() {
        let up: slint::SharedString = Key::UpArrow.into();
        let down: slint::SharedString = Key::DownArrow.into();
        let enter: slint::SharedString = Key::Return.into();
        let esc: slint::SharedString = Key::Escape.into();
        // Ctrl+V pastes (image → attachment, else text).
        assert!(matches!(
            goal_key(0, false, &msg("v", true, false, false)),
            Some(Command::GoalPasteClipboard)
        ));
        // ↓ on the bare text field with the list closed is native caret movement — goal_key
        // never opens the list itself (that's Cmd+H now); on a chip it still opens that chip's
        // list, and an already-open list navigates either way. ↑ only navigates an open list.
        assert!(goal_key(0, false, &msg(down.as_str(), false, false, false)).is_none());
        assert!(matches!(
            goal_key(2, false, &msg(down.as_str(), false, false, false)),
            Some(Command::GoalMenu(true))
        ));
        assert!(matches!(
            goal_key(1, true, &msg(down.as_str(), false, false, false)),
            Some(Command::GoalMenuNav(1))
        ));
        assert!(matches!(
            goal_key(1, true, &msg(up.as_str(), false, false, false)),
            Some(Command::GoalMenuNav(-1))
        ));
        assert!(goal_key(0, false, &msg(up.as_str(), false, false, false)).is_none());
        // Cmd+H opens the goal-history list, or cycles it forward if already open.
        assert!(matches!(
            goal_key(0, false, &msg_meta("h")),
            Some(Command::GoalMenu(true))
        ));
        assert!(matches!(
            goal_key(0, true, &msg_meta("h")),
            Some(Command::GoalMenuNav(1))
        ));
        // Plain "h" without the meta modifier is ordinary typing — the TextInput's job.
        assert!(goal_key(0, false, &msg("h", false, false, false)).is_none());
        // Enter picks a HISTORY row (text field + list open); submits everywhere else.
        assert!(matches!(
            goal_key(0, true, &msg(enter.as_str(), false, false, false)),
            Some(Command::GoalMenuPick)
        ));
        // Enter on a chip confirms the live selection and collapses the options.
        assert!(matches!(
            goal_key(2, true, &msg(enter.as_str(), false, false, false)),
            Some(Command::GoalCollapse)
        ));
        // Enter on the bare text field submits.
        assert!(matches!(
            goal_key(0, false, &msg(enter.as_str(), false, false, false)),
            Some(Command::GoalSubmit)
        ));
        // Esc collapses the options/list first (chip focused, or a list open), then closes the box.
        assert!(matches!(
            goal_key(1, true, &msg(esc.as_str(), false, false, false)),
            Some(Command::GoalCollapse)
        ));
        assert!(matches!(
            goal_key(0, false, &msg(esc.as_str(), false, false, false)),
            Some(Command::CloseOverlay)
        ));
    }

    #[test]
    fn goal_key_ignores_text_keys() {
        // Printable text + backspace are the TextInput's job — goal_key returns None for them.
        assert!(goal_key(0, false, &msg("x", false, false, false)).is_none());
        let bs: slint::SharedString = Key::Backspace.into();
        assert!(goal_key(0, false, &msg(bs.as_str(), false, false, false)).is_none());
    }

    // ---- grid_chord: a keystroke in the module keymap dialect ----

    fn named(key: Key) -> KeyMsg {
        let s: slint::SharedString = key.into();
        msg(s.as_str(), false, false, false)
    }

    /// The spelling has to match what a module author writes in a preset, because the two
    /// are compared as strings after normalization and nothing else reconciles them.
    #[test]
    fn a_keystroke_spells_the_chord_a_preset_would_have_written() {
        assert_eq!(grid_chord(&msg("h", false, false, false)).unwrap().0, "h");
        assert_eq!(
            grid_chord(&msg("H", false, false, true)).unwrap().0,
            "shift+h"
        );
        assert_eq!(
            grid_chord(&msg("j", false, true, false)).unwrap().0,
            "alt+j"
        );
        assert_eq!(
            grid_chord(&named(Key::Escape)).unwrap().0,
            "escape",
            "a named key is spelled by its token, not by the character it carries"
        );

        // Modifier order is fixed so `ctrl+alt+shift+x` has exactly one spelling.
        let all = KeyMsg {
            text: "X".into(),
            control: !cfg!(target_os = "macos"),
            meta: cfg!(target_os = "macos"),
            alt: true,
            shift: true,
        };
        assert_eq!(grid_chord(&all).unwrap().0, "ctrl+alt+shift+x");
    }

    /// The six keys a module needs that the app's own binding vocabulary never had to name.
    /// They are checked before `key_tok` so they cannot fall through to its printable branch.
    #[test]
    fn the_six_editor_keys_the_app_never_needed_to_name_are_named_here() {
        for (key, token) in [
            (Key::Backspace, "backspace"),
            (Key::Delete, "delete"),
            (Key::Home, "home"),
            (Key::End, "end"),
            (Key::PageUp, "pageup"),
            (Key::PageDown, "pagedown"),
        ] {
            assert_eq!(grid_chord(&named(key)).unwrap().0, token, "{token}");
        }
    }

    /// Shift is only spelled when it is not already visible in the key itself — otherwise
    /// `shift+!` would name a chord no preset writes and `!` would never fire.
    #[test]
    fn shift_is_spelled_only_when_the_key_does_not_already_say_it() {
        assert_eq!(grid_chord(&msg("!", false, false, true)).unwrap().0, "!");
        assert_eq!(
            grid_chord(&msg("A", false, false, true)).unwrap().0,
            "shift+a"
        );
        let s: slint::SharedString = Key::Tab.into();
        assert_eq!(
            grid_chord(&msg(s.as_str(), false, false, true)).unwrap().0,
            "shift+tab",
            "a multi-character key name cannot carry the shift itself"
        );
    }

    /// An unbound chord still has to type. The text rides along so the host does not have to
    /// ask a second time, and a control chord types nothing at all.
    #[test]
    fn a_printable_chord_carries_its_text_and_a_control_chord_carries_none() {
        assert_eq!(
            grid_chord(&msg("q", false, false, false))
                .unwrap()
                .1
                .as_deref(),
            Some("q")
        );
        let ctrl_s = if cfg!(target_os = "macos") {
            msg_meta("s")
        } else {
            msg("s", true, false, false)
        };
        let (chord, text) = grid_chord(&ctrl_s).unwrap();
        assert_eq!(chord, "ctrl+s");
        assert!(text.is_none(), "Ctrl+S types nothing, it does something");
        assert!(grid_chord(&named(Key::Escape)).unwrap().1.is_none());
    }
}
