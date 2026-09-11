# avada-hyperpane

The **Hyperpane** tab for [Avada Terminal](https://github.com/bshuler/avada-terminal): the
always-on agent session, and the rule that teaches that agent it can drive the app it is
running inside. This used to be built into the terminal as `avada/hyperpane`; it is now a
module, which means it can be disabled, updated, or replaced without touching the terminal.

## Install

```sh
avada marketplace install bshuler/avada-hyperpane
```

The rail entry appears as **Hyperpane** and, being tier 1, is drawn entirely by the host:
the module sends rows, never pixels.

## What it does

It opens one terminal pane in its own data directory and then types into it. That is the
whole shape of a shell-tier module — `panes.spawn` to get a shell, `panes.input` to drive
it — and it is why this module asks for both, separately.

| Gesture | Effect |
| --- | --- |
| Click the **Hyperpane** row | Open the tab, or say it is already open |
| Alt-click the **Hyperpane** row | Restart the agent in a fresh shell |
| Click the directory row | Reveal the working directory in a file pane |
| The filter box | Narrows the two rows on their label or their path |

Commands, all invokable from the palette:

| Id | Label |
| --- | --- |
| `open` | Hyperpane: Open the tab |
| `restart` | Hyperpane: Restart the agent |
| `send` | Hyperpane: Send text to the agent — `{ text }` |
| `reveal` | Hyperpane: Reveal the working directory |

It subscribes to one host event, `rail.query` — the filter box under the rail entry
changed.

### The working directory

The shell starts in the module's **data directory**, the path the host names in
`host.hello`'s `data_dir`, not in the open workspace. That is deliberate: the agent tab is
the one session that follows the human between projects, and a tab that restarted itself
every time the workspace changed would not be the same session at all. Nothing in the
contract guards that directory, so the module asks for no filesystem capability whatever.

### The skill

`skills/hyperpane/SKILL.md` is an always-on rule, materialised into the agent's own rules
by the host because this manifest declares `[skills] paths` together with the
`skills.materialize` capability. There is no runtime method for materialising skills —
it happens around the module at install time, not through it — so the module itself does
nothing to make it happen. Without that capability the pane still opens; the agent simply
does not know that `avada ctl` exists.

## Capabilities

| Capability | Why |
| --- | --- |
| `ui.rail` | The **Hyperpane** entry and its two rows. |
| `ui.pane` | The tab surface itself. |
| `ui.commands` | The four palette commands above. |
| `ui.toast` | "Hyperpane is already open", and the reason a command failed. |
| `events.subscribe` | `rail.query`. |
| `panes.spawn` | Opening the shell, and revealing the directory. |
| `panes.input` | Typing into the shell it opened. Granted separately from `panes.spawn`, because opening a terminal and driving somebody's shell are different powers. |
| `skills.materialize` | The always-on rule above. |

It reads no configuration, writes nothing, and makes no network calls. The only I/O is the
one socket the host hands it in `AVADA_MODULE_FD`; it never calls `std::fs` at all.

Denials degrade rather than fail: without `panes.spawn` the commands answer and toast the
refusal, and without `panes.input` the shell still opens and `send` says why nothing was
typed. `spawning_without_panes_input_still_opens_the_shell_but_types_nothing` proves it.

## Building and testing

```sh
cargo build --release --locked
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

`cargo test` runs the unit tests (the state machine has no I/O in it, so they are instant
and deterministic) and `tests/e2e.rs`, which spawns the real binary over a socketpair with
a fake host that mints pane ids the way `avada_core::module::rpc` does — so a passing e2e
test is a test that would pass against the app. No real terminal is ever started: the point
of proof is the conversation, not the shell at the far end of it.

`avada-hyperpane --manifest` prints the embedded `avada.toml`, which is how the host reads
a source module's capabilities before granting any.

## Licence

MIT OR Apache-2.0.
