# macOS packaging — Avada.app + dmg

`bundle.sh <version>` (version without a leading `v`) builds `rs/crates/app`
in release mode, assembles `Avada.app`, signs it, notarizes and staples it when
a Developer ID is available, and emits `rs/packaging/out/avada-<version>.dmg`
containing the app plus an `/Applications` symlink. It runs from any cwd and
uses only stock tools (`cargo`, `sips`, `iconutil`, `hdiutil`, `plutil`,
`codesign`, `xcrun notarytool`, `xcrun stapler`).

Releases are built **on this Mac**. GitHub Actions does not run on this account
(the `release-rust.yml` workflow is inert), and signing needs the certificate in
the login keychain anyway. The one environment detail that matters is the SDK:
the Command Line Tools SDK breaks the `whisper-rs` build, so pin `SDKROOT` to
Xcode's:

```sh
SDKROOT=/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX26.5.sdk \
  bash rs/packaging/macos/bundle.sh 0.2.48
```

## Bundle layout

```
Avada.app/
  Contents/
    Info.plist                      # to.avada.terminal, the NS*UsageDescription
                                    # strings, .avada doc type + exported UTI
    _CodeSignature/                 # written by codesign; seals everything below
    CodeResources                   # notarization ticket, stapled (Developer ID only)
    MacOS/
      avada                    # the release binary
      resources -> ../Resources     # symlink, so the app's exe-relative lookup
                                    # and the bundle-idiomatic one land together
    Resources/
      avada.icns               # generated from build/icon.png via sips + iconutil
      shell-integration/            # hp-init.ps1 / hp-init.sh / zdotdir
      claude/goal-orchestrator/     # SKILL.md, SPEC.md, IMPL.md
```

Two lookups have to agree here. `core::shell_integration::shell_integration_dir()`
and `submit_new_goal()` both resolve `exe_dir/resources/...`, and in a bundle
`exe_dir` is `Contents/MacOS/`; the idiomatic bundle location is
`Contents/Resources/...`. That used to be answered by shipping two copies, which
signing forbids: `codesign` treats every directory under `Contents/MacOS` as
nested code and refuses to sign a bundle whose `Contents/MacOS` holds a
`SKILL.md`. So the files live in `Contents/Resources`, where the signature seals
them, and `Contents/MacOS/resources` is a relative symlink onto it — sealed as a
symlink, never descended into. One set of bytes, both lookups satisfied.

## Signing tiers

`bundle.sh` picks the signing identity from the login keychain and degrades to a
warning rather than a failure, so it stays runnable on a machine with no
certificate at all:

| Tier | Identity | Ships? | Permission grants survive a rebuild? |
|---|---|---|---|
| Developer ID | `Developer ID Application: … (VGX52CQGPT)` | **Yes** — notarized and stapled, Gatekeeper accepts it on any Mac | Yes |
| Apple Development | `Apple Development: Bert Shuler (F56QS6SHKK)` | No — cannot be notarized, Gatekeeper refuses it elsewhere | Yes, on the Mac that built it |
| ad-hoc | `-` | No | **No** — every rebuild drops them |

`Apple Distribution` is in the keychain too and is deliberately never picked: it
is Mac-App-Store-only and yields a build that looks signed and still fails
Gatekeeper.

| Variable | Effect |
|---|---|
| `AVADA_SIGN_ID` | Force a `codesign` identity. Unset → the first `Developer ID Application`, then the first `Apple Development`, then ad-hoc with a loud warning. |
| `AVADA_NOTARY_ITEM` | 1Password item holding the App Store Connect API key, as `op://<vault>/<item>`. Default `op://global_secret_store/avada-notarytool-api-key`. See *Notarization credential*. |
| `AVADA_NOTARY_PROFILE` | Alternative: an `xcrun notarytool store-credentials` keychain profile name. Takes precedence over the 1Password item when set. |
| `AVADA_NOTARIZE=0` | Build a Developer ID dmg **without** notarizing (for testing the signing step only; never ship it). |

Notarization is **mandatory on the Developer ID tier** and never attempted on
the others, which Apple rejects anyway. A Developer ID build with no reachable
credential fails before the multi-minute build starts, because an un-notarized
Developer ID dmg is a release that every other Mac refuses. The script also
refuses to run at all if a password (`AVADA_NOTARY_PASSWORD`,
`APPLE_APP_SPECIFIC_PASSWORD`, `AC_PASSWORD`, `AVADA_NOTARY_KEY`) has been
exported in the hope that it would be picked up: it never accepts a credential
from the environment.

### What the Developer ID tier does

1. `codesign` the app with the hardened runtime and `entitlements.plist`.
2. Zip the app (`ditto -c -k --keepParent`; `notarytool` takes zip/dmg/pkg, not
   a bare `.app`), `xcrun notarytool submit --wait` it, and fail unless the
   JSON status is exactly `Accepted` — `notarytool` exits 0 on `Invalid`, so
   the status is checked explicitly and `notarytool log` is printed on failure.
3. `xcrun stapler staple` the app, so the bundle that lands in `/Applications`
   carries its own ticket and passes Gatekeeper offline.
4. Build the dmg from the stapled app, sign it, notarize it, staple it.
5. Print Gatekeeper's own verdict and the designated requirement (below), so a
   bad release fails here and not on someone else's desktop.

Apple usually answers in one to five minutes; the wait times out after 30.

The app is signed with the hardened runtime and `entitlements.plist`, which
claims exactly two exceptions — `com.apple.security.device.audio-input` and
`com.apple.security.automation.apple-events`. Both are gates the hardened
runtime closes before TCC gets to ask; the reasoning, including why the JIT and
library-validation exceptions are deliberately absent, is in `bundle.sh` beside
the `ENTITLEMENTS` assignment. The file itself carries no XML comments because
AMFI, the parser `codesign` hands it to, rejects them.

## One-time setup (human steps)

Everything below is done by the account holder, once. Team `VGX52CQGPT`
("Albert Shuler") is the team that holds the paid membership; the certificate,
the API key and the notarization submissions must all be on that one team.

### A. Developer ID Application certificate

Only the Account Holder of the team can create this certificate type.

**Recommended — Xcode:** Xcode → Settings → Accounts → (add the Apple ID with
**+** if it is not there) → select the team → **Manage Certificates…** → **+**
→ **Developer ID Application**. Xcode generates the key pair in the login
keychain and installs the certificate. Done when

```sh
security find-identity -v -p codesigning
```

lists `Developer ID Application: … (VGX52CQGPT)`.

**Alternative — portal with a CSR:** Keychain Access → Certificate Assistant →
Request a Certificate From a Certificate Authority (saved to disk, key pair
left in the login keychain), then developer.apple.com → Certificates → **+** →
**Developer ID Application** → upload the CSR → download and double-click the
`.cer`. Same `find-identity` check.

Either way, afterwards export the certificate **with its private key** from
Keychain Access as a password-protected `.p12` and store it as a 1Password
Document (Private vault). Apple lets a team hold only a handful of Developer ID
certificates and will not re-issue a lost private key; without the backup a
rebuilt Mac cannot sign releases.

### B. App Store Connect API key for `notarytool`

App Store Connect → Users and Access → **Integrations** → **Team Keys** →
**Generate API Key** (or **+**) → name it `avada-notarytool`, role
**Developer** (enough to notarize). Then, on that page:

- **Download** `AuthKey_<KEYID>.p8` — Apple offers it exactly once. Save it to
  `~/Downloads`, not under `~/code`.
- Note the **Key ID** (10 characters) and the **Issuer ID** (a UUID, shown at
  the top of the Team Keys list).

An app-specific password on the Apple ID also works with `notarytool`, but it
is tied to a person rather than the team and goes stale with every password
change, so the API key is the path `bundle.sh` is built around.

### C. Store the key in 1Password

```sh
rs/packaging/macos/notary-credential-store.sh ~/Downloads/AuthKey_<KEYID>.p8 <KEYID> <ISSUER-UUID>
```

Run it from a human terminal (or with the `!` prefix in Claude Code): it costs
one Touch ID dialog and the item exists when it returns, at which point the
script shreds the `.p8` — 1Password is then the only copy. Run from an agent
shell the write is queued on the secret-broker slate instead and the script
exits 75 with the `.p8` kept; `secret-broker approve` lands it. `--dry-run`
prints the item it would create, with the key redacted.

## Notarization credential

The credential is one 1Password item, `global_secret_store /
avada-notarytool-api-key` by default (`AVADA_NOTARY_ITEM` moves it):

| Field | Content |
|---|---|
| `private_key_b64` | `AuthKey_<KEYID>.p8`, base64, **one line** (concealed) |
| `key_id` | the 10-character Key ID |
| `issuer_id` | the team's Issuer ID (UUID) |
| `team_id` | `VGX52CQGPT`, documentary |

`bundle.sh` resolves the three fields once per build through
`secret get --name … --op-ref op://…` (cache → secret-broker → `op`), decodes
the key into a `0600` file under a `0700` `mktemp -d` directory and hands
`notarytool` `--key <file> --key-id … --issuer …`; the directory is removed on
exit, including on failure. Nothing is echoed and nothing touches the git tree.
The key is base64 on one line because `secret get` caches single-line values
only, and it is stored as a field rather than a Document so one `secret get`
reads it.

If `secret get` cannot resolve the item — the broker is down, or 1Password
wants a Touch ID that no human is there to give — the build stops before
signing with a message saying so. That is a pause, not a bug
(secret-management.md §7.1): start `secret-broker serve` in a human terminal
and rerun. `AVADA_NOTARY_PROFILE` is the escape hatch for a machine that keeps
its credential in a `notarytool` keychain profile instead.

## Verifying a release

`bundle.sh` runs these itself at the end of a Developer ID build; they are the
checks to repeat by hand on a dmg of unknown provenance.

```sh
spctl -a -vv -t exec rs/packaging/out/macos-stage/Avada.app
spctl -a -vv -t open --context context:primary-signature rs/packaging/out/avada-<version>.dmg
xcrun stapler validate rs/packaging/out/avada-<version>.dmg
codesign -d -r- rs/packaging/out/macos-stage/Avada.app
```

Both `spctl` lines must say `accepted` with `source=Notarized Developer ID`;
`stapler validate` must say `The validate action worked!`; the `designated =>`
line from `codesign` must be anchored on Developer ID (the
`1.2.840.113635.100.6.2.6` / `…6.1.13` certificate fields) with
`certificate leaf[subject.OU] = "VGX52CQGPT"`. A dmg that was signed but not
notarized says `rejected` with `source=Unnotarized Developer ID`.

## Permission grants and the designated requirement

macOS keys TCC grants (Screen Recording, Accessibility, Full Disk Access, …)
to the app's **designated requirement**, the rule `codesign` writes into the
signature. The rule's form depends on the certificate kind, and a grant made
against one form is silently dropped when the app is replaced by a build
carrying another: the session daemon's process chain is not allowed to
re-prompt, so the grant looks ON in System Settings while every
`screencapture` in a pane fails with "could not create image from display".

| Signature | Designated requirement keys on | Grant survives |
|---|---|---|
| ad-hoc | the cdhash of that one build | nothing — every rebuild |
| Apple Development | `anchor apple generic` + `certificate leaf[subject.CN] = "Apple Development: Bert Shuler (F56QS6SHKK)"` | rebuilds and a renewal under the same name |
| Developer ID Application | the Developer ID certificate-chain fields + `certificate leaf[subject.OU] = "VGX52CQGPT"` (the team, not a name) | rebuilds, renewals and a reissued certificate on the same team |

Consequence for the switch: **the first Developer ID install breaks the
existing Screen Recording grant once**, because the stored requirement names
the Apple Development CN and the new signature does not satisfy it. Fix it once by hand: System
Settings → Privacy & Security → Screen & System Audio Recording → remove Avada
with **−**, add it again with **+**, relaunch. After that the grant is keyed to
the team and stays put through every later release. The `designated =>` line
`bundle.sh` prints at the end of the build is the thing to compare when a grant
goes missing again.

The Apple Development certificate on this Mac expires 2027-07-16; it stays the
fallback for local builds on a Mac that has no Developer ID.

## Installing

Use `scripts/install-macos.sh`, not a hand-rolled copy:

```sh
SDKROOT=/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX26.5.sdk \
  bash rs/packaging/macos/bundle.sh 0.2.48
bash scripts/install-macos.sh
```

Replacing `/Applications/Avada.app` by deleting it first kills the session
daemon — it is executing out of that bundle, and macOS kills a process whose
executable is unlinked underneath it — which ends every program running in every
pane. The installer renames the old bundle aside instead, verifies the daemon
survived, and locks the result with `chflags uchg` so the shortcut stops being
possible. See [docs/live-session-safety.md](../../../docs/live-session-safety.md).

### Installing an ad-hoc or Apple Development build (Gatekeeper)

A notarized Developer ID dmg opens with a plain double-click. Anything else is
quarantined on download, and a plain double-click shows "Avada is damaged" or
"cannot be opened because the developer cannot be verified". Either of these
gets past it:

- **Right-click → Open**: after copying `Avada.app` to `/Applications`,
  right-click (or Ctrl-click) the app → **Open** → **Open** in the dialog.
  Only needed once; afterwards it launches normally.
- **Strip the quarantine attribute** (Terminal):

  ```sh
  xattr -dr com.apple.quarantine /Applications/Avada.app
  ```

On newer macOS the first launch may instead be blocked outright with no Open
override; then use System Settings → Privacy & Security → "Open Anyway", or
the `xattr` command above.

## `.avada` file association

`Info.plist` declares the `to.avada.workspace` exported UTI (extension
`.avada`, conforms to `public.json`) and registers the app as its Owner
editor. LaunchServices picks the declaration up when the app is first copied
into `/Applications` (or launched). Double-clicking a `.avada` file then
opens it in the app — macOS passes the path as `argv[1]`, which flows through
the CLI's positional-path capture, same as the Windows `"%1"` association.

To verify a registration:

```sh
mdls -name kMDItemContentType some.avada
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -dump | grep -i avada
```

## Versioning

`CFBundleShortVersionString` carries the full `<version>` string;
`CFBundleVersion` gets the numeric prefix only (`0.1.0-test` → `0.1.0`)
because Apple requires period-separated numbers there.

The icon source is `build/icon.png` (512×512 — the same art as the Windows
`icon.ico`), so the iconset tops out at 512 px and omits the `512@2x` (1024)
slot; `iconutil` accepts that.
