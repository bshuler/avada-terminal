# `avada://` links

A link opens one tab in the running Avada window: an ssh session, a shell, or a document.
Click it in a browser, a note, a chat message or a bookmark, or run `avada <link>`.

| Kind | Link | Opens |
|---|---|---|
| ssh | `avada://ssh/[user@]host[:port][?tmux=NAME][&title=LABEL]` | `ssh` to the host; with `tmux=`, attaches to (or creates) that tmux session |
| ssh | `ssh://[user@]host[:port]` | the same pane, from the standard scheme |
| terminal | `avada://terminal[?cwd=DIR][&title=LABEL]` | a local shell, in `DIR` if given |
| doc | `avada://doc?path=FILE[&title=LABEL]` or `avada://doc/<absolute path>` | the file in its viewer: Markdown, table (csv/tsv), data tree (json), code or image; a directory opens the file browser |

Values are percent-encoded. `DIR` and `FILE` are absolute or start with `~`.
Aliases: `term`/`shell` for `terminal`, `open`/`view` for `doc`.

## Making links

```sh
avada url make ssh bert@hp3:2222 --tmux work     # avada://ssh/bert@hp3:2222?tmux=work
avada url make terminal --cwd ~/code/avada        # relative paths are made absolute
avada url make doc README.md --title "Read me"
```

`make` round-trips what it prints through the parser, so a printed link always opens.

## Making Avada the handler

The installers declare `avada://`: the macOS bundle's `Info.plist`, the Linux `.desktop`
file (`Exec=avada %U`), and the Windows installer (`HKCU\Software\Classes\avada`).

`ssh://` is declared on macOS and Linux but not claimed, so Avada shows up as a choice
(macOS: the "Open ssh URLs with" setting) without taking over from whatever you use now. To
claim it, or to register a build run from a checkout:

```sh
avada url register          # avada://
avada url register --ssh    # avada:// and ssh://
```

macOS uses LaunchServices (open Avada.app once first, so macOS knows it). Linux uses
`xdg-mime`. Windows writes `HKCU\Software\Classes`, and may still ask which app to use
the first time.

## Where the tab goes

A link takes the same path as `avada -c …`. With no window running it starts one. With one
running, the link becomes a new tab of the focused window, and that window comes to the
front. On macOS the URL arrives through `application:openURLs:`
(`crates/app/src/window/macos.rs`) and is handed to the running instance like a second launch.

## What a link can never do

Any web page can put a link in front of a click, so links are parsed strictly
(`crates/core/src/cli/link.rs`):

- There is no `cmd` parameter on any kind: a link never runs a command. An unknown or
  repeated parameter rejects the whole link.
- The ssh pane spawns `ssh` directly with a fixed argument list (no shell), and puts `--` before
  the destination. User, host and tmux name may contain only `[A-Za-z0-9._-]` and may not
  start with `-`. So nothing in a link can become an ssh option such as `-oProxyCommand=…`.
- Control characters are refused, raw or percent-encoded. A title is capped at 120 characters.
- A link that fails any of this opens nothing; the reason goes to the log.
- Every pane a link opens records `origin.via = url:<kind>`, so its ⓘ shows where it came from.
