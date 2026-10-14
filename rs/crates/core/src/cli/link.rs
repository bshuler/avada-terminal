//! `avada://` links (and `ssh://`): a URL that opens one tab.
//!
//! A link is how a browser, a chat message, a note or the macOS "Open ssh URLs with" setting
//! hands Avada something to open. Every route reaches the same place: the URL arrives as a
//! positional argument (`avada <url>` on the command line, `Exec=avada %U` on Linux, the
//! macOS `application:openURLs:` handler forwarding it as a hand-off), and
//! [`crate::cli::parse`] turns it into a pane in a new tab of the running window.
//!
//! # Grammar
//!
//! ```text
//! avada://ssh/[user@]host[:port][?tmux=NAME][&title=LABEL]
//! ssh://[user@]host[:port][/]                       (the standard scheme; same pane)
//! avada://terminal[?cwd=DIR][&title=LABEL]
//! avada://doc?path=FILE[&title=LABEL]               (or avada://doc/<absolute path>)
//! ```
//!
//! Values are percent-encoded; `DIR`/`FILE` are absolute or start with `~`.
//!
//! # What a link can never do
//!
//! A link is attacker-controlled input: any web page can put one in front of a click. So a
//! link opens a shell, an ssh session or a document — it never names a command to run.
//!
//! * There is no `cmd` parameter, on any kind. An unknown parameter is refused, not ignored,
//!   so a link written for a future build fails loudly instead of half-working.
//! * The ssh pane is a direct spawn of `ssh` with a fixed argv (no shell parses anything),
//!   `--` before the destination, and user/host restricted to `[A-Za-z0-9._-]` with no
//!   leading `-` — so nothing in a link can become an ssh option (`-oProxyCommand=…`).
//! * The tmux session name is restricted the same way and is passed as its own word.
//! * Control characters are refused everywhere; every pane records `origin.via = url:<kind>`
//!   so its ⓘ says where it came from.
//!
//! A doc link picks the view from the extension here (markdown, table, data, file browser
//! for a directory, plain file otherwise); the app refines a plain file to its code or image
//! view, which need the app's highlighter and decoders.

use crate::pane_origin;
use crate::pane_remote;
use crate::tools::kind::PaneKind;
use crate::workspace::model::PaneSpec;
use std::collections::BTreeMap;
use std::path::Path;

/// One parsed link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Ssh {
        user: Option<String>,
        host: String,
        port: Option<u16>,
        tmux: Option<String>,
        title: Option<String>,
    },
    Terminal {
        cwd: Option<String>,
        title: Option<String>,
    },
    Doc {
        path: String,
        title: Option<String>,
    },
}

/// The `origin.via` a link-opened pane records: `url:ssh`, `url:terminal`, `url:doc`.
pub const VIA_PREFIX: &str = "url:";

/// Whether `arg` is a link this module handles (by scheme only — it may still fail to parse).
pub fn is_link(arg: &str) -> bool {
    let lower = arg.get(..8).unwrap_or(arg).to_ascii_lowercase();
    lower.starts_with("avada://") || lower.starts_with("ssh://")
}

/// Parse a link. `home` expands a leading `~` in a path value.
pub fn parse(url: &str, home: Option<&str>) -> Result<Link, String> {
    if url.chars().any(char::is_control) {
        return Err("link contains a control character".into());
    }
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("not a link: {url}"))?;
    let (body, query) = match rest.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (rest, None),
    };
    // A fragment is never meaningful here; drop it rather than let it leak into a value.
    let query = query.map(|q| q.split('#').next().unwrap_or(""));
    let body = body.split('#').next().unwrap_or("");
    let mut params = parse_query(query.unwrap_or(""))?;

    let link = match scheme.to_ascii_lowercase().as_str() {
        "ssh" => {
            // RFC-draft `ssh://user;fingerprint=…@host:port/` — the fingerprint is advisory
            // and ssh checks known_hosts itself, so it is dropped.
            let authority = body.trim_end_matches('/');
            ssh_link(authority, &mut params)?
        }
        "avada" => {
            let (kind, tail) = match body.split_once('/') {
                Some((k, t)) => (k, t),
                None => (body, ""),
            };
            match kind.to_ascii_lowercase().as_str() {
                "ssh" => ssh_link(tail.trim_end_matches('/'), &mut params)?,
                "terminal" | "term" | "shell" => {
                    if !tail.is_empty() {
                        return Err("avada://terminal takes its directory as ?cwd=".into());
                    }
                    let cwd = match params.remove("cwd") {
                        Some(c) => Some(path_value("cwd", &c, home)?),
                        None => None,
                    };
                    Link::Terminal {
                        cwd,
                        title: title(&mut params)?,
                    }
                }
                "doc" | "open" | "view" => {
                    let raw = match params.remove("path") {
                        Some(p) if tail.is_empty() => p,
                        Some(_) => return Err("avada://doc: give the path once".into()),
                        None if !tail.is_empty() => format!("/{}", decode_path(tail)?),
                        None => return Err("avada://doc needs ?path=".into()),
                    };
                    Link::Doc {
                        path: path_value("path", &raw, home)?,
                        title: title(&mut params)?,
                    }
                }
                other => return Err(format!("unknown avada:// link kind: {other:?}")),
            }
        }
        other => return Err(format!("unsupported scheme: {other}")),
    };
    if let Some(k) = params.keys().next() {
        return Err(format!("unknown link parameter: {k:?}"));
    }
    Ok(link)
}

fn ssh_link(authority: &str, params: &mut BTreeMap<String, String>) -> Result<Link, String> {
    if authority.contains('/') {
        return Err("an ssh link carries no path".into());
    }
    let (user, hostport) = match authority.rsplit_once('@') {
        Some((u, h)) => {
            let u = decode(u.split(';').next().unwrap_or(""))?;
            (Some(u), h)
        }
        None => (None, authority),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() => {
            let port: u16 = p
                .parse()
                .ok()
                .filter(|n| *n > 0)
                .ok_or_else(|| format!("bad ssh port: {p:?}"))?;
            (h, Some(port))
        }
        Some((h, _)) => (h, None),
        None => (hostport, None),
    };
    let host = decode(host)?;
    check_word("host", &host)?;
    if let Some(u) = &user {
        check_word("user", u)?;
    }
    let tmux = match params.remove("tmux") {
        Some(t) => {
            check_word("tmux", &t)?;
            Some(t)
        }
        None => None,
    };
    Ok(Link::Ssh {
        user,
        host,
        port,
        tmux,
        title: title(params)?,
    })
}

/// A user, host or tmux session name: non-empty, `[A-Za-z0-9._-]`, not starting with `-`.
fn check_word(what: &str, v: &str) -> Result<(), String> {
    let ok = !v.is_empty()
        && v.len() <= 253
        && !v.starts_with('-')
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(format!("bad ssh {what}: {v:?}"))
    }
}

fn title(params: &mut BTreeMap<String, String>) -> Result<Option<String>, String> {
    match params.remove("title") {
        Some(t) if t.chars().count() > 120 => Err("link title is too long".into()),
        Some(t) if t.trim().is_empty() => Ok(None),
        Some(t) => Ok(Some(t)),
        None => Ok(None),
    }
}

fn path_value(what: &str, v: &str, home: Option<&str>) -> Result<String, String> {
    let expanded = if v == "~" || v.starts_with("~/") {
        let h = home.ok_or_else(|| format!("{what}: no home directory to expand ~"))?;
        format!("{}{}", h.trim_end_matches('/'), &v[1..])
    } else {
        v.to_string()
    };
    if !Path::new(&expanded).is_absolute() {
        return Err(format!("{what} must be an absolute path: {v:?}"));
    }
    Ok(expanded)
}

fn parse_query(q: &str) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    for pair in q.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let k = decode(k)?.to_ascii_lowercase();
        if out.insert(k.clone(), decode(v)?).is_some() {
            return Err(format!("link parameter given twice: {k:?}"));
        }
    }
    Ok(out)
}

/// Percent-decode a query value (`+` → space, as forms and browsers write it). The result must
/// be UTF-8 with no control characters — a decoded newline is as dangerous as a literal one.
pub fn decode(s: &str) -> Result<String, String> {
    decode_with(s, true)
}

/// The local path a `file://` URL names (`file:///p` or `file://localhost/p`), or `None` for
/// anything else. Linux desktops launch with `%U` so links arrive, and then hand local files
/// over as `file://` URLs too.
pub fn file_url_path(a: &str) -> Option<String> {
    let rest = a
        .get(..7)
        .filter(|p| p.eq_ignore_ascii_case("file://"))
        .map(|_| &a[7..])?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') {
        return None;
    }
    decode_path(rest).ok()
}

/// [`decode`] for a path segment, where `+` is a literal plus.
fn decode_path(s: &str) -> Result<String, String> {
    decode_with(s, false)
}

fn decode_with(s: &str, plus_is_space: bool) -> Result<String, String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let hex = s
                    .get(i + 1..i + 3)
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                    .ok_or_else(|| format!("bad percent-escape in {s:?}"))?;
                out.push(hex);
                i += 3;
            }
            b'+' if plus_is_space => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    let v = String::from_utf8(out).map_err(|_| format!("link value is not UTF-8: {s:?}"))?;
    if v.chars().any(char::is_control) {
        return Err("link value contains a control character".into());
    }
    Ok(v)
}

/// The canonical `avada://` form of a link — what "copy link" and `avada url make` print.
/// `parse(format(l)) == l` for every link `parse` returns.
pub fn format(link: &Link) -> String {
    use crate::cli::invoke::percent_encode as enc;
    let mut q: Vec<String> = Vec::new();
    let (head, title) = match link {
        Link::Ssh {
            user,
            host,
            port,
            tmux,
            title,
        } => {
            if let Some(t) = tmux {
                q.push(format!("tmux={}", enc(t)));
            }
            let user = user.as_deref().map(|u| format!("{u}@")).unwrap_or_default();
            let port = port.map(|p| format!(":{p}")).unwrap_or_default();
            (format!("avada://ssh/{user}{host}{port}"), title)
        }
        Link::Terminal { cwd, title } => {
            if let Some(c) = cwd {
                q.push(format!("cwd={}", enc(c)));
            }
            ("avada://terminal".to_string(), title)
        }
        Link::Doc { path, title } => {
            q.push(format!("path={}", enc(path)));
            ("avada://doc".to_string(), title)
        }
    };
    if let Some(t) = title {
        q.push(format!("title={}", enc(t)));
    }
    if q.is_empty() {
        head
    } else {
        format!("{head}?{}", q.join("&"))
    }
}

/// The view a document opens in, decided from the path alone (`is_dir` is injected). The app
/// upgrades a [`PaneKind::FileViewer`] to its code/image view; see the module header.
pub fn doc_kind(path: &str, is_dir: bool) -> PaneKind {
    if is_dir {
        return PaneKind::FileBrowser;
    }
    let ext = Path::new(path)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "md" | "markdown" | "mdown" | "mkd" => PaneKind::Markdown,
        "csv" | "tsv" | "tab" => PaneKind::Table,
        "json" | "jsonl" | "ndjson" => PaneKind::Data,
        _ => PaneKind::FileViewer,
    }
}

trait WithKind {
    fn with_kind(self, k: &PaneKind) -> Self;
}

impl WithKind for PaneSpec {
    fn with_kind(mut self, k: &PaneKind) -> Self {
        self.set_pane_kind(k);
        self
    }
}

/// The pane a link opens. `now` is the provenance timestamp (unix seconds); `is_dir` decides a
/// doc link that names a directory.
pub fn to_pane_spec(link: &Link, now: u64, is_dir: impl Fn(&str) -> bool) -> PaneSpec {
    let mut meta = BTreeMap::new();
    let kind = match link {
        Link::Ssh { .. } => "ssh",
        Link::Terminal { .. } => "terminal",
        Link::Doc { .. } => "doc",
    };
    meta.insert(pane_origin::VIA.to_string(), format!("{VIA_PREFIX}{kind}"));
    meta.insert(pane_origin::AT.to_string(), pane_origin::utc_stamp(now));
    let mut spec = match link {
        Link::Ssh {
            user,
            host,
            port,
            tmux,
            title,
        } => {
            let dest = match user {
                Some(u) => format!("{u}@{host}"),
                None => host.clone(),
            };
            // Fixed argv, direct spawn: `ssh [-p N] [-t] -- dest [tmux new-session -A -s NAME]`.
            // Interactive on purpose (no BatchMode): a first connection asks about the host key
            // in the pane, where the person who clicked the link can answer it.
            let mut args = Vec::new();
            if let Some(p) = port {
                args.extend(["-p".to_string(), p.to_string()]);
            }
            if tmux.is_some() {
                args.push("-t".into());
            }
            args.extend(["--".to_string(), dest.clone()]);
            if let Some(t) = tmux {
                args.extend(["tmux", "new-session", "-A", "-s", t.as_str()].map(String::from));
                meta.insert(pane_remote::TMUX.to_string(), t.clone());
            }
            meta.insert(pane_remote::HOST.to_string(), dest.clone());
            PaneSpec {
                label: Some(title.clone().unwrap_or_else(|| format!("ssh {host}"))),
                command: Some("ssh".into()),
                args: Some(args),
                ..Default::default()
            }
        }
        Link::Terminal { cwd, title } => PaneSpec {
            label: title.clone(),
            cwd: cwd.clone(),
            ..Default::default()
        },
        Link::Doc { path, title } => {
            let k = doc_kind(path, is_dir(path));
            let label = title.clone().or_else(|| {
                Path::new(path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
            });
            PaneSpec {
                label,
                cwd: Some(path.clone()),
                ..Default::default()
            }
            .with_kind(&k)
        }
    };
    spec.meta.get_or_insert_with(BTreeMap::new).extend(meta);
    spec
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_file_url_becomes_its_local_path() {
        assert_eq!(
            file_url_path("file:///home/b/My%20ws.avada").as_deref(),
            Some("/home/b/My ws.avada")
        );
        assert_eq!(
            file_url_path("file://localhost/tmp/c++.md").as_deref(),
            Some("/tmp/c++.md")
        );
        assert_eq!(file_url_path("file://otherhost/tmp/x"), None);
        assert_eq!(file_url_path("/tmp/x"), None);
    }

    use super::*;

    fn p(u: &str) -> Result<Link, String> {
        parse(u, Some("/home/b"))
    }

    fn ssh(user: Option<&str>, host: &str, port: Option<u16>, tmux: Option<&str>) -> Link {
        Link::Ssh {
            user: user.map(String::from),
            host: host.into(),
            port,
            tmux: tmux.map(String::from),
            title: None,
        }
    }

    #[test]
    fn ssh_links_parse_in_both_schemes() {
        assert_eq!(p("ssh://hp3").unwrap(), ssh(None, "hp3", None, None));
        assert_eq!(
            p("ssh://bert@hp3.example.com:2222/").unwrap(),
            ssh(Some("bert"), "hp3.example.com", Some(2222), None)
        );
        assert_eq!(
            p("SSH://bert;fingerprint=ssh-ed25519-abc@hp3").unwrap(),
            ssh(Some("bert"), "hp3", None, None)
        );
        assert_eq!(
            p("avada://ssh/bert@hp3:22?tmux=work").unwrap(),
            ssh(Some("bert"), "hp3", Some(22), Some("work"))
        );
    }

    #[test]
    fn nothing_in_an_ssh_link_can_become_an_ssh_option_or_a_command() {
        for bad in [
            "ssh://-oProxyCommand=touch%20x@hp3",
            "ssh://-oProxyCommand=x",
            "ssh://bert@-p",
            "ssh://bert@hp3%20-o%20x",
            "ssh://bert@hp3;id",
            "ssh://hp3/etc/passwd",
            "ssh://hp3:0",
            "ssh://hp3:99999",
            "ssh://hp3:22x",
            "ssh://hp3?cmd=id",
            "avada://ssh/hp3?tmux=-x",
            "avada://ssh/hp3?tmux=a;b",
            "avada://ssh/hp3?tmux=a%20b",
            "avada://ssh/hp3?command=id",
            "avada://ssh/",
            "ssh://hp3%0a",
            "ssh://hp3\n",
        ] {
            assert!(p(bad).is_err(), "{bad} should be refused, got {:?}", p(bad));
        }
    }

    #[test]
    fn a_terminal_link_takes_a_directory_and_never_a_command() {
        assert_eq!(
            p("avada://terminal").unwrap(),
            Link::Terminal {
                cwd: None,
                title: None
            }
        );
        assert_eq!(
            p("avada://terminal?cwd=~/code/a%20b&title=My+shell").unwrap(),
            Link::Terminal {
                cwd: Some("/home/b/code/a b".into()),
                title: Some("My shell".into())
            }
        );
        for bad in [
            "avada://terminal?cmd=rm",
            "avada://terminal?command=rm",
            "avada://terminal?cwd=relative",
            "avada://terminal/tmp",
            "avada://terminal?cwd=/tmp&cwd=/etc",
            "avada://terminal?cwd=/tmp%0Aid",
            "avada://nope",
        ] {
            assert!(p(bad).is_err(), "{bad} should be refused");
        }
    }

    #[test]
    fn a_doc_link_takes_its_path_as_a_query_or_a_path() {
        let want = Link::Doc {
            path: "/Users/b/notes/a b.md".into(),
            title: None,
        };
        assert_eq!(
            p("avada://doc?path=%2FUsers%2Fb%2Fnotes%2Fa%20b.md").unwrap(),
            want
        );
        assert_eq!(p("avada://doc/Users/b/notes/a%20b.md").unwrap(), want);
        assert!(p("avada://doc").is_err());
        assert!(p("avada://doc?path=notes.md").is_err());
        assert!(p("avada://doc/x?path=/y").is_err());
        assert_eq!(
            p("avada://doc/n/c++.md").unwrap(),
            Link::Doc {
                path: "/n/c++.md".into(),
                title: None
            }
        );
    }

    #[test]
    fn format_round_trips() {
        for l in [
            ssh(None, "hp3", None, None),
            ssh(Some("bert"), "10.0.0.5", Some(2222), Some("main")),
            Link::Ssh {
                user: None,
                host: "hp3".into(),
                port: None,
                tmux: None,
                title: Some("build box & co".into()),
            },
            Link::Terminal {
                cwd: None,
                title: None,
            },
            Link::Terminal {
                cwd: Some("/tmp/a b?c=d&e".into()),
                title: Some("x".into()),
            },
            Link::Doc {
                path: "/Users/b/ü/#notes.md".into(),
                title: None,
            },
        ] {
            let s = format(&l);
            assert_eq!(p(&s).as_ref(), Ok(&l), "{s}");
        }
        assert_eq!(
            format(&ssh(Some("bert"), "hp3", Some(22), Some("w"))),
            "avada://ssh/bert@hp3:22?tmux=w"
        );
    }

    #[test]
    fn an_ssh_pane_is_a_direct_spawn_with_the_destination_after_the_end_of_options() {
        let s = to_pane_spec(&ssh(Some("bert"), "hp3", Some(2222), Some("w")), 0, |_| {
            false
        });
        assert_eq!(s.command.as_deref(), Some("ssh"));
        assert_eq!(
            s.args.as_deref().unwrap(),
            [
                "-p",
                "2222",
                "-t",
                "--",
                "bert@hp3",
                "tmux",
                "new-session",
                "-A",
                "-s",
                "w"
            ]
        );
        let m = s.meta.as_ref().unwrap();
        assert_eq!(m[pane_origin::VIA], "url:ssh");
        assert_eq!(m[pane_remote::HOST], "bert@hp3");
        assert_eq!(m[pane_remote::TMUX], "w");
        assert_eq!(s.label.as_deref(), Some("ssh hp3"));
        assert_eq!(s.pane_kind(), PaneKind::Terminal);

        let plain = to_pane_spec(&ssh(None, "hp3", None, None), 0, |_| false);
        assert_eq!(plain.args.as_deref().unwrap(), ["--", "hp3"]);
    }

    #[test]
    fn a_terminal_pane_is_a_plain_shell_in_the_directory() {
        let s = to_pane_spec(
            &Link::Terminal {
                cwd: Some("/tmp".into()),
                title: None,
            },
            0,
            |_| false,
        );
        assert_eq!(s.command, None);
        assert_eq!(s.args, None);
        assert_eq!(s.cwd.as_deref(), Some("/tmp"));
        assert_eq!(s.pane_kind(), PaneKind::Terminal);
        assert_eq!(s.meta.as_ref().unwrap()[pane_origin::VIA], "url:terminal");
    }

    #[test]
    fn a_doc_pane_opens_the_view_its_extension_names() {
        let doc = |path: &str, dir: bool| {
            to_pane_spec(
                &Link::Doc {
                    path: path.into(),
                    title: None,
                },
                0,
                move |_| dir,
            )
        };
        let s = doc("/n/README.md", false);
        assert_eq!(s.pane_kind(), PaneKind::Markdown);
        assert_eq!(s.cwd.as_deref(), Some("/n/README.md"));
        assert_eq!(s.label.as_deref(), Some("README.md"));
        assert_eq!(s.command, None);
        assert_eq!(s.meta.as_ref().unwrap()[pane_origin::VIA], "url:doc");
        assert_eq!(doc("/n/a.csv", false).pane_kind(), PaneKind::Table);
        assert_eq!(doc("/n/a.json", false).pane_kind(), PaneKind::Data);
        assert_eq!(doc("/n/a.log", false).pane_kind(), PaneKind::FileViewer);
        assert_eq!(doc("/n", true).pane_kind(), PaneKind::FileBrowser);
    }

    #[test]
    fn is_link_matches_only_the_handled_schemes() {
        assert!(is_link("avada://terminal"));
        assert!(is_link("AVADA://doc?path=/x"));
        assert!(is_link("ssh://hp3"));
        assert!(!is_link("http://x"));
        assert!(!is_link("dev.avada"));
        assert!(!is_link("ssh"));
    }
}
