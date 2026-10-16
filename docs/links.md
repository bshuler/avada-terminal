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
running, the link becomes a new tab of the focused window, **in the background**: Avada does
not raise its window and does not switch to the new tab. Stealing focus is not allowed, because
a hand-off (a link, `avada -c` from another terminal, an agent) is not Bert choosing where to
type, and his keystrokes must not land in a pane he did not pick. When Bert clicks a link
himself, macOS may bring Avada forward on its own; Avada adds nothing to that. On macOS the URL arrives through `application:openURLs:`
(`crates/app/src/window/macos.rs`) and is handed to the running instance like a second launch.

## Clicking a link printed inside a pane

A terminal pane linkifies `http://`, `https://`, `avada://` and `ssh://` tokens in its own
rendered output (`crates/terminal-widget/src/links.rs`) — so a link a program prints (e.g.
`avada url make`'s output, or one typed by hand) is clickable without leaving the pane.

A plain click on an `avada://`/`ssh://` token parses it with the same strict parser described
below and opens it as a new pane split into the **current tab** — deliberately not a new tab
(an external link's default when it lands on an already-running window). The link was clicked
from inside a specific tab's working context, so it stays there, the same way clicking a file
path reveals it in the left tree instead of jumping away, and a clicked commit hash shows up
in the left git panel rather than a new pane. A plain click on `http`/`https` instead routes
through Preferences → Browser, same as always. Outside a mouse grab (see below), Ctrl/Cmd-click
copies the literal text instead of opening, regardless of scheme.

This is plain-text detection only. A program can also *declare* a link with an OSC 8 escape
(the href can differ from the visible label); those stay restricted to `http`/`https`, because
an arbitrary OSC 8 href is not something the user can see before clicking — unlike text typed
or printed into the pane, which is exactly the characters on screen. An OSC 8 `file://` href —
as emitted by `ls --hyperlink` or `rg --hyperlink-format` — is deliberately not one of the two
schemes let through, so it never becomes clickable at all; it needs to show up as plain text
(or get caught by the pane's own path detector) to open.

When the program running in the pane holds the mouse (vim, htop, Claude Code's TUI — DECSET
1000/1002/1003), a plain click belongs to that program, not to the link: Ctrl/Cmd-click or
Shift-click is what reaches the link instead — and, held only to get past the grab, opens it
rather than copying (the hover tooltip switches to "Ctrl+click to open" to say so). This
applies to every link kind, not just avada/ssh — a file path needs the same modifier to reveal
while, say, `claude` is running in the pane.

A plain click on a file path (not a URL) reveals it in the left file tree rather than opening
it directly, same as it always has; right-clicking a path in a pane offers the same context
menu as right-clicking it in the file tree.

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
