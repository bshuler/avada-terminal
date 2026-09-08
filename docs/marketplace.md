# Marketplace (track F2)

`avada_core::marketplace` finds modules on GitHub, installs them from source and
records enable/disable state per workspace. It does **not** spawn modules: track H4
reads the install store and the workspace state and starts what is enabled.

## Routes

All twelve `/marketplace/...` routes need the `marketplace.manage` capability (an
escape hatch: never granted by default, master token or explicit grant only) and
answer **503 `marketplace unavailable`** until the app calls
`Shared::install_marketplace(Arc<Marketplace>)` (`control/server.rs`, same pattern
as `install_route_invoker`). The routes are listed in `descriptor_table::core_routes`
under the `// ---- track F2 marketplace` fence and mounted from `routes.rs::handlers`.

| Method | Route | Answer |
|---|---|---|
| `marketplace.search` | `GET /marketplace/search?q=` | `{modules: [RepoSummary]}` — GitHub search, `topic:avada-module` |
| `marketplace.show` | `GET /marketplace/modules/{owner}/{repo}` | repo, manifest at newest tag, tags with commits, installed versions, enabled map |
| `marketplace.install` | `POST /marketplace/install` `{module, tag?, accepted?, workspace?, commit?}` | **202** `{job}` |
| `marketplace.jobs` / `.job` | `GET /marketplace/jobs[/{id}]` | job phase, progress, log tail, error/version |
| `marketplace.enable` / `.disable` | `POST /marketplace/modules/{owner}/{repo}/(enable|disable)` `{workspace}` | `{module, enabled: {workspace: bool}}` |
| `marketplace.uninstall` | `DELETE /marketplace/modules/{owner}/{repo}/{version}` | `{ok: true}` |
| `marketplace.installed` | `GET /marketplace/installed` | every version: tag, commit, sha256, accepted caps, enabled map, `broken` |
| `marketplace.toolchain` | `GET /marketplace/toolchain` | `{ready, missing, guide, toolchain, signed_in}` |
| `marketplace.signin` / `.signin.poll` | `POST /marketplace/signin`, `GET /marketplace/signin/{id}` | device-flow view: user code, URL, status |

Statuses come from `MarketplaceError::http_status`: 400 bad id/workspace/body, 404
not installed / no such job or sign-in, 409 refused, 412 toolchain missing (body
carries the install `guide`), 502 GitHub, 503 sign-in unavailable (no client id).

## Install pipeline (one background job per request)

1. **Fetch** — `git ls-remote --tags` on `https://github.com/<owner>/<repo>.git`
   (`MarketplaceOptions::git_base` in tests points at local bare repos). The tag is
   the request's or the newest `vX.Y.Z`; `commit` from the request must be a prefix
   of what the tag names, else refused. Shallow `git clone --branch <tag>` into
   `<state>/scratch/<job>/<owner>__<repo>`, then `rev-parse HEAD` must equal the
   `ls-remote` answer (a clone that disagrees is refused).
2. **Verify** — read `avada.toml`; its id and version must match the request/tag;
   `distribution.kind` must be `source` and not `commercial` (**free build**: the
   build is only ever from source the user can read; prebuilt binaries and paid
   modules are refused with the reason). Then the whole dependency graph is resolved
   and everything missing from it is installed first, as `InstallKind::Dependency`
   jobs — see §Dependencies.
3. **Build** — `cargo build --release --locked` with `CARGO_TARGET_DIR` inside the
   scratch dir; the last 40 lines of output are the job's `log_tail`. The toolchain
   (`rustup`, `cargo`, `git` on `PATH`) is checked before the job starts; when it is
   missing the route answers 412 with a per-OS install guide (`toolchain::guide_for`).
4. **Install** — SHA-256 the binary, write `InstallRecord` (accepted caps default to
   the manifest's request minus escape hatches), copy the binary into the install
   store (`InstallStore`, one dir per id+version, side-by-side versions, newest
   active), pin `tag`+`commit`+`sha256` in the lockfile, enable in `workspace` if
   given. Scratch is removed on success and failure.

Refusals found during fetch/verify are **failed jobs** (202 then `phase: failed`,
`error: "refused: ..."`), not 4xx answers — the route returns as soon as the request
is well-formed so the UI can show progress.

## Dependencies (track G6)

A module declares what it needs two ways, and both are resolved before anything is
built (`install::resolver`, `marketplace::resolve`):

```toml
[dependencies]                            # identity: this module, these versions
"acme/avada-files" = ">=1, <1.2"

[[requires]]                              # shape: whoever answers this contract
shape = "avada.files.tree"
version = "^1"
provider = "acme/avada-files"             # optional: name one, or let the host pick
multi = false                             # optional: accept several providers
```

### Resolution

`resolver::resolve(roots, source, installed, defaults, pins)` answers with a `Plan`:
the exact version of every module to have on disk, in dependency order, plus the
provider chosen for each `[[requires]]`. It is a **pubgrub** solve (pubgrub 0.4),
so a refusal is a real explanation and not "some dependency failed".

- **Packages** are module ids; **versions** are `semver::Version`; a synthetic root
  package depends on every module the request names, so one solve covers the whole
  request. `ranges.rs` translates a Cargo-style `VersionReq` into pubgrub's interval
  sets, following the semver crate's own operator rules.
- **Candidates**, in preference order: the active installed version, then the other
  installed versions newest first, then published versions newest first. **What is
  already installed wins over what is newer** — an install does not churn the
  machine to satisfy a range that is already satisfied. Published versions come from
  `git ls-remote --tags`, keeping only `vX.Y.Z`.
- **Pre-releases** are never offered unless asked for exactly (an install by tag, or
  a pin). semver and interval arithmetic disagree here; semver's rule wins.
- **Named requirements go through the solver.** `provider = "..."` becomes a
  dependency on that module restricted to the versions whose manifest `provides` the
  shape at a matching version. Unnamed requirements are matched **after** the solve,
  against the plan plus the active installed modules.
- **One version per module** is the rule. When that has no solution the solve is run
  again with each compatibility line (`2.x`, `0.3.x`) as its own package, and the
  side-by-side answer is accepted only if no single root reaches two lines of one
  module through its own dependency graph: two roots may disagree about a major
  version, one module's dependencies may not. Otherwise the first pass's derivation
  tree is the refusal.
- **Manifests are read lazily**, newest candidate first, and cached for the life of
  the install job. Reading one means a shallow clone of one tag, which is thrown
  away immediately; a manifest already in the install store or in the job's own
  checkout is used without any network call.
- **Cycles** need no depth limit: a cycle is simply an unsatisfiable constraint, and
  pubgrub reports it as one.

Every step of the plan that is not already installed is fetched, verified and built
by the same pipeline as the module the user asked for, as an `InstallKind::Dependency`
job — including the free-build check, so a dependency that is a prebuilt binary or a
paid module fails the whole install with the reason and the module's name.

A refusal is a `Conflict` — `NoSolution`, `NoProvider` or `Source` — carrying a
message, pubgrub's rendered derivation, and notes gathered while solving (what is
pinned, which module has no `vX.Y.Z` tag, which shape nobody provides). The
marketplace turns it into a failed job with `error: "refused: ..."`.

### Provider defaults

When several installed modules provide the same shape and a requirement does not name
one, the host needs a tie-break. `<data_dir>/modules/defaults.json` (schema version 1,
owner-only, written atomically) maps shape → module:

- Installing a module **by hand** makes it the default for every shape it provides
  that has no default yet. It never displaces a default that already exists.
- Installing a module **as a dependency**, and upgrading any module, never changes a
  default.
- The user can set one explicitly; the explicit choice always wins.
- Uninstalling the last version of a module clears every default naming it.

With no default, providers are ordered: highest shape version, then highest module
version, then earliest installed, then id. `multi = true` takes them all; otherwise
the first. A requirement nobody provides is `ConflictKind::NoProvider`.

A file this host cannot read (newer schema, not JSON) is an error rather than a
silently empty set, so a later save cannot erase what a newer host wrote.

### Pins

A workspace may pin a module to one version (`modules.json`, beside the enable
state). A pin is absolute: the resolver offers that version and no other, so a
dependency range the pin does not satisfy is a refusal rather than a silent upgrade.

`Marketplace::pin` therefore checks first. It collects what every *other* module
enabled in that workspace demands of this one — `[dependencies]` entries naming it,
and `[[requires]]` whose `provider` is it — and refuses (409) if the pinned version
would break any of them, listing which module and which requirement. The refusal
offers the **nearest installed version that satisfies every demand**, searching both
directions and preferring the higher on a tie, so the answer is "1.0.0 works" rather
than "no". `unpin` removes the pin; uninstalling the module removes it too.

## Where things live

Modules root: `<data_dir>/modules` (`InstallPaths::host()`), owned by the install
store, which lists **every** directory there as a module. The marketplace therefore
keeps its own state beside it — a routine call made when the store reported the cache
as a broken module:

```
<data_dir>/marketplace/cache/        GitHub answer cache (ETag + TTL, 304 refresh)
<data_dir>/marketplace/scratch/      clones and builds in progress (per job)
<data_dir>/marketplace/workspaces/<key>/modules.json   per-workspace enable state
<data_dir>/modules/keys/github-token.json              stored GitHub token (0600)
<data_dir>/modules/defaults.json                        shape -> default provider (0600)
```

Workspace keys are `[A-Za-z0-9._-]{1,128}`, never `.`/`..`; the file is the SDK's
`WorkspaceModuleState` keyed by workspace. Uninstalling the last version of a module
forgets it in every workspace.

## GitHub access and sign-in

Unauthenticated by default (60 req/h, cached). `POST /marketplace/signin` starts the
OAuth **device flow** with `GitHubConfig::client_id` (empty by default → 503
`sign-in unavailable`); the answer is the user code and URL, never the device code.
Polling stores the access token through `TokenStore` (`FileTokenStore` in production,
`MemoryTokenStore` in tests) and later requests send it as a bearer. The token is never
serialized into a route answer, a job log or a tracing event.

## Testing

`marketplace::testing` runs everything offline: an axum fake of GitHub (search,
repos, tags, contents, device flow, ETag/304/403), local bare git repos with tagged
commits, and a fake `cargo` first on `PATH` that writes a stub binary (or fails with
`E0425`). Route tests boot the real axum stack and install a fake-backed marketplace.
One `#[ignore]` test (`real_github_search_and_tags`) hits api.github.com; run it with
`cargo test -p avada-core --lib real_github -- --ignored`.

## Follow-ups (outside F2)

- **Keyring**: `FileTokenStore` writes a 0600 JSON file; swap for the OS keychain when
  a `keyring` dependency is allowed.
- **H4 wiring**: the app must call `Shared::install_marketplace(Marketplace::host())`
  at boot and start enabled modules from `InstallStore` + `WorkspaceStates`.
- **OAuth client id**: register an Avada GitHub OAuth app and set
  `GitHubConfig::client_id`; until then sign-in answers 503.
- **Companion module**: `bshuler/avada-marketplace` (private) renders these routes in
  a rail entry; it depends on the SDK by git rev.
