#!/usr/bin/env bash
# Prove every module repo still builds and passes its own tests.
#
# The modules live in their own private repos (topic `avada-module`) plus the
# commercial bundle and the licence server, none of which run CI (the Test
# workflow is off by choice — see scripts/check-linux-docker.sh). This is the
# local stand-in: clone each through `gh` (which reads the logged-in account's
# token itself and never prints it), then `cargo test` with one shared target
# directory so the SDK is compiled once per pinned revision, not once per repo.
#
#   scripts/check-modules.sh            # all of them
#   scripts/check-modules.sh avada-git  # just one
#
# Prints PASS/FAIL per repo and MODULES_CHECK_GREEN when every one passed.
# Set MODULES_DIR to keep the clones somewhere other than /tmp.
set -u
cd "$(dirname "$0")/.."

command -v gh >/dev/null || { echo "FAIL: gh not on PATH"; exit 1; }
command -v cargo >/dev/null || { echo "FAIL: cargo not on PATH"; exit 1; }

owner=bshuler
if [ $# -gt 0 ]; then
  repos=("$@")
else
  # macOS ships bash 3.2, which has no mapfile.
  repos=()
  while IFS= read -r name; do repos+=("$name"); done < <(gh repo list "$owner" --topic avada-module --json name --jq '.[].name' | sort)
  repos+=(avada-commercial avada-license)
fi

dir="${MODULES_DIR:-/tmp/avada-modules-check}"
mkdir -p "$dir"
export CARGO_TARGET_DIR="$dir/target"

failed=0
for repo in "${repos[@]}"; do
  src="$dir/$repo"
  if [ -d "$src/.git" ]; then
    git -C "$src" fetch -q origin && git -C "$src" reset -q --hard origin/HEAD || { echo "FAIL: $repo: update"; failed=$((failed+1)); continue; }
  else
    gh repo clone "$owner/$repo" "$src" -- -q || { echo "FAIL: $repo: clone"; failed=$((failed+1)); continue; }
  fi
  head=$(git -C "$src" rev-parse --short HEAD)
  sdk=$(grep -o 'module-sdk.*rev = "[0-9a-f]*"' "$src/Cargo.toml" 2>/dev/null | grep -o '[0-9a-f]*"$' | tr -d '"' || true)
  log="$dir/$repo.log"
  if (cd "$src" && cargo test -q --all 2>&1 | tail -60 > "$log"); then
    echo "PASS: $repo at $head (sdk ${sdk:-n/a}) — $(grep -c '^test result: ok' "$log") ok suites"
  else
    echo "FAIL: $repo at $head (sdk ${sdk:-n/a}) — see $log"
    grep -E '^(error|test .* FAILED|failures:|---- )' "$log" | head -10 | sed 's/^/   /'
    failed=$((failed+1))
  fi
done

if [ "$failed" -eq 0 ]; then echo "MODULES_CHECK_GREEN"; fi
exit "$failed"
