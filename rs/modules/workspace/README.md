# avada-workspace

The **Workspace** rail entry for [Avada Terminal](https://github.com/bshuler/avada-terminal):
the project layout, the library of saved workspaces, and workspace sets. This used to be a
mode of the app's left panel; it is now a module, which means it can be disabled, updated,
or replaced without touching the terminal.

## Install

```sh
avada marketplace install bshuler/avada-workspace
```

Then enable it in the workspace you want it in (`avada marketplace enable
bshuler/avada-workspace`). The rail entry appears as **Workspace** and, being tier 1, is
drawn entirely by the host: the module sends rows, never pixels.

## What it does

It sweeps the workspace root for the files that describe a layout and sorts what it finds
into three sections:

| Section | What is in it |
| --- | --- |
| **PROJECT** | `.avada/project.json` — the layout checked in beside the code (`.hyperpanes/project.json` is still read) |
| **LIBRARY** | Every other saved workspace file under the root (`*.avada.json`, `*.workspace.json`) |
| **SETS** | Workspace sets (`*.set.json`) — a named list of workspaces to open together |

Each workspace opens down to its windows, its tabs and its individual panes, so a row is a
thing you can act on rather than a filename you have to remember the contents of.

| Gesture | Effect |
| --- | --- |
| Click a section, or a window | Expand or collapse it |
| Click a workspace or a tab | Open every pane it contains |
| Click a single pane row | Open just that pane |
| Click a set, or one of its members | Open every workspace the set names |
| Right-click a row | The host's own context menu; the module answers with where the row came from |
| The filter box | Matches rows by name and keeps the path down to each match |

Commands, all invokable from the palette:

| Id | Label |
| --- | --- |
| `refresh` | Workspace: Refresh |
| `filter` | Workspace: Filter — `{ query }` |
| `reveal` | Workspace: Reveal a path — `{ path }` |
| `open-group` | Workspace: Open every pane of a tab |
| `note` | Workspace: Note what a pane is for — `{ note }` |

It also subscribes to two host events:

- `rail.query` — the filter box under the rail entry changed.
- `files.reveal` — something in the app asked for a path to be shown. The module expands
  every row on the way down to it, marks the row `selected`, and the host scrolls to it.

## What it cannot do, and why

Two things the old left panel did are not reachable over the v1 module contract, and the
module says so out loud rather than failing quietly:

- **A restored pane loses its command line.** `host.panes.spawn` takes only
  `{ kind, path, surface }` — there is no way to convey a saved pane's command, argv,
  label, colour, font size or the tab's split layout. Opening a tab therefore opens the
  right *number* of panes at the right paths, and toasts how many saved commands could not
  be restored. Closing this needs a richer `host.panes.spawn` in the host.
- **The user-data-dir library and the detached-session list are invisible.** Those live
  outside the workspace root, and `fs.read` is scoped to the workspace. The module only
  ever shows what is under the root. Closing this needs a `host.workspace.*` method — the
  `workspace.read` / `workspace.write` capabilities exist in the vocabulary but no wire
  method maps to them, so declaring them would grant nothing, and this module does not.

## Capabilities

| Capability | Why |
| --- | --- |
| `fs.read` | To sweep the root and read the workspace files. Scoped by the host to the open workspace — the module never asks for `fs.read_any`, so a path outside the project is refused by the host rather than trusted to the module. |
| `fs.write` | The `note` command, which is the only thing the module writes. Read-modify-write of one field of one file; also scoped. |
| `ui.rail` | The **Workspace** entry and its rows. |
| `ui.pane` | The placeholder pane when the module is disabled. |
| `ui.commands` | The five palette commands above. |
| `ui.toast` | What could not be restored, where a row came from, and why a command failed. |
| `events.subscribe` | `rail.query` and `files.reveal`. |
| `panes.spawn` | Opening the panes of the thing you clicked. |

It reads no configuration and makes no network calls. The only I/O is the one socket the
host hands it in `AVADA_MODULE_FD`; it never calls `std::fs` on the workspace itself.

## Building and testing

```sh
cargo build --release --locked
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

`cargo test` runs the unit tests (a fake filesystem drives the sweep, the flatten and the
state machine, so they are instant and deterministic) and `tests/e2e.rs`, which spawns the
real binary over a socketpair with a fake host answering `host.fs.list`, `host.fs.read` and
`host.fs.write` from a real temp directory — scoped the way the host scopes them, so a
passing e2e test is a test that would pass against the app.

`avada-workspace --manifest` prints the embedded `avada.toml`, which is how the host reads
a source module's capabilities before granting any.

## Licence

MIT OR Apache-2.0.
