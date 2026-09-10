#!/bin/bash
# Module round trip, end to end, on this machine and without a display: an ISOLATED
# headless control server installs the Files module from a git mirror, builds it with the
# real toolchain, records it, and then enables and disables it in a workspace. This is
# the §7.5 check from docs/modules-fanout-plan.md, minus the pixels — the rail entry
# appearing and disappearing is photographed by scripts/gui-harness/roundtrip-shot.sh on
# a box with no seat, because a window on this Mac would land on the human's desk.
#
# Nothing here touches the developer's real state: HOME is a sandbox, so the modules
# root (~/Library/Application Support/avada/modules on macOS, $XDG_DATA_HOME/avada/modules
# on Linux) lands inside it. The install itself is pointed at a bare mirror of the module
# repo through AVADA_MARKETPLACE_GIT_BASE, so GitHub is only reached for the module's own
# dependency on the SDK (cargo fetches that into the real cargo home, which is kept).
#
#   scripts/module-roundtrip-demo.sh                # clones bshuler/avada-files to mirror
#   AVADA_FILES_SRC=/path/to/avada-files scripts/module-roundtrip-demo.sh   # mirror this checkout
#
# Exit status is the number of failed checks. The token never leaves the Authorization header.
set -u
REPO="$(cd "$(dirname "$0")/.." && pwd)"
A="${ROUNDTRIP_DEMO_DIR:-$(mktemp -d /tmp/avada-roundtrip.XXXXXX)}"
MODULE="bshuler/avada-files"
WS="demo-ws"
rm -rf "$A"; mkdir -p "$A/state/avada" "$A/config/avada" "$A/data" "$A/mirror/bshuler"
REAL_HOME="$HOME"
export CARGO_HOME="${CARGO_HOME:-$REAL_HOME/.cargo}"
export RUSTUP_HOME="${RUSTUP_HOME:-$REAL_HOME/.rustup}"
export HOME="$A/home"
mkdir -p "$A/home/Library/Application Support/avada"
FAILED=0
ok()   { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; FAILED=$((FAILED+1)); }
check() { if [ "$2" = "$3" ]; then ok "$1"; else fail "$1 (want '$3', got '$2')"; fi; }

for tool in git jq curl cargo rustup; do
  command -v "$tool" >/dev/null || { echo "FAIL: $tool not on PATH"; exit 1; }
done

# 1. The mirror: a bare clone (tags included) of either the given checkout or GitHub.
SRC="${AVADA_FILES_SRC:-}"
if [ -z "$SRC" ]; then
  SRC="$A/src"
  # The module repos are private: without a tty git cannot ask for a username, so
  # prefer `gh`, which reads the logged-in account's token itself and never prints it.
  if command -v gh >/dev/null 2>&1; then
    gh repo clone "$MODULE" "$SRC" -- -q || { echo "FAIL: clone $MODULE (gh)"; exit 1; }
  else
    git clone -q "https://github.com/$MODULE" "$SRC" || { echo "FAIL: clone $MODULE"; exit 1; }
  fi
fi
git clone -q --bare "$SRC" "$A/mirror/$MODULE.git" || { echo "FAIL: bare mirror"; exit 1; }
TAG=$(git -C "$A/mirror/$MODULE.git" tag | sort -V | tail -1)
COMMIT=$(git -C "$A/mirror/$MODULE.git" rev-list -n1 "$TAG")
VERSION=${TAG#v}
echo "== mirror $A/mirror/$MODULE.git at $TAG ($COMMIT)"

# Always build: a stale headless from before the marketplace was wired in answers 503.
(cd "$REPO/rs" && cargo build -q --locked -p avada-core --bin headless) || exit 1

CJ="$A/state/avada/control.json"
# AVADA_CONTROL_FILE leaks in from a pane's env and would clobber the LIVE app's discovery
# file — pin it into the sandbox. HOME is already the sandbox, so InstallPaths::host()
# resolves under it on macOS; the XDG dirs do the same on Linux.
env -u AVADA_PANE_ID \
  XDG_STATE_HOME="$A/state" XDG_CONFIG_HOME="$A/config" XDG_DATA_HOME="$A/data" \
  AVADA_CONTROL_FILE="$CJ" AVADA_MSG_NUDGE=0 \
  AVADA_MARKETPLACE_GIT_BASE="file://$A/mirror" \
  "$REPO/rs/target/debug/headless" > "$A/headless.log" 2>&1 &
HPID=$!
trap 'kill $HPID 2>/dev/null' EXIT

for i in $(seq 1 50); do [ -s "$CJ" ] && break; sleep 0.2; done
[ -s "$CJ" ] || { echo "FAIL: control.json never appeared"; cat "$A/headless.log"; exit 1; }
T=$(jq -r .token "$CJ"); P=$(jq -r .port "$CJ"); H=$(jq -r '.bindAddress // "127.0.0.1"' "$CJ")
hp() { local m=$1 p=$2 b=${3:-}; if [ -n "$b" ]; then curl -sS -m 30 -X "$m" "http://$H:$P$p" -H "Authorization: Bearer $T" -H 'content-type: application/json' -d "$b"; else curl -sS -m 30 -X "$m" "http://$H:$P$p" -H "Authorization: Bearer $T"; fi }
echo "== headless up on $H:$P (pid $HPID), sandbox $A"

# 2. The toolchain report says a free build can run here.
TC=$(hp GET /marketplace/toolchain)
check "toolchain is ready" "$(echo "$TC" | jq -r .ready)" true
[ "$(echo "$TC" | jq -r .ready)" = true ] || { echo "$TC" | jq -c .; exit 1; }
check "nothing installed in a fresh sandbox" "$(hp GET /marketplace/installed | jq -c .modules)" '[]'

# 3. Install: 202 with a job in the fetch phase, then the job walks to done.
RESP=$(hp POST /marketplace/install "$(jq -nc --arg m "$MODULE" --arg w "$WS" '{module:$m, workspace:$w}')")
JOB=$(echo "$RESP" | jq -r '.job.id // empty')
[ -n "$JOB" ] || { fail "install accepted"; echo "$RESP"; exit 1; }
check "install job starts in fetch" "$(echo "$RESP" | jq -r .job.phase)" fetch
SEEN=""
for i in $(seq 1 1800); do
  J=$(hp GET "/marketplace/jobs/$JOB")
  PH=$(echo "$J" | jq -r .phase)
  case " $SEEN " in *" $PH "*) ;; *) SEEN="$SEEN $PH"; echo "   phase: $PH ($(echo "$J" | jq -r '.progress // 0')%)";; esac
  [ "$PH" = done ] || [ "$PH" = failed ] && break
  sleep 0.5
done
if [ "$PH" != done ]; then
  fail "install job finished (phase $PH)"
  echo "$J" | jq -r '.error // empty'; echo "$J" | jq -r '.log_tail[]?' | tail -40
  exit $FAILED
fi
ok "install job finished"
# Polling every half second misses the quick phases (verify, install), so the check is
# that whatever was seen came in the pipeline's order and the long ones all appeared.
check "the phases seen are in pipeline order" \
  "$(echo "fetch verify build install done" | tr ' ' '\n' | grep -Fxf <(echo "$SEEN" | tr ' ' '\n' | grep .) | tr '\n' ' ')" "$(echo "$SEEN" | sed 's/^ //') "
case "$SEEN" in *fetch*build*done*) ok "fetch, build and done were all observed";; *) fail "fetch, build and done were all observed ($SEEN)";; esac
check "job records the version" "$(echo "$J" | jq -r .version)" "$VERSION"
check "job records the tag" "$(echo "$J" | jq -r .tag)" "$TAG"
check "job is at 100%" "$(echo "$J" | jq -r .progress)" 100
check "the job list has exactly this job" "$(hp GET /marketplace/jobs | jq -r '.jobs | length, .[0].id' | tr '\n' ' ')" "1 $JOB "

# 4. The install record: active, at the mirror's commit, enabled in the workspace, and
#    the binary really is inside the sandbox.
INST=$(hp GET /marketplace/installed)
M=$(echo "$INST" | jq -c --arg m "$MODULE" '.modules[] | select(.module == $m)')
check "the module is recorded" "$(echo "$M" | jq -r .module)" "$MODULE"
check "the record is active" "$(echo "$M" | jq -r .active)" true
check "the record is at the mirror's commit" "$(echo "$M" | jq -r .commit)" "$COMMIT"
check "installing into a workspace enables it there" "$(echo "$M" | jq -r --arg w "$WS" '.enabled[$w]')" true
BIN=$(find "$A/home" "$A/data" -type f -perm -u+x -name 'avada-files*' 2>/dev/null | head -1)
[ -n "$BIN" ] && ok "the built binary lives in the sandbox ($BIN)" || fail "the built binary lives in the sandbox"
SHA=$(echo "$M" | jq -r .sha256)
if [ -n "$BIN" ]; then
  check "the recorded sha256 matches the binary on disk" "$(shasum -a 256 "$BIN" | cut -d' ' -f1)" "$SHA"
fi

# 5. Disable, then enable again — the workspace state moves and the record stays.
D=$(hp POST "/marketplace/modules/$MODULE/disable" "$(jq -nc --arg w "$WS" '{workspace:$w}')")
check "disable reports the workspace off" "$(echo "$D" | jq -r --arg w "$WS" '.enabled[$w]')" false
check "installed view agrees it is off" "$(hp GET /marketplace/installed | jq -r --arg m "$MODULE" --arg w "$WS" '.modules[] | select(.module == $m) | .enabled[$w]')" false
E=$(hp POST "/marketplace/modules/$MODULE/enable" "$(jq -nc --arg w "$WS" '{workspace:$w}')")
check "enable reports the workspace on" "$(echo "$E" | jq -r --arg w "$WS" '.enabled[$w]')" true
check "a second workspace is untouched" "$(hp GET /marketplace/installed | jq -r --arg m "$MODULE" '.modules[] | select(.module == $m) | .enabled["other-ws"] // "absent"')" absent
check "enabling an unknown module is refused" "$(hp POST /marketplace/modules/nobody/nothing/enable "$(jq -nc --arg w "$WS" '{workspace:$w}')" | jq -r '.error // .message // "no error"' | grep -c .)" 1

echo "== sandbox $A kept for inspection; headless log at $A/headless.log"
if [ "$FAILED" = 0 ]; then echo "ROUNDTRIP_DEMO_GREEN"; else echo "ROUNDTRIP_DEMO_FAILED=$FAILED"; fi
exit $FAILED
