#!/usr/bin/env bash
# Avada — macOS .app bundle + .dmg packaging (track T6 packaging-macos).
#
# Contract (docs/ports-seams.md §3, frozen):
#   rs/packaging/macos/bundle.sh <version>   # <version> WITHOUT a leading "v"
#   -> rs/packaging/out/avada-<version>.dmg
# Runs from any cwd (resolves the repo root from its own location), exits
# non-zero on any failure, puts ALL artifacts under rs/packaging/out/.
#
# Releases are built on this Mac (GitHub Actions does not run on this account);
# only stock tools are used (cargo, sips, iconutil, hdiutil, plutil, codesign,
# xcrun notarytool / stapler).
#
# Signing is picked from the keychain and degrades to a warning rather than a
# failure, so this stays runnable on a machine with no certificate at all.
# Notarization is MANDATORY on the Developer ID tier (an un-notarized Developer
# ID build is a release that Gatekeeper refuses on every other Mac) and never
# attempted on any other tier:
#
#   AVADA_SIGN_ID          codesign identity. Defaults to the first
#                          "Developer ID Application" in the login keychain,
#                          then to the first "Apple Development" (local-only:
#                          keeps TCC grants across rebuilds, cannot ship),
#                          then to ad-hoc ("-") with a warning.
#   AVADA_NOTARY_ITEM      1Password item holding the App Store Connect API key,
#                          as op://<vault>/<item>. Default
#                          op://global_secret_store/avada-notarytool-api-key.
#                          Fields read: private_key_b64 (the AuthKey_*.p8,
#                          base64, one line), key_id, issuer_id. Resolved ONCE
#                          per build through `secret get` (cache -> broker ->
#                          op), the key lands in a 0600 file under mktemp -d
#                          that is removed on exit. See README.md.
#   AVADA_NOTARY_PROFILE   alternative: an `xcrun notarytool store-credentials`
#                          keychain profile name. Takes precedence when set.
#   AVADA_NOTARIZE=0       explicit opt-out: build a Developer ID dmg without
#                          notarizing (local testing of the signing step only).
#
# An ad-hoc signature is not nothing: it is what lets macOS attach TCC grants to
# the app at all. It just changes on every rebuild, so the grants do too — which
# is exactly the problem a real Developer ID solves.
set -euo pipefail

VERSION="${1:-}"
if [[ -z "$VERSION" ]]; then
    echo "usage: bundle.sh <version>   (e.g. bundle.sh 0.1.0 — no leading 'v')" >&2
    exit 2
fi
if [[ "$VERSION" == v* ]]; then
    echo "error: <version> must not carry a leading 'v' (got '$VERSION')" >&2
    exit 2
fi

# Fail before doing any work if someone has exported a password expecting this
# script to pick it up. It never will: notarization authenticates with an App
# Store Connect API key resolved from 1Password, or with a keychain profile, so a
# password in the environment is a misunderstanding worth stopping for rather
# than silently ignoring.
for leaked in AVADA_NOTARY_PASSWORD APPLE_APP_SPECIFIC_PASSWORD AC_PASSWORD AVADA_NOTARY_KEY; do
    if [[ -n "${!leaked:-}" ]]; then
        echo "error: $leaked is set. This script never accepts a credential from the environment." >&2
        echo "       Store the App Store Connect API key in 1Password (rs/packaging/macos/README.md," >&2
        echo "       'Notarization credential'); bundle.sh resolves it itself through 'secret get'." >&2
        exit 2
    fi
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"          # repo root (rs/packaging/macos -> ../../..)
OUT="$ROOT/rs/packaging/out"
STAGE="$OUT/macos-stage"                            # scratch; recreated every run
APP="$STAGE/Avada.app"
DMG="$OUT/avada-$VERSION.dmg"

echo "==> repo root: $ROOT"
echo "==> building rs/crates/app (release)"
cargo build --release --manifest-path "$ROOT/rs/crates/app/Cargo.toml" -j 4

# The app crate is NOT a workspace member: depending on local config the release
# binary lands in $CARGO_TARGET_DIR when set, else the crate-local target dir or a shared rs/target.
BIN=""
for c in ${CARGO_TARGET_DIR:+"$CARGO_TARGET_DIR/release/avada"} "$ROOT/rs/crates/app/target/release/avada" "$ROOT/rs/target/release/avada" "$ROOT/target/release/avada"; do
    if [[ -x "$c" ]]; then BIN="$c"; break; fi
done
if [[ -z "$BIN" ]]; then
    echo "error: release binary 'avada' not found under any known target dir" >&2
    exit 1
fi
echo "==> binary: $BIN"

echo "==> assembling Avada.app"
rm -rf "$STAGE"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/avada"
chmod 755 "$APP/Contents/MacOS/avada"

# Two lookups have to be satisfied at once, and they disagree about where a
# resource lives. Both resolve relative to the RUNNING BINARY, i.e. Contents/MacOS:
# core::shell_integration::shell_integration_dir() and submit_new_goal() want
# exe_dir/resources/..., while a bundle-aware lookup wants exe_dir/../Resources/....
#
# This used to be answered by shipping two copies. It cannot be any more: codesign
# treats every directory under Contents/MacOS as nested code and refuses to sign a
# bundle whose Contents/MacOS holds a SKILL.md ("code object is not signed at all").
# So the files live in Contents/Resources, where a signature seals them properly,
# and Contents/MacOS/resources is a relative symlink onto it — sealed as a symlink,
# never descended into. Both lookups land on the same bytes, and there is now only
# one set of them.
mkdir -p "$APP/Contents/Resources/shell-integration"
cp "$ROOT/resources/shell-integration/hp-init.ps1" "$APP/Contents/Resources/shell-integration/"
cp "$ROOT/resources/shell-integration/hp-init.sh" "$APP/Contents/Resources/shell-integration/"
cp -R "$ROOT/resources/shell-integration/zdotdir" "$APP/Contents/Resources/shell-integration/"

mkdir -p "$APP/Contents/Resources/claude/goal-orchestrator"
cp "$ROOT/resources/claude/goal-orchestrator/SKILL.md" "$APP/Contents/Resources/claude/goal-orchestrator/"
cp "$ROOT/resources/claude/goal-orchestrator/SPEC.md" "$APP/Contents/Resources/claude/goal-orchestrator/"
cp "$ROOT/resources/claude/goal-orchestrator/IMPL.md" "$APP/Contents/Resources/claude/goal-orchestrator/"

# The always-on Orchestrator tab's working directory (README + the hidden .claude/skills tree its
# agent loads). `cp -R` of the directory itself, so the dot-directory rides along — a glob would
# silently drop it and ship the tab an agent that can't drive anything.
mkdir -p "$APP/Contents/Resources/claude"
rm -rf "$APP/Contents/Resources/claude/orchestrator"
cp -R "$ROOT/resources/claude/orchestrator" "$APP/Contents/Resources/claude/orchestrator"

# CLI-agent session hooks (tool-resume feature). These had never been bundled at all, so a
# shipped .app registered no hook and every hand-started tool pane fell back to the
# scan-and-diff heuristic. They go in Contents/Resources for the same signing reason as the
# personas above, and are reached through the MacOS/resources symlink.
for h in claude/hp-claude-session-hook.sh cursor/hp-cursor-session-hook.sh copilot/hp-copilot-session-hook.sh \
         codex/hp-codex-session-hook.sh gemini/hp-gemini-session-hook.sh; do
  mkdir -p "$APP/Contents/Resources/$(dirname "$h")"
  install -m 755 "$ROOT/resources/$h" "$APP/Contents/Resources/$h"
done

# First-party modules, shipped precompiled so first run seeds them offline (requirement
# #2). stage_seed_modules builds each in-tree module and lays out
# Contents/Resources/seed-modules/<owner>__<repo>/{avada.toml,bin/<name>,skills...},
# which core::install::seed::seed_modules_dir() finds via the ../Resources rule.
# Reached through the MacOS/resources symlink like the other bundled resources; the
# binaries it drops in are nested Mach-O and are signed inside-out below.
source "$SCRIPT_DIR/../seed-modules.sh"
stage_seed_modules "$APP/Contents/Resources/seed-modules"

ln -s ../Resources "$APP/Contents/MacOS/resources"

echo "==> generating avada.icns from build/icon.png"
# Source icon is 1024x1024 (build/icon.png — the retina master; same art as
# the Windows icon.ico/icon.svg). Standard iconset, every size derived with sips,
# including the full-resolution 512x512@2x (1024) entry for Retina displays.
ICONSET="$STAGE/avada.iconset"
mkdir -p "$ICONSET"
SRC_ICON="$ROOT/build/icon.png"
sips -z 16 16     "$SRC_ICON" --out "$ICONSET/icon_16x16.png"      >/dev/null
sips -z 32 32     "$SRC_ICON" --out "$ICONSET/icon_16x16@2x.png"   >/dev/null
sips -z 32 32     "$SRC_ICON" --out "$ICONSET/icon_32x32.png"      >/dev/null
sips -z 64 64     "$SRC_ICON" --out "$ICONSET/icon_32x32@2x.png"   >/dev/null
sips -z 128 128   "$SRC_ICON" --out "$ICONSET/icon_128x128.png"    >/dev/null
sips -z 256 256   "$SRC_ICON" --out "$ICONSET/icon_128x128@2x.png" >/dev/null
sips -z 256 256   "$SRC_ICON" --out "$ICONSET/icon_256x256.png"    >/dev/null
sips -z 512 512   "$SRC_ICON" --out "$ICONSET/icon_256x256@2x.png" >/dev/null
sips -z 512 512   "$SRC_ICON" --out "$ICONSET/icon_512x512.png"    >/dev/null
sips -z 1024 1024 "$SRC_ICON" --out "$ICONSET/icon_512x512@2x.png" >/dev/null
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/avada.icns"

echo "==> writing Info.plist"
# The NS*UsageDescription strings below are the text macOS puts in the consent
# dialog, so they are written to be read by the person deciding, not by us.
# Screen Recording, Accessibility and Full Disk Access have no such key — Apple
# fixes their dialog text — which is why `core::permissions::request` deep-links
# into the Settings pane for those three instead.
# CFBundleVersion must be period-separated numbers; strip any prerelease suffix
# (0.1.0-test -> 0.1.0). The full string stays in CFBundleShortVersionString.
BUNDLE_VERSION="${VERSION%%-*}"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key>      <string>to.avada.terminal</string>
    <key>CFBundleName</key>            <string>Avada</string>
    <key>CFBundleDisplayName</key>     <string>Avada Terminal</string>
    <key>CFBundleExecutable</key>      <string>avada</string>
    <key>CFBundlePackageType</key>     <string>APPL</string>
    <key>CFBundleIconFile</key>        <string>avada</string>
    <key>CFBundleVersion</key>         <string>$BUNDLE_VERSION</string>
    <key>CFBundleShortVersionString</key> <string>$VERSION</string>
    <key>LSMinimumSystemVersion</key>  <string>11.0</string>
    <key>NSHighResolutionCapable</key> <true/>
    <key>NSMicrophoneUsageDescription</key>
    <string>Avada uses the microphone only while you are dictating into a pane, and stops listening the moment you stop.</string>
    <key>NSAppleEventsUsageDescription</key>
    <string>Avada sends Apple events so a pane can hand a file or a link to the editor or browser you picked, instead of opening it here.</string>
    <key>NSDesktopFolderUsageDescription</key>
    <string>Avada needs this to open a workspace, or start a terminal, in a project you keep on your Desktop.</string>
    <key>NSDocumentsFolderUsageDescription</key>
    <string>Avada needs this to open a workspace, or start a terminal, in a project you keep in Documents.</string>
    <key>NSDownloadsFolderUsageDescription</key>
    <string>Avada needs this to open a workspace, or start a terminal, in a project you keep in Downloads.</string>
    <key>NSRemovableVolumesUsageDescription</key>
    <string>Avada needs this to open a workspace, or start a terminal, in a project you keep on an external drive.</string>
    <key>CFBundleDocumentTypes</key>
    <array>
        <dict>
            <key>CFBundleTypeName</key>       <string>Avada Workspace</string>
            <key>CFBundleTypeRole</key>       <string>Editor</string>
            <key>LSHandlerRank</key>          <string>Owner</string>
            <key>CFBundleTypeIconFile</key>   <string>avada</string>
            <key>LSItemContentTypes</key>
            <array>
                <string>to.avada.workspace</string>
            </array>
        </dict>
    </array>
    <!-- avada:// opens a pane (core/src/cli/link.rs). ssh:// is declared so Avada appears in
         "Open ssh URLs with"; it only becomes the handler when the user picks it there or runs
         `avada url register --ssh`. -->
    <key>CFBundleURLTypes</key>
    <array>
        <dict>
            <key>CFBundleURLName</key>     <string>to.avada.link</string>
            <key>CFBundleTypeRole</key>    <string>Viewer</string>
            <key>CFBundleURLSchemes</key>
            <array>
                <string>avada</string>
                <string>ssh</string>
            </array>
        </dict>
    </array>
    <key>UTExportedTypeDeclarations</key>
    <array>
        <dict>
            <key>UTTypeIdentifier</key>   <string>to.avada.workspace</string>
            <key>UTTypeDescription</key>  <string>Avada Workspace</string>
            <key>UTTypeConformsTo</key>
            <array>
                <string>public.json</string>
            </array>
            <key>UTTypeTagSpecification</key>
            <dict>
                <key>public.filename-extension</key>
                <array>
                    <string>avada</string>
                </array>
            </dict>
        </dict>
    </array>
</dict>
</plist>
PLIST
plutil -lint "$APP/Contents/Info.plist"

# ---------------------------------------------------------------- code signing
# Identity resolution, in order: the environment, then the login keychain, then
# ad-hoc. Only the identity NAME is ever printed — it is not a secret, but the
# key behind it is, and nothing here reads, prompts for, or writes one. Signing
# happens entirely through the keychain, which is where the key stays.
#
# entitlements.plist carries no XML comments because AMFI, the parser codesign
# hands it to, rejects them outright ("syntax error near line 4"), so the reasons
# for its two keys live here instead.
#
# Both are hardened-runtime gates that close *before* TCC gets to ask. Without
# them macOS refuses the microphone and Apple events outright and the user never
# sees a dialog to say yes to; with them the user is still asked and still free
# to say no. They are the packaging half of core::permissions::Right::Microphone
# and Right::Automation.
#
# Deliberately absent: `allow-jit` and `allow-unsigned-executable-memory` —
# nothing in this build generates code at runtime (Metal via wgpu, the bundled
# SQLite amalgamation, no scripting engine), and turning off W^X in a process
# that hosts the user's shells needs a better reason than "just in case".
# `disable-library-validation` — the binary is statically linked and loads no
# dylibs of its own. `app-sandbox` — Avada runs the user's shells and opens
# the user's repositories, which is the opposite of a container; Full Disk Access
# is a grant the user makes in Settings, not an entitlement we can claim.
ENTITLEMENTS="$SCRIPT_DIR/entitlements.plist"
plutil -lint "$ENTITLEMENTS"

SIGN_ID="${AVADA_SIGN_ID:-}"
SIGN_TIER=""
if [[ -z "$SIGN_ID" ]]; then
    # `find-identity` prints every usable identity. First choice: a Developer ID
    # Application cert, the only kind that notarizes and passes Gatekeeper on
    # another Mac. Apple Distribution is deliberately never picked: it is
    # Mac-App-Store-only and would produce a build that looks signed and still
    # fails Gatekeeper.
    SIGN_ID="$(security find-identity -v -p codesigning 2>/dev/null \
        | awk -F'"' '/"Developer ID Application/ { print $2; exit }')"
    [[ -n "$SIGN_ID" ]] && SIGN_TIER="developer-id"
fi
if [[ -z "$SIGN_ID" ]]; then
    # Second choice: an Apple Development cert. It cannot ship either (no
    # notarization, Gatekeeper refuses it on any other Mac), but on the Mac that
    # built it it is a real, certificate-anchored identity, and that is what
    # matters for the installed app: TCC (Screen Recording, Accessibility, Full
    # Disk Access, ...) keys each grant to the app's designated requirement.
    # With a certificate the requirement is "identifier + team", stable across
    # rebuilds; ad-hoc has no certificate, so the requirement is the cdhash of
    # that one build and every reinstall silently drops every grant (and the
    # daemon chain cannot even re-prompt — the denial is logged, not shown).
    SIGN_ID="$(security find-identity -v -p codesigning 2>/dev/null \
        | awk -F'"' '/"Apple Development:/ { print $2; exit }')"
    [[ -n "$SIGN_ID" ]] && SIGN_TIER="apple-development"
fi
if [[ -n "$SIGN_ID" && -z "$SIGN_TIER" ]]; then
    # AVADA_SIGN_ID was given explicitly: classify it so the notarize step and
    # the notices below say the right thing.
    case "$SIGN_ID" in
        "Developer ID Application"*) SIGN_TIER="developer-id" ;;
        *)                           SIGN_TIER="other" ;;
    esac
fi

SIGN_TIMESTAMP=(--timestamp)
if [[ -z "$SIGN_ID" ]]; then
    SIGN_ID="-"
    SIGN_TIMESTAMP=(--timestamp=none)   # ad-hoc has no identity to timestamp
    echo "" >&2
    echo "WARNING: no Developer ID Application certificate found — signing AD-HOC." >&2
    echo "WARNING: Gatekeeper will refuse this build on any Mac but this one, and" >&2
    echo "WARNING: macOS will drop every permission grant the moment it is rebuilt." >&2
    echo "WARNING: Set AVADA_SIGN_ID, or install a Developer ID, for a release." >&2
    echo "" >&2
elif [[ "$SIGN_TIER" == "developer-id" ]]; then
    echo "==> signing identity: $SIGN_ID"
else
    echo "==> signing identity: $SIGN_ID"
    echo "==> local-only identity: permission grants survive rebuilds on this Mac," \
         "but this build cannot be notarized or shipped (needs Developer ID Application)"
fi

# ------------------------------------------------- notarization credentials
# Resolved BEFORE signing so a missing credential fails here, not after a
# multi-minute sign + dmg. Only the Developer ID tier notarizes; Apple rejects
# everything else, so the other tiers skip with a notice and nothing is resolved.
NOTARY_AUTH=()                    # notarytool auth arguments, empty = do not notarize
NOTARY_TMP=""
cleanup_notary() { [[ -n "$NOTARY_TMP" ]] && rm -rf "$NOTARY_TMP"; return 0; }
trap cleanup_notary EXIT

if [[ "$SIGN_ID" == "-" ]]; then
    echo "==> notarization skipped (ad-hoc signature: Apple only notarizes Developer ID)"
elif [[ "$SIGN_TIER" != "developer-id" ]]; then
    echo "==> notarization skipped ($SIGN_ID is not a Developer ID Application identity; Apple would reject it)"
elif [[ "${AVADA_NOTARIZE:-1}" == "0" ]]; then
    echo "WARNING: AVADA_NOTARIZE=0 — Developer ID build will NOT be notarized; do not ship it." >&2
elif ! xcrun --find notarytool >/dev/null 2>&1; then
    echo "error: xcrun notarytool unavailable (needs Xcode 13+); set AVADA_NOTARIZE=0 to build without notarizing" >&2
    exit 1
elif [[ -n "${AVADA_NOTARY_PROFILE:-}" ]]; then
    NOTARY_AUTH=(--keychain-profile "$AVADA_NOTARY_PROFILE")
    echo "==> notarization: keychain profile '$AVADA_NOTARY_PROFILE'"
else
    # App Store Connect API key from 1Password. `secret get` is the only sanctioned
    # reader from a script (secret-management.md §2 rule 10): it answers from its
    # cache, then the broker, then the CLI, so a warm cache costs no Touch ID at
    # all. The .p8 is stored base64 on ONE line because the proxy caches
    # single-line values only; it is decoded into a 0600 file that lives exactly as
    # long as this script. The key id and issuer id are identifiers, not secrets,
    # and go to notarytool the way notarytool takes them (as arguments).
    NOTARY_ITEM="${AVADA_NOTARY_ITEM:-op://global_secret_store/avada-notarytool-api-key}"
    NOTARY_ITEM="${NOTARY_ITEM%/}"
    if ! command -v secret >/dev/null 2>&1; then
        echo "error: 'secret' (claude-standards secret proxy) not on PATH; cannot read $NOTARY_ITEM" >&2
        echo "       Set AVADA_NOTARY_PROFILE for a keychain profile, or AVADA_NOTARIZE=0 to skip." >&2
        exit 1
    fi
    NOTARY_TMP="$(umask 077 && mktemp -d "${TMPDIR:-/tmp}/avada-notary.XXXXXX")"
    NOTARY_KEY_FILE="$NOTARY_TMP/AuthKey.p8"
    echo "==> notarization: App Store Connect API key from $NOTARY_ITEM (via secret get)"
    if ! secret get --name avada-notary-private-key --op-ref "$NOTARY_ITEM/private_key_b64" \
            | tr -d '[:space:]' | (umask 077 && base64 -d > "$NOTARY_KEY_FILE") \
       || [[ ! -s "$NOTARY_KEY_FILE" ]]; then
        echo "error: could not resolve $NOTARY_ITEM/private_key_b64 (secret get failed or empty)." >&2
        echo "       If the cache is cold and no human is at the keyboard, that is a pause, not a bug:" >&2
        echo "       retry on the next human turn (secret-management.md §7.1), or AVADA_NOTARIZE=0." >&2
        exit 1
    fi
    if ! grep -q 'BEGIN PRIVATE KEY' "$NOTARY_KEY_FILE"; then
        echo "error: $NOTARY_ITEM/private_key_b64 did not decode to a PEM private key (store \`base64 < AuthKey_*.p8\`)" >&2
        exit 1
    fi
    NOTARY_KEY_ID="$(secret get --name avada-notary-key-id --op-ref "$NOTARY_ITEM/key_id" | tr -d '[:space:]')"
    NOTARY_ISSUER="$(secret get --name avada-notary-issuer-id --op-ref "$NOTARY_ITEM/issuer_id" | tr -d '[:space:]')"
    if [[ -z "$NOTARY_KEY_ID" || -z "$NOTARY_ISSUER" ]]; then
        echo "error: $NOTARY_ITEM is missing key_id or issuer_id" >&2
        exit 1
    fi
    NOTARY_AUTH=(--key "$NOTARY_KEY_FILE" --key-id "$NOTARY_KEY_ID" --issuer "$NOTARY_ISSUER")
fi

# notarize <path> <label>: submit, wait, and REQUIRE status Accepted. `--wait`
# alone is not enough — notarytool exits 0 when Apple answers "Invalid", so the
# JSON status is checked, and on anything else the service log (the per-file
# reasons) is printed before failing.
notarize() {
    local path="$1" label="$2" out status id
    echo "==> notarizing $label (waits on Apple; usually 1-5 min, 30 min timeout)"
    if ! out="$(xcrun notarytool submit "$path" "${NOTARY_AUTH[@]}" --wait --timeout 30m --output-format json)"; then
        printf '%s\n' "$out" >&2
        echo "error: notarytool submit failed for $label" >&2
        exit 1
    fi
    status="$(printf '%s' "$out" | plutil -extract status raw -o - - 2>/dev/null || true)"
    id="$(printf '%s' "$out" | plutil -extract id raw -o - - 2>/dev/null || true)"
    if [[ "$status" != "Accepted" ]]; then
        echo "error: notarization of $label ended as '${status:-unknown}' (submission ${id:-?})" >&2
        if [[ -n "$id" ]]; then
            xcrun notarytool log "$id" "${NOTARY_AUTH[@]}" >&2 || true
        fi
        exit 1
    fi
    echo "==> notarization accepted: $label (submission $id)"
}

echo "==> signing Avada.app (hardened runtime)"
# Nested Mach-O must be signed inside-out: the seeded module binaries under
# Contents/Resources/seed-modules/*/bin/* are separate executables, and a signature on
# the app that finds an unsigned one nested inside fails --verify --strict. So sign each
# module binary first (hardened runtime, no entitlements — they are headless RPC helpers
# that render nothing and need none of the app's exceptions), then seal the app over them.
# We avoid --deep (deprecated, signs blindly) in favour of signing the known set.
while IFS= read -r -d '' modbin; do
    codesign --force --options runtime "${SIGN_TIMESTAMP[@]}" --sign "$SIGN_ID" "$modbin"
done < <(find "$APP/Contents/Resources/seed-modules" -type f -path '*/bin/*' -perm -u+x -print0 2>/dev/null)

codesign --force --options runtime "${SIGN_TIMESTAMP[@]}" \
    --entitlements "$ENTITLEMENTS" \
    --sign "$SIGN_ID" "$APP"
codesign --verify --strict --verbose=2 "$APP"

# Two passes: the APP is notarized and stapled first, so the bundle that lands in
# /Applications carries its own ticket (Contents/CodeResources) and passes
# Gatekeeper offline; then the dmg is built from that stapled app and notarized
# and stapled itself, so the download passes too. Stapling only the dmg leaves
# the installed app dependent on a network check.
if [[ -n "${NOTARY_AUTH[*]:-}" ]]; then
    APP_ZIP="$STAGE/Avada.zip"
    rm -f "$APP_ZIP"
    ditto -c -k --keepParent "$APP" "$APP_ZIP"      # notarytool takes zip/dmg/pkg, not a bare .app
    notarize "$APP_ZIP" "Avada.app"
    rm -f "$APP_ZIP"
    xcrun stapler staple "$APP"
    xcrun stapler validate "$APP"
fi

echo "==> creating dmg"
DMG_STAGE="$STAGE/dmg-root"
mkdir -p "$DMG_STAGE"
# ditto, not cp: it is the copy that carries extended attributes across intact,
# and a signature that loses them is a signature that fails to verify.
ditto "$APP" "$DMG_STAGE/Avada.app"
ln -s /Applications "$DMG_STAGE/Applications"
rm -f "$DMG"
# The image size is given EXPLICITLY. `hdiutil create -srcfolder` otherwise sizes the
# volume from the folder's apparent bytes, which is not what lands on it: `ditto` carries
# the extended attributes across (that is why it is ditto), and those — plus the APFS
# metadata for a signed bundle's several thousand hashes — do not fit the estimate. It
# fails as "No space left on device" naming /Volumes/Avada, which is the mounted IMAGE and
# not the disk, so it reads like the machine is full when it has hundreds of gigabytes
# free. Payload plus half again, and never less than 128 MiB, so this does not come back
# the next time the bundle grows.
DMG_MB=$(du -sm "$DMG_STAGE" | cut -f1)
DMG_MB=$(( DMG_MB * 3 / 2 + 64 ))
if [ "$DMG_MB" -lt 128 ]; then DMG_MB=128; fi
hdiutil create -volname "Avada" -srcfolder "$DMG_STAGE" -ov -format UDZO \
    -size "${DMG_MB}m" "$DMG"
codesign --force "${SIGN_TIMESTAMP[@]}" --sign "$SIGN_ID" "$DMG"

if [[ -n "${NOTARY_AUTH[*]:-}" ]]; then
    notarize "$DMG" "$(basename "$DMG")"
    xcrun stapler staple "$DMG"
    xcrun stapler validate "$DMG"

    # Gatekeeper's own verdict, the way a downloading Mac would see it. Both must
    # say "accepted" with source=Notarized Developer ID; anything else fails the
    # release here rather than on someone else's desktop.
    echo "==> Gatekeeper assessment"
    spctl -a -vv -t exec "$APP"
    spctl -a -vv -t open --context context:primary-signature "$DMG"
    echo "==> designated requirement (what TCC keys permission grants to)"
    codesign -d -r- "$APP" 2>&1 | sed -n '/^designated/p'
fi

echo "==> done: $DMG"
