# Plan: rename "pane" to "panel" everywhere

Status: **approved 2026-10-10, scope "everything, in stages"** (all steps in §3, Tier 3
included). Nothing renamed yet. Written against `27c93ab`.

**Gate (checked 2026-10-10):** unmerged lanes hold the files every step touches.
`subagent-panes` (14 commits, 47 files, incl. `ctl_cli.rs`, `control_host.rs`,
`control/dispatch.rs`, `state.rs`, the avada skill docs), `sp-pane`, `sp-chrome` and
`sp-discovery` (each about 35 files), and the uncommitted session-daemon/launchd work in
the main checkout (`app.rs`, `state.rs`, `main.rs`, `session/*`). Start step 2 only when
`git rev-list --count origin/main..<lane>` is 0 for each of them (or the lane is abandoned)
and the main checkout's `git status` no longer shows the daemon work.

`9917131` (2026-10-08) already changed every string a user *reads* to Window > Tab > Panel.
This plan covers what is left: the control API, CLI verbs, persisted formats, docs, skill
files, identifiers, and file names.

## 1. How big it is

`git grep -wi` on `origin/main` (`-w` has the same word boundaries as `\bpane\b`; the
underscore counts as a word character):

| Tier | What | Hits | Files |
|---|---|---|---|
| 1 | the word `pane` on its own: prose, comments, CLI verbs (`new-pane`), the `"pane"` kind string, docs, skill files | 7,611 | 396 |
| 2 | the plural `panes`: the workspace JSON key `panes`, `ctl panes`, prose | 2,133 | 232 |
| 3 | `pane` inside a longer identifier: `pane_id`, `PaneKind`, `PaneSpec`, `paneId`, `newPane`, `HOST_PANES_SPAWN`, `AVADA_PANE_ID` (862 distinct identifiers) | 7,849 | — |
| 3 | file names: `paneview.rs`, `viewpane.rs`, `imagepane.rs`, `gridpane.rs`, `newpane.slint`, `terminal-widget/src/pane.rs`, `core/src/claude_panes.rs`, `pane_origin.rs`, `ai/pane_buffer.rs`, eight `uitest/*pane*.rs`, four `docs/*-pane*-plan.md` | 27 files | — |

Strictly, `\bpane\b` matches Tier 1 only. Tier 2 is the same word in the plural. Tier 3 is
not matched by the regex, but it is what "every instance" would mean in practice.

**Never touched by this rename:** `hyperpane` (renamed separately, see below), the
read-compat spellings in `core/src/compat.rs`, `runpane` (about 400 hits; none of them match `\bpane\b`).

**Name collisions:** none. There is no `Panel`, `Panels`, `PanelKind`, `PanelSpec`,
`PanelId` or `panel_id` type or identifier in any `.rs` or `.slint` file. The roughly 630
existing `panel` hits are `leftpanel`, `LeftPanelAdapter` and UI strings. Re-run
`git grep -nwE 'Panel|Panels|PanelKind|PanelSpec|panel_id' -- '*.rs' '*.slint'` just before
Tier 3, in case a lane adds one.

## 2. What must keep working (compatibility)

Every name a running process, a file on disk, or another agent already depends on keeps
being accepted. The old name is read; the new name is written.

| Surface | Today | Mechanism |
|---|---|---|
| Workspace files (`last-workspace.json`, project files) | key `panes` in `TabSpec` (`core/src/workspace/model.rs:129`) | `#[serde(alias = "panes")]` on the renamed field; write `panels`. No one-way migration: Bert's live workspace must relaunch cleanly on the new build, and roll back cleanly to the old one (the old build reads `panes`, so the write has to stay `panes` until no old build is in use. Simplest: **keep writing `panes` for one release, then switch**). |
| Recently Closed entries | `ClosedSpec.kind == "pane"` (`state.rs:7954`), persisted since `27c93ab` (B5, shipped today) | The reader accepts `"pane"` and `"panel"`. Without this, every entry saved by the B5 build is dropped on the first relaunch after the rename. |
| Control API JSON types | `newPane` (31 uses), `movePane`, `recoverPane`, `closePane`, `focusPane`, `restartPane`, `renamePane`, `recolorPane`, `speakingPane`, `recordingPanes` | Accept both names for a deprecation window (serde `alias`); answers use the new names only after the skill docs move. |
| CLI verbs | `new-pane`, `close-pane`, `move-pane`, `focus-pane`, `rename-pane`, `restart-pane`, `recolor-pane`, `swap-pane`, `ctl panes` | Old verbs stay as hidden aliases indefinitely. `scripts/gui-harness/gui-test.sh` (59 hits), `scripts/dictation-demo.sh`, the `goal-orchestrator` skill and other agents' scripts call them. |
| Session ids | `pane-<…>` ids held by the session daemon for every live session | **Don't change the id format.** The ids are opaque, the daemon must not be restarted, and every live pane, attach command and Recently Closed entry carries one. This is the riskiest item in the whole rename. |
| `AVADA_PANE_ID` env var | exported into every running pane | Export both `AVADA_PANE_ID` and `AVADA_PANEL_ID`; read either. Live sessions keep the old one until they restart. |
| Module SDK | `HOST_PANES_SPAWN`, `PanesSpawn`, `ModulePaneRef` | Marketplace modules compile against these: add the new names, keep the old ones as deprecated aliases. |
| Shipped skill docs | `resources/claude/hyperpane/.claude/skills/avada/` (125 hits) | Other lanes' agents read these at session start and drive the **installed** app. Change them **last**, in the same release that installs the new verbs. |
| `AGENTS.md` | `~/code/AGENTS.md` (Avada usage) and the repo's "Pane" block | The repo block describes runpane.com, which is gone; it says "do not delete". **Bert decides** whether it goes. The renamer must not touch it. |

## 3. Order of work

The uncommitted session-daemon/launchd work in the main checkout and the subagent-panes
lane both edit `app.rs`, `state.rs` and `daemon*.rs`. A repo-wide rename landed underneath
them conflicts with every hunk they hold. So:

1. **Wait** until both have landed on `main` (check `git -C <main checkout> status` and the
   Subagents tab).
2. **Compat layer first, one commit:** the serde aliases, the CLI and JSON verb aliases,
   the dual env var, and the dual `ClosedSpec.kind` reader, with a test for each one that
   feeds in the *old* spelling. These tests prove step 3 broke nothing on disk or on the wire.
3. **Tier 1 + 2, one scripted commit** in a fresh worktree off `main`: a case-preserving
   table (`pane→panel`, `Pane→Panel`, `PANE→PANEL`, `panes→panels`, `Panes→Panels`) applied
   with `perl -pi`, word-bounded, skipping an allowlist (the exclusions in §1, the alias
   strings from step 2, the `pane-` id prefix, the AGENTS.md block). Not by hand.
4. **Skill docs + `~/code/AGENTS.md`**, in the same release as step 3's install.
5. **Tier 3 later**, in a quiet window when no lane holds `rs/`: identifiers via the same
   table, then `git mv` the 27 files and fix `mod`/`import` lines. It is pure churn with
   nothing visible to the user, so it is optional.

## 4. Checks before each commit

- `git grep -wiE 'panes?'` returns only the allowlist.
- Full build, tests and clippy on hp2 (the Mac's cargo is paused by the disk watchdog
  below 10 GB free). Core lib about 2,023 tests, app about 949, plus `workspace_format`.
- The headless UI harness feature matrix stays at 106/106 (`src/uitest/`).
- `scripts/check-windows.sh` and `scripts/check-linux-docker.sh` (hp1/hp2).
- The `avada_mutants` cfg gates and the workspace-root mutants config still resolve.
- **No bare `cargo fmt`**: local rustfmt 1.9.0 reformats unrelated code. Format only the
  files that changed, and only if CI does.
- Live: install, `ctl command '{"type":"restartApp","scope":"gui"}'`, then confirm the
  pre-rename `last-workspace.json` loads, its `closed` entries survive, every live session
  is still attached, and both `avada ctl new-pane` and `avada ctl new-panel` work.

## 5. Size of each step

| Step | Rough diff | Risk |
|---|---|---|
| 2 compat layer | ~15 files, small | low; tested with old spellings |
| 3 Tier 1+2 | ~450 files, ~9,700 lines, mostly comments and docs | medium; mechanical, guarded by step 2 |
| 4 skill docs | ~10 files | low, but timing matters |
| 5 Tier 3 | ~300 files, ~7,800 identifiers, 27 renames | high merge-conflict cost; no user-visible gain |

## 6. The singular `hyperpane`

The last trace of the old product name: the orchestrator module (`modules/hyperpane`,
id `bshuler__avada-hyperpane`), `resources/claude/hyperpane/`, the workspace directory
`~/Library/Application Support/avada/hyperpane`, and the Claude cache directory derived
from that path. It is renamed in the same release as step 3, to a name Bert picks; the
workspace directory moves with a read-the-old-path fallback, like the data-dir copy in
`compat.rs`.
