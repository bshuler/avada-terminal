# Plan: relative path links prefer a known project root, then the screen

Status: plan only, nothing executed. Written 2026-09-07 against `main` at 38186fa.

## 1. Goal

A relative path token on screen (`docs/language/sql/35-gaps.yaml`) that the pane's own
cwd and its own repository cannot place is resolved, in this order:

1. the pane's cwd (unchanged)
2. the pane's own repository, one unique `git ls-files` match (unchanged, `elsewhere`)
3. **NEW: the app's remembered project roots** (the sidebar's projects list)
4. the rooted directories printed above the row, nearest first (unchanged,
   `relative_to_screen`)

Decision from Bert, 2026-09-07: "prefer the project root, then screen text".

## 2. Where things are today

| Concern | Location |
|---|---|
| Resolver chain | `rs/crates/terminal-widget/src/pane.rs:538-556` (`locate`) |
| Repo fallback | `pane.rs:854` (`elsewhere`, cache `found`) |
| Screen fallback | `pane.rs:897` (`relative_to_screen`), `rooted_bases` at 945, caps at 879/883 |
| Pane caches | `pane.rs:55-75` (`verified`, `commits`, `found`), all cleared in `set_cwd` at 365 |
| Pane constructors | `pane.rs:220` (`new`), 241 (`with_scrollback`) |
| Path resolver | `rs/crates/core/src/paths.rs:126` (`resolve_path(cwd, token) -> ResolveResult`) |
| Project model | `rs/crates/core/src/persistence/projects.rs:25` (`Project { id, path, name, color, last_opened_at }`, `path` is the normalized git root) |
| Project list (newest first, prunes dead dirs) | `rs/crates/app/src/sidebar.rs:25` (`list()`) |
| App copy of the list | `rs/crates/app/src/state.rs:1443` (`State::projects`) |
| List refresh sites | `state.rs:3591` (`note_pane_cwd`), 3732 (`open_new_goal`), 5228 (`toggle_projects`), 5448 (`refresh_projects`), 5578 (`set_project_color`), 5614 (`rename_project`), 5625 (`remove_project`), 5655 (`submit_add_project`); initial load at 1721 in the `State` constructor |
| cwd → pane push | `state.rs:3565` (`set_pane_cwd`), `rs/crates/app/src/app.rs:1781` (`SessionEvent::Cwd`) |
| Pane construction in app | `state.rs:2154` (`make_pane`), 2471, 6695, 6780, 8100 (`make_pane_from_spec`), all `TerminalPane::with_scrollback`; 1710 (preview pane) |
| cwd → project matcher | `state.rs:474` (`goal_project_for_cwd`, longest matching root wins) |
| Existing context tests | `pane.rs:2779-2895` (`context_fixture` + four tests) |

The widget crate knows nothing about projects. That is the one real design constraint: the
list has to be *pushed* into each pane, the way cwd already is.

## 3. Design decisions

**D1. Roots are pushed, not pulled.** `TerminalPane::set_project_roots(Vec<String>)`,
mirroring `set_cwd`. The widget stays free of the persistence layer and stays testable with
a plain `Vec`.

**D2. A project root answers only when exactly one does.** `README.md` exists in every
project; with the pane elsewhere and nothing printed above, linking it to whichever project
happens to be newest is a wrong link, and a wrong link is worse than none. So step 3
collects every root under which the token exists and accepts the answer only when that set
has one member. Two or more members fall through to step 4, where a printed `cd` can break
the tie. This mirrors `git::find_in_repo`, which declines when more than one file matches.

**D3. Order among roots does not matter for correctness** (uniqueness makes it
order-independent), so no recency sort and no "pane's own project first" logic. The pane's
own project is already covered by steps 1 and 2.

**D4. Cost bound.** One `resolve_path` (one `stat`) per root, capped at
`PROJECT_ROOT_STATS = 64` roots. The list is short in practice; the cap is a backstop.

**D5. Cache behavior.** Hits land in `verified` exactly like the other steps. Misses are
never cached (a project added later must light the token on the next hover).
`set_project_roots` clears `verified` when the set actually changes, because a token
answered earlier by the screen may now have a project answer that should win. `commits` and
`found` are untouched: they key on cwd, which did not move.

**D6. Not in scope.** Control-mode headless panes (`control_mode_cli.rs:423`) do no
linkification and get no roots. The preview pane at `state.rs:1710` gets none either.
The screen fallback's own policy (nearest rooted token, then its ancestors) is unchanged.

## 4. Steps

### Step 1. terminal-widget: state and setter (`pane.rs`)

1. Add a field after `found` (line ~75):
   ```rust
   /// The app's remembered project roots (absolute, normalized), tried as bases for a
   /// relative token after the pane's cwd and its own repository could not place it and
   /// before the screen is read. Pushed by the app whenever its project list changes;
   /// the widget never reads the store itself.
   project_roots: Vec<String>,
   ```
2. Initialize `project_roots: Vec::new()` in `with_scrollback` (line ~249); `new` delegates.
3. Add the setter next to `set_cwd` (line ~373):
   ```rust
   /// Replace the project roots. A changed set drops the verify cache: a token the
   /// screen answered earlier may now have a project answer, and the policy says the
   /// project wins.
   #[tracing::instrument(level = "debug", ret, skip(self))]
   pub fn set_project_roots(&mut self, roots: Vec<String>) {
       if roots != self.project_roots {
           self.project_roots = roots;
           self.verified.clear();
       }
   }
   ```

### Step 2. terminal-widget: the resolver step (`pane.rs`)

1. Add next to the context caps (line ~883):
   ```rust
   /// The stat budget for the project-root step: one per root, so a pathological
   /// project list cannot turn a hover into a disk walk.
   const PROJECT_ROOT_STATS: usize = 64;
   ```
2. Add the function between `elsewhere` and `relative_to_screen`:
   ```rust
   /// The project-root fallback for a relative path that neither the pane's cwd nor its
   /// repository could place: every remembered project is tried as a base, and the
   /// answer counts only when exactly one project holds the file. `README.md` is in all
   /// of them, and a link that opens the wrong project's copy is worse than no link, so
   /// a tie is handed on to the screen, where a printed `cd` can settle it.
   #[tracing::instrument(level = "debug", ret, skip(self))]
   fn relative_to_project(&self, token: &str) -> Option<ResolveResult> {
       let chars: Vec<char> = token.chars().collect();
       if is_path_root(&chars) {
           return None;
       }
       let mut hit: Option<ResolveResult> = None;
       for root in self.project_roots.iter().take(Self::PROJECT_ROOT_STATS) {
           let r = paths::resolve_path(Some(root), token);
           if r.exists {
               if hit.is_some() {
                   return None; // two projects answer: not ours to guess
               }
               hit = Some(r);
           }
       }
       hit
   }
   ```
3. Insert the step into `locate` between the `elsewhere` arm and the
   `relative_to_screen` arm (line ~551):
   ```rust
   } else if let Some(found) = self.relative_to_project(&cand.path) {
       // Inside exactly one remembered project. Tried before the screen on purpose:
       // a project the app knows is stronger evidence than a directory that happened
       // to be printed above, and it holds even after that line scrolls away.
       self.verified.insert(key, found.clone());
       found
   } else if let Some(found) = self.relative_to_screen(row, &cand.path) {
   ```
4. Update the doc comment on `relative_to_screen` (line ~886): it is now the fallback
   "that neither the pane's cwd, its repository, nor a remembered project could place".

### Step 3. terminal-widget: tests (`pane.rs`, after `a_path_inside_the_project_names_the_project_too`)

Extend `context_fixture` to also create `proj2/docs/language/sql/35-gaps.yaml` and
`proj2/README.md`, `proj/README.md` (the existing four tests do not look at `proj2`, so
they are unaffected). New tests:

- `a_remembered_project_places_a_path_the_pane_cannot`
  cwd = `elsewhere`, nothing printed above, `set_project_roots(vec![proj])`, hover
  `docs/language/sql/35-gaps.yaml` → hit, `abs_path == proj/docs/…`.
- `a_remembered_project_wins_over_a_directory_printed_above`
  screen: `cd {proj2}` then the path; roots = `[proj]`; both hold the file → `abs_path`
  is under `proj`, not `proj2`. This is the stated policy, pinned.
- `two_projects_holding_the_path_leave_it_to_the_screen`
  roots = `[proj, proj2]`, screen `cd {proj2}` above `README.md` → `abs_path` under
  `proj2` (the screen broke the tie). Then with no `cd` above → `link_at` is `None`.
- `a_project_added_later_lights_a_path_that_was_dark`
  no roots, hover → `None`; `set_project_roots(vec![proj])`, hover → hit (proves misses
  are not cached).
- `changing_the_project_roots_forgets_a_screen_answer`
  screen `cd {proj2}` above the path, no roots → hit under `proj2` (cached). Then
  `set_project_roots(vec![proj])` → hit under `proj` (cache was dropped, project wins).
- `a_rooted_token_never_asks_the_projects` — `/usr/bin/env` with roots set: the
  function returns `None` (unit-call `relative_to_project` directly; it is private, the
  test module is inside the crate).

Run: `cd rs/crates/terminal-widget && cargo test --lib pane::` (expect 5 existing context
tests + 6 new; whole lib currently 154 passing).

### Step 4. app: one refresh seam, push to every pane (`state.rs`)

1. Add a helper near `refresh_projects` (line ~5445):
   ```rust
   /// Reload the remembered projects and hand every pane the roots, so a relative path
   /// a pane cannot place on its own can be placed inside a project the app knows.
   /// The single seam for the list: the eight sites that used to assign
   /// `self.projects` directly go through here so no pane is left with a stale set.
   #[tracing::instrument(level = "debug", skip(self))]
   fn reload_projects(&mut self) {
       self.projects = sidebar::list();
       let roots: Vec<String> = self.projects.iter().map(|p| p.path.clone()).collect();
       for t in &mut self.tabs {
           for p in &mut t.panes {
               p.pane.set_project_roots(roots.clone());
           }
       }
   }
   ```
2. Replace the eight `self.projects = sidebar::list();` assignments at 3591, 3732, 5228,
   5448, 5578, 5614, 5625 and 5655 with `self.reload_projects();` (`refresh_projects`
   keeps its `dirty = true`). The one at 1721 is the struct literal in the `State`
   constructor and stays as it is: no panes exist yet. Check
   `grep -n 'sidebar::list()' rs/crates/app/src/state.rs` afterwards: the only calls
   left are the constructor's and the one inside `reload_projects`.
   `remove_project` matters most: a root that leaves the list must stop answering, and
   `set_project_roots` sees the smaller set and drops `verified` for it.
3. New panes: in `make_pane` (line 2056, the constructor at 2154),
   `make_pane_from_spec` (7850, the constructor at 8100) and the other three
   `TerminalPane::with_scrollback` sites (2471, 6695, 6780), call
   `pane.set_project_roots(self.project_roots())` right after construction, where
   `fn project_roots(&self) -> Vec<String>` is the one-line map over `self.projects`
   that `reload_projects` also uses. Skip the preview pane at 1710 (D6).
   Confirm with `grep -n 'with_scrollback' rs/crates/app/src/state.rs` that every
   construction site is followed by the call.

### Step 5. app: tests (`state.rs`)

Add a `mod project_roots_tests` next to `new_pane_cwd_tests` (line 736), following its
fixture style:

- `reloading_projects_hands_every_pane_the_roots` — build a `State` with two panes,
  point the projects store at a temp dir (see how `goal_defaults_tests` or
  `new_pane_cwd_tests` isolate persistence; if `sidebar::list()` cannot be redirected,
  test the pure half instead: extract `fn roots_of(projects: &[Project]) -> Vec<String>`
  and assert on it, and assert the pane-sweep by calling `set_project_roots` through a
  public-in-crate helper).
- `a_new_pane_starts_with_the_current_roots` — `add_pane` after `reload_projects`;
  observe through a link test: feed `docs/x.md` into the new pane with cwd elsewhere and
  a temp project root holding `docs/x.md`, `link_at` → hit.

Run: `cd rs/crates/app && cargo test --bin avada project_roots` then the full
`cargo test --bin avada` (625 passing before this work).

### Step 6. Verify the whole

```bash
cd /Users/bshuler/code/hyperpanes/rs/crates/core && cargo test
```
```bash
cd /Users/bshuler/code/hyperpanes/rs/crates/terminal-widget && cargo test --lib
```
```bash
cd /Users/bshuler/code/hyperpanes/rs/crates/app && cargo test --bin avada && cargo build
```

Live check (do not install to /Applications unless asked; run the built binary from
`target/`): in a pane whose cwd is `~`, with `~/code/hyperpanes` in the sidebar, print
`echo rs/crates/core/src/git.rs` and hover it. Expect an underline and a tooltip naming
`/Users/bshuler/code/hyperpanes/rs/crates/core/src/git.rs`. Then print
`echo README.md` with two projects that both have one: expect dark. Then
`cd /Users/bshuler/code/hyperpanes && echo README.md` in the same pane: expect the
avada copy (the screen broke the tie).

### Step 7. Commit

Another session is editing this checkout (file-viewer highlighting: `highlight.rs`,
`viewpane.rs`, `Cargo.toml`, and hunks in `state.rs` / `paneview.slint`). Before staging:

1. `git status` fresh; do not trust the session-start snapshot.
2. Stage by hunk in `state.rs` (`git diff -U0`, filter to this work's hunks,
   `git apply --cached`), whole-file for `pane.rs`.
3. Verify the staged subset alone: `git worktree add --detach <scratch> HEAD`, apply
   `git diff --cached`, run Step 6 there with
   `CARGO_TARGET_DIR=/Users/bshuler/code/hyperpanes/rs/crates/app/target`
   (about 10 minutes). Never `git stash` in this tree.
4. Commit as `Bert Shuler <BertShuler@proton.me>`, signed, no AI attribution, push to
   `main` directly. Suggested message:

   ```
   feat(links): a relative path is placed in a remembered project before the screen is read

   `docs/x.yaml` with the pane standing elsewhere used to depend on a `cd` line
   still being on screen. The app's project list is stronger evidence and does
   not scroll away, so it is tried first — and only when exactly one project
   holds the file. `README.md` is in all of them; a tie goes to the screen,
   where a printed `cd` can settle it, and otherwise stays dark.

   Roots are pushed into each pane (`set_project_roots`) from the one seam
   that reloads the list, and handed to every new pane at construction.
   ```

## 5. Risks and what to watch

- **A pane's own project through the wrong door.** If `elsewhere` (git ls-files) misses
  because the file is untracked, step 3 finds it via the pane's own root. Correct, and
  cheaper than before.
- **Uniqueness starves common names.** `Cargo.toml`, `README.md`, `src/main.rs` stay
  dark unless the screen names a directory. That is the deliberate D2 trade; note it in
  the final report so it is not mistaken for a regression.
- **`verified` clears on every list reload.** `note_pane_cwd` fires on each `cd`; the
  set rarely changes, and `set_project_roots` compares before clearing, so the cache
  survives a reload that changed nothing.
- **Tests that count stats.** None exist for this path; the 64 cap has no test beyond
  compile. If one is wanted, feed 65 fake roots and assert the 65th is never tried
  (make the 65th the only one holding the file, expect `None`).

## 6. Estimate

About 120 lines of production code across two files, about 250 lines of tests, one
isolated verification build. Half a day including the live check.
