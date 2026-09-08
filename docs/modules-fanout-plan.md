# Avada Terminal modules — fan-out plan

Status (2026-09-07): **plan only, nothing implemented.** Output of an eleven-turn design
brainstorm on turning avada into a modular host with a marketplace of modules. Every
decision below is binding for the agents that execute it; the reasoning lives in the
brainstorm transcript, not here. Companion review reports (left panel, control plane, core,
features, build, Slint) informed the file maps.

The product is being renamed **Avada Terminal** (binary `avada`, domain `avada.to`) as part of
this work, so new contracts are born with the new name. Until the rename wave lands, paths in
this document use today's names.

---

## 1. Scope

**In:** the module contract and SDK crate, the module host, rights, install store and
lockfile, marketplace, source-compile pipeline, route descriptor table and schema-driven
CLI, skills materialization for every supported AI tool, extraction of the left-panel
features and the Hyperpane pane into modules, a native editor module, licensing verifier
with a stub issuer, notarization checks, and the product rename with a compatibility layer.
All three OSes from day one.

**Out:** the mobile client (`mobile/hyperpanes_mobile`, Flutter) except for the rename; the
production license server on the ptah VPS (a separate repo, sketched in §9); the commercial
crate's real policy implementations (skeleton only); tier-3 pixel surfaces beyond the
contract type.

---

## 2. Decisions (binding)

### Vocabulary and shape
- **Module** everywhere. Extension points are defined by modules too (Eclipse model); core
  ships the first-party ones. **Workspace == tab.**
- **Minimal core:** window, tab, pane grid, terminal widget, PTY daemon, control plane,
  module host, settings, rights, git service (via `gix`; `git2` only in the worktree
  service). Everything else is a module, including the marketplace and the Hyperpane pane.
- **Process modules** are the default; a linked loader exists only for a short first-party
  **shell tier** (Hyperpane pane, left-panel shell). Native Rust editor pane via
  `helix-core`, keymap swappable (named presets from the editor module) and configurable
  (per-binding overrides in the existing keymap file, namespaced under the module id).
- **UI tiers**, declared per contribution in the manifest and enforced by core:
  (1) rows/models/typed prefs/string-id commands/path-string icons, (2) `slint-interpreter`
  `ComponentContainer` fixed slots, (3) pixel surface over shared memory, (4) route
  descriptor table → `GET /schema` → CLI, (5) grid surface (editor).
- **Left panel** = a rail of string-id entries with a path-string icon and a tier slot,
  registered at handshake. Reveal-in-files and show-commit become RPCs to the owning module.

### Manifest, discovery, dependencies
- One `avada.toml` per module repo: identity, version, deps, contributions with tier,
  capabilities, permission profiles, skill paths. GitHub topic `avada-module`, tags
  `vX.Y.Z`. Identity deps `owner/repo = "^1.2"` resolved with `pubgrub`.
- **Both dependency kinds:** identity deps AND extension-point `provides`/`requires` by
  shape and semver. A requirement is met if *any* installed module implements the required
  shape at the required version. Named beats shaped. Among shape providers: the
  user-level default provider, else highest version, else earliest install. A
  **hand-installed** provider becomes the default for its shapes; a dependency-pulled
  provider or an upgrade that adds a new shape never displaces an existing default.
  Extension points declare `multi = true` when every provider is active (additive points
  such as rail entries).
- **Side-by-side versions.** Multiple versions of one module may be installed. User-level
  default = latest unless changed; the default flows to workspaces; a workspace may pin any
  installed version. A pin that conflicts with a dependency range is refused with the
  conflict named and an upgrade/downgrade offered.
- Lockfile pins tag + commit + SHA-256 artifact; the tag is verified against the commit
  with `ls-remote`. Modules live under the app-support dir, one directory per id+version,
  single lockfile.
- GitHub access is unauthenticated with a cache; optional device-flow sign-in; token in the
  OS keychain.

### Free vs commercial
- **Free build:** source-compiled modules only, from public **or private** repos using the
  user's own git credentials. Requires `rustup` (and Visual Studio Build Tools on Windows);
  the marketplace detects and guides installation. A dependency graph containing a
  commercial or private-binary module is refused, the module named, the commercial edition
  pointed at.
- **Commercial build:** private `avada-commercial` crate behind a feature flag; loads any
  correctly tagged module, compiled or source, including private repos. Ships precompiled,
  **notarized** binaries (macOS codesign/spctl + Team ID, Windows Authenticode via
  `WinVerifyTrust`, Linux minisign ed25519). Policy traits with OSS impls; the commercial
  crate swaps them.
- **Licensing** (one mechanism for core and modules, revocable):
  - License = JWT (RFC 7519) signed EdDSA (RFC 8037) with claims: license id, product id
    (`core` or module id), licensee display name, seat count, not-before, not-after (the
    contract length is baked in: a 12-month contract is a 12-month license), key id,
    optional download URL, check-in interval (default 7 days, **max 365**).
  - Publisher endpoint is an **OAuth profile**: keys as JWKS (RFC 7517) discovered via
    `/.well-known/oauth-authorization-server` (RFC 8414); verification is token
    introspection (RFC 7662) with the signed response of RFC 9701; purchase download via
    the device authorization grant (RFC 8628). The license is its own introspection
    credential; no client registration. Publishers may use their own issuer or the
    Avada broker.
  - Core verifies the signature offline with cached keys on every launch and introspects at
    most once per interval when online; a stale revocation answer beyond the interval shows
    a banner but keeps running until expiry. Expiry: a grace window of `avada_module_sdk::
    license::GRACE_DAYS` (**14** days --- the SDK constant is the contract; this line once
    said 7 and the SDK won) with toast + prefs banner, then the module refuses to spawn and
    its panes become placeholders.
  - Three install paths: manual file install while disconnected; store purchase downloaded
    on sign-in; corporate-configured URL serving a signed license.
  - Binding is bearer plus displayed licensee name. Issuance is third-party, the verifier is
    ours, and the state at the end of this refactor is a **stub issuer**.

### Trust chain and rights
- Rights come **only** from the install record: module id, repo, tag, commit, accepted
  capabilities, SHA-256 artifact hash; the record is HMAC'd with a key in the OS keychain
  (Credential Manager on Windows). Core re-hashes the binary at every spawn, spawns the
  module as parent, hands it one end of a socketpair (named pipe with a per-user DACL on
  Windows), and mints a per-module token. The handshake manifest must match the record or
  the connection is closed and the module marked broken.
- Transport: bidirectional JSON-RPC over one stream; handshake exchanges manifests and
  negotiates an **integer contract version** (module declares a supported range).
- Capabilities are a closed namespaced vocabulary, checked server-side per connection.
  Every route and RPC declares its capability in the **route descriptor table**; an
  undeclared route fails to register. Subprocesses: `panes.spawn` is the normal path;
  direct fork is gated by `process.spawn`, shown at install.
- Rights values `never | always | workspace | ask` at user level with a workspace override
  column; `ask` is a toast, not a modal. Prefs has a rights page per module (row per
  capability) and the marketplace shows a badge. Modules may declare named **permission
  profiles** ("High security mode") that pre-select rows; never a hidden grant.
- An update that adds a capability is **held** until the user accepts the diff.
- Crash: restart ≤3/min with backoff and a toast; the pane keeps a **placeholder** (the
  grid never loses a slot); after the cap, disable in the workspace and tell the user. The
  same placeholder serves disabled modules ("reopen, or open another like it") and
  not-installed modules ("install from marketplace"), carrying module id and pinned version.
- The control plane is secured against unauthorized local use (peer identity, owner-only
  files on every OS).

### Skills
- Source format: Agent Skills `skills/<name>/SKILL.md` plus `kind: rule|skill`,
  `activation: always|model|glob|manual`, `globs`, `tools` allow-list; optional per-tool
  override `skills/<tool>/<name>/`.
- Core materializes into project scope (the workspace's project root) and user scope
  (tools that have one). Shared layer: `AGENTS.md` sections and `.agents/skills/`;
  adapters for the holdouts (Claude Code `.claude/skills/` + `@AGENTS.md`, Aider
  `CONVENTIONS.md`, Cline, Kiro, Continue, Augment, glob rules in each tool's native form,
  size caps respected). Only detected tools, with a per-tool toggle.
- Every emitted file or section carries an ownership fence
  `<!-- avada:module=<id> -->`; regeneration replaces only fences; uninstall removes them;
  user-authored files are never rewritten. Two workspaces sharing a root: **union**.
- A generated index skill lists installed modules and their CLI verbs from `GET /schema`.

### CLI and clients
- CLI generated from `GET /schema` with a disk cache and shell completions; `clap` adopted.
- Mobile client: out of scope except the rename.

### Rename
- Full rename: repo, crates, bundle id (`to.avada.terminal`), app-support dir, env vars,
  project dotfiles, topic, manifest, pairing scheme, docs, mobile. Compatibility layer:
  read both env prefixes for two releases; migrate the app-support dir and keychain items on
  first launch; treat `.hyperpanes` and `.avada` project files as one (the old directory is
  renamed on the first write into that checkout); users re-grant TCC permissions once.
  Details and the user-facing notes: `docs/rename-compat.md`.

---

## 3. Target layout

```
rs/
  Cargo.toml                       workspace: core, module-sdk
  crates/
    module-sdk/                    NEW, public, source-only. The contract.
      src/manifest.rs              avada.toml schema, parse, validate
      src/contract.rs              JSON-RPC types, handshake, CONTRACT_VERSION
      src/caps.rs                  capability enum, RightValue, resolve order
      src/rights.rs                install record, profiles, held-update diff
      src/install.rs               lockfile types, version pin resolution types
      src/descriptor.rs            RouteDescriptor, RpcDescriptor, schema JSON
      src/rail.rs                  RailEntry {id, icon, tier}
      src/skills.rs                SKILL.md frontmatter, adapter table types
      src/license.rs               license claims, introspection response types
      src/client.rs                what a module links: connect, handshake, serve
    core/
      src/module/                  host: spawn, transport, supervise, registry
      src/rights/                  rights service (NOT permissions/, which is OS probing)
      src/install/                 store, lockfile, HMAC record, keyring, resolver
      src/marketplace/             discovery (topic), build (rustup), fetch, cache
      src/skills/                  materializer + adapters
      src/license/                 verifier, introspection client, stub issuer (test)
      src/control/descriptor_table.rs, schema.rs
      src/policy/                  traits with OSS impls (commercial crate swaps)
    app/
      src/leftpanel/rail.rs        rail rendering from RailEntry
      src/module_ui/               tier-1 renderer, tier-2 slots, placeholder pane
      src/prefs/rights.rs          rights page
    avada-commercial/              PRIVATE repo, git dependency behind `commercial` feature
modules/                           first-party module repos live OUTSIDE this repo, one each:
  avada-files, avada-git, avada-tools, avada-workspace, avada-hyperpane, avada-editor,
  avada-marketplace
```

First-party modules are separate repositories from the start (private until the free
edition ships), because the free edition must build them from source through the same
marketplace path a third-party module uses. Developing them in-tree would hide that path.

---

## 4. Tracks

| Track | What | Family | Depends on |
|---|---|---|---|
| **W0** contracts | `module-sdk` types, descriptor table type, keymap loader keeps unknown ids, W0 deps | sdk+core | — |
| **R** rename | product, ids, env, dirs, dotfiles, docs, mobile, compat layer, repo rename | all | W0 |
| **H1** module host | spawn, socketpair/pipe, handshake, tokens, supervise, placeholder | core+app | W0 |
| **H2** rights | rights service, prefs page, ask-toast, profiles, held update | core+app | W0 |
| **H3** descriptor table | migrate every existing route/verb, `GET /schema`, cap check per route | core | W0 |
| **H4** rail | left panel → rail of string ids; modes become entries | app | W0 |
| **H5** install store | dirs, lockfile, HMAC record, keyring, artifact hash, side-by-side | core | W0 |
| **H6** SDK client | `module-sdk::client`, example module, integration test harness | sdk | W0 |
| **H7** Windows | named pipe + DACL, file DACLs, Credential Manager, `WinVerifyTrust` | core | H1, H5 |
| **F1** files module | file browser extracted as `avada-files` | module | H1, H4, H6 |
| **F2** marketplace | discovery by topic, rustup detect/guide, source build, install/enable/disable | core+module | H5, H6 |
| **F3** CLI | clap + schema-driven verbs + completions + cache | core/cli | H3 |
| **F4** skills | materializer, shared layer, Claude Code + Aider adapters, index skill | core | W0, H5 |
| **G1** git | git service on `gix`, `avada-git` module | core+module | F1 |
| **G2** tools | `avada-tools` (Claude/Cursor/Copilot tabs) | module | F1 |
| **G3** editor | `avada-editor` on `helix-core`, keymap presets + overrides | module+app | F1 |
| **G4** workspace | `avada-workspace` (tree/library/sets/detached) | module | F1 |
| **G5** hyperpane | `avada-hyperpane` shell-tier module | module | F1 |
| **G6** resolver | pubgrub, shape resolution, provider defaults, pin conflicts | core | H5 |
| **G7** notarization | three OS verifiers behind a policy trait | core | H5 |
| **G8** licensing | verifier, introspection client, stub issuer, three install paths, expiry/grace | core+app | W0, H2 |
| **G9** skills adapters | remaining tools, glob rules, detection table, user scope | core | F4 |
| **G10** commercial skeleton | `avada-commercial` crate, feature flag, policy swap, precompiled loader | private | G7, G8 |
| **G11** marketplace UI | full marketplace pane, badges, profiles UI, version picker | module+app | F2, H2, G6 |
| **L1** link base | `TerminalPane::set_project_roots`, relative paths resolved against remembered project roots before the screen is read; `state.rs` `reload_projects()` (`docs/link-base-project-roots-plan.md`) | widget+app | — |
| **V1** data tree | `view:data` pane: JSON tree with collapse state, `src/datatree.rs`, role 18 `DATA_NODE`, `Command::ViewToggleNode` (`docs/viewer-panes-plan.md` WP1) | app | W0 |
| **V2** table | `view:table` pane: `src/csv.rs` RFC 4180, cell roles 13/14 (WP2) | app | W0 |
| **V3** image | `view:image` pane: sibling `ImagePane` branch, fit never upscale, caption, checkerboard (WP3) | app | W0 |
| **V4** annotation gap | accessibility roles on `FileRowView`/`GitRowView`, raw `TouchArea` walk (WP4) | app | V1–V3 |
| **V5** proof pass | `docs/feature-test-matrix.md`, gap-closing tests, a test that fails on an unlisted `Command`/callback (WP5) | app | V1–V4 |

---

## 5. Wave plan

The governing rule is inherited from `docs/ports-seams.md` §2 and `docs/tool-panes-plan.md`
§5: **contracts before fan-out.** One agent lands every shared seam as a compiling stub while
nobody else runs; then parallel agents each own a disjoint file set; every `mod.rs`, every
`Cargo.toml`, every per-OS file and the whole `module-sdk` crate are **frozen, orchestrator
only** after Wave 0.

### Wave 0 — contracts (ONE agent, serial)

Everything compiles and is inert. No behaviour changes.

- New crate `rs/crates/module-sdk`, added to the `rs/Cargo.toml` workspace, with the nine
  files in §3 as complete types plus doc comments and serde round-trip tests. `core`
  depends on it. `CONTRACT_VERSION = 1`.
- `core/src/control/descriptor_table.rs`: `RouteDescriptor` table type and a registry that
  the router will be built from; not yet wired (H3 wires it).
- Keymap loader (`app/src/keybindings.rs`) keeps unknown binding ids instead of dropping them, with
  a test.
- Placeholder pane kind added to `PaneKind` at all construction sites, with a compat test
  cloned from `workspace_kind_compat.rs` in all four directions.
- New deps, the ones Wave 0 needs and no more: `toml`, `hmac`, `keyring`, `semver`.
  (`pubgrub`, `ed25519-dalek`/`jsonwebtoken`, `clap`, `gix` land with their first user.)
- CI: `module-sdk` is a member of the `rs` workspace, so the three-OS `cargo test --all`
  and the Linux clippy gate in `.github/workflows/verify.yml` already cover it; no matrix
  edit was needed.
- `docs/module-contract.md`: the contract as a module author reads it, generated from the
  SDK doc comments where possible.
- Pane kinds reserved: `PaneKind::{Data, Table, Image, Module(ModulePaneRef)}` with
  `ui_kind` 7/8/9/10 and `pane_mark` -7/-8/-9/-10, meta `view:data` / `view:table` /
  `view:image` / `module:owner/repo#surface[@semver]`; a malformed `module:` reference is
  kept verbatim as a tool id so no build ever drops a pane it does not understand.
- Deferred until the network is back: `keyring` (not in the offline cache); `serde_yaml`
  likewise, so V1 reads JSON only in its first cut.

**Exit:** both workspaces green on three OSes; a module author can `cargo doc` the SDK.

### Wave 0.5 — rename (parallel by directory, then serial compat layer)

Runs before any host work so that every new identifier is born as Avada. Four agents:

- **R1** `rs/` mechanical: crate names, binary `avada`, `AVADA_*` → `AVADA_*` with
  the alias reader, discovery and daemon names, user-visible strings, `.avada` →
  `.avada`. Owns everything under `rs/` except `Cargo.toml`s.
- **R2** build/release: `build/`, `scripts/`, `.github/`, bundle id `to.avada.terminal`,
  notarization profile names, sandbox bundle id, installer script.
- **R3** prose and clients: `README.md`, `AGENTS.md`, `docs/`, `resources/`, skill files,
  `mobile/` (text and ids only; verified by CI, not locally — no Flutter on the dev machine).
- **R4** (after R1) compat layer — DONE as `rs/crates/core/src/compat.rs`: app-support
  dir copied on first launch (marker-guarded, never moved: the old install may still be
  running), both env prefixes read and `HYPERPANES_*` twins injected into panes for two
  releases, `.hyperpanes`/`.avada` equivalence (read both; renamed to `.avada` on the first
  write rather than prompted — a prompt has no home in the CLI and headless paths, and the
  rename is one git-visible change), legacy `hyperpanes-set` format and `.hyperpanes`
  workspace suffix accepted, old process name accepted by the discovery guard. No keychain
  items existed under the old name (the `keyring` dependency is still deferred), so there
  is nothing to migrate there. TCC re-grant and stale-config notes: `docs/rename-compat.md`.
- Orchestrator, last: `gh repo rename` (GitHub redirects the old name), directory rename on
  the dev machine, update `hyperpanes-buildinstall` worktree remote, memory file.

**Exit:** `avada --version` prints the new name; a pre-rename user-data dir migrates cleanly
in a `HOME=/tmp/...` sandbox; no `avada` string remains outside the compat layer and
the git history.

### Wave 1 — host and shell (seven agents in parallel)

| Agent | Track | Owns (exclusively) |
|---|---|---|
| A1 | H1 | `core/src/module/**`, `app/src/module_ui/placeholder.rs` |
| A2 | H2 | `core/src/rights/**`, `app/src/prefs/rights.rs`, `ui/prefs_rights.slint` |
| A3 | H3 | `core/src/control/{routes,dispatch,schema}.rs`, `descriptor_table.rs` wiring |
| A4 | H4 | `app/src/leftpanel/**`, `ui/leftpanel.slint`, `LeftPanelAdapter` in `ui/types.slint` |
| A5 | H5 | `core/src/install/**`, `core/src/persistence/lockfile.rs` |
| A6 | H6 | `module-sdk/src/client.rs`, `module-sdk/examples/hello/`, `core/tests/module_host_e2e.rs` |
| A7 | H7 | `core/src/module/transport/windows.rs`, `core/src/persistence/acl_windows.rs`, `core/src/install/keyring_windows.rs` |

Seams pre-carved in Wave 0 so these do not collide: A1 and A6 meet at the SDK contract
types; A3 and A2 meet at `RouteDescriptor.capability`; A4 and A1 meet at `RailEntry`
registration, which A1 exposes as a channel and A4 consumes. A7 starts one week behind A1
and A5 against their Unix implementations.

Alongside the seven host agents, six more run with no seam into `core/src/module`:

| Agent | Track | Owns (exclusively) |
|---|---|---|
| A8 | L1 | `terminal-widget/src/{terminal,links}.rs`, the `reload_projects()` sites in `app/src/state.rs` |
| A9 | V1 | `app/src/datatree.rs`, the `Data` arms of `viewpane.rs`, `ui/viewpane.slint` data rows |
| A10 | V2 | `app/src/csv.rs`, the `Table` arms of `viewpane.rs`, `ui/viewpane.slint` cell rows |
| A11 | V3 | `app/src/imagepane.rs`, `ui/imagepane.slint`, the `Image` arm of `paneview.rs` |
| A12 | V4 | accessibility annotations in `ui/leftpanel.slint` rows, `uitest.rs` raw walk (after A9–A11) |
| A13 | V5 | `docs/feature-test-matrix.md`, `uitest.rs` matrix test (after A12) |

A8 touches `state.rs` only at the eight `self.projects = sidebar::list();` sites, which A4
does not; A9–A11 share `viewpane.rs` by arm, and each adds its own `Command` variants at the
end of the enum. The bar for V1–V5 is the viewer plan's: every new `Command` and Slint
callback reaches the headless harness, and a mutation check (delete the branch, watch a
test fail) on each new arm. One track, one commit, pushed to `main`. `state.rs` and `app.rs` are hot files: A4 is
their only Wave-1 owner; A1's placeholder pane and A2's ask-toast reach them through
insertion points Wave 0 stubs.

**Exit:** the example module spawns, handshakes, registers a rail entry and a tier-1 row
list, survives a kill with a placeholder, and is refused when its binary hash changes. All
existing routes go through the descriptor table and `GET /schema` lists them. Works on
three OSes.

### Wave 2 — first module end to end, then ship (four agents, then release)

| Agent | Track | Owns |
|---|---|---|
| B1 | F1 | new repo `avada-files`; deletes `app/src/filetree.rs`, FILES mode in `leftpanel.slint`, `files_*` in `state.rs` |
| B2 | F2 | `core/src/marketplace/**`, `avada-marketplace` repo (tier-1 UI only) |
| B3 | F3 | `core/src/cli/**`, `app/src/main.rs` argument handling, `ctl_cli.rs` |
| B4 | F4 | `core/src/skills/**`, replaces the materializer in `core/src/hyperpane.rs` |

B1 is the milestone the whole plan was ordered around: **old app + host + one extracted
module.** It lands last in the wave because it deletes code the other three do not touch.

**Exit and release:** a free-edition build with `rustup` present installs `avada-files` from
its GitHub repo by topic, compiles it, enables it in a workspace, and the file browser works
as before, including reveal-in-files from a link. Disable it and the rail entry disappears
and the pane becomes the placeholder. Tag a release.

### Wave 3 — fan out (eleven agents, three sub-waves by dependency)

- **3a, parallel:** C1 G1 git, C2 G2 tools, C4 G4 workspace, C5 G5 hyperpane, C6 G6
  resolver, C7 G7 notarization, C8 G8 licensing, C9 G9 skills adapters. Each module agent
  owns its new repo plus the deletion of exactly one left-panel mode or pane kind in this
  repo; C6–C9 own their core directories only.
- **3b, after 3a:** C3 G3 editor (needs the git service for gutters and the keymap presets
  contract), C11 G11 marketplace UI (needs resolver + rights + profiles).
- **3c, after 3b:** C10 G10 commercial skeleton in the private repo, precompiled loader,
  feature flag wiring in `Cargo.toml` (orchestrator).

**Exit:** the left panel in this repo has no built-in modes; `core/src/git.rs`,
`claude_*.rs`, `hyperpane.rs`, `filetree.rs`, `gitpanel.rs` are gone or reduced to the git
service; two versions of one module install side by side and a workspace pins the older;
a license file installs offline and a revoked one stops the module after the grace period
against the stub issuer.

### Wave 4 — commercial (separate repos, not part of this fan-out)

License server on the ptah VPS at `avada.to` (§9), store integration, precompiled
first-party module pipeline with notarization on three OSes, commercial policy
implementations. Planned separately once Wave 3 ships.

---

## 6. Frozen files (orchestrator only after Wave 0)

- Every `Cargo.toml` and `Cargo.lock` in all three workspaces.
- The whole `rs/crates/module-sdk` crate. A change to it is a new contract version and a
  mini-wave with re-verification of every module.
- Every `mod.rs` under `core/src` and `app/src`; every `windows.rs`, `macos.rs`, `linux.rs`.
- `.github/workflows/*`, `build/**`, `scripts/install-macos.sh`.
- `ui/types.slint` outside the adapter each agent is assigned.

An agent that needs a frozen change files it as a request in its report; the orchestrator
lands it between waves.

---

## 7. Verification contract, every wave

1. `cargo test --manifest-path rs/Cargo.toml` and
   `cargo test --manifest-path rs/crates/app/Cargo.toml --bins`; neither count regresses.
2. `cargo check` for `core`, `module-sdk` and `app` on macOS, Windows and Linux (CI matrix).
3. Compat suite green: `workspace_kind_compat.rs` and its Wave 0 clone in all four
   directions; from Wave 2, the lockfile and install-record round-trip tests.
4. The headless Slint harness (`app/src/uitest.rs`) has a test for every new control: rail
   entry click, placeholder buttons, rights rows, marketplace install button.
5. Empirical check on the isolated sandbox bundle (`/tmp/hphr/HP.app`, bundle id
   `to.avada.hotreload`, `HOME=/tmp/hphr`; ids change with the rename) — never the
   user's real install, never `/Applications`, never focus-stealing input. From Wave 2 the
   check is the module round trip in §5 Wave 2 exit, recorded as a screenshot in `shots/`.
6. Secrets: no token, HMAC key or license key is logged, passed as an argument, or written
   into a working tree, including tests. The keyring is mocked in tests.
7. `cargo fmt --check` and `cargo clippy -D warnings` on the files the agent owns.

---

## 8. Agent briefing template

Each agent receives, and nothing else:

1. This document's §2 and its own track row.
2. Its exclusive file list and the frozen list.
3. The Wave 0 contract: `cargo doc -p module-sdk` output and `docs/module-contract.md`.
4. The verification contract, and the sandbox rule from `docs/live-session-safety.md`.
5. The repo rules: stage by hunk, verify the staged subset in a scratch worktree, never
   `git stash`; commit as `Bert Shuler <BertShuler@proton.me>`, push to `main`; every new
   repo private.
6. A required final report: what landed, what was verified how, what it needs from a frozen
   file, and what it left undone.

Agents do not read each other's diffs. Cross-track questions go to the orchestrator.

---

## 9. License server sketch (Wave 4, separate repo `avada-license`)

Runs on the ptah VPS behind `https://avada.to`. Routes, all from the OAuth profile in §2:
`/.well-known/oauth-authorization-server`, `/.well-known/jwks.json`, `POST /introspect`
(RFC 7662 → RFC 9701 signed JWT), `POST /device_authorization` and `POST /token`
(RFC 8628) for purchase download, an admin route to issue and revoke. Signing key in
1Password, read with `op` at service start, never on disk. Store webhook issues; revocation
edits one row. Publishers who broker through Avada get a sub-issuer under their own key id.
The Wave 3 stub issuer implements the same routes in-process for tests, so Wave 4 changes a
URL.

---

## 10. Assumptions and risks

- **Rename first is the largest single risk to the schedule**: 256 files, and every
  concurrent session in the checkout must be idle during Wave 0.5. Mitigation: R1–R3 are
  disjoint directories and mechanical; R4 is the only logic.
- **`slint-interpreter` for tier 2** has not been spiked. If `ComponentContainer` cannot host
  an interpreted component inside a compiled tree with acceptable startup cost, tier 2
  collapses into tier 1 plus tier 3. The spike is A1's first week; the contract type is
  written so that removal costs nothing.
- **`gix` coverage** for status, diff and log is assumed sufficient; `git2` remains available
  for the worktree service.
- **Free-edition build times**: compiling a module on install can take minutes on a laptop.
  The marketplace shows progress and builds in the background; nothing in the plan hides
  this.
- **First-party modules in separate repos** means the free edition cannot ship them
  prebuilt; that is the decision, not an oversight.
- The `permissions/` directory name collides with the rights concept; the new code is
  `rights/` and the OS-probing directory keeps its name until the rename wave, where it
  becomes `os_permissions/`.
