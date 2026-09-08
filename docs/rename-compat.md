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

**Mirror the environment for two releases.** Every `AVADA_*` variable the app injects
into a pane (`AVADA_PANE_ID`, `AVADA_CONTROL_FILE`, `AVADA_CONTROL_TOKEN`, …) gets a
`HYPERPANES_*` twin with the same value, so a hook script or MCP configuration written
against the old name keeps working. The mirror is removed two releases after the
rename; rename your scripts before then.

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
  `hyperpanes-mcp` entry in `~/.claude.json` still works while the env mirror lasts. The
  shipped skill is now `.claude/skills/avada`; update the entry when convenient.
- **Workspace files** named `*.hyperpanes` still open (`avada ./dev.hyperpanes`); the
  file association on Windows and Linux registers only `*.avada`.
- **The old app** keeps its own directory and settings and can run alongside the new one.
