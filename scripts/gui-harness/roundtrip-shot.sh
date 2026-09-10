#!/usr/bin/env bash
# Runs INSIDE the harness container (scripts/gui-harness/Dockerfile).
#
# The §7.5 module round trip from docs/modules-fanout-plan.md, photographed: a real
# Avada window on a real X server installs the Files module from a git mirror, is
# relaunched, and the module's entry is on the left panel's rail; the module is then
# disabled in the workspace, the window relaunched again, and the entry is gone.
#
#   roundtrip-shot.sh [outdir]      default /work/shots
#
# It writes:
#   rt-1-fresh.png       the window before anything is installed
#   rt-2-installed.png   after install + relaunch: the Files entry is on the rail
#   rt-3-disabled.png    after disable + relaunch: the rail is back to rt-1
#   rt-N-left.png        the left panel band of each, the crop the pixel checks read
#   rt-report.txt        every PASS/FAIL line, plus the pixel counts
#
# Relaunching between steps is deliberate: the app decides which modules to start once,
# at launch, from the workspace state on disk (`ModuleRuntime::installs_to_start`); an
# enable or disable made through the control API is honoured at the NEXT launch. That is
# the product today, and the pictures show exactly that contract.
#
# Nothing touches the container's own state: the modules root, workspace state and
# config all sit under a fresh sandbox through the XDG variables (the app's Linux paths
# are $XDG_DATA_HOME/avada etc.), and the install reads the module's source from the
# bare mirror at /work/fixtures — only the module's own `avada-module-sdk` dependency
# is fetched from GitHub by cargo.
set -uo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="${AVADA_BIN:-$ROOT/rs/crates/app/target/release/avada}"
OUT="${1:-/work/shots}"
MIRROR="${AVADA_MODULE_MIRROR:-$ROOT/fixtures}"
MODULE="bshuler/avada-files"
WS_KEY="ws"     # the launch file is $STATE/ws.json, whose stem is the workspace key

[[ -x "$BIN" ]] || { echo "no binary at $BIN — build it first" >&2; exit 1; }
[[ -d "$MIRROR/$MODULE.git" ]] || { echo "no bare mirror at $MIRROR/$MODULE.git — ptah.sh roundtrip makes it" >&2; exit 1; }
for tool in git jq cargo rustup xdotool import convert compare pgrep; do
    command -v "$tool" >/dev/null || { echo "$tool is not on PATH" >&2; exit 1; }
done
mkdir -p "$OUT"; rm -f "$OUT"/rt-*.png "$OUT"/rt-report.txt
REPORT="$OUT/rt-report.txt"
FAILED=0
ok()   { echo "PASS: $1" | tee -a "$REPORT"; }
fail() { echo "FAIL: $1" | tee -a "$REPORT"; FAILED=$((FAILED+1)); }
check() { if [[ "$2" == "$3" ]]; then ok "$1"; else fail "$1 (want '$3', got '$2')"; fi; }

export DISPLAY="${DISPLAY:-:99}"
Xvfb "$DISPLAY" -screen 0 1600x1000x24 -nolisten tcp >/tmp/xvfb.log 2>&1 &
XVFB=$!
for _ in $(seq 1 50); do xdpyinfo >/dev/null 2>&1 && break; sleep 0.2; done
xdpyinfo >/dev/null 2>&1 || { echo "Xvfb never came up; see /tmp/xvfb.log" >&2; exit 1; }
openbox --sm-disable >/tmp/openbox.log 2>&1 &
OPENBOX=$!
for _ in $(seq 1 50); do
    xprop -root _NET_SUPPORTING_WM_CHECK >/dev/null 2>&1 && break
    sleep 0.2
done

STATE="$(mktemp -d /tmp/hp-roundtrip.XXXXXX)"
export XDG_DATA_HOME="$STATE/data" XDG_STATE_HOME="$STATE/state" XDG_CONFIG_HOME="$STATE/config"
export AVADA_CONTROL_FILE="$STATE/control.json"
export AVADA_MARKETPLACE_GIT_BASE="file://$MIRROR"
export AVADA_OPEN=leftpanel
# Debug for the host RPC module only, so its `host.fs.list` line lands in the app log: it
# names the directory the module listed and how many entries it found, which is how the
# run proves the file browser was given the workspace (a screenshot alone only proves that
# *something* drew). Not a blanket `debug`: thousands of functions are instrumented at that
# level and the flood kept the window from answering its control API in time.
export AVADA_LOG=info,avada_core::module::rpc=debug
# The mirror is bind-mounted from the host and owned by the host's user, while the
# container runs as root; git refuses such a repo ("dubious ownership") unless it is
# declared safe. The setting must come from global config: the git in this image
# (2.39) does not honour the GIT_CONFIG_COUNT environment form for safe.directory
# (tested; the upload-pack it spawns for a file:// URL still refuses). The container
# is thrown away after the run, so root's global config is harmless. `-C /` keeps
# git away from the synced tree's `.git` file, which names a gitdir that only exists
# on the host and makes every git command in /work fatal.
git -C / config --global --add safe.directory '*'
mkdir -p "$XDG_DATA_HOME" "$XDG_STATE_HOME" "$XDG_CONFIG_HOME"
MODULES_ROOT="$XDG_DATA_HOME/avada/modules"

# Everything the sandbox started carries its path on the command line or in its
# binary path: the app (its launch file), its session daemon (salted with the data
# dir) and the module child (installed under the modules root).
stop_all() {
    [[ -n "${APP:-}" ]] && kill "$APP" 2>/dev/null
    pkill -f -- "$STATE" 2>/dev/null
    for _ in $(seq 1 40); do pgrep -f -- "$STATE" >/dev/null 2>&1 || break; sleep 0.25; done
    pkill -9 -f -- "$STATE" 2>/dev/null
    APP=""
}
cleanup() {
    stop_all
    kill "${OPENBOX:-}" 2>/dev/null
    kill "$XVFB" 2>/dev/null
    rm -rf "$STATE"
}
trap cleanup EXIT

# A small project for the file browser to show. The pane starts two levels down and the
# `.git` marker sits at the top, so the root the module is handed has to come from the
# project-root walk, not from the pane's own directory or the launch file's.
PROJECT="$STATE/project"
mkdir -p "$PROJECT/src/nested" "$PROJECT/.git"
printf 'fn main() {}\n' > "$PROJECT/src/main.rs"
printf '[package]\nname = "demo"\n' > "$PROJECT/Cargo.toml"
printf '# demo\n' > "$PROJECT/README.md"
PROJECT_ENTRIES=4   # .git  Cargo.toml  README.md  src
cat > "$STATE/ws.json" <<JSON
{
  "groups": [
    { "title": "shell", "layout": "single",
      "panes": [ { "label": "shell", "command": "cat", "cwd": "$PROJECT/src/nested" } ] }
  ],
  "active": 0
}
JSON

ctl() { "$BIN" ctl "$@"; }
# Launch the window on the workspace file, wait for the control API and the first
# frame, then photograph it as $1.
launch_and_shoot() {
    local name=$1
    rm -f "$AVADA_CONTROL_FILE"
    "$BIN" "$STATE/ws.json" >>"$STATE/app.log" 2>&1 &
    APP=$!
    for _ in $(seq 1 100); do
        [[ -s "$AVADA_CONTROL_FILE" ]] && ctl health >/dev/null 2>&1 && break
        ps -p "$APP" >/dev/null 2>&1 || { echo "app died on launch:"; tail -30 "$STATE/app.log"; exit 1; }
        sleep 0.3
    done
    ctl health >/dev/null 2>&1 || { echo "control API never answered:"; tail -30 "$STATE/app.log"; exit 1; }
    WIN=""
    for _ in $(seq 1 60); do
        WIN="$(xdotool search --onlyvisible --class -- . 2>/dev/null | head -1)"
        [[ -n "$WIN" ]] && break
        sleep 0.25
    done
    [[ -n "$WIN" ]] || { echo "no window appeared" >&2; exit 1; }
    xdotool windowactivate --sync "$WIN" 2>/dev/null
    # Software Vulkan: the first frame lands well after the window maps, and a module
    # child has to handshake before its rail entry is drawn.
    sleep 5
    import -window "$WIN" "$OUT/$name.png" 2>/dev/null || xwd -id "$WIN" | convert xwd:- "$OUT/$name.png"
    # The left panel band: the rail strip with one button per module entry.
    convert "$OUT/$name.png" -crop '300x1000+0+0' +repage "$OUT/$name-left.png"
}
module_running() { pgrep -f -- "$MODULES_ROOT" >/dev/null 2>&1 && echo yes || echo no; }
pixels_differ() { compare -metric AE "$1" "$2" null: 2>&1 | awk '{print int($1)}'; }

# ---- 1. fresh: nothing installed, nothing on the rail ----
launch_and_shoot rt-1-fresh
check "toolchain is ready in the container" "$(ctl get /marketplace/toolchain | jq -r .ready)" true
check "nothing installed in a fresh sandbox" "$(ctl get /marketplace/installed | jq -c .modules)" '[]'
check "no module process before install" "$(module_running)" no

# ---- 2. install into the workspace, then relaunch ----
RESP="$(ctl post /marketplace/install "$(jq -nc --arg m "$MODULE" --arg w "$WS_KEY" '{module:$m, workspace:$w}')")"
JOB="$(echo "$RESP" | jq -r '.job.id // empty')"
[[ -n "$JOB" ]] || { fail "install accepted"; echo "$RESP"; exit 1; }
ok "install accepted as job $JOB"
PH=""
for _ in $(seq 1 3600); do
    J="$(ctl get "/marketplace/jobs/$JOB")"
    NEW="$(echo "$J" | jq -r .phase)"
    [[ "$NEW" != "$PH" ]] && { PH=$NEW; echo "   phase: $PH"; }
    [[ "$PH" == done || "$PH" == failed ]] && break
    sleep 0.5
done
if [[ "$PH" != done ]]; then
    fail "install job finished (phase $PH)"
    echo "$J" | jq -r '.error // empty'; echo "$J" | jq -r '.log_tail[]?' | tail -40
    exit $FAILED
fi
ok "install job finished"
check "installing into the workspace enables it there" \
    "$(ctl get /marketplace/installed | jq -r --arg m "$MODULE" --arg w "$WS_KEY" '.modules[] | select(.module == $m) | .enabled[$w]')" true
[[ -n "$(find "$MODULES_ROOT" -type f -perm -u+x -name 'avada-files*' 2>/dev/null | head -1)" ]] \
    && ok "the built module binary lives under the sandbox modules root" \
    || fail "the built module binary lives under the sandbox modules root"
stop_all
launch_and_shoot rt-2-installed
check "the module process is running after relaunch" "$(module_running)" yes
# Click the module's rail button (the strip's first button, top-left of the panel) and
# photograph what it projects: the file browser's rows replace the built-in sections.
xdotool mousemove --window "$WIN" 20 77 click 1
sleep 3
import -window "$WIN" "$OUT/rt-2b-files-open.png" 2>/dev/null || xwd -id "$WIN" | convert xwd:- "$OUT/rt-2b-files-open.png"
convert "$OUT/rt-2b-files-open.png" -crop '300x1000+0+0' +repage "$OUT/rt-2b-files-open-left.png"
D2B="$(pixels_differ "$OUT/rt-2-installed-left.png" "$OUT/rt-2b-files-open-left.png")"
[[ "$D2B" -gt 1000 ]] && ok "clicking the rail entry opened the module's file browser ($D2B px)" \
    || fail "clicking the rail entry opened the module's file browser ($D2B px)"
# What it drew: the host logs every directory a module lists. The module must have been
# given the project root (two levels above the pane) and found the whole tree there.
LISTED="$(cat "$XDG_STATE_HOME"/avada/logs/avada-*.log 2>/dev/null | grep -F 'host.fs.list' | grep -F "path=$PROJECT " | grep -oE 'entries=[0-9]+' | head -1)"
check "the file browser listed the project root the pane sits under" "$LISTED" "entries=$PROJECT_ENTRIES"
if [[ "$LISTED" != "entries=$PROJECT_ENTRIES" ]]; then
    echo "   host.fs.list lines in the app log:"
    cat "$XDG_STATE_HOME"/avada/logs/avada-*.log 2>/dev/null | grep -F 'host.fs.list' | sed 's/^/   | /' | tail -5
fi

# ---- 3. disable in the workspace, then relaunch ----
D="$(ctl post "/marketplace/modules/$MODULE/disable" "$(jq -nc --arg w "$WS_KEY" '{workspace:$w}')")"
check "disable reports the workspace off" "$(echo "$D" | jq -r --arg w "$WS_KEY" '.enabled[$w]')" false
stop_all
launch_and_shoot rt-3-disabled
check "no module process after disable + relaunch" "$(module_running)" no
check "the install record survives the disable" \
    "$(ctl get /marketplace/installed | jq -r --arg m "$MODULE" '.modules[] | select(.module == $m) | .active')" true

# ---- 4. the pictures: the rail changed on install and changed back on disable ----
D12="$(pixels_differ "$OUT/rt-1-fresh-left.png" "$OUT/rt-2-installed-left.png")"
D13="$(pixels_differ "$OUT/rt-1-fresh-left.png" "$OUT/rt-3-disabled-left.png")"
echo "left-panel pixels differing: fresh vs installed = $D12, fresh vs disabled = $D13" | tee -a "$REPORT"
[[ "$D12" -gt 200 ]] && ok "the left panel changed when the module was installed ($D12 px)" \
    || fail "the left panel changed when the module was installed ($D12 px)"
[[ "$D13" -lt "$D12" && "$D13" -le 200 ]] && ok "the left panel went back when the module was disabled ($D13 px)" \
    || fail "the left panel went back when the module was disabled ($D13 px)"

echo "==> shots in $OUT" | tee -a "$REPORT"
ls -1 "$OUT" | grep '^rt-'
if [[ "$FAILED" == 0 ]]; then echo "ROUNDTRIP_SHOT_GREEN" | tee -a "$REPORT"; else echo "ROUNDTRIP_SHOT_FAILED=$FAILED" | tee -a "$REPORT"; fi
exit $FAILED
