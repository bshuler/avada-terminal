#!/usr/bin/env bash
# Compile-check and clippy the Rust crates for Windows from a Mac, without a
# Windows machine and without CI. Uses the windows-gnu target through mingw-w64.
#
# Why this exists: the GitHub "Test (Rust native)" workflow has been disabled
# since 2026-08-29, so nothing else catches a `#[cfg(windows)]` arm that does not
# compile before it lands in a release (7f434e0 broke v0.2.0 exactly that way).
#
# One-time setup:
#   brew install mingw-w64
#   rustup target add x86_64-pc-windows-gnu
#
# Usage:
#   scripts/check-windows.sh            # core + module-sdk (offline) and the app crate
#   scripts/check-windows.sh --no-app   # skip the app crate (needs network the first time)
#
# The msvc target is not used: `ring` needs the Windows SDK, which only comes
# through `xwin --accept-license`, and accepting that licence is a human decision.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
mingw_root="${MINGW_ROOT:-/opt/homebrew/opt/mingw-w64/toolchain-x86_64/x86_64-w64-mingw32}"
target=x86_64-pc-windows-gnu
with_app=1
[[ "${1:-}" == "--no-app" ]] && with_app=0

for tool in x86_64-w64-mingw32-gcc x86_64-w64-mingw32-g++ x86_64-w64-mingw32-ar; do
  command -v "$tool" >/dev/null || { echo "missing $tool: brew install mingw-w64" >&2; exit 2; }
done
rustup target list --installed | grep -qx "$target" || { echo "missing target: rustup target add $target" >&2; exit 2; }
[[ -d "$mingw_root/include" ]] || { echo "mingw headers not at $mingw_root (set MINGW_ROOT)" >&2; exit 2; }

# whisper-rs-sys runs bindgen at build time; without the mingw include path it
# reads the Mac headers and fails with "attempt to compute 12_usize - 16_usize".
export CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc
export CXX_x86_64_pc_windows_gnu=x86_64-w64-mingw32-g++
export AR_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ar
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc
export BINDGEN_EXTRA_CLANG_ARGS_x86_64_pc_windows_gnu="--target=x86_64-w64-mingw32 -I$mingw_root/include"

status=0
echo "=== core + module-sdk: clippy --target $target"
( cd "$root/rs" && cargo clippy --target "$target" -p avada-core -p avada-module-sdk --all-targets -- -D warnings ) || status=1
echo "CORE_WIN_EXIT=$status"

if (( with_app )); then
  app=0
  echo "=== app: clippy --target $target"
  ( cd "$root/rs/crates/app" && cargo clippy --target "$target" --bins -- -D warnings ) || app=1
  echo "APP_WIN_EXIT=$app"
  (( app == 0 )) || status=1
fi

if (( status == 0 )); then echo "WINDOWS_CHECK_GREEN"; else echo "WINDOWS_CHECK_RED" >&2; fi
exit $status
