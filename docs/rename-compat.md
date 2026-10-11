# Rename compatibility: Hyperpanes → Avada Terminal

Avada Terminal shipped as **Hyperpanes** through 0.0.36. The rename touched every
outward-facing name at once: binary `avada`, bundle id `to.avada.terminal`, app-support
directory `avada`, `AVADA_*` environment variables, `.avada/` project directory, `.avada`
workspace suffix, `avada-set` format tag.

## Compatibility has ended (0.2.34)

Up to 0.2.33 the app read the old spellings wherever it read the new ones, and copied
the old app-support directory into the new one on first launch. From 0.2.34 it reads
only the new names:

- **Environment.** Only `AVADA_*` variables are read or injected into panes. A hook
  script or MCP configuration that still reads `HYPERPANES_*` must be renamed.
- **Data directory.** The old app-support directory is no longer copied. A machine
  still holding `~/Library/Application Support/hyperpanes` (macOS), `%APPDATA%\hyperpanes`
  (Windows) or `$XDG_*_HOME/hyperpanes` (Linux) and no `avada` directory must copy it
  across by hand before launching 0.2.34. Copy into an empty `avada` directory only:
  merging the two by modification time replaces newer settings with older ones.
- **Project directories.** A checkout's layout lives in `.avada/project.json`. Rename a
  leftover `.hyperpanes/` to `.avada/` (`git mv .hyperpanes .avada`).
- **Workspace files and sets.** `*.hyperpanes` files and sets tagged
  `"format": "hyperpanes-set"` no longer open; rename the file to `*.avada` or change the
  tag to `avada-set`.

## What you may notice after upgrading from Hyperpanes

- **Microphone (macOS).** Permissions are granted per bundle id. `to.avada.terminal`
  is new to the system, so the first dictation prompts for the microphone again. The
  old grant for `com.hyperpanes.app` is unused and can be removed under System
  Settings → Privacy & Security → Microphone.
- **Accessibility / automation (macOS).** Same rule; re-grant once if you use a feature
  that needs it.
- **Claude Code skills and MCP config.** The shipped skill is `.claude/skills/avada`; a
  leftover `.claude/skills/hyperpanes` directory or `hyperpanes-mcp` entry in
  `~/.claude.json` no longer works and can be deleted.

## The orchestrator directory and module (0.2.36)

0.2.36 renamed the last singular `hyperpane`. Nothing needs doing by hand inside the app:

- **Workspace directory.** On first launch `~/Library/Application Support/avada/hyperpane`
  (and its Windows/Linux equivalents) is renamed in place to `…/avada/orchestrator`. If
  both exist, both are kept and a warning is logged; merge them by hand.
  From 0.2.37 this runs on every launch; 0.2.36 ran it only when it created the tab, so
  an install that already had the tab kept the old name until 0.2.37.
- **Module.** `bshuler/avada-hyperpane` is uninstalled once `bshuler/avada-orchestrator`
  is installed, and its block leaves the directory's `AGENTS.md`.
- **Tab title.** A system tab still titled "Hyperpane" or "Avada" is renamed
  "Orchestrator"; a title you chose yourself is kept.

Outside the app: Claude Code keys its session history and memory by directory path, so
`~/.claude/projects/-Users-<you>-Library-Application-Support-avada-hyperpane` must be
moved to the matching `…-avada-orchestrator` name to keep the orchestrator's past
sessions and memory. Any doc or `AGENTS.md` that names
`resources/claude/hyperpane/` must point at `resources/claude/orchestrator/`.
