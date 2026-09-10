#!/usr/bin/env bash
# Run the core + module-sdk test suite and clippy, plus clippy on the GUI crate,
# on Linux inside Docker, from any machine. This is the local stand-in for the disabled GitHub test matrix.
#
# Usage:
#   scripts/check-linux-docker.sh          # build, test, clippy; blocks until done
#   scripts/check-linux-docker.sh --wait   # re-attach to a run that is still going
#
# Two named volumes make reruns fast: avada-linux-target (build output) and
# avada-cargo-registry (crate downloads). If a download is ever corrupted
# (e.g. "invalid gzip header" after a full disk), `docker volume rm
# avada-cargo-registry` and rerun; nothing in it is irreplaceable.
#
# The image (scripts/linux-ci.Dockerfile) builds whisper.cpp with clang and
# GGML_NATIVE=OFF: on an arm64 Docker host
# (Apple Silicon) GCC 12 rejects ggml's fp16 NEON intrinsics with "target
# specific option mismatch". x86_64 hosts never hit it; the setting is harmless there.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
name=avada-linux-test
image=avada-linux-ci
dockerfile="$root/scripts/linux-ci.Dockerfile"

if [[ "${1:-}" != "--wait" ]]; then
  # The image carries the apt packages and cargo components (see the Dockerfile);
  # docker caches it until that file changes, so this is a no-op on a rerun.
  docker build -q -t "$image" -f "$dockerfile" "$root/scripts" >/dev/null
  docker rm -f "$name" >/dev/null 2>&1 || true
  docker run -d --name "$name" \
    -v "$root:/src" \
    -v avada-linux-target:/target \
    -v avada-cargo-registry:/usr/local/cargo/registry \
    "$image" bash -c '
      set -o pipefail
      echo "=== TEST"
      cargo test --all --no-fail-fast -- --skip permissions 2>&1 \
        | grep -E "^(test result|error|warning: unused|failures:|    [a-z_:]+$|---- |thread |  left:|  right:|note: )|panicked at|assertion" | cut -c1-240
      echo "CARGO_TEST_EXIT=${PIPESTATUS[0]}"
      echo "=== CLIPPY"
      cargo clippy --all --all-targets -- -D warnings 2>&1 | grep -E "^(error|warning)" | cut -c1-200
      echo "CLIPPY_EXIT=${PIPESTATUS[0]}"
      echo "=== FMT"
      cargo fmt --all -- --check 2>&1 | head -40; echo "FMT_EXIT=${PIPESTATUS[0]}"
      echo "=== APP CLIPPY"
      # The GUI crate is its own workspace and is what ships on Linux; the image
      # already carries the Slint system libraries, so a compile check is cheap
      # next to the tests above. Tests stay on the GUI harness (scripts/gui-harness).
      cd /src/rs/crates/app
      cargo clippy --all-targets -- -D warnings 2>&1 | grep -E "^(error|warning)" | cut -c1-200
      echo "APP_CLIPPY_EXIT=${PIPESTATUS[0]}"
    ' >/dev/null
  echo "container $name started (image $image)"
fi

code=$(docker wait "$name")
docker logs "$name" 2>&1 | tail -80
log=$(docker logs "$name" 2>&1)
ok=1
for key in CARGO_TEST_EXIT CLIPPY_EXIT FMT_EXIT APP_CLIPPY_EXIT; do
  v=$(printf '%s\n' "$log" | grep -o "^$key=[0-9]*" | tail -1 | cut -d= -f2)
  [[ "$v" == "0" ]] || ok=0
done
if (( ok )); then echo "LINUX_CHECK_GREEN"; exit 0; fi
echo "LINUX_CHECK_RED (container exit $code)" >&2
exit 1
