# Rename compatibility: Hyperpanes → Avada Terminal

Avada Terminal shipped as **Hyperpanes** through 0.0.36. The rename touched every
outward-facing name at once (binary `avada`, bundle id `to.avada.terminal`, app-support
directory, `AVADA_*` environment variables, `.avada/` project directory, `.avada`
workspace suffix, `avada-set` format tag). Everything that carried the old name is still
on users' disks and in their scripts, so the code keeps a compatibility layer in
`rs/crates/core/src/compat.rs` — the one module that knows the old spellings.

## Policy

**Read both, write the new.** A legacy env var, project directory, suffix or format tag
is accepted wherever the new one is. The app only ever writes the new one.

**The environment mirror has ended.** Until 0.2.33 every `AVADA_*` variable the app
injected into a pane (`AVADA_PANE_ID`, `AVADA_CONTROL_FILE`, `AVADA_CONTROL_TOKEN`, …)
also got a `HYPERPANES_*` twin. From 0.2.34 panes get only the `AVADA_*` names; a hook
script or MCP configuration that reads `HYPERPANES_*` must be renamed. Avada itself
still *reads* a `HYPERPANES_*` value when the `AVADA_*` one is absent.

**Copy the data directory once.** On the first launch, the old app-support directory is
copied into the new one:

| Platform | Old | New |
|---|---|---|
| macOS | `~/Library/Application Support/hyperpanes` | `~/Library/Application Support/avada` |
| Windows | `%APPDATA%\hyperpanes` | `%APPDATA%\avada` |
| Linux | `$XDG_CONFIG_HOME/hyperpanes`, `$XDG_DATA_HOME/hyperpanes`, `$XDG_STATE_HOME/hyperpanes` | same, `avada` |

It is a copy, not a move: the old install may still be installed and running against
its directory. Files the new directory already has are kept (a launch of the renamed
app before this shipped is not undone), and live state is never carried over: `logs/`,
`control.json`, `control-pane-ids.json`, SQLite `-wal`/`-shm` side files, `*.bak`. A
marker file `.migrated-from-hyperpanes` in the new directory records what was copied
and makes the copy a one-time event; delete it to copy again (existing files still win).

**Project directories.** A checkout with only `.hyperpanes/project.json` is discovered
and read as before. The first time Avada writes into that checkout it renames
`.hyperpanes/` to `.avada/` — one git-visible rename, no copy. A checkout with both
directories uses `.avada/` and leaves the other alone.

## What you may notice after upgrading

- **Microphone (macOS).** Permissions are granted per bundle id. `to.avada.terminal`
  is new to the system, so the first dictation prompts for the microphone again. The
  old grant for `com.hyperpanes.app` is unused and can be removed under System
  Settings → Privacy & Security → Microphone.
- **Accessibility / automation (macOS).** Same rule; re-grant once if you use a feature
  that needs it.
- **Claude Code skills and MCP config.** A `.claude/skills/hyperpanes` directory or a
  `hyperpanes-mcp` entry in `~/.claude.json` that reads `HYPERPANES_*` stopped working
  when the env mirror ended (0.2.34). The shipped skill is `.claude/skills/avada`.
- **Workspace files** named `*.hyperpanes` still open (`avada ./dev.hyperpanes`); the
  file association on Windows and Linux registers only `*.avada`.
- **The old app** keeps its own directory and settings and can run alongside the new one.
