#!/usr/bin/env bash
# Avada — drive the GUI harness on a remote Linux box (default: ptah).
#
#   scripts/gui-harness/ptah.sh sync    # push the working tree over
#   scripts/gui-harness/ptah.sh image   # build the harness image
#   scripts/gui-harness/ptah.sh build   # cargo build --release inside it
#   scripts/gui-harness/ptah.sh test    # run gui-test.sh inside it
#   scripts/gui-harness/ptah.sh shot    # photograph the window, fetch the PNGs
#   scripts/gui-harness/ptah.sh roundtrip # install/enable/disable the Files module, photographed
#   scripts/gui-harness/ptah.sh all     # sync, image, build, test, in order
#
# Why remote and not this Mac: the harness drives a GUI with synthetic mouse and
# keyboard events. On the local machine those land on whatever the human is
# doing. On a box with no seat they land nowhere but the Xvfb display the
# container just created, which is the entire point — see
# docs/live-session-safety.md.
#
# ptah is shared. Take a lease before a long run and release it after:
#   ssh ptah '~/.local/bin/ptah-lease acquire avada shared "gui harness"'
#   ssh ptah '~/.local/bin/ptah-lease release <token>'
set -euo pipefail

HOST="${AVADA_HARNESS_HOST:-ptah}"
REMOTE="${AVADA_HARNESS_DIR:-/home/ubuntu/avada-harness}"
IMAGE=avada-harness
REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"

# Named volumes, so a rebuild is minutes rather than the whole dependency tree
# (Slint comes from git and is not cheap).
CARGO_VOL=avada-cargo
TARGET_VOL=avada-target

note() { echo "==> $*"; }
rsh() { ssh -o BatchMode=yes "$HOST" "$@"; }

# --workdir /work with the tree bind-mounted, target/ on a volume so it survives.
docker_run() {
    rsh docker run --rm \
        -v "$REMOTE:/work" \
        -v "$CARGO_VOL:/usr/local/cargo/registry" \
        -v "$TARGET_VOL:/work/rs/crates/app/target" \
        -w /work "$IMAGE" "$@"
}

cmd_sync() {
    note "syncing $REPO_ROOT -> $HOST:$REMOTE"
    rsh "mkdir -p '$REMOTE'"
    # shots/ is an output the container writes as root; pushing our copy back into it
    # is both pointless and a permission error.
    rsync -a --delete \
        --exclude '.git/' \
        --exclude 'target/' \
        --exclude 'rs/packaging/out/' \
        --exclude 'node_modules/' \
        --exclude 'shots/' \
        "$REPO_ROOT/" "$HOST:$REMOTE/"
}

cmd_image() {
    note "building image $IMAGE on $HOST"
    rsh "docker build -t '$IMAGE' -f '$REMOTE/scripts/gui-harness/Dockerfile' '$REMOTE/scripts/gui-harness'"
}

cmd_build() {
    note "cargo build --release (app crate) inside $IMAGE"
    docker_run bash /work/scripts/gui-harness/in-container.sh build
}

cmd_test() {
    note "running gui-test.sh inside $IMAGE"
    docker_run bash /work/scripts/gui-harness/in-container.sh test
}

# The container writes into the bind-mounted tree, so the PNGs come back over the
# same path the sources went out on. They land in $SHOT_DIR locally (default
# ./shots, which is gitignored) — a rendering change is argued from these.
cmd_shot() {
    note "photographing the window inside $IMAGE"
    docker_run bash /work/scripts/gui-harness/in-container.sh shot /work/shots
    local dest="${AVADA_SHOT_DIR:-$REPO_ROOT/shots}"
    mkdir -p "$dest"
    note "fetching PNGs -> $dest"
    rsync -a "$HOST:$REMOTE/shots/" "$dest/"
    ls -1 "$dest"
}

# The §7.5 module round trip (docs/modules-fanout-plan.md), photographed. The module
# is installed from a bare mirror under fixtures/ (gitignored; rsync ships it) so the
# container never needs GitHub for the module itself. AVADA_FILES_SRC names a local
# checkout to mirror; otherwise the mirror is cloned from GitHub once and kept.
cmd_roundtrip() {
    local mirror="$REPO_ROOT/fixtures/bshuler/avada-files.git"
    if [[ ! -d "$mirror" ]]; then
        note "mirroring bshuler/avada-files -> $mirror"
        mkdir -p "$(dirname "$mirror")"
        git clone -q --bare "${AVADA_FILES_SRC:-https://github.com/bshuler/avada-files}" "$mirror"
    fi
    cmd_sync
    # Rebuild every time: the binary lives on the target volume, and a round trip run on
    # a stale one photographs last week's app with this week's checks. Incremental, so
    # an unchanged tree costs seconds.
    cmd_build
    note "module round trip inside $IMAGE"
    # `set -e` is on: keep a failed run's pictures reachable before reporting it.
    local rc=0
    docker_run bash /work/scripts/gui-harness/in-container.sh roundtrip /work/shots || rc=$?
    local dest="${AVADA_SHOT_DIR:-$REPO_ROOT/shots}"
    mkdir -p "$dest"
    note "fetching PNGs + report -> $dest"
    rsync -a "$HOST:$REMOTE/shots/" "$dest/"
    ls -1 "$dest" | grep '^rt-'
    return $rc
}

case "${1:-all}" in
    sync)  cmd_sync ;;
    image) cmd_image ;;
    build) cmd_build ;;
    test)  cmd_test ;;
    shot)  cmd_shot ;;
    roundtrip) cmd_roundtrip ;;
    all)   cmd_sync; cmd_image; cmd_build; cmd_test ;;
    *) echo "usage: ptah.sh [sync|image|build|test|shot|roundtrip|all]" >&2; exit 2 ;;
esac
