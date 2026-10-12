//! macOS: run the session daemon as its own launchd job.
//!
//! Why. A daemon forked straight out of the GUI — even `setsid`'d, even re-parented to
//! init — stays a *subordinate process* of the application in LaunchServices' eyes: same
//! launchd service, same coalition, same responsible pid. Force Quit (the Dock, the ⌥⌘⎋
//! dialog, Activity Monitor) terminates an application "including its subordinate
//! processes", so a Force Quit of a wedged GUI killed the daemon silently, closed every
//! pty master and took every pane's shell with it (incident 2026-10-09,
//! `docs/incidents/2026-10-09-b3/`). An orderly Avada ▸ Quit never did, which is why the
//! handoff looked solid for months.
//!
//! The Linux build already dodges the same class of bug with `systemd-run --scope`
//! (`daemon_client::spawn_daemon_detached`). This is the macOS analog: write a tiny
//! per-spawn plist and `launchctl bootstrap` it into the user's `gui/<uid>` domain. The
//! daemon is then launchd's child, in its own service, and no application's teardown
//! reaches it.
//!
//! Shape of the job:
//!  * **Unique label per spawn** (`to.avada.terminal.daemon.<salt-hash>.<nonce>`): the
//!    successor is started while the incumbent is still alive (build handshake / takeover),
//!    so two daemons for one salt coexist for a moment and one label would collide.
//!  * `RunAtLoad` + `KeepAlive=false`: launchd starts it once and never restarts it. A
//!    respawned daemon after a handoff would fight the successor for the flock — which is
//!    why `launchctl submit` (keeps jobs alive) is not used.
//!  * `AbandonProcessGroup=true`: when the daemon exits after handing over, launchd must
//!    not kill the shells that were in its process group.
//!  * `EnvironmentVariables`: only `TMPDIR` / `XDG_RUNTIME_DIR` (the socket path derives
//!    from them, `daemon::runtime_dir`, so the daemon must see the GUI's values) and the
//!    job's own label, which the daemon logs and then strips so it never reaches a pane.
//!    Nothing else from the GUI's environment is written to disk. The daemon gets launchd's
//!    minimal environment — exactly what a LaunchServices-launched GUI had anyway, so pane
//!    environments are unchanged. (A dev build started from a terminal used to leak that
//!    terminal's environment into panes through the daemon; under launchd it no longer
//!    does.)
//!  * Stale jobs are garbage-collected before each spawn: a bootstrapped job whose process
//!    has exited stays registered (pid-less, `state = not running`) until it is booted
//!    out. Jobs with a live pid are never touched.
//!
//! Everything launchd-specific is behind `target_os = "macos"`; the plist rendering and
//! the `launchctl print` parsing are plain string work and tested everywhere.

use std::path::{Path, PathBuf};

/// Every daemon job label starts with this; GC only ever looks at labels that do.
pub const LABEL_PREFIX: &str = "to.avada.terminal.daemon.";

/// The environment variable the plist hands the daemon so it can log which job it is.
/// The daemon removes it from its own environment right after reading it.
pub const LABEL_ENV: &str = "AVADA_DAEMON_LAUNCHD_LABEL";

/// `to.avada.terminal.daemon.<salt-hash>.<nonce>` — see the module docs for why the nonce.
pub fn daemon_label(salt_hash: u64, nonce: u64) -> String {
    format!("{LABEL_PREFIX}{salt_hash:016x}.{nonce:x}")
}

/// Where the per-spawn plists live (`<state_dir>/launchd/<label>.plist`).
#[tracing::instrument(level = "debug", ret)]
pub fn plist_dir() -> PathBuf {
    crate::persistence::paths::state_dir().join("launchd")
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// The launchd property list for one daemon job. `env` is written verbatim (escaped), so
/// callers pass only what the module docs allow — never the whole GUI environment.
pub fn render_daemon_plist(
    label: &str,
    exe: &Path,
    salt: &str,
    env: &[(String, String)],
) -> String {
    let mut s = String::new();
    s.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    s.push_str("<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n");
    s.push_str("<plist version=\"1.0\">\n<dict>\n");
    s.push_str(&format!(
        "\t<key>Label</key>\n\t<string>{}</string>\n",
        xml_escape(label)
    ));
    s.push_str("\t<key>ProgramArguments</key>\n\t<array>\n");
    s.push_str(&format!(
        "\t\t<string>{}</string>\n",
        xml_escape(&exe.to_string_lossy())
    ));
    s.push_str("\t\t<string>--session-daemon</string>\n");
    s.push_str(&format!("\t\t<string>{}</string>\n", xml_escape(salt)));
    s.push_str("\t</array>\n");
    s.push_str("\t<key>RunAtLoad</key>\n\t<true/>\n");
    s.push_str("\t<key>KeepAlive</key>\n\t<false/>\n");
    s.push_str("\t<key>AbandonProcessGroup</key>\n\t<true/>\n");
    if !env.is_empty() {
        s.push_str("\t<key>EnvironmentVariables</key>\n\t<dict>\n");
        for (k, v) in env {
            s.push_str(&format!(
                "\t\t<key>{}</key>\n\t\t<string>{}</string>\n",
                xml_escape(k),
                xml_escape(v)
            ));
        }
        s.push_str("\t</dict>\n");
    }
    s.push_str("</dict>\n</plist>\n");
    s
}

/// `true` when `launchctl print gui/<uid>/<label>` output describes a job with a live
/// process (`pid = 1234`). A loaded-but-exited job prints `state = not running` and no pid
/// line; an unloaded label makes `launchctl print` fail instead.
pub fn print_shows_pid(output: &str) -> bool {
    output.lines().any(|l| {
        l.trim().strip_prefix("pid = ").is_some_and(|rest| {
            rest.trim()
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit())
        })
    })
}

/// The environment the plist carries: the runtime-dir variables (same values the GUI has,
/// so the socket path matches) and the job label. Nothing else.
pub fn job_env(label: &str) -> Vec<(String, String)> {
    let mut env = Vec::new();
    for key in ["TMPDIR", "XDG_RUNTIME_DIR"] {
        if let Some(v) = std::env::var_os(key) {
            if let Some(v) = v.to_str() {
                if !v.is_empty() {
                    env.push((key.to_string(), v.to_string()));
                }
            }
        }
    }
    env.push((LABEL_ENV.to_string(), label.to_string()));
    env
}

/// Bootstrap `exe --session-daemon <salt>` as a fresh launchd job in `gui/<uid>`. Returns
/// the job label. Any failure (no launchctl, a domain that refuses the bootstrap, an
/// unwritable state dir) is an `Err` the caller turns into the `setsid` fallback — a daemon
/// with the old exposure beats no daemon.
#[cfg(target_os = "macos")]
#[tracing::instrument(level = "debug", ret)]
pub fn spawn_daemon_launchd(exe: &Path, salt: &str) -> std::io::Result<String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use std::process::{Command, Stdio};

    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    let domain = format!("gui/{uid}");
    let dir = plist_dir();
    std::fs::create_dir_all(&dir)?;
    gc_stale_jobs(&domain, &dir);

    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
        ^ (u64::from(std::process::id()) << 40);
    let label = daemon_label(crate::session::daemon::fnv1a64(salt), nonce);
    let path = dir.join(format!("{label}.plist"));
    let body = render_daemon_plist(&label, exe, salt, &job_env(&label));
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?
        .write_all(body.as_bytes())?;

    let out = Command::new("/bin/launchctl")
        .args(["bootstrap", &domain])
        .arg(&path)
        .stdin(Stdio::null())
        .output();
    match out {
        Ok(o) if o.status.success() => Ok(label),
        Ok(o) => {
            let _ = std::fs::remove_file(&path);
            Err(std::io::Error::other(format!(
                "launchctl bootstrap {domain} failed ({}): {}",
                o.status,
                String::from_utf8_lossy(&o.stderr).trim()
            )))
        }
        Err(e) => {
            let _ = std::fs::remove_file(&path);
            Err(e)
        }
    }
}

/// Boot out every daemon job of ours whose process has exited, and drop its plist. A job
/// that still has a pid is a live daemon (incumbent or successor) and is left alone; a
/// label launchd no longer knows (after a reboot, or a bootstrap that never happened) just
/// loses its plist.
#[cfg(target_os = "macos")]
#[tracing::instrument(level = "debug")]
fn gc_stale_jobs(domain: &str, dir: &Path) {
    use std::process::{Command, Stdio};
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("plist") {
            continue;
        }
        let Some(label) = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else {
            continue;
        };
        if !label.starts_with(LABEL_PREFIX) {
            continue;
        }
        let target = format!("{domain}/{label}");
        let printed = Command::new("/bin/launchctl")
            .args(["print", &target])
            .stdin(Stdio::null())
            .output();
        if let Ok(o) = &printed {
            if o.status.success() {
                if print_shows_pid(&String::from_utf8_lossy(&o.stdout)) {
                    continue; // live daemon: never touched
                }
                let _ = Command::new("/bin/launchctl")
                    .args(["bootout", &target])
                    .stdin(Stdio::null())
                    .output();
                tracing::info!(label, "launchd: booted out an exited daemon job");
            }
        }
        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_has_prefix_hash_and_nonce() {
        let l = daemon_label(0xdead_beef, 0x2a);
        assert_eq!(l, "to.avada.terminal.daemon.00000000deadbeef.2a");
        assert!(l.starts_with(LABEL_PREFIX));
        assert_ne!(
            daemon_label(1, 1),
            daemon_label(1, 2),
            "nonce must make labels distinct"
        );
    }

    #[test]
    fn plist_carries_program_flags_and_escapes() {
        let exe = Path::new("/Apps/A&B <x>/Contents/MacOS/avada");
        let env = vec![("TMPDIR".to_string(), "/var/folders/x/T/".to_string())];
        let p = render_daemon_plist("to.avada.terminal.daemon.1.2", exe, "sa\"lt", &env);
        assert!(p.contains("<key>Label</key>\n\t<string>to.avada.terminal.daemon.1.2</string>"));
        assert!(p.contains("<string>/Apps/A&amp;B &lt;x&gt;/Contents/MacOS/avada</string>"));
        assert!(p.contains("<string>--session-daemon</string>\n\t\t<string>sa&quot;lt</string>"));
        assert!(p.contains("<key>RunAtLoad</key>\n\t<true/>"));
        assert!(p.contains("<key>KeepAlive</key>\n\t<false/>"));
        assert!(p.contains("<key>AbandonProcessGroup</key>\n\t<true/>"));
        assert!(p.contains("<key>TMPDIR</key>\n\t\t<string>/var/folders/x/T/</string>"));
        assert!(!p.contains("&amp;amp;"), "no double escaping");
    }

    #[test]
    fn plist_without_env_has_no_env_dict() {
        let p = render_daemon_plist("l", Path::new("/x"), "s", &[]);
        assert!(!p.contains("EnvironmentVariables"));
    }

    #[test]
    fn print_output_pid_detection() {
        let running = "\tpath = /x.plist\n\ttype = LaunchAgent\n\tstate = running\n\tpid = 35370\n\tlast exit code = (never exited)\n";
        let exited = "\tpath = /x.plist\n\ttype = LaunchAgent\n\tstate = not running\n\tlast exit code = 0\n";
        assert!(print_shows_pid(running));
        assert!(!print_shows_pid(exited));
        assert!(!print_shows_pid(""));
        assert!(!print_shows_pid("\tpid = -\n"));
    }

    #[test]
    fn job_env_is_minimal_and_carries_label() {
        let env = job_env("lbl");
        assert!(env.iter().any(|(k, v)| k == LABEL_ENV && v == "lbl"));
        for (k, _) in &env {
            assert!(
                k == "TMPDIR" || k == "XDG_RUNTIME_DIR" || k == LABEL_ENV,
                "unexpected key {k} written to a plist"
            );
        }
    }

    /// Real launchd round-trip around `/bin/sleep`: bootstrap, see a pid, boot out. Needs a
    /// macOS login session, so it is opt-in: `cargo test -- --ignored launchd_round_trip`.
    #[test]
    #[ignore]
    #[cfg(target_os = "macos")]
    fn launchd_round_trip() {
        use std::process::Command;
        let uid = unsafe { libc::getuid() };
        let domain = format!("gui/{uid}");
        let label = format!("{LABEL_PREFIX}test.{}", std::process::id());
        let dir = std::env::temp_dir().join("avada-launchd-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{label}.plist"));
        // `sleep` instead of the daemon: the shape is what is under test.
        let body = render_daemon_plist(&label, Path::new("/bin/sleep"), "30", &[])
            .replace("<string>--session-daemon</string>\n\t\t", "");
        std::fs::write(&path, body).unwrap();
        let b = Command::new("/bin/launchctl")
            .args(["bootstrap", &domain])
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            b.status.success(),
            "bootstrap: {}",
            String::from_utf8_lossy(&b.stderr)
        );
        let target = format!("{domain}/{label}");
        let p = Command::new("/bin/launchctl")
            .args(["print", &target])
            .output()
            .unwrap();
        assert!(
            print_shows_pid(&String::from_utf8_lossy(&p.stdout)),
            "job should be running"
        );
        let o = Command::new("/bin/launchctl")
            .args(["bootout", &target])
            .output()
            .unwrap();
        assert!(o.status.success());
        let _ = std::fs::remove_file(&path);
    }
}
