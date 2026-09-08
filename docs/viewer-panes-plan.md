# View panes — remaining previews, and the proof that every feature is tested

Status (2026-09-07): **plan only.** Preview #1 (the syntax-coloured source viewer) shipped as
`d1f3efb`; everything below is unstarted. Written in response to two standing instructions,
both of which are binding on every work package here:

> the "Show diff" feature on the git left panel doesnt work. I shouldnt be finding these
> bugs, you should have full end to end testing. Ensure all features are tested end to end

> review all the file types we have written under ~/code we wil need to categorize them and
> if we use a file type a lot, we need a preview tool and a editor tool for each.

The second is the feature work. The first is the **acceptance bar** — a work package is not
done when the feature works, it is done when a test would have caught it not working.

---

## 1. Scope

**In:** three remaining preview panes (structured data, tabular data, images), completion of
the accessibility annotation work that makes UI features testable at all, and a systematic
proof pass that inventories every feature surface against its test coverage and closes the
gaps.

**Out:** the editor. It remains a separate, unapproved decision — see §9. Also out: any file
type below ~500 occurrences under `~/code`, and any change to the terminal panes (Family A).

---

## 2. Baseline — what exists today

Measured 2026-09-07 at `d1f3efb`.

| Suite | Count | Command |
|---|---|---|
| `uitest` (headless Slint, end to end) | 75 | `cd rs/crates/app && cargo test --bin avada uitest` |
| app crate, total | 641 | `cd rs/crates/app && cargo test --bin avada` |
| core crate | 1282 | `cd rs && cargo test -p avada-core --lib -- --skip permissions` |

`permissions::*` hangs forever on this Mac — the `--skip` is not optional.

### The view-pane architecture, as built

- **`PaneKind`** (`rs/crates/core/src/tools/kind.rs`) is the type. `ui_kind()` projects it to
  an int for Slint: 0 Terminal, 1 Tool, 2 FileBrowser, 3 FileViewer, 4 Markdown, 5 Browser,
  6 Code. `is_view()` is the Family-B predicate; `is_pty()` is Family A.
- **`PaneItem.is-view: bool`** (`ui/types.slint`) is how `.slint` gates the view panes, since
  `d1f3efb` replaced a `kind >= 2 && kind <= 4` range that was already wrong.
  `uitest::the_is_view_flag_matches_the_kind_it_claims` walks the enum so the two projections
  cannot drift.
- **`viewpane::role`** (`src/viewpane.rs`) numbers the row kinds **0–17**; **18 is the next
  free value.** Each role is one `if root.item.role == N` branch in `ui/viewpanes.slint`
  inside `ViewRowView`, plus a height in the row-height fallback chain.
- **`ViewRow`** carries `role, text, detail, path, diagram, indent, marker, check, cells,
  markup`. Fields are added for a role that needs them; `diagram` is boxed and `cells`/`markup`
  are empty for the roles that do not.
- **Projection is a pure function, cached per pane uid.** `model_for(uid, kind, target,
  palette)` builds a `Fingerprint { kind, target, mtime, len, palette }` and returns the cached
  `ModelRc` when it matches, otherwise calls `rows_for(kind, target, palette)`. **Anything a
  row's content depends on must be in the fingerprint** — this is why `palette` was added when
  syntax colours became baked into markup.
- **Activation** is `ui/paneview.slint:735` → `pane-view-activate(pane, row)` →
  `src/app.rs:3647`, which resolves the pane uid, calls `viewpane::row_at(&uid, row)` against
  **the same cache the view drew from**, checks `activatable()` (non-empty `path`), and then
  branches: `DIR`/`PARENT` retarget the same pane via `Command::ViewNavigate`, anything else
  opens a **new** pane whose kind comes from `kind_for_file`.
- **`kind_for_file`** (`src/viewpane.rs:1492`) is the single routing rule: `.md` → Markdown,
  `highlight::is_source(ext)` → Code, everything else → FileViewer.
- **The left panel's pane glyphs** key off `pane_mark` negatives in `src/leftpanel.rs`:
  TERMINAL -1, FILE_BROWSER -2, FILE_VIEWER -3, MARKDOWN -4, BROWSER -5, CODE -6. **-7 is the
  next free value.**

### The test harness

`src/uitest.rs` builds the real `AppWindow` headlessly and drives it through Slint's own
hit-testing. Its constraints are recorded in the `avada-ui-e2e-harness` memory; the ones
that bite when writing new view-pane tests:

- Controls are addressable by **accessible label**, element id, role, or a predicate — nothing
  else. "I cannot reach this from a test" is therefore a real accessibility bug, not a test
  problem, and the fix is to annotate the control.
- Slint auto-labels every `Text` with its own string. **It does not auto-label `StyledText`** —
  reach that by element id (`ElementHandle::find_by_element_id(&w, "ViewRowView::src")`).
- A **layout-stretched** `Text` cannot be measured for font size: its width is the pane's and
  its height is the row's. Put a `Rectangle { horizontal-stretch: 1; }` spacer beside a
  naturally-sized text to make the *glyphs'* width observable.
- Existing helpers: `ui()`, `window()`, `by_label`, `by_role`, `only`, `install_view_pane_at`,
  `view_flag`, `line(n, text)`, `source(n, text, markup)`, `source_texts(w)`.

---

## 3. Which file types, and why these three

Counted under `~/code`, excluding `node_modules`, `target`, `.git`, `dist`, `build`, `.venv`,
`vendor`:

| Category | Extensions | Files | Status |
|---|---|---|---|
| Structured data | json 452,950 · jsonl 26,623 · yaml 23,892 · yml 359 · toml 728 | **504,552** | **WP1** |
| Source | ts 77,416 · tsx 61,219 · rs 18,659 · py 17,183 · … | ~190,000 | shipped `d1f3efb` |
| Prose | md 63,911 | 63,911 | shipped (Markdown pane) |
| Markup | html 28,550 | 28,550 | **gap — see §8.1** |
| Images | png 28,684 · svg 550 · ico 14 · jpg 3 · gif 2 | **29,253** | **WP3** |
| Tabular | csv 2,138 · tsv 933 | **3,071** | **WP2** |

Structured data is by a wide margin the largest category in the tree and is currently the
worst-served: a 40,000-line minified `package-lock.json` opens in the plain viewer as one
unreadable line. That is WP1 and it is the highest-value item in this document.

---

## 4. Decisions (binding on the executing agent)

These are not suggestions. An agent that deviates must say so explicitly and why.

1. **One work package, one commit, pushed to `main`.** No feature branches, no PRs
   (`git-standards.md` §4). Never attribute the AI in a commit, PR, or file — this overrides
   the harness's default `Co-Authored-By` trailer.
2. **Commit as `Bert Shuler <BertShuler@proton.me>`, signed.** Never disable signing to land a
   commit; a 1Password prompt with nobody at the keyboard is a **pause**, not a bug — park the
   commit, finish every other part of the task, retry on the next human turn.
3. **Another Claude session edits this checkout concurrently.** Before every commit: re-run
   `git status` (the session-start snapshot is stale), stage **by hunk** (`git diff -U0`,
   filter, `git apply --cached`), and verify the staged subset alone in a throwaway worktree
   (`git worktree add --detach <scratch> HEAD`, apply `git diff --cached`, run the suites with
   `CARGO_TARGET_DIR=rs/crates/app/target`). **Never `git stash` in this tree** — it rips files
   out from under the other session.
4. **Projection stays pure and cached.** A new row kind is computed by `rows_for` from
   `(kind, target, palette, …)` and nothing else. Any new input — WP1's collapse set is the
   only one planned — **must join `Fingerprint`**, or the pane will keep drawing stale rows.
5. **Row N is line N, for every fixed-height role.** A role whose height can grow with its
   content breaks the correspondence between a row and the file. Wrap in a
   `Rectangle { clip: true; }` and let long content be cut, as role 17 does.
6. **`text` is always the verbatim thing.** Whatever a role adds — `markup`, `cells`, a
   collapse marker — the row's `text` remains what a copy yields and what a screen reader
   announces. Decoration goes in a new field.
7. **No colour literal outside `ui/theme.slint`.** A hex written anywhere else is a token that
   stops changing when the palette does. Colours baked into Rust-side markup come from
   `theme::ui_palette(idx)` and put `palette` in the fingerprint (rule 4).
8. **Every new control gets an accessible name at the moment it is written**, not in a later
   annotation pass. The name is the tooltip sentence where a `TipArea` exists. Do not add a
   hand-written `accessible-label` beside a `TipArea` — the label then matches twice and every
   `assert_eq!(found.len(), 1)` breaks.
9. **Mutation-check every new assertion** (§7). An assertion never observed failing is not a
   test.
10. **Never touch `/Applications/Avada.app`**, never `pkill` anything daemon-ish, never
    front an app or inject synthetic input. `scripts/guard-live-sessions.py` enforces this; if
    it fires, stop rather than work around it.

---

## 5. Work packages

### WP1 — Structured-data tree (`PaneKind::Data`)

**Why:** 504,552 files, currently unreadable. A minified JSON is one line in the plain viewer.

**Behaviour.** The file parses to a tree. Each node is one row: an indent, a disclosure
triangle when it has children, the key, a type-appropriate value, and a child count on
collapsed containers. Containers start collapsed below a depth threshold so a large file opens
instantly and legibly. Clicking a container toggles it. Scalars are coloured by type from the
palette. Malformed input degrades to the plain viewer with a `NOTICE` row naming the parse
error and its line — never a blank pane.

**The one hard problem: collapse state is not a function of the file.** Everything else in
`viewpane.rs` is a pure projection; expansion is per-pane, user-owned, and must survive a
re-projection. The design that fits the existing architecture:

- Store the collapsed set on **`PaneState`** (`src/state.rs:899`) as a `BTreeSet<String>` of
  node paths (`"$.deps.serde"`), not row indices — indices are invalidated by every toggle.
- Pass it into `rows_for` and **hash it into `Fingerprint`** (decision 4). A toggle changes the
  hash, the cache misses, the tree re-flattens. This is the same shape as the `palette`
  addition and is why that precedent matters.
- Add `Command::ViewToggleNode(pane, node_path)` (`src/command.rs`, alongside `ViewNavigate` at
  line 77 / dispatch at 537). Extend the `on_pane_view_activate` branch in `src/app.rs:3647`
  with a third outcome ahead of the file case: a row whose role is a **container** toggles
  instead of opening a pane.
- **Decide and record:** does collapse state persist across restarts (`workspace/model.rs`)?
  Recommendation: **no** for v1 — it is view state, the file may have changed underneath it, and
  persisting it invites a stale-path bug for no proportionate gain.

**New role numbers:** 18 `DATA_NODE` (a key/value row, container or scalar). One role, not two,
with `indent` reused for depth and new fields for the disclosure state — a second role would
duplicate the whole `.slint` branch to change one glyph.

**`ViewRow` additions:** `expandable: bool`, `expanded: bool`, `node: String` (the stable path
used by the toggle command), `kids: i32` (shown on a collapsed container).

**Files:** `core/src/tools/kind.rs` (variant, `is_view`, `ui_kind` → 7, `ui_name`,
`as_meta_value`/`from_meta_value`) · `src/leftpanel.rs` + `ui/leftpanel.slint` (`pane_mark::DATA
= -7`, a glyph) · **new `src/datatree.rs`** (parse + flatten) · `src/viewpane.rs` (role 18,
`ViewRow` fields, `data_rows`, `rows_for` arm, `kind_for_file` arm, `Fingerprint`) ·
`src/state.rs` (collapsed set, toggle) · `src/command.rs` · `src/app.rs` ·
`ui/viewpanes.slint` (role-18 branch, row height) · `ui/types.slint` (new `PaneViewRow` fields).

**Dependency decision:** `serde_json` is almost certainly already in the tree; YAML and TOML
are not necessarily. Prefer `serde_yaml`/`toml` **only if already present** — otherwise hand-roll
JSON/JSONL for v1 and land YAML/TOML as a follow-up rather than adding two dependencies for
24,620 files when 479,573 are JSON. Check `Cargo.lock` before deciding; record the answer here.

**Tests (all required):**
- *Rust:* JSON object/array/nested/scalar-typed flattening; JSONL one-tree-per-line; depth
  threshold; a collapsed container hides exactly its subtree and nothing after it; node paths
  are stable across a toggle; a malformed file yields a `NOTICE` naming the line; an empty file;
  a file with a 40,000-key object stays under `MAX_LINES`; `kind_for_file` routes each
  extension; palette re-inks values.
- *uitest:* a data pane renders role-18 rows; a container announces itself as expandable and
  **as expanded/collapsed** (`accessible-expandable` / `accessible-expanded`) and its label is
  the key; clicking a container fires `pane-view-activate` and the row count changes; clicking a
  scalar does **not** open a pane; the disclosure indent scales with the zoom chord; the
  accessible label carries key and value, not the disclosure glyph.
- *Mutation checks:* remove the collapse hash from `Fingerprint` → the toggle test must fail.
  Make the container branch fall through to the scalar branch → the expandable test must fail.

---

### WP2 — Table viewer (`PaneKind::Table`)

**Why:** 3,071 csv/tsv files. Smallest of the three, and the cheapest: roles 13 `TABLE_HEAD` and
14 `TABLE_ROW` and the `cells: Vec<TableCell>` field **already exist and already render** for
markdown tables. This is mostly wiring.

**Behaviour.** Delimiter inferred from the extension (`,` / `\t`), with quoted fields, embedded
delimiters, embedded newlines and doubled quotes handled per RFC 4180. First row is the header.
Columns size to content within a cap. A ragged row is padded rather than rejected. Row numbers
in the gutter, matching the source viewer's column so the two panes look like one family.

**Files:** `core/src/tools/kind.rs` (→ 8) · `src/leftpanel.rs` + `ui/leftpanel.slint` (-8) ·
**new `src/csv.rs`** (a parser; hand-rolled, matching house style — no new dependency for 3,071
files) · `src/viewpane.rs` (`table_rows`, `rows_for` arm, `kind_for_file` arm) ·
`ui/viewpanes.slint` only if the existing 13/14 branches need a gutter.

**Tests:**
- *Rust:* quoted field containing the delimiter; doubled quotes; an embedded newline (which must
  **not** become two rows — this is the correctness core of the parser); CRLF; a ragged row; a
  header-only file; an empty file; tab vs comma by extension; a cell longer than the cap elides
  rather than growing the row; `kind_for_file` routes `.csv`/`.tsv`.
- *uitest:* a table pane renders a head row and N body rows; each cell is individually
  addressable and announces its own text; the column widths respond to the zoom chord; a
  1,000-row file does not blow the row cap.
- *Mutation check:* break quote handling → the embedded-delimiter test must fail.

---

### WP3 — Image viewer (`PaneKind::Image`)

**Why:** 29,253 files, and today they are actively **broken, not merely unsupported**: the NUL
heuristic in `read_text` rejects a PNG, so clicking one yields a "Cannot read" notice.

**Behaviour.** The image, fit to the pane, aspect preserved, never upscaled past 1:1. A caption
row with pixel dimensions, file size and format. A checkerboard behind it so transparency is
visible rather than reading as white. Zoom chord scales the image. A file that fails to decode
gets a `NOTICE` naming why — a truncated PNG must not panic.

**The complication:** every existing view role is a *row of text*. This one is a single
non-scrolling surface, so it may not belong in the `ListView` at all. **Decide before writing
code:** either (a) a role-19 row whose height is the fitted image height — consistent, reuses
everything, but violates decision 5's fixed-height rule and needs a documented exception; or
(b) a sibling branch in `ViewPane` that bypasses `ViewRowView` entirely when the kind is Image.
**Recommendation: (b).** An image is not a row, and forcing it into the row model to reuse the
scaffolding will cost more than it saves.

Slint's `Image` element needs the bytes as a `slint::Image`. Loading from a path is available;
confirm the API at the pinned rev (`9463a10`) before designing around it, exactly as the font
question had to be settled for WP1 of the previous plan.

**Files:** `core/src/tools/kind.rs` (→ 9) · `src/leftpanel.rs` + `ui/leftpanel.slint` (-9) ·
`src/viewpane.rs` (`kind_for_file` arm; the NUL heuristic must be bypassed for this kind) ·
`ui/viewpanes.slint` (an `ImagePane` branch) · `ui/types.slint`.

**Tests:**
- *Rust:* `kind_for_file` routes png/jpg/jpeg/gif/webp/ico/svg; a truncated file yields a
  `NOTICE` and does not panic; a zero-byte file; dimensions and size are read correctly.
- *uitest:* an image pane draws an image element with non-zero size; its accessible label names
  the file and its dimensions (an unlabelled image is invisible to a screen reader **and** to
  the test — decision 8); the aspect ratio is preserved across two different pane sizes; a
  small image is not upscaled; the zoom chord changes the drawn size; a broken file shows the
  notice **instead of** the image.
- *Mutation check:* remove the aspect-fit clamp → the ratio test must fail.

---

### WP4 — Close the annotation gap that makes features untestable

**Why:** this is the direct answer to "I shouldnt be finding these bugs". The Show-diff defect
was invisible to every unit test because the affordance did not exist on screen; only a test
that asks the built tree "is there a button here, and does clicking it reach Rust" sees that
class of bug. **A control with no accessible name cannot be asked about.** Five annotation
blocks have landed; the audit below is what remains.

Raw `TouchArea`s (excluding `TipArea`, which carries its name centrally from `ui/tooltip.slint`
and already names 42 controls):

| File | raw TouchArea | TipArea | `accessible-role` |
|---|---|---|---|
| `overlays.slint` | 14 | 3 | 7 |
| `sidebar.slint` | 11 | 11 | 4 |
| `newgoal.slint` | 7 | 2 | 4 |
| `newpane.slint` | 6 | 2 | 2 |
| `paneview.slint` | 5 | 1 | **0** |
| `askbrowser.slint` | 4 | 0 | 2 |
| `confirmclose.slint` | 4 | 0 | 2 |
| `leftpanel.slint` | 3 | 12 | **0** |
| `addproject.slint` | 3 | 0 | 1 |
| `contextmenu.slint` | 3 | 2 | 1 |
| `app.slint` | 2 | 2 | **0** |
| `topbar.slint` | 2 | 7 | **0** |

**WP4a — `leftpanel.slint` rows (the known, specified gap).** `FileRowView` (line ~1021, its
`TouchArea` at ~1083) and `GitRowView` (~1103, `ta` at ~1152) are the two rows the Show-diff
defect lived among, and neither is reachable. Annotate: `accessible-role: list-item`, label from
`root.item.label`, `accessible-description` from `root.item.detail`,
`accessible-item-selectable` / `-selected`, `accessible-expandable` / `-expanded` for
directories (`kind == 0`), `accessible-enabled: root.item.kind != 2`, and
`accessible-action-default => { root.clicked(); }`. Then tests that click a file row, a
directory row, and a git row and assert the command each reaches.

**WP4b — the remaining raw `TouchArea`s.** Walk the table above, top to bottom. For each: decide
whether it is an affordance (needs a name and a test) or pure scaffolding (a scrim, a
click-away catcher, a drag surface — needs a one-line comment saying so and nothing else).
Known scaffolding, to be confirmed not blindly trusted: `app.slint` scrims at ~536/~545,
`contextmenu.slint` `away` at ~412, `addproject.slint` blockers at ~90/~104, `topbar.slint`
`drag` at ~432.

**The recurring defect to look for**, which every previous block found: a control whose visible
name is a *sibling or child* `Text`. Slint auto-labels that `Text`, so the surface looks named
while the element carrying the `TouchArea` announces nothing. Grep symptoms: a `TouchArea` on a
`Rectangle` whose only `Text` is a child; N copies of one control sharing a generic tip; a row
of icons differing only by colour.

**Also required:** the annotated element must be the element a click can reach. A `PrefToggle`
annotated on its track passed `only()` and then failed the click, because the pressable element
was the row. Put the annotation where the `TouchArea` is.

---

### WP5 — The proof pass

**Why:** WP1–WP4 test what they build. "Ensure all features are tested end to end" is a claim
about the *whole* app, and nobody can currently answer it, because there is no artifact that
says what the features are. This work package produces that artifact and then closes what it
exposes.

**5a. Inventory.** Build `docs/feature-test-matrix.md`: one row per user-visible feature,
derived from three sources cross-checked against each other — every `callback` in `ui/*.slint`
that reaches Rust, every `Command` variant in `src/command.rs`, and every entry in
`src/keybindings.rs`. For each: the surface that triggers it, the Rust that answers it, the
test that proves it, and a verdict of **proven** (an end-to-end test drives the real tree),
**unit-only** (the logic is tested, the affordance is not — the Show-diff failure mode), or
**unproven**.

The three sources must be cross-checked because each misses a different thing: a callback with
no `Command` is UI-local state, a `Command` with no callback is reachable only by keybinding or
control plane, and a keybinding with neither is dead.

**5b. Close the gaps.** Work the matrix from unproven upward. Expect the bulk to be
**unit-only** — that is precisely the category the standing complaint is about.

**5c. Guard against regression.** Add a test that fails when a new `Command` variant or a new
`.slint` callback appears without a matrix entry, in the same spirit as
`the_is_view_flag_matches_the_kind_it_claims`. A matrix that is not enforced is a document that
rots; this is the difference between fixing the problem once and fixing it.

**Known already-recorded gaps to fold in:** talk + TTS are **faked, never live-verified**; two
crash defects are open (see the `avada-open-items` memory). Neither is in scope to *fix*
here, but both belong in the matrix with an honest verdict rather than being quietly omitted.

---

## 6. Sequencing

WP1 → WP2 → WP3 → WP4 → WP5, one commit each, each pushed before the next begins.

WP2 and WP3 are independent of WP1 and could be reordered; WP1 goes first because it is the
largest category by two orders of magnitude and because its collapse-state problem is the only
genuine architecture question in the document — if it forces a change to the projection model,
that change should land before two more panes are built on the old one.

WP4 could precede WP1–3 and there is an argument for it (it is the direct answer to the standing
complaint). It is placed after because the three previews each add controls that need naming,
and doing the audit before them guarantees a second audit afterwards.

WP5 is last because it inventories a moving target; running it before the previews land means
inventorying an app that is about to change.

---

## 7. The bar for "tested and proven"

A work package is complete when all six hold:

1. **The feature is reachable in a test the way a user reaches it** — through the real component
   tree, by accessible name, with a real click. Not by calling the Rust function directly, and
   not via `invoke_accessible_default_action()`, which passes just as happily for a control laid
   out to zero size.
2. **Both outcomes are asserted** — the thing appears *and* the thing that should not appear
   does not. A test that only ever asserts presence cannot see a branch that fires always.
3. **Every new assertion has been observed failing.** Revert the fix or break the feature, run
   the test, confirm it fails *with a message that names the actual problem*, restore. This is
   not optional: the pane zoom chord shipped broken in `d3808d0` past five tests that all
   measured the row's height rather than the text's, and the only reason the source viewer's
   equivalents are trusted is that both were mutation-checked and both failed correctly
   (`the line-number gutter ignored the zoom: 34 → 34`).
4. **The degraded path is tested, not just the happy one** — malformed input, an empty file, a
   file that vanishes mid-render, a permission error. Each must produce a `NOTICE` the user can
   read, not a blank pane and not a panic.
5. **Both suites are green**, and a fresh count is recorded in the commit.
6. **The commit message says what was wrong**, in prose, in the past tense — the house style
   throughout this repo. A defect found on the way gets its own paragraph.

---

## 8. Open questions, and one gap outside this plan

**8.1 — HTML has no viewer and is the fourth-largest category (28,550 files).** It falls between
the panes: `highlight` has an HTML syntax so `.html` routes to the source viewer today, which is
defensible for a template but wrong for a rendered document. There is also already a Browser
pane (kind 5) that could render it. **Not scoped here** — it needs a decision from Bert on which
of the two a double-clicked `.html` should open, and whether that should be a preference.

**8.2 — WP1's YAML/TOML dependency.** Resolve from `Cargo.lock` before writing code (§5, WP1).

**8.3 — WP3's row model.** Recommendation (b) — a sibling branch, not a role — is stated but not
binding; it should be confirmed against Slint's `Image` API at the pinned rev before committing
to it.

**8.4 — Collapse-state persistence.** Recommended **no** for v1. Reversible either way; record
the decision in the WP1 commit message.

---

## 9. Deferred: the editor

The original request asked for "a preview tool and a editor tool for each". Only the previews
are planned here. The recommendation, unchanged and still awaiting a decision:

**One shared editable text buffer, not one editor per type.** Editing is a single problem —
cursor, selection, undo, dirty state, save, external-change detection, conflict resolution —
and it is the same problem for JSON as for Rust. Per-type editors would mean five copies of it.
The per-type work that genuinely differs (syntax colouring, tree folding, table cells) is
already the preview layer built by WP1–WP3, and a shared buffer can adopt it.

This is a materially larger piece of work than all of WP1–WP5 combined, and it is the point at
which the app stops being a terminal multiplexer with viewers and starts being an editor. It
should be decided on its own merits, not inherited from a sentence about file types.
