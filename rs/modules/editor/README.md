# avada-editor

A modal text editor for [Avada Terminal](https://avada.to), as a module.

The module owns the text; the host owns the pixels. `avada-editor` keeps the buffers, the
caret, the undo stack and the modes, and hands the host a `GridFrame` of styled spans to
draw. It never touches the filesystem itself — every read and write goes through
`host.fs.read` / `host.fs.write`, so the workspace scoping and the capability grant the
user approved are the same ones the editor is subject to.

```
cargo build --release
```

Installed like any other module: the host reads `avada.toml`, asks the user about the
capabilities it lists, and runs the binary with a socket on `AVADA_MODULE_FD`.

## What it does

| | |
|---|---|
| Rail entry | The open buffers, with a `modified` mark on the dirty ones. Click to switch. |
| Pane | One tier-5 grid surface, `editor`, painted by the module. |
| Commands | `open`, `save`, `close`, `next`, `prev`. |
| Prefs | `start_mode` — see below. |
| Events | Subscribes to `files.reveal`, so opening a hit from Files or Git lands here. |

Motions and edits are Helix-shaped: `w`/`b`/`e` word motion with three character classes,
`h`/`j`/`k`/`l`, `gg`-less document ends, `i`/`a`/`o`/`O` to enter insert mode, `u` and `U`
for undo and redo, `x` to delete a line. Three presets ship — `helix` (default), `vim` and
`basic` — and the host's keybindings page lets the user re-bind any of them, because the
module declares its actions with `host.keymap.declare` and receives already-resolved
action names rather than raw chords.

## One surface, many buffers

`DeclareKeymap` is per surface. A surface per document would put a fresh copy of every
binding on the keybindings page each time a file was opened, so there is exactly one
surface named `editor` and the buffer list lives inside the module. Switching buffers
repaints the same pane.

## Modes and the modeless keymap

The rule is one sentence: **in insert mode a key that produced text inserts that text,
otherwise the action wins; in normal mode the action wins and unclaimed text is dropped.**
That is what makes `q` type a `q` in insert mode and do nothing in normal mode, while
`enter` — which carries no text — keeps working in both.

The `basic` preset is modeless: arrows, `ctrl+s`, `ctrl+z`, and typing that always types.
It has no key that leaves normal mode, so on its own it would leave you in a buffer that
refuses to accept characters. Set **Editor → Start in → insert** in preferences and every
buffer opens ready to type. The preference is read at startup and followed live.

## Known limitations

**Single-chord bindings only.** `KeymapPreset::bindings` maps one chord to one action, so
there is no `gg`, no `dd`, and no operator-pending state. Both modal presets are
single-chord approximations of the editors they are named after — `x` deletes a line
outright rather than waiting for a motion.

**No syntax highlighting.** Real highlighting means tree-sitter, and a module installed
from source would then compile a grammar per language on the user's machine. The gutter is
coloured; the text is not.

**No git gutter.** A module may only call `host.*` — there is no way for one module to call
another — so `avada-git` is unreachable from here. Added and removed lines would have to
come through the host, and no method for it exists in contract version 1.

**No selections.** v0.1 has a caret and no anchor. `edit.delete.line` stands in for the
select-then-operate pairs the modal presets would otherwise want.

## `ropey`, not `helix-core`

The plan called for building on `helix-core`. It is not usable as a dependency: the
`helix-core` on crates.io is an unrelated placeholder last published in 2021, and the real
crate exists only inside the `helix-editor/helix` git workspace, where taking it would pull
tree-sitter and some two hundred crates into a module that users compile at install time.

So the text is a [`ropey`](https://crates.io/crates/ropey) `Rope` — the same rope Helix
itself uses — and the Helix-shaped semantics are implemented directly on top of it: char
indices rather than byte offsets, a goal column preserved across vertical motions and
forgotten by horizontal ones, word classes of Word/Punct/Space, auto-indent carried onto
new lines, and a bounded undo stack whose groups are broken by every motion and mode
change.

## Tests

```
cargo test
```

The unit tests cover the buffer, the edit dispatch, the buffer list, the keymap and the
renderer. `tests/e2e.rs` runs the real binary against a fake host over a socketpair and a
real temp directory: the handshake, registration, opening, painting, resizing, the modality
rule, saving to disk, undo, `files.reveal`, buffer switching, a refused pane, a refused
file, and a clean shutdown.
