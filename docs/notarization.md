# Notarization: is this binary signed by anyone?

Before Avada records a module as installed, it asks the operating system — or, on
Linux, a detached minisign signature — whether the artifact it is about to bless was
signed by someone. The answer is an *observation*; what to do about it is a separate,
written-down *policy*. This document is the policy.

Code: `rs/crates/core/src/policy/`. Wiring: `marketplace/mod.rs` (install time) and
`module/spawn.rs` (spawn time), both inside `// ---- track G7 policy` fences.

---

## 1. The shape of it

Three nouns, in the order they are produced:

| | | |
|---|---|---|
| **`Source`** | where the artifact came from | `Built { commit }` — this host compiled it from that commit; `Prebuilt { url }` — it arrived already built |
| **`Verdict`** | what the platform verifier concluded | `Trusted { by }`, `Unsigned`, `Invalid { reason }`, `Unavailable { reason }` |
| **`Decision`** | what Avada does | `Run`, `Warn { reason }`, `Refuse { reason }` |

A verifier only ever produces a `Verdict`. It never refuses anything. `policy::decide`
is the only function that turns a verdict into a decision, and it is a pure function of
(policy, verdict, source) — which is what makes the policy testable.

`Unavailable` is deliberately distinct from `Unsigned`. "There is no signature on this
file" and "I could not run `spctl`, so I do not know" are different facts, and under a
strict rule they must both be refused; conflating them would let a broken toolchain
read as a clean bill of health.

### The decision matrix

Rows are the rule that applies to the artifact's source; columns are the verdict.

| rule \ verdict | `Trusted` | `Unsigned` | `Invalid` | `Unavailable` |
|---|---|---|---|---|
| `allow` | run | run | **warn** | run |
| `warn` | run | warn | warn | warn |
| `require-signature` | run | refuse | refuse | refuse |

Two cells are worth arguing about, so they are argued about here rather than discovered
later:

- **`allow` + `Invalid` warns rather than running silently.** "Allow" means "do not
  require a signature". It does not mean "do not tell me that the signature that *is*
  there is broken". A broken signature is a positive signal of tampering, not an
  absence of information, and silence about it would be a bug.
- **`require-signature` + `Unavailable` refuses.** The gate fails closed. A host that
  has asked for signatures does not get a pass because PowerShell was missing.

`Trusted` runs under every rule, including `warn`: there is nothing to warn about.

---

## 2. `policy.json`

Lives at `<modules root>/policy.json` — the same directory the install store keeps one
subdirectory per module in. (The store lists directories, so a file at the root is
ignored by it.) It is optional; a missing file is the default shown below.

```json
{
  "locally_built": "allow",
  "prebuilt": "require-signature",
  "publisher_keys_source": "manifest-or-defaults",
  "publishers": {
    "acme/avada-files": [
      "RW<the base64 key line from your minisign .pub file>"
    ]
  }
}
```

| field | values | default |
|---|---|---|
| `locally_built` | `allow` \| `warn` \| `require-signature` | `allow` |
| `prebuilt` | same | `require-signature` |
| `publisher_keys_source` | `manifest-or-defaults` \| `policy-file-only` | `manifest-or-defaults` |
| `publishers` | `"owner/repo"` → list of minisign public keys | `{}` |

**The default says the useful thing:** a module this host compiled itself, from a commit
it verified against `ls-remote` and pinned in the lockfile, runs with no signature
anywhere. A binary someone else built and shipped must be signed. The commit *is* the
provenance in the first case; in the second there is none, so a signature has to supply
it.

Unknown fields are rejected rather than ignored (`deny_unknown_fields`): a typo in a
security policy must be an error, not a silently disabled rule. A malformed file is
reported into the install job's log and the **default** policy is used — never an empty
or permissive one.

### Where publisher keys live, and why not in the manifest

They belong in the module's `avada.toml`, next to the rest of its distribution
metadata. They are not there because `avada_module_sdk::manifest`'s `[distribution]`
section is `deny_unknown_fields` and the SDK crate is frozen; adding a `keys` field
would be an SDK change this track cannot make. `publisher_keys_source` exists so that
the day the manifest grows the field, `manifest-or-defaults` starts consulting it and
`policy-file-only` remains for hosts that want the policy file to be the last word.

A key that does not parse is a load error naming the module it was listed under — not
a silently dropped entry.

---

## 3. Per-OS behaviour

Every parser compiles and is unit-tested on every platform, against captured tool
output. Only `policy::platform_verifier()` is `cfg`-gated, and each verifier
additionally carries one `#[ignore]`d test that runs the real tool against a real
binary (`cargo test -- --ignored`).

### macOS — `policy::macos::Spctl`

Runs `spctl --assess --type execute -vv` (Gatekeeper's own answer) and
`codesign -dv --verbose=2` (who signed it), and combines them. Both tools write to
stderr, which is why both streams are merged before parsing.

- accepted by Gatekeeper → `Trusted`, named by the leaf `Authority=` of the codesign
  chain and the `TeamIdentifier` when there is one
- `code object is not signed at all` → `Unsigned`
- ad-hoc signed (`Signature=adhoc`) → **not** trusted: an ad-hoc signature identifies
  nobody, which is exactly what this gate is asking about
- rejected with a signature present → `Invalid`, carrying Gatekeeper's reason
- `spctl` missing or erroring → `Unavailable`

A locally built module is unsigned on macOS in the normal case, which under the default
`locally_built: allow` runs — as intended.

### Windows — `policy::windows::Authenticode`

Runs `powershell -NoProfile -NonInteractive -Command` with a three-line
`Get-AuthenticodeSignature` script and maps its `Status`:

| `Status` | verdict |
|---|---|
| `Valid` | `Trusted`, named by the certificate subject's `CN=` |
| `NotSigned` | `Unsigned` |
| `NotSupportedFileFormat`, `Incompatible` | `Unavailable` |
| `HashMismatch`, `NotTrusted`, `UnknownError`, anything else | `Invalid` |

The path is passed as a single-quoted PowerShell literal with `'` doubled, and with
`-LiteralPath` so that `[`, `]` and `?` are not treated as wildcards.

**This is a shell-out, not the real API — see the follow-ups.**

### Linux — `policy::linux::Minisign`

There is no OS-level notion of "signed by someone" on Linux, so the policy supplies
one: a detached minisign signature at `<binary>.minisig`, verified with Ed25519 against
the keys `policy.json` lists for that module.

- no `.minisig` → `Unsigned`
- present and valid against one of the module's keys → `Trusted`, named by the key id
  and the signature's trusted comment
- present and not valid → `Invalid`
- present, but the module has no keys configured → `Unavailable` (naming the module):
  the question could not be asked, so it must not read as a pass

The implementation is minisign v2 written by hand (`policy::minisign`) because the
workspace has no minisign crate and this track may not edit a `Cargo.toml`. It uses the
workspace's existing `ed25519-dalek` and `base64`. Both signatures in the file are
checked: the signature over the file, and the global signature over
`signature ++ trusted_comment` — so the trusted comment cannot be edited either.

**Prehashed signatures (algorithm `ED`, BLAKE2b) are refused** with a clear error
rather than guessed at. See the follow-ups.

---

## 4. How a publisher signs a release

```sh
# once: make a keypair, keep the secret key out of the repository
minisign -G -p avada-publisher.pub -s avada-publisher.key

# per release: sign the artifact, with a trusted comment that says what this is
minisign -S -s avada-publisher.key -m avada-files \
         -t 'avada-files 1.2.0 linux-x86_64'
# -> avada-files.minisig, shipped next to the binary
```

Publish the contents of `avada-publisher.pub`'s key line (the `RW…` base64 blob, not the
comment line above it) and have hosts add it to `publishers` in `policy.json` under the
module's `owner/repo` id.

`minisign -S` defaults to the prehashed algorithm in recent versions; pass the flag your
minisign build uses for the legacy `Ed` algorithm, or sign with a tool that emits `Ed`,
until the follow-up below lands.

---

## 5. Where the check happens

**At install time**, in `Marketplace::run_install`, between "the artifact exists" and
"the record blesses it" — after the build or fetch produces a binary and before the
signed `InstallRecord` is written. A `Refuse` fails the install job with the verdict
text (HTTP 409, `refused: …`) and nothing is recorded. A `Warn` installs and says so in
the job log.

The verdict is then written next to the installed artifact as
`<binary>.notarization.json`:

```json
{
  "verdict": "unsigned",
  "detail": "unsigned",
  "decision": "warn",
  "reason": "unsigned (built here from 0e1c…)",
  "verifier": "spctl",
  "source": "built",
  "at": 1788869022
}
```

**At spawn time**, `module::spawn::verify_hash` reads that sidecar before it hashes the
binary. A recorded refusal returns `SpawnError::Notarized`, which the host turns into
`ModuleStatus::Broken` through its existing catch-all arm. The hash check is unchanged
and still runs.

### The sidecar's one honest limitation

`<binary>.notarization.json` is **not signed**, unlike `record.json`. It therefore can
only ever make the host *more* restrictive, never less: a missing, deleted or corrupt
sidecar returns the spawn path to exactly the pre-notarization behaviour (hash check
only), and a forged sidecar can only assert a refusal that stops a module from running.
Anyone who can write into the install directory can already replace the binary, so this
adds no new attack. It is written this way because `InstallRecord`'s shape is frozen
SDK and cannot grow a verdict field; the proper home is listed below.

---

## 6. Follow-ups

1. **Real `WinVerifyTrust`.** `Get-AuthenticodeSignature` calls the same trust provider
   and reports the same statuses, but costs a process launch and needs PowerShell
   present. The direct call needs the `windows` crate's `Win32_Security_WinTrust`
   feature enabled in `rs/crates/core/Cargo.toml`. The parser and its captured-output
   tests stay; only `Authenticode::verify` changes.
2. **Prehashed (`ED` / BLAKE2b) minisign signatures.** Modern `minisign -S` produces
   these by default. Supporting them needs a BLAKE2b-512 implementation, which the
   workspace does not currently have. Until then they are refused explicitly, with an
   error that says so, rather than silently mis-verified.
3. **A `keys` field in the manifest's `[distribution]` section**, so a module can
   declare its own signing keys and `publisher_keys_source: manifest-or-defaults` means
   what its name says. Needs an SDK change and a contract bump.
4. **The verdict inside the signed install record.** Moves the refusal under the same
   HMAC as the rights, and retires the unsigned sidecar and its limitation above.
5. **`Prebuilt` is unreachable in the free tier today.** `check_free_build` refuses
   `distribution.kind = "binary"` before the gate is reached, so `prebuilt:
   require-signature` currently guards a path only the commercial pipeline will open.
   The mapping is written and tested so that path arrives already gated.
