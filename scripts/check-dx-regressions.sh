#!/usr/bin/env bash
#
# scripts/check-dx-regressions.sh — regression gate for the goal-g2 DX fixes.
#
# Proves, against THIS checkout's code (never the installed /usr/bin/avada):
#   1. `avada --help` prints usage and exits 0 (and `--version` prints the version) —
#      previously both were swallowed by the single-instance gate: silent forward, exit 0.
#   2. A flat `newPane` spec (spawn fields at the top level instead of nested under "pane")
#      is rejected with 400 — previously it silently spawned the default shell in $HOME.
#   3. `build_env` never injects an empty `AVADA_CONTROL_FILE` — previously every
#      GUI-native pane got the var set to "".
#
# Usage: scripts/check-dx-regressions.sh
# Env:   CARGO_TARGET_DIR respected (defaults to each workspace's ./target).

set -euo pipefail
cd "$(dirname "$0")/.."

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "ok:   $*"; }

# --- 2 + 3: unit-level regression tests (fast, no GUI, no running app needed) --------------
(
  cd rs
  cargo test -p avada-core -- --exact \
    control::dispatch::tests::new_pane_with_flat_spec_fields_is_400 \
    control::dispatch::tests::new_pane_with_non_object_pane_is_400 \
    control::dispatch::tests::new_pane_with_empty_pane_object_still_spawns_default_shell \
    session::spawn::tests::build_env_omits_control_file_when_empty_string \
    session::spawn::tests::build_env_omits_control_file_when_none
) || fail "core regression tests (flat newPane 400 / empty control-file env)"
pass "flat newPane spec -> 400; empty AVADA_CONTROL_FILE never injected (unit tests)"

# --- 1: the built binary answers --help / --version ----------------------------------------
(cd rs/crates/app && cargo build --bin avada)
BIN="${CARGO_TARGET_DIR:-rs/crates/app/target}/debug/avada"
[ -x "$BIN" ] || fail "built binary not found at $BIN"

# Run from a neutral cwd so nothing about this repo influences the binary.
BIN_ABS="$(realpath "$BIN")"
HELP_OUT="$(cd /tmp && "$BIN_ABS" --help)" || fail "avada --help exited non-zero"
echo "$HELP_OUT" | grep -qi "usage" || fail "avada --help printed no usage text"
pass "avada --help prints usage, exit 0"

VER_OUT="$(cd /tmp && "$BIN_ABS" --version)" || fail "avada --version exited non-zero"
echo "$VER_OUT" | grep -q "avada" || fail "avada --version printed nothing useful"
pass "avada --version prints '$VER_OUT', exit 0"

echo "all DX regression checks passed"
