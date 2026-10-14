//! `avada url …` — make `avada://` links and make Avada the program that opens them.
//!
//! The link grammar and its safety rules live in [`avada_core::cli::link`]; opening one is
//! just `avada <link>`. This module is the two helpers around that:
//!
//! * `avada url make ssh|terminal|doc …` prints a link, built the same way the parser reads
//!   it back, so a link pasted into a note, a bookmark or a chat always opens.
//! * `avada url register [--ssh]` makes Avada the handler for `avada://` (and, on request,
//!   `ssh://`) for this user: LaunchServices on macOS, `xdg-mime` on Linux, `HKCU` on
//!   Windows. The installers already declare the scheme; this is for a build run from a
//!   checkout, or for taking `ssh://` over from another app.

use avada_core::cli::link::{self, Link};

const URL_USAGE: &str = "\
avada url — links that open a tab in the running Avada window

USAGE:
    avada url make ssh [user@]host[:port] [--tmux <session>] [--title <label>]
    avada url make terminal [--cwd <dir>] [--title <label>]
    avada url make doc <file> [--title <label>]
                        Print the avada:// link (open it with `avada <link>`, `open <link>`
                        on macOS, or `xdg-open <link>` on Linux)
    avada url register [--ssh]
                        Make Avada the handler for avada:// links for this user;
                        --ssh also takes ssh:// links (\"Open ssh URLs with\")

A link opens a shell, an ssh session or a document in a new tab. It never runs a command.
";

/// Whether `argv` is `avada url …`. Checked in `main` alongside the other subcommands,
/// before the GUI/single-instance path.
#[tracing::instrument(level = "debug", ret)]
pub fn wants_url(argv: &[String]) -> bool {
    argv.get(1).map(|a| a == "url").unwrap_or(false)
}

/// Run `avada url …`; the error is the message to print.
#[tracing::instrument(level = "debug", ret)]
pub fn run(argv: &[String]) -> Result<(), String> {
    let rest = argv.get(2..).unwrap_or_default();
    match rest.first().map(String::as_str) {
        Some("make") => {
            let cwd = std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "/".into());
            let home = std::env::var("HOME").ok();
            println!("{}", make(&rest[1..], &cwd, home.as_deref())?);
            Ok(())
        }
        Some("register") => {
            let ssh = match &rest[1..] {
                [] => false,
                [f] if f == "--ssh" => true,
                other => return Err(format!("url register: unexpected {other:?}\n\n{URL_USAGE}")),
            };
            register(ssh)
        }
        None | Some("--help") | Some("-h") | Some("help") => {
            print!("{URL_USAGE}");
            Ok(())
        }
        Some(other) => Err(format!("url: unknown command {other:?}\n\n{URL_USAGE}")),
    }
}

/// Build the link `avada url make <args>` prints. `cwd` makes a relative doc/dir path
/// absolute; the result is round-tripped through the parser, so whatever this prints opens.
pub fn make(args: &[String], cwd: &str, home: Option<&str>) -> Result<String, String> {
    let (kind, rest) = args
        .split_first()
        .ok_or_else(|| format!("url make: which kind — ssh, terminal or doc?\n\n{URL_USAGE}"))?;
    let mut positional: Vec<&str> = Vec::new();
    let mut flags: Vec<(&str, &str)> = Vec::new();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            f @ ("--tmux" | "--title" | "--cwd") => {
                let v = it
                    .next()
                    .ok_or_else(|| format!("url make: {f} needs a value"))?;
                flags.push((f, v));
            }
            f if f.starts_with("--") => return Err(format!("url make: unknown flag {f}")),
            p => positional.push(p),
        }
    }
    let flag = |name: &str| {
        flags
            .iter()
            .rev()
            .find(|(f, _)| *f == name)
            .map(|(_, v)| v.to_string())
    };
    let allow = |ok: &[&str]| -> Result<(), String> {
        match flags.iter().find(|(f, _)| !ok.contains(f)) {
            Some((f, _)) => Err(format!("url make {kind}: {f} does not apply")),
            None => Ok(()),
        }
    };
    let absolute = |p: &str| -> String {
        if p.starts_with('/') || p.starts_with('~') || std::path::Path::new(p).is_absolute() {
            p.to_string()
        } else {
            std::path::Path::new(cwd)
                .join(p)
                .to_string_lossy()
                .into_owned()
        }
    };
    let built = match (kind.as_str(), positional.as_slice()) {
        ("ssh", [dest]) => {
            allow(&["--tmux", "--title"])?;
            // Reuse the parser's own authority rules rather than re-implementing them.
            let probe = link::parse(&format!("ssh://{dest}"), home)?;
            let Link::Ssh {
                user, host, port, ..
            } = probe
            else {
                unreachable!("an ssh:// link parses to Link::Ssh")
            };
            Link::Ssh {
                user,
                host,
                port,
                tmux: flag("--tmux"),
                title: flag("--title"),
            }
        }
        ("terminal" | "term" | "shell", []) => {
            allow(&["--cwd", "--title"])?;
            Link::Terminal {
                cwd: flag("--cwd").map(|c| absolute(&c)),
                title: flag("--title"),
            }
        }
        ("doc" | "open" | "view", [path]) => {
            allow(&["--title"])?;
            Link::Doc {
                path: absolute(path),
                title: flag("--title"),
            }
        }
        ("ssh", _) => return Err("url make ssh: give one destination, [user@]host[:port]".into()),
        ("doc" | "open" | "view", _) => return Err("url make doc: give one file".into()),
        ("terminal" | "term" | "shell", _) => {
            return Err("url make terminal: the directory goes in --cwd".into())
        }
        (other, _) => {
            return Err(format!(
                "url make: unknown kind {other:?} (ssh, terminal, doc)"
            ))
        }
    };
    let url = link::format(&built);
    link::parse(&url, home)?;
    Ok(url)
}

/// The bundle id the macOS Info.plist declares (`packaging/macos/bundle.sh`).
#[cfg(target_os = "macos")]
const BUNDLE_ID: &str = "to.avada.terminal";

/// The schemes `register` claims.
fn schemes(ssh: bool) -> &'static [&'static str] {
    if ssh {
        &["avada", "ssh"]
    } else {
        &["avada"]
    }
}

#[cfg(target_os = "macos")]
#[tracing::instrument(level = "debug", ret)]
fn register(ssh: bool) -> Result<(), String> {
    use objc2_foundation::NSString;
    use std::ffi::c_void;
    #[link(name = "CoreServices", kind = "framework")]
    extern "C" {
        // Deprecated in favour of an async NSWorkspace call, but synchronous, still honoured,
        // and the only form a CLI can wait on.
        fn LSSetDefaultHandlerForURLScheme(scheme: *const c_void, bundle_id: *const c_void) -> i32;
    }
    let bundle = NSString::from_str(BUNDLE_ID);
    for s in schemes(ssh) {
        let scheme = NSString::from_str(s);
        // SAFETY: NSString is toll-free bridged to CFStringRef; both strings outlive the call,
        // which only reads them.
        let status = unsafe {
            LSSetDefaultHandlerForURLScheme(
                &*scheme as *const NSString as *const c_void,
                &*bundle as *const NSString as *const c_void,
            )
        };
        if status != 0 {
            return Err(format!(
                "macOS refused to make Avada the {s}:// handler (status {status}). Open Avada.app \
                 once from /Applications so macOS knows it, then run this again."
            ));
        }
        println!("{s}:// now opens in Avada");
    }
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
#[tracing::instrument(level = "debug", ret)]
fn register(ssh: bool) -> Result<(), String> {
    for s in schemes(ssh) {
        let mime = format!("x-scheme-handler/{s}");
        let ok = std::process::Command::new("xdg-mime")
            .args(["default", "avada.desktop", &mime])
            .status()
            .map_err(|e| format!("xdg-mime: {e} (install xdg-utils)"))?
            .success();
        if !ok {
            return Err(format!(
                "xdg-mime could not set {mime}; is avada.desktop installed?"
            ));
        }
        println!("{s}:// now opens in Avada");
    }
    Ok(())
}

#[cfg(windows)]
#[tracing::instrument(level = "debug", ret)]
fn register(ssh: bool) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot find avada.exe: {e}"))?;
    let command = format!("\"{}\" \"%1\"", exe.display());
    for s in schemes(ssh) {
        let key = format!(r"HKCU\Software\Classes\{s}");
        let open_key = format!(r"{key}\shell\open\command");
        let writes: [Vec<&str>; 3] = [
            vec!["add", &key, "/ve", "/d", "URL:Avada link", "/f"],
            vec!["add", &key, "/v", "URL Protocol", "/d", "", "/f"],
            vec![
                "add",
                &open_key,
                "/ve",
                "/d",
                &command,
                "/f",
            ],
        ];
        for w in &writes {
            let ok = std::process::Command::new("reg")
                .args(w)
                .stdout(std::process::Stdio::null())
                .status()
                .map_err(|e| format!("reg: {e}"))?
                .success();
            if !ok {
                return Err(format!("could not write {key}"));
            }
        }
        println!(
            "{s}:// now opens in Avada (Windows may still ask which app to use the first time)"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(args: &[&str]) -> Result<String, String> {
        let v: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        make(&v, "/work", Some("/home/b"))
    }

    #[test]
    fn make_builds_links_the_parser_reads_back() {
        assert_eq!(
            mk(&["ssh", "bert@hp3:2222", "--tmux", "w"]).unwrap(),
            "avada://ssh/bert@hp3:2222?tmux=w"
        );
        assert_eq!(mk(&["ssh", "hp3"]).unwrap(), "avada://ssh/hp3");
        assert_eq!(mk(&["terminal"]).unwrap(), "avada://terminal");
        let t = mk(&["terminal", "--cwd", "src", "--title", "my shell"]).unwrap();
        assert_eq!(
            link::parse(&t, None).unwrap(),
            Link::Terminal {
                cwd: Some("/work/src".into()),
                title: Some("my shell".into())
            }
        );
        let d = mk(&["doc", "~/notes/c++ tips.md"]).unwrap();
        assert_eq!(
            link::parse(&d, Some("/home/b")).unwrap(),
            Link::Doc {
                path: "/home/b/notes/c++ tips.md".into(),
                title: None
            }
        );
    }

    #[test]
    fn make_refuses_what_a_link_cannot_carry() {
        assert!(mk(&["ssh", "-oProxyCommand=x"]).is_err());
        assert!(mk(&["ssh", "hp3", "--tmux", "a;b"]).is_err());
        assert!(mk(&["ssh"]).is_err());
        assert!(mk(&["terminal", "/tmp"]).is_err());
        assert!(mk(&["doc", "a.md", "--tmux", "w"]).is_err());
        assert!(mk(&["shellcmd"]).is_err());
        assert!(mk(&["ssh", "hp3", "--cmd", "rm"]).is_err());
    }

    #[test]
    fn only_url_is_the_url_command() {
        assert!(wants_url(&["avada".into(), "url".into()]));
        assert!(!wants_url(&["avada".into(), "avada://ssh/hp3".into()]));
    }
}
