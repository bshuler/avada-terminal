#!/usr/bin/env bash
# Live demo: the operating-model module's fence leaves and returns in the orchestrator
# folder's AGENTS.md when the module is disabled/enabled in the `default` workspace.
#
#   scripts/opmodel-live-demo.sh          # needs the build that carries 98a89ed installed
#
# Touches no pane, no focus, and never restarts the app: it only flips the module's
# workspace state through `ctl marketplace` and reads the file back.
set -euo pipefail

CTL=/Applications/Avada.app/Contents/MacOS/avada
FILE="$HOME/Library/Application Support/avada/orchestrator/AGENTS.md"
MOD=(bshuler avada-operating-model)

fences() { grep -c 'avada:module=bshuler/avada-operating-model' "$FILE" || true; }

echo "before:   $(fences) fence markers (expect 2)"
"$CTL" ctl marketplace disable "${MOD[@]}" --workspace default >/dev/null
sleep 3
echo "disabled: $(fences) fence markers (expect 0)"
"$CTL" ctl marketplace enable "${MOD[@]}" --workspace default >/dev/null
sleep 3
echo "enabled:  $(fences) fence markers (expect 2)"
