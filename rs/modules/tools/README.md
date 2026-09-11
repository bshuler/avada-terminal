# avada-tools

The **AI CLI** rail entries for [Avada Terminal](https://github.com/bshuler/avada-terminal):
one entry per tool — Claude Code, Cursor, Copilot — listing the conversations you can pick
back up, and opening the pane that resumes one. These used to be tabs built into the left
panel; they are now a module, which means they can be disabled, reordered, updated, or
replaced without touching the terminal.

## Install

```sh
avada marketplace install bshuler/avada-tools
```

Then enable it in the workspace you want it in (`avada marketplace enable bshuler/avada-tools`,
or the marketplace rail entry). The entries are tier 1, so the host draws them: the module
sends rows, never pixels.

## What it does

Conversations are grouped under the project directory they were held in, newest first.

| Gesture | Effect |
| --- | --- |
| Click a project heading | Fold or unfold that project |
| Click a conversation | Resume it in a new pane, in its own project directory |
| Select a conversation without opening it | Marks it, and nothing else — a resume launches a process, which is not something an arrow key should do |
| Right-click a row | The host's own context menu — the module answers and does nothing |
| The filter box | Matches the conversation's label, its project, or its branch |

Commands, all invokable from the palette:

| Id | Label |
| --- | --- |
| `refresh` | Tools: Re-read the conversation history |
| `filter` | Tools: Filter conversations — `{ query, entry? }`; no entry means every entry |
| `resume` | Tools: Resume a conversation — `{ tool, session }` |

It subscribes to one host event, `rail.query`: the filter box under an entry changed.

**Which entries exist** is the human's own choice. The host's `toolFavorites` setting — the
same stars that used to order the tabs — picks the tools and their order. With nothing
starred, every tool Avada can read a transcript for gets an entry. A tool with no transcript
reader never gets one: the entry would be permanently empty and there would be no way to
tell that from "no conversations yet".

## Where the work happens

Almost nothing this module draws comes down the module pipe. Reading a transcript means
parsing several thousand lines of per-tool history formats out of stores that live outside
any workspace root, and probing the filesystem for binaries — that stays in the host, where
it is shared with the rest of the app and covered by the host's own tests. The module is
the *presentation*: which entries exist, how conversations group and read, what a click
does.

The seam between the two is four HTTP routes on the host's control server, which the host
names in `host.hello` along with a per-run bearer token:

| Route | For |
| --- | --- |
| `GET /tools` | The catalogue: id, name, brand colour, whether Avada can read its history, where the binary is |
| `GET /tools/{tool}/sessions` | That tool's conversations: project, branch, start time, summary, message count, and either the command that resumes one or the reason it cannot be |
| `GET /settings` | `toolFavorites`. A host with no GUI answers 503, which means "no stars known", not a failure |
| `GET /state` + `POST /command` | Resuming: `newPane` needs a window id and the module has never been told one, so it asks for the first window and opens the pane there |

The module opens a loopback socket and speaks HTTP itself rather than asking the host to
fetch for it, so it needs no `net.fetch` and no HTTP dependency. A host that offers no
control server at all still gets its entries — each one saying why it is empty, because an
entry that drew nothing would read as a crash.

Transcript *text* never crosses this boundary. The host sends the summary and the first
prompt, both clipped, and never the conversation.

## Capabilities

| Capability | Why |
| --- | --- |
| `settings.read` | `GET /tools` and `GET /settings` — the catalogue and the stars |
| `fs.read_any` | `GET /tools/{tool}/sessions`. The transcript stores are outside every workspace root, so `fs.read` could not reach them; the host does the reading either way |
| `workspace.read` | `GET /state`, for the window id a `newPane` must name |
| `workspace.write` | The floor `POST /command` sits behind |
| `panes.spawn` | The `newPane` verb itself |
| `ui.rail` | The entries and their rows |
| `ui.commands` | The three palette commands |
| `ui.toast` | Why a conversation could not be resumed |
| `events.subscribe` | `rail.query` |

Five non-UI capabilities rather than one: each names a different thing the host is being
asked for, and a human refusing `panes.spawn` should still get the lists.

It writes nothing, reads no files of its own, and makes no network call off 127.0.0.1.

## Building and testing

```sh
cargo build --release --locked
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

`cargo test` runs the unit tests — a fake `Api` drives the state machine and the row
shaping, so they are instant and deterministic — and `tests/e2e.rs`, which spawns the real
binary over a socketpair with a fake host on the other end *and* a real loopback HTTP
server standing in for the control plane. That server answers the way the host answers,
including the 503 on `GET /settings` and the 400 a `POST /command` with no `windowId`
gets, so a passing e2e test is a test that would pass against the app.

`avada-tools --manifest` prints the embedded `avada.toml`, which is how the host reads a
source module's capabilities before granting any.

## Licence

MIT OR Apache-2.0.
