#!/usr/bin/env bash
# Avada — store the App Store Connect API key that bundle.sh notarizes with.
#
#   rs/packaging/macos/notary-credential-store.sh <AuthKey_XXXXXXXXXX.p8> <KEY_ID> <ISSUER_ID>
#
# Creates ONE 1Password item (default: global_secret_store / avada-notarytool-api-key)
# with the fields bundle.sh reads through `secret get`:
#
#   private_key_b64   the .p8, base64 on one line (concealed)
#   key_id            the 10-character App Store Connect key id
#   issuer_id         the team's issuer UUID
#   team_id           VGX52CQGPT, so the item says which team it belongs to
#
# The key value never appears on a command line (secret-management.md §2 MUST #6):
# the item is written with `op item create --template <file>`, the template being a
# 0600 file under mktemp -d, outside every git tree, removed on exit. Run from a
# human terminal this is one Touch ID dialog and the item exists when it returns.
# Run from an agent shell the op-guard hands the write to the secret-broker's
# slate and exits 75: the item does NOT exist yet, Bert approves it with
# `secret-broker approve`. The .p8 is only shredded once the item really exists.
#
#   --dry-run       print the template with the key redacted; write nothing
#   AVADA_NOTARY_ITEM=op://<vault>/<item>   store somewhere else (bundle.sh must
#                                           then be run with the same value)
set -euo pipefail

usage() {
    echo "usage: notary-credential-store.sh [--dry-run] <AuthKey_*.p8> <KEY_ID> <ISSUER_ID>" >&2
    exit 2
}

DRY_RUN=0
if [[ "${1:-}" == "--dry-run" ]]; then DRY_RUN=1; shift; fi
[[ $# -eq 3 ]] || usage
P8="$1"; KEY_ID="$2"; ISSUER_ID="$3"

if [[ ! -s "$P8" ]]; then
    echo "error: $P8 is missing or empty" >&2
    exit 2
fi
if ! grep -q 'BEGIN PRIVATE KEY' "$P8"; then
    echo "error: $P8 is not a PEM private key (expected the AuthKey_<KEYID>.p8 App Store Connect downloads)" >&2
    exit 2
fi
if [[ ! "$KEY_ID" =~ ^[A-Z0-9]{10}$ ]]; then
    echo "error: KEY_ID '$KEY_ID' does not look like an App Store Connect key id (10 upper-case alphanumerics)" >&2
    exit 2
fi
if [[ ! "$ISSUER_ID" =~ ^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$ ]]; then
    echo "error: ISSUER_ID '$ISSUER_ID' is not a UUID" >&2
    exit 2
fi
case "$P8" in
    "$HOME"/code/*|*/.git/*)
        echo "error: $P8 is inside a code tree; move it out (e.g. ~/Downloads) before storing" >&2
        exit 2 ;;
esac

ITEM_REF="${AVADA_NOTARY_ITEM:-op://global_secret_store/avada-notarytool-api-key}"
ITEM_REF="${ITEM_REF%/}"
if [[ ! "$ITEM_REF" =~ ^op://([^/]+)/([^/]+)$ ]]; then
    echo "error: AVADA_NOTARY_ITEM must be op://<vault>/<item> (got '$ITEM_REF')" >&2
    exit 2
fi
VAULT="${BASH_REMATCH[1]}"; TITLE="${BASH_REMATCH[2]}"
TEAM_ID="${AVADA_TEAM_ID:-VGX52CQGPT}"

TMP="$(umask 077 && mktemp -d "${TMPDIR:-/tmp}/avada-notary-store.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT
TEMPLATE="$TMP/item.json"

# base64 on ONE line: `secret get` caches single-line values only, and bundle.sh
# strips whitespace before decoding anyway.
KEY_B64="$(base64 < "$P8" | tr -d '\n')"

# Written by a tiny Python so the JSON is escaped correctly whatever the inputs;
# the key reaches it through the environment of this one child, never argv.
KEY_B64="$KEY_B64" KEY_ID="$KEY_ID" ISSUER_ID="$ISSUER_ID" TEAM_ID="$TEAM_ID" TITLE="$TITLE" \
    DRY_RUN="$DRY_RUN" /usr/bin/python3 -I - "$TEMPLATE" <<'PY'
import json, os, sys
dry = os.environ["DRY_RUN"] == "1"
key = os.environ["KEY_B64"]
item = {
    "title": os.environ["TITLE"],
    "category": "API_CREDENTIAL",
    "tags": ["avada", "notarization"],
    "fields": [
        {"id": "private_key_b64", "label": "private_key_b64", "type": "CONCEALED",
         "value": ("<redacted %d chars>" % len(key)) if dry else key},
        {"id": "key_id", "label": "key_id", "type": "STRING", "value": os.environ["KEY_ID"]},
        {"id": "issuer_id", "label": "issuer_id", "type": "STRING", "value": os.environ["ISSUER_ID"]},
        {"id": "team_id", "label": "team_id", "type": "STRING", "value": os.environ["TEAM_ID"]},
        {"id": "notesPlain", "label": "notesPlain", "type": "STRING", "purpose": "NOTES",
         "value": "App Store Connect Team API key (role Developer) for `xcrun notarytool`. "
                  "Read by avada-terminal rs/packaging/macos/bundle.sh via `secret get`; "
                  "private_key_b64 is AuthKey_<key_id>.p8 base64-encoded on one line."},
    ],
}
with open(sys.argv[1], "w") as fh:
    json.dump(item, fh, indent=2)
PY
chmod 600 "$TEMPLATE"

if [[ "$DRY_RUN" == 1 ]]; then
    cat "$TEMPLATE"; echo
    echo "dry run: would create '$TITLE' in vault '$VAULT' (op item create --template …); nothing written."
    exit 0
fi

echo "==> creating 1Password item '$TITLE' in vault '$VAULT' (one Touch ID dialog, or a queued slate entry)"
set +e
op item create --vault "$VAULT" --title "$TITLE" --template "$TEMPLATE" >/dev/null
rc=$?
set -e
case $rc in
    0)
        echo "==> stored. Shredding $P8 (the only copy outside 1Password)."
        rm -P "$P8"
        echo "==> next: bundle.sh resolves $ITEM_REF/{private_key_b64,key_id,issuer_id} via 'secret get'."
        ;;
    75)
        echo "==> QUEUED, not stored: an agent shell cannot pay the Touch ID. The write is on the" >&2
        echo "    secret-broker slate — review with 'secret-broker slate', run with 'secret-broker approve'." >&2
        echo "    $P8 is kept until the item exists; shred it then with: rm -P $P8" >&2
        exit 75
        ;;
    *)
        echo "error: op item create failed (exit $rc); $P8 untouched" >&2
        exit "$rc"
        ;;
esac
