# Remote panes — report

Shipped: `de29864` on origin/main (signed, Bert Shuler, no AI attribution).

Small extension of the existing persisted meta/snapshot path, not a new subsystem: `remote.host/tmux/cwd/cmd/launch`
ride the same meta bag as `origin.*`; restore re-runs the stored `ssh … tmux new-session -A` line.

## Verified
- Core on hp2: `remote-cargo test -p avada-core --lib` 2046 passed, 0 failed; after rebase `pane_remote pane_origin new_pane` 23 passed.
- App on Mac: compiled; `remote_pane_reconnect_facts_survive_a_relaunch` passed.

## NEEDS HUMAN (pending Bert)
- Live-tab test: create a throwaway remote pane (`avada ctl` new-pane with `--remote hp2` and a `--why`) in a new tab and
  check the `[ssh user@host]` title and `ctl info`. Not done: it would take keyboard focus from Bert.
- Reconnect-on-restart: only proves out on Bert's next manual relaunch (relaunching Avada is barred for agents).
  Running lanes are not tmux-backed until relaunched.

## Cleanup (2026-10-14)
- `rs/crates/app/target/` (regenerable Mac build output, per BUILD-POLICY.md) was already absent when I went to delete it,
  so nothing was removed there and I can't quote space freed. Disk afterwards: 66G free (86% used).
- Removed my private cargo-home symlink dir contents under `/private/tmp/claude-501/ch` (links only; `~/.cargo/registry` intact).
- No further Mac builds; any later build/test goes through `remote-cargo` on the hp hosts.

## Live proof (2026-10-15, installed Avada 0.2.48, Bert away)
- `ctl new-pane --window 0 --remote hp2 --cmd 'bash -l' --label remote-demo2` → pane 661edf3b. `ctl info`: "Remote: runs on hp2 over ssh (first hop)", "tmux session: avada-remote-demo2-be8818 — recoverable after a restart", "Remote command: bash -l".
- `ctl read --screen`: `bshuler@hp2:~$`. On hp2, `tmux ls`: `avada-remote-demo2-be8818: 1 windows … (attached)`.
- `ctl state`: persisted `command` is the ssh+`tmux new-session -A` launch line; meta carries `remote.cmd`, `remote.tmux`, `remote.host`, `remote.launch`.
- Bug found live and fixed: bare `--remote hp2` with no `--cmd` opened a LOCAL zsh with no tmux (pane remote-demo, f11bfe65). Fix: a bare `--remote HOST` now runs `${SHELL:-bash} -l` in tmux; new unit test; core `pane_remote` tests 11/11 on hp2. Needs the next app build to take effect live.
- Not seen by me: the `[ssh user@host]` title-bar text itself (ctl shows no rendered title) and reattach after restart. Both pending Bert.
- Two demo panes are left open in the Open Questions tab (remote-demo = the buggy local one, remote-demo2 = the working one) for Bert to inspect/close.
