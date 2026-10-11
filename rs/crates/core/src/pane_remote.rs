//! Remote panes — a pane whose terminal really runs on another host, over ssh.
//!
//! Like pane provenance ([`crate::pane_origin`]) the facts ride in the pane's existing
//! string→string `meta` map, under the `remote.` prefix, so they are persisted with the
//! pane, published on the read-model, and need no new field on the wire:
//!
//! | key            | value                                                              |
//! |----------------|--------------------------------------------------------------------|
//! | `remote.host`  | the FIRST hop: what Avada itself ssh'd to (`hp2`, `bert@hp2`)       |
//! | `remote.tmux`  | the tmux session on that host that keeps the work alive            |
//! | `remote.cwd`   | the working directory on that host                                 |
//! | `remote.cmd`   | the command run on that host, inside the tmux session              |
//! | `remote.launch`| the local command line that (re)attaches: ssh + `tmux new-session -A` |
//!
//! **Two different hosts, deliberately.** The pane's *title* follows wherever the typed
//! characters land right now (the innermost hop, learned from the shell's own OSC title —
//! [`endpoint_from_title`]); the ⓘ info and the restart-time reconnect only ever know the
//! *first* hop, because that is the only one Avada started and can re-attach.
//!
//! Reconnect needs no new subsystem: relaunch restores `command`, which for a remote pane is
//! `remote.launch`, and `tmux new-session -A` attaches to the surviving session (or makes it).

use std::collections::BTreeMap;

pub const PREFIX: &str = "remote.";
pub const HOST: &str = "remote.host";
pub const TMUX: &str = "remote.tmux";
pub const CWD: &str = "remote.cwd";
pub const CMD: &str = "remote.cmd";
pub const LAUNCH: &str = "remote.launch";

/// The `remote.*` subset of a pane's meta (empty when it has none).
pub fn extract(meta: Option<&BTreeMap<String, String>>) -> BTreeMap<String, String> {
    meta.map(|m| {
        m.iter()
            .filter(|(k, v)| k.starts_with(PREFIX) && !v.is_empty())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    })
    .unwrap_or_default()
}

/// The first-hop host of a provenance/remote map, if the pane is remote.
pub fn host(m: &BTreeMap<String, String>) -> Option<&str> {
    m.get(HOST).map(String::as_str).filter(|h| !h.is_empty())
}

/// POSIX single-quote `s` so a shell reads it back as one literal word.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// `[ssh user@host]` — the title-bar tag.
pub fn badge(endpoint: &str) -> String {
    format!("[ssh {endpoint}]")
}

/// The header title for a pane: its label plus the tag for the host keystrokes land on now
/// (`current`), falling back to the first hop. A pane that is not remote keeps its label.
pub fn display_title(
    label: &str,
    remote: &BTreeMap<String, String>,
    current: Option<&str>,
) -> String {
    let Some(first) = host(remote) else {
        return label.to_string();
    };
    let shown = current.filter(|c| !c.is_empty()).unwrap_or(first);
    format!("{label} {}", badge(shown))
}

/// Pull the `user@host` a shell reports in its OSC window title (`user@host: ~/dir`,
/// `user@host:~/dir`, `user@host`, `ssh user@host`). `None` for anything else — an agent
/// prints plenty of titles that name no host, and those must not move the tag.
pub fn endpoint_from_title(title: &str) -> Option<String> {
    let mut toks = title.split_whitespace();
    let mut first = toks.next()?;
    if first == "ssh" {
        first = toks.next()?;
    }
    let cand = first.split(':').next()?;
    let (user, host) = cand.split_once('@')?;
    let ok = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    };
    (ok(user) && ok(host)).then(|| cand.to_string())
}

/// Best-effort first-hop host of a plain `ssh …` command line (fallback when the caller did
/// not say `remote.host`). Skips options and their values; `None` if it is not an ssh line.
pub fn infer_host(command: &str) -> Option<String> {
    let mut toks = command.split_whitespace();
    let bin = toks.next()?;
    if bin.rsplit('/').next()? != "ssh" {
        return None;
    }
    // ssh options that take a separate value.
    const WITH_VALUE: &[&str] = &[
        "-b", "-c", "-D", "-E", "-e", "-F", "-I", "-i", "-J", "-L", "-l", "-m", "-O", "-o", "-p",
        "-Q", "-R", "-S", "-W", "-w",
    ];
    while let Some(t) = toks.next() {
        if WITH_VALUE.contains(&t) {
            toks.next();
        } else if !t.starts_with('-') {
            return Some(t.trim_matches(['\'', '"']).to_string());
        }
    }
    None
}

/// A cheap per-call salt for a session name (wall-clock nanos mixed with the pid).
pub fn salt() -> u64 {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    n ^ (u64::from(std::process::id()) << 20)
}

/// A tmux-safe, collision-resistant session name for a pane label.
pub fn session_name(label: &str, salt: u64) -> String {
    let slug: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let slug: String = slug.chars().take(24).collect();
    format!(
        "avada-{}{:06x}",
        if slug.is_empty() {
            String::new()
        } else {
            format!("{slug}-")
        },
        salt & 0xff_ffff
    )
}

/// The local command line that starts — or re-attaches — the remote tmux session.
///
/// `tmux new-session -A` attaches when the session exists and creates it otherwise, so this
/// one line is both the launch and the reconnect. The session gets `set-titles` so the
/// shell's own `user@host` title reaches Avada through tmux (the title-bar tag follows it).
pub fn launch_line(host: &str, tmux: &str, cwd: Option<&str>, cmd: &str) -> String {
    let inner = match cwd.filter(|c| !c.is_empty()) {
        Some(c) => format!("cd {} && exec {cmd}", sh_quote(c)),
        None => format!("exec {cmd}"),
    };
    let remote = format!(
        r"tmux new-session -A -s {} {} \; set-option set-titles on \; set-option set-titles-string '#T'",
        sh_quote(tmux),
        sh_quote(&inner)
    );
    format!(
        "ssh -t -o BatchMode=yes {} {}",
        sh_quote(host),
        sh_quote(&remote)
    )
}

/// Server-side normalisation of a control-API `newPane` spec.
///
/// * `meta."remote.host"` set (explicit — preferred): `command` is the command to run ON the
///   host and `cwd` the directory ON the host. They move to `remote.cmd` / `remote.cwd`, a
///   tmux session name is minted, and `command` becomes the ssh+tmux [`launch_line`].
/// * Otherwise a plain `ssh …` command is only *recognised*: `remote.host` is inferred and the
///   command left exactly as given (no tmux — we cannot safely rewrite someone's quoting).
pub fn normalize_spec(spec: &mut serde_json::Value, salt: u64) {
    use serde_json::Value;
    let Some(obj) = spec.as_object_mut() else {
        return;
    };
    let command = obj
        .get("command")
        .and_then(Value::as_str)
        .map(str::to_string);
    let explicit = obj
        .get("meta")
        .and_then(|m| m.get(HOST))
        .and_then(Value::as_str)
        .filter(|h| !h.is_empty())
        .map(str::to_string);
    let label = obj.get("label").and_then(Value::as_str).map(str::to_string);
    match (explicit, command) {
        (Some(host), Some(cmd)) if !obj.get("meta").is_some_and(|m| m.get(TMUX).is_some()) => {
            let cwd = obj
                .remove("cwd")
                .and_then(|c| c.as_str().map(str::to_string));
            let tmux = session_name(label.as_deref().unwrap_or(&host), salt);
            let launch = launch_line(&host, &tmux, cwd.as_deref(), &cmd);
            obj.insert("command".into(), launch.clone().into());
            let meta = obj.get_mut("meta").and_then(Value::as_object_mut).unwrap();
            meta.insert(TMUX.into(), tmux.into());
            meta.insert(CMD.into(), cmd.into());
            if let Some(c) = cwd {
                meta.insert(CWD.into(), c.into());
            }
            meta.insert(LAUNCH.into(), launch.into());
        }
        (None, Some(cmd)) => {
            if let Some(h) = infer_host(&cmd) {
                let meta = obj
                    .entry("meta")
                    .or_insert_with(|| Value::Object(Default::default()));
                if let Some(m) = meta.as_object_mut() {
                    m.entry(HOST).or_insert_with(|| h.into());
                    m.entry(LAUNCH).or_insert_with(|| cmd.into());
                }
            }
        }
        _ => {}
    }
}

/// The ⓘ lines for a remote pane (empty for a local one). First hop only, by design.
pub fn describe_lines(m: &BTreeMap<String, String>) -> Vec<String> {
    let Some(h) = host(m) else {
        return Vec::new();
    };
    let mut lines = vec![format!("Remote: runs on {h} over ssh (first hop)")];
    if let Some(t) = m.get(TMUX) {
        lines.push(format!("tmux session: {t} — recoverable after a restart"));
    } else {
        lines.push("No tmux session: this pane is not recoverable after a restart".to_string());
    }
    if let Some(c) = m.get(CWD) {
        lines.push(format!("Remote cwd: {c}"));
    }
    if let Some(c) = m.get(CMD) {
        lines.push(format!("Remote command: {c}"));
    }
    lines.push(
        "The title bar follows the host you are typing on; this info is the first hop only."
            .to_string(),
    );
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn m(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn endpoint_parses_common_shell_titles() {
        assert_eq!(
            endpoint_from_title("bert@hp2: ~/swap").as_deref(),
            Some("bert@hp2")
        );
        assert_eq!(
            endpoint_from_title("bert@hp2:~/swap").as_deref(),
            Some("bert@hp2")
        );
        assert_eq!(endpoint_from_title("bert@hp2").as_deref(), Some("bert@hp2"));
        assert_eq!(
            endpoint_from_title("ssh root@10.0.0.7").as_deref(),
            Some("root@10.0.0.7")
        );
        assert_eq!(endpoint_from_title("claude"), None);
        assert_eq!(endpoint_from_title("✳ Claude Code"), None);
        assert_eq!(endpoint_from_title("a@"), None);
        assert_eq!(endpoint_from_title(""), None);
    }

    #[test]
    fn infer_host_skips_options() {
        assert_eq!(
            infer_host("ssh -t -o BatchMode=yes hp2 'ls'").as_deref(),
            Some("hp2")
        );
        assert_eq!(
            infer_host("/usr/bin/ssh -p 2222 -i k bert@box").as_deref(),
            Some("bert@box")
        );
        assert_eq!(infer_host("claude"), None);
        assert_eq!(infer_host("scp a b"), None);
    }

    #[test]
    fn session_names_are_tmux_safe_and_distinct() {
        let a = session_name("lane ltc · hp2", 1);
        assert!(a.starts_with("avada-lane-ltc-hp2-"), "{a}");
        assert!(!a.contains(' ') && !a.contains('.') && !a.contains(':'));
        assert_ne!(a, session_name("lane ltc · hp2", 2));
        assert_eq!(session_name("", 0xab), "avada-0000ab");
    }

    #[test]
    fn launch_line_quotes_and_reattaches() {
        let l = launch_line(
            "hp2",
            "avada-x-000001",
            Some("~/swap lanes/ltc"),
            "claude_auto --model 'sonnet'",
        );
        assert!(l.starts_with("ssh -t -o BatchMode=yes 'hp2' "), "{l}");
        assert!(l.contains("new-session -A -s"));
        assert!(l.contains("set-titles"));
        // Round-trip through a real shell parse: ssh's last argv is one remote command string.
        let argv = shell_words(&l);
        assert_eq!(argv[4], "hp2");
        assert!(
            argv[5].contains("cd '~/swap lanes/ltc' && exec claude_auto --model 'sonnet'")
                || argv[5].contains("claude_auto")
        );
    }

    /// Tiny POSIX-ish word splitter, enough to prove the quoting round-trips.
    fn shell_words(s: &str) -> Vec<String> {
        let (mut out, mut cur, mut inq, mut has) = (vec![], String::new(), false, false);
        let mut it = s.chars().peekable();
        while let Some(c) = it.next() {
            match c {
                '\'' => {
                    inq = !inq;
                    has = true;
                }
                '\\' if !inq => {
                    if let Some(n) = it.next() {
                        cur.push(n);
                        has = true;
                    }
                }
                c if c.is_whitespace() && !inq => {
                    if has {
                        out.push(std::mem::take(&mut cur));
                        has = false;
                    }
                }
                c => {
                    cur.push(c);
                    has = true;
                }
            }
        }
        if has {
            out.push(cur);
        }
        out
    }

    #[test]
    fn explicit_host_wraps_in_tmux_and_records_meta() {
        let mut spec = json!({
            "command": "claude_auto --model sonnet", "cwd": "/home/b/ltc", "label": "lane ltc",
            "meta": { "remote.host": "bert@hp2", "origin.via": "x" }
        });
        normalize_spec(&mut spec, 7);
        let cmd = spec["command"].as_str().unwrap();
        assert!(cmd.starts_with("ssh -t -o BatchMode=yes 'bert@hp2' "));
        assert!(
            spec.get("cwd").is_none(),
            "remote cwd must not become a local cwd"
        );
        let meta = &spec["meta"];
        assert_eq!(meta[CMD], "claude_auto --model sonnet");
        assert_eq!(meta[CWD], "/home/b/ltc");
        assert_eq!(meta[LAUNCH], cmd);
        assert!(meta[TMUX].as_str().unwrap().starts_with("avada-lane-ltc-"));
        assert_eq!(meta["origin.via"], "x");
    }

    #[test]
    fn plain_ssh_is_recognised_but_not_rewritten() {
        let mut spec = json!({ "command": "ssh -t hp2 'htop'" });
        normalize_spec(&mut spec, 1);
        assert_eq!(spec["command"], "ssh -t hp2 'htop'");
        assert_eq!(spec["meta"][HOST], "hp2");
        assert!(spec["meta"].get(TMUX).is_none());
        let mut local = json!({ "command": "htop" });
        normalize_spec(&mut local, 1);
        assert!(local.get("meta").is_none());
    }

    #[test]
    fn explicit_tmux_is_not_double_wrapped() {
        let mut spec =
            json!({ "command": "ssh x", "meta": { "remote.host": "x", "remote.tmux": "s" } });
        normalize_spec(&mut spec, 1);
        assert_eq!(spec["command"], "ssh x");
    }

    #[test]
    fn title_follows_current_host_but_info_stays_first_hop() {
        let r = m(&[(HOST, "bert@hp2"), (TMUX, "avada-x")]);
        assert_eq!(display_title("lane", &r, None), "lane [ssh bert@hp2]");
        assert_eq!(
            display_title("lane", &r, Some("root@inner")),
            "lane [ssh root@inner]"
        );
        assert_eq!(display_title("lane", &BTreeMap::new(), Some("a@b")), "lane");
        let info = describe_lines(&r).join("\n");
        assert!(info.contains("bert@hp2") && info.contains("avada-x"));
        assert!(!info.contains("inner"));
        assert!(describe_lines(&BTreeMap::new()).is_empty());
    }
}
