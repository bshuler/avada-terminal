#!/usr/bin/env bash
# Run scripts/check-windows.sh on a Linux build host (default hp2) inside the
# scripts/windows-check.Dockerfile image, so the windows-gnu target dir (~10 GB)
# never lands on the Mac's disk. Prints the same *_EXIT / WINDOWS_CHECK_* lines.
#
# Needs ~/.local/bin/remote-cargo (it copies the tree git sees to the host) and
# docker on the host. Target dir and cargo registry live in docker volumes
# avada-win-target and avada-win-registry on the host.
#
# Usage:
#   scripts/check-windows-remote.sh            # same arguments as check-windows.sh
#   REMOTE_CARGO_HOST=hp1 scripts/check-windows-remote.sh --no-app
set -euo pipefail

host="${REMOTE_CARGO_HOST:-hp2}"
root="$(cd "$(dirname "$0")/.." && pwd)"
# Same remote layout as remote-cargo: src/<checkout>-<hash>, so the copy is shared.
key="$(basename "$root")-$(printf '%s' "$root" | shasum | cut -c1-8)"

# remote-cargo copies the tree, then runs `cargo --version` there.
( cd "$root" && remote-cargo --host "$host" --version ) >/dev/null
args=$(printf '%q ' "$@")
exec ssh -T -o BatchMode=yes "$host" "
  set -e
  src=\$HOME/remote-cargo/src/$key
  docker build -q -t avada-windows-check -f \$src/scripts/windows-check.Dockerfile \$src/scripts >/dev/null
  exec nice -n 10 docker run --rm \
    -v \$src:/src -v avada-win-target:/target -v avada-win-registry:/usr/local/cargo/registry \
    avada-windows-check bash scripts/check-windows.sh $args"
