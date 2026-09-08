# Wave 1 — common briefing for every track agent

You are one of several agents executing Wave 1 of `docs/modules-fanout-plan.md` in
parallel. Read, in this order, before touching anything:

1. `docs/modules-fanout-plan.md` §2 (Decisions, binding), §6 (frozen files), §7
   (verification contract) and the Wave 1 table in §5.
2. `docs/module-contract.md` — the module author contract. The SDK crate
   `rs/crates/module-sdk` implements it; `cargo doc --offline -p avada-module-sdk --no-deps`
   renders the API. The SDK crate is FROZEN: use it, never edit it.
3. `docs/live-session-safety.md` — the sandbox rule.
4. Your own track section (given in your prompt).

## Where you are

You are in a git worktree of `/Users/bshuler/code/hyperpanes` (the main checkout). Never
edit files in the main checkout, only in your worktree. Absolute paths below are the main
checkout's; substitute your worktree root where a path refers to source. `pwd` drifts
between shell calls: always `cd` explicitly with an absolute path.

Three Cargo workspaces: `rs` (members `crates/core` = `avada-core`, `crates/module-sdk`),
`rs/crates/app` (its own workspace, crate `avada`, bin `avada`), `rs/crates/terminal-widget`
(its own workspace). Toolchain 1.96. **Work offline** (`--offline` on every cargo
command); the crates `keyring`, `pubgrub`, `serde_yaml` are NOT in the local registry —
do not use them, and never add a dependency (every Cargo.toml is frozen; file a request in
your report instead).

**Disk is tight (38 GB free) and builds are big. Share the main checkout's target dirs.**
Before every cargo command in a workspace, export the matching target dir:

```
# rs workspace (core, module-sdk):
export CARGO_TARGET_DIR=/Users/bshuler/code/hyperpanes/rs/target
# app workspace:
export CARGO_TARGET_DIR=/Users/bshuler/code/hyperpanes/rs/crates/app/target
# terminal-widget workspace:
export CARGO_TARGET_DIR=/Users/bshuler/code/hyperpanes/rs/crates/terminal-widget/target
```

Other agents build into the same dirs: cargo serialises on its build-dir lock, so a
command may sit "Blocking waiting for file lock" for minutes. That is normal; wait, do not
kill it and do not create a private target dir. Never run `cargo clean`.

## Verification (run all that apply to the workspaces you touched; none may regress)

```
cd <worktree>/rs && cargo test --offline --all --no-fail-fast -- --skip permissions
cd <worktree>/rs && cargo clippy --offline --all-targets -- -D warnings
cd <worktree>/rs && cargo fmt --all -- --check
cd <worktree>/rs/crates/app && cargo test --offline --bins            # 644 tests before Wave 1
cd <worktree>/rs/crates/app && cargo fmt --all -- --check
cd <worktree>/rs/crates/terminal-widget && cargo test --offline --lib # 154 before Wave 1
cd <worktree>/rs/crates/terminal-widget && cargo fmt --all -- --check
```

`--skip permissions` is mandatory: the `permissions::*` tests hang on this machine. The app
crate is not clippy-gated in CI (it has ~37 pre-existing clippy errors); keep clippy clean
in the files you own anyway. `cargo test --bins` does not rebuild `target/debug/avada`.
CI (`.github/workflows/verify.yml`) also `cargo check`s on Windows and Linux; keep every
`#[cfg(windows)]` / `#[cfg(unix)]` path compiling by reading it twice, since you cannot
cross-compile here.

The headless Slint harness lives in `rs/crates/app/src/uitest/mod.rs` (helpers `ui()`,
`window()`, `click`, `right_click`, `click_in_popup`, `by_label`, `by_id`, ...). Each track has
its own child file under `rs/crates/app/src/uitest/` that starts with `use super::*;` — put
your UI tests there, never in `mod.rs`. Every new control (button, row, adapter callback)
needs a harness test that drives it and asserts the effect; and for every new `Command`
variant or Slint callback, a test that fails if its arm is deleted.

Empirical checks: never launch the real GUI, never touch `/Applications`, never touch
`~/Library/Application Support/hyperpanes` or `.../avada`, never front a window or inject
input. Run CLI/headless binaries with `HOME=<scratch dir>` only. No secrets are needed
for this wave: never print, log, or write a token or key anywhere including tests; keystores
are mocked in tests.

## Ownership

Edit only the files your track section lists as owned. Frozen (orchestrator only): every
`Cargo.toml`/`Cargo.lock`; the whole `rs/crates/module-sdk` crate; every `mod.rs` under
`core/src` and `app/src` EXCEPT `mod.rs` files inside a directory you own outright; every
`lib.rs`, `main.rs`, `windows.rs`, `macos.rs`, `linux.rs`; `.github/workflows/*`,
`build/**`, `scripts/install-macos.sh`; `rs/crates/app/ui/types.slint` outside the adapter
block assigned to you (each block is fenced with a `// ---- track XX` comment; edit only
inside yours). Your track's entry points are already declared in the frozen files; if you
need anything more from a frozen file, write it down in your report and work around it
(a `pub` item in a file you own, a `#[cfg(test)]` shim) rather than editing it.

Do not read other agents' diffs or branches. If a seam with another track is unclear,
choose the simplest interface, document it in a doc comment on your side, and say so in
the report.

## Git

Commit early on your worktree branch, in small commits with messages that explain why.
Commit as `Bert Shuler <BertShuler@proton.me>` (`git -c user.name=... -c user.email=...` or
the repo's config, which is already set). End every commit message with the trailer line
`Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>`. Never `git stash`, never
`git add -A` / `git add .` — add by path. Never rewrite history that has been pushed.

When your track is complete and verified:

```
git fetch origin && git rebase origin/main    # resolve conflicts, re-run the checks
git push origin HEAD:main                     # retry the fetch/rebase/push loop if rejected
```

Push to `main` directly; no PR. The plan says "one track, one commit": squash your
worktree commits into one (`git reset --soft $(git merge-base HEAD origin/main)` then a
single commit) before the final push, with a commit body that reads as a changelog entry.

## Final report (required, this is what the orchestrator reads)

1. What landed: files, public items, and the seam interfaces you exposed or consumed.
2. What was verified and how: exact commands, test counts before and after.
3. What you need from a frozen file (exact diff or item).
4. What you left undone, and why.
5. The commit hash on `main`.

## Cargo hygiene (added after Batch 1)

- Run every cargo command in the **foreground** (Bash `timeout: 600000`). Never background a cargo run and end your turn waiting for it: the orchestrator cannot tell a waiting agent from a finished one.
- The shared `CARGO_TARGET_DIR` can leave a stale `avada-core` / `avada-module-sdk` rlib from another worktree (cargo does not hash a path package's location). If you see an error about a field or method that plainly exists on main, run `touch crates/core/src/lib.rs crates/module-sdk/src/lib.rs` in your worktree and rebuild.
- Two host tests (`module::host_tests::crash_restarts…`, `…never_says_hello…`) are slow under parallel load; if one fails in a full run, rerun it alone once before treating it as a regression.
