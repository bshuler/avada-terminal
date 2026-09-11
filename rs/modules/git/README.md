# avada-git

The Git entry on the Avada Terminal rail: the branch you are on, what is staged,
what has changed, what is untracked — and, when you click a commit hash anywhere
in the terminal, the files that commit touched.

This used to be a built-in mode on the left panel. It is now a module, and the
host has no idea what a working tree is; it only knows how to answer questions
about one.

## Install

```
avada module install bshuler/avada-git
```

## What it does

The rail entry lists the repository of the open workspace.

| Gesture | What happens |
| --- | --- |
| Click a file | The file opens in a pane |
| Right-click a file | The host's file menu, including **Show Diff** |
| Right-click the top row | The diff of the whole tree, or of the whole commit |
| Type in the filter box | The listing narrows; nothing is re-read |

The top row is the branch (`main ↑1`) in the working tree, and the commit's
subject when you are looking at a commit. Sections — Staged, Changed, Untracked
— appear only when they have something in them, and each file's second column is
git's own status letter: `M`, `A`, `D`, `R`, `?`.

## Commands

| Command | What it does |
| --- | --- |
| `Git: Refresh` | Read the repository again |
| `Git: Show a commit` | Take `rev` and list that commit's files |
| `Git: Back to the working tree` | Leave a commit view |
| `Git: Filter` | Narrow the listing by path |

## Events

- `rail.query` — the filter box.
- `git.commit` — a commit hash was clicked in a pane. The payload carries the
  repository it belongs to, so a hash in one checkout does not resolve against
  another.

## Capabilities

| Capability | Why |
| --- | --- |
| `git.read` | The whole point: `host.git.status` and `host.git.commit`. The module never runs `git` itself — the host does, scoped to the workspace. |
| `ui.rail` | To be an entry at all, and to hand over rows. |
| `ui.pane` | To be drawn in the left panel. |
| `ui.commands` | The four commands above. |
| `ui.toast` | To say why a refresh failed instead of blanking the panel. |
| `events.subscribe` | The filter box and the commit link. |
| `panes.spawn` | Clicking a file opens it. |

Notably absent: anything that runs a process. A row states which revision it came
from and the *host* offers the diff; a module holding a read capability must not
be able to start a command.

## Building and testing

```
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

`tests/e2e.rs` builds a real repository in a temp directory, runs the real
binary against a fake host that answers `host.git.status` from that repository,
and checks the rows that come back. It skips itself if git is not installed.

## Licence

MIT OR Apache-2.0.
