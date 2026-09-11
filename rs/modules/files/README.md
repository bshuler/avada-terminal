# avada-files

The **Files** rail entry for [Avada Terminal](https://github.com/bshuler/avada-terminal):
the workspace as a tree, plus a fuzzy file finder. This used to be built into the app; it
is now a module, which means it can be disabled, updated, or replaced without touching the
terminal.

## Install

```sh
avada marketplace install bshuler/avada-files
```

Then enable it in the workspace you want it in (the marketplace rail entry does this for
you, or `avada marketplace enable bshuler/avada-files`). The rail entry appears as **Files**
and, being tier 1, is drawn entirely by the host: the module sends rows, never pixels.

## What it does

| Gesture | Effect |
| --- | --- |
| Click a directory | Expand or collapse it in place |
| Double-click a directory | Make it the root |
| Click a file | Open it in a pane (`host.panes.spawn { kind: "file" }`) |
| Right-click a row | The host's own context menu — the module answers and does nothing |
| `..` | Go up one directory, leaving the one you came from open |
| The filter box | A recursive fuzzy find over the whole tree, ranked |

Commands, all invokable from the palette:

| Id | Label |
| --- | --- |
| `up` | Files: Go up a directory |
| `refresh` | Files: Refresh |
| `reveal` | Files: Reveal a path — `{ path, line?, col? }` |
| `filter` | Files: Filter — `{ query }` |
| `set-root` | Files: Set the root directory — `{ path }` |

It also subscribes to two host events:

- `rail.query` — the filter box under the rail entry changed.
- `files.reveal` — something in the app (a link in the terminal, "reveal this pane's cwd")
  asked for a path to be shown. The module expands every directory on the way down, marks
  the row `selected`, and the host scrolls to it.

## Capabilities

| Capability | Why |
| --- | --- |
| `fs.read` | To list directories. Scoped by the host to the open workspace — the module never asks for `fs.read_any`, so a path outside the project is refused by the host rather than trusted to the module. |
| `ui.rail` | The **Files** entry and its rows. |
| `ui.pane` | The placeholder pane when the module is disabled. |
| `ui.commands` | The five palette commands above. |
| `ui.toast` | "Already at the top", and the reason a command failed. |
| `events.subscribe` | `rail.query` and `files.reveal`. |
| `panes.spawn` | Opening the file you clicked. |

It reads no configuration, writes nothing, and makes no network calls. The only I/O is the
one socket the host hands it in `AVADA_MODULE_FD`; it never calls `std::fs` on the
workspace itself.

## Building and testing

```sh
cargo build --release --locked
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

`cargo test` runs the unit tests (a fake filesystem drives the tree and the state machine,
so they are instant and deterministic) and `tests/e2e.rs`, which spawns the real binary
over a socketpair with a fake host answering `host.fs.list` from a real temp directory —
scoped the way the host scopes it, so a passing e2e test is a test that would pass against
the app.

`avada-files --manifest` prints the embedded `avada.toml`, which is how the host reads a
source module's capabilities before granting any.

## Licence

MIT OR Apache-2.0.
