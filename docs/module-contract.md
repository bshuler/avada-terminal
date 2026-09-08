# The Avada module contract

*What a module author has to know to build a module for Avada Terminal.* Every
type here lives in the `avada-module-sdk` crate (`rs/crates/module-sdk`); the
Rust doc comments are the normative text and this page is the reading order.
`cargo doc -p avada-module-sdk --open` from `rs/` gives the full API.

| Constant | Value | Where |
|---|---|---|
| `CONTRACT_VERSION` | `1` | `contract.rs` |
| `PRODUCT_NAME` | `Avada Terminal` | `lib.rs` |
| `GITHUB_TOPIC` | `avada-module` | `lib.rs` |
| `MANIFEST_FILE` | `avada.toml` | `lib.rs` |
| `LOCKFILE_NAME` | `modules.lock` | `lib.rs` |

## 1. What a module is

A module is a git repository, tagged `avada-module` on GitHub, with an
`avada.toml` at its root and a program the host can run. The host discovers it
by topic, builds it (`distribution.kind = "source"`) or downloads a signed
artifact (`"binary"`), records what the user accepted, and runs it as a child
process that speaks newline-delimited JSON-RPC 2.0 over one pipe.

A module never draws pixels unless it asked for a tier that allows it. The
default is *data*: the module sends rows, entries and commands; the host renders
them with its own components, theme and accessibility tree.

## 2. `avada.toml`

The reference manifest is `rs/crates/module-sdk/tests/fixtures/avada.toml`.
TOML rule that bites: **top-level keys come before the first table header**, so
`capabilities` is the first line.

```toml
capabilities = ["fs.read", "workspace.read", "ui.rail", "ui.pane", "ui.commands", "skills.materialize"]

[module]
id = "acme/avada-files"      # owner/repo, lowercase; ModuleId::new validates
name = "Files"
version = "1.2.0"            # semver; the git tag is `v` + version (Manifest::tag)
description = "A file browser rail and viewer pane"
publisher = "Acme"
contract = "^1"              # VersionReq against CONTRACT_VERSION

[distribution]
kind = "source"              # "source" | "binary"
# commercial = true          # needs a license token at run time (§9)
# issuer = "https://avada.to/license"
# bin = "avada-files"        # binary name inside the artifact when kind = "binary"

[dependencies]
"acme/avada-git" = "^1.0"    # ModuleId -> VersionReq; installed as InstallKind::Dependency

[[provides]]                 # a shape this module implements
shape = "avada.files.tree"   # dotted lowercase, is_shape() validates
version = "1.0.0"

[[requires]]                 # a shape this module consumes
shape = "avada.git.status"
version = "^1"
provider = "acme/avada-git"  # optional default provider; the user may pick another
# multi = true               # accept every provider, not just one

[[contributions]]            # kind = rail | pane | command | prefs | route
kind = "rail"
id = "files"                 # shown as <module>/<id>
tier = 1                     # 1 data, 2 slot, 3 pixels, 4 route, 5 grid
label = "Files"
icon = "icons/files.svg"     # relative to the install dir; the host reads it

[[profiles]]                 # named permission presets the user can pick
name = "High security"
description = "Read only, and ask before touching the workspace"
[profiles.values]
"fs.read" = "always"
"workspace.read" = "ask"

[skills]
paths = ["skills"]           # directories of SKILL.md files (§8)
```

`Manifest::parse` then `Manifest::validate` is the whole pipeline; the host
refuses a manifest that parses but does not validate (an unknown capability
spelling, a shape that is not dotted lowercase, a contribution id with a space).
`Manifest::to_toml` round-trips.

## 3. Capabilities

`caps::Capability`, wire spelling in the second column. A module lists what it
needs; the user sees the list at install time and accepts, declines per item,
or picks a profile.

| Variant | Wire | Grants |
|---|---|---|
| `FsRead` / `FsWrite` | `fs.read` / `fs.write` | inside the active workspace root |
| `FsReadAny` / `FsWriteAny` | `fs.read_any` / `fs.write_any` | anywhere the user can |
| `PanesSpawn` / `PanesInput` / `PanesOutput` | `panes.spawn` / `panes.input` / `panes.output` | open panes, type into them, read their scrollback |
| `ProcessSpawn` | `process.spawn` | run programs through the host |
| `GitRead` / `GitWrite` | `git.read` / `git.write` | the host's git service |
| `NetFetch` | `net.fetch` | outbound HTTP through the host |
| `SettingsRead` / `SettingsWrite` | `settings.read` / `settings.write` | the host's own preferences |
| `WorkspaceRead` / `WorkspaceWrite` | `workspace.read` / `workspace.write` | workspace metadata |
| `UiRail` / `UiPane` / `UiCommands` / `UiPrefs` / `UiToast` | `ui.rail` … `ui.toast` | each UI surface |
| `Clipboard` | `clipboard` | read/write the clipboard |
| `Keychain` | `keychain` | a module-private keychain namespace |
| `SkillsMaterialize` | `skills.materialize` | write skill files into projects (§8) |
| `ControlRoute` | `control.route` | mount routes on the control server (§7) |
| `EventsSubscribe` | `events.subscribe` | receive host events |
| `MarketplaceManage` | `marketplace.manage` | search, install, enable, disable and remove modules through the host's marketplace routes (§7); an escape hatch, since installing builds and runs code |

`contract::methods::required_capability(method)` maps every `host.*` method to
the capability it needs, or `None` for the always-allowed ones. The host checks
it before dispatch and answers `ErrorCode::CapabilityDenied` otherwise.

`rights::RightValue` is what the user set: `never`, `always`, `workspace` (only
while a workspace that allowed it is active), `ask` (toast each time). A
`PermissionProfile` is a named map of those; `ModuleRights::value(cap, profiles)`
resolves profile then per-capability override.

## 4. The handshake

Transport: the host spawns the module with the pipe on the file descriptor named
by `AVADA_MODULE_FD` (Unix) or the named pipe in `AVADA_MODULE_PIPE` (Windows),
and the module's private data directory in `AVADA_MODULE_DATA`. One JSON object
per line, at most `client::MAX_LINE` (16 MiB).

1. Module writes a `ModuleHello`: `kind = "module"`, its `Manifest`, the
   contract range it speaks (`contract_min ..= contract_max`), the methods it
   serves, and its `sdk_version`.
2. Host answers a `HostHello`: `kind = "host"`, the negotiated
   `contract_version`, `host_version`, `product`, the `granted` capabilities
   (a subset of what the manifest asked for), the methods it serves, the
   `data_dir`, and the active `WorkspaceInfo { id, name, root }` if any.
3. `contract::negotiate(module_min, module_max, host_min, host_max)` picks the
   highest common version; `None` means the host closes the pipe and shows the
   placeholder pane with the reason.

The host compares the presented manifest with the signed install record
(`InstallRecord::matches_hello`). A module whose manifest changed since the
user accepted it is not started; the update is *held* (§6).

`client::Connection::handshake` does steps 1–2 for a module written in Rust;
`client::from_env` builds the connection from the environment variables.

## 5. Methods

`contract::methods` has one `const` per name so neither side can drift on
spelling. `HOST_REQUIRED_V1` and `MODULE_REQUIRED_V1` are the two sets a
version-1 peer must serve.

**Host serves** (module calls):

| Method | Params → Result |
|---|---|
| `host.rail.register` | `{ entries: [RailEntry] }` |
| `host.rows.set` | `{ entry, rows: [Row] }` |
| `host.command.register` | `{ commands: [{ id, label, chord? }] }` |
| `host.prefs.declare` | `{ page: PrefsPage }` |
| `host.prefs.get` | → `{ values }` |
| `host.panes.spawn` | `{ kind, path?, surface? }` → `{ pane_id }` |
| `host.panes.input` | `{ pane_id, text }` |
| `host.fs.read` | `{ path }` → `{ text }` or `{ bytes_b64 }` |
| `host.fs.write` | `{ path, text }` |
| `host.fs.list` | `{ path }` → `{ entries: [{ name, kind }] }` |
| `host.events.subscribe` | `{ kinds: [string] }` |
| `host.toast` | `{ text, level? }` |
| `host.routes.register` | `{ routes: [RouteDescriptor] }` — the whole set; registering again replaces it (§7) |
| `host.keychain.get` / `host.keychain.set` | `{ key }` → `{ value? }` / `{ key, value }` |

**Module serves** (host calls):

| Method | Params |
|---|---|
| `module.activate` | `{ workspace: WorkspaceInfo }` |
| `module.deactivate` | `{ workspace_id }` |
| `module.command.invoke` | `{ id, args? }` |
| `module.row.activate` | `{ entry, row, data, gesture }` (`rail::RowActivate`) |
| `module.route.invoke` | `{ route, params, body? }` → the JSON body to return |
| `module.event` | notification |
| `module.prefs.changed` | notification, `{ values }` |
| `module.shutdown` | notification; **exit within 5 s** or be killed |

Framing types: `Request::new(id, method, params)`, `Request::ok(value)` /
`Request::err(RpcError)`, `Notification::new(method, params)`, `Response`,
`Message::parse(line)` / `to_line()`. `Id` is a number or a string.
`ErrorCode` covers the JSON-RPC standard codes plus `CapabilityDenied`,
`UserDenied`, `ShuttingDown`, `NoWorkspace`; `ErrorCode::from_code(i64)`
recovers it and `Other(i64)` keeps what it does not know.

## 6. Install, rights and updates

`rights::InstallRecord` is what the user accepted: module id, repo, tag, commit,
version, distribution kind, the accepted capability set, the full manifest, the
time, and `InstallKind::{Manual, Dependency}`. `SignedInstallRecord::sign`
MACs its `canonical_bytes()` with a key the host keeps in the OS keychain;
`verify` refuses a hand-edited record. `declined()` is the manifest's asked-for
set minus the accepted set.

`HeldUpdate::check(installed, candidate)` returns `Some` when a new manifest
adds or removes capabilities; the host keeps running the installed version and
asks before switching.

`install::Lockfile` (`modules.lock`, JSON, `LOCKFILE_VERSION = 1`) lists every
`LockedModule` (id, version, tag, commit, source, installed_at, kind), the
chosen `providers` per shape, and `defaults` (enabled unless the workspace says
otherwise). `Lockfile::orphans(deps)` finds dependency-installed modules nobody
needs any more. `WorkspaceModuleState` holds per-workspace `enabled` overrides
and `pins`; `is_enabled(id, lock)` resolves workspace over lockfile default.

## 7. Control-plane routes

A module with `control.route` registers `descriptor::RouteDescriptor`s: verb,
path with `{param}` segments (`path_params()`), scope, typed `Param`s with a
`ParamLocation`, the capability the route needs, and the RPC method it forwards
to (`RpcDescriptor`). `mounted_path()` prefixes the module id so two modules
cannot collide. `validate_table` rejects duplicate mounts and malformed method
names (`is_method_name`: dotted segments, lowercase). The host's `GET /schema`
returns a `SchemaDocument` listing core and module routes; the CLI generates its
verbs from it.

On the wire a registered route answers at `/m/<owner>/<repo><path>`. The
control server looks the request up in the live schema registry (so a route
appears the moment it is registered and vanishes when it is not), then forwards
it as `module.route.invoke` with `route` = the descriptor's `method`, `params`
= the `{param}` captures merged with the query string (a capture wins on a
clash), and `body` = the request body parsed as JSON when one was sent. The
module's returned value is the HTTP body. Status codes, in the order they are
checked: `401` for a bearer that is not an identity, `404` when nothing the
module registered has that shape, `405` when the shape matches but the verb
does not, `403 {"error":"capability","capability":…}` when the caller lacks
the descriptor's capability, `400 {"error":"bad request"}` for a body that is
not JSON, `503 {"error":"module unavailable"}` when the module is not installed
or not running, `400 {"error":"module","code","message","data"}` when the module
answered with a JSON-RPC error, and `502 {"error":"module failed"}` when the
host could not complete the exchange (timeout, closed pipe).

Calling routes over HTTP: the hello carries `control_url`
(`http://127.0.0.1:<port>`, absent when the host runs without a control server)
next to `token`; a module calls any control route it is granted with
`Authorization: Bearer <token>` against that base, exactly as the CLI does.

Registering: `host.routes.register` (gated on `control.route`) carries the
module's whole route set; sending it again replaces the set, and an empty set
withdraws every route. The host stamps each descriptor's `module` with the
caller's id and answers `InvalidParams` (naming the route) for a descriptor
that names another module, names no `capability`, asks for a scope other than
`token` (a module can neither open a route to the world nor restrict one to the
master token), or for a batch `validate_table` rejects (duplicate method,
duplicate mount, an undeclared `{param}`). The accepted set is published as
`HostEvent::Routes`; `control::modules::attach_host` (the app's one wiring
call: it also installs the host as the `/m/...` invoker) folds it into the
schema registry, so the routes show up in `GET /schema` and answer at
`/m/<owner>/<repo>/...` a moment later. Method names are global across the
core and every module: a set whose name clashes is refused by the registry,
logged, and the module's previous set stands. When the module's status stops
being live (crash, disable, shutdown) its routes leave the registry — `404`
from then on — and come back when the restarted process registers again, as
`examples/hello.rs` does with `GET /greet/{name}` → `module.route.invoke`
`{ route: "hello.greet", params: { name } }`.

## 8. Skills

`skills::Skill::parse` reads a `SKILL.md`: YAML-ish front matter (`name`,
`description`, `kind = skill | rule`, `activation = always | model | glob |
manual`, `globs`, `tools`) then the body. `adapters()` is the table of AI tools
the host can write into (each with `skills_dir`, `rules_file`, `project_scoped`).
`fence(module_id)` gives the begin/end markers and `splice_fenced(file, id,
block)` replaces exactly that module's block in a shared rules file, so
uninstalling removes only what the module wrote.

## 9. Licensing (commercial modules)

`license::LicenseClaims`: `jti`, `product`, `licensee`, `seats`, `nbf`, `exp`,
optional `max_major` (`covers_major`), `kid`, optional `download_url`, and
`checkin_interval_days` (clamped to `MAX_CHECKIN_DAYS = 365`). The host keeps a
`CheckinRecord { last_ok, revoked }` per license and calls the issuer's
introspection endpoint (`IntrospectionResponse { active, reason, token }`).
`evaluate(claims, checkin, now)` yields a `LicenseState`: `Valid`,
`GracePeriod` (`GRACE_DAYS = 14` after a missed check-in), `StaleCheckin`,
`Expired`, `Revoked`, `NotYet`; `allows_run()` is true for the first two only.

## 10. Rail and rows

`rail::RailEntry { id, label, icon, tier, module, order, component }` and
`rail::Row { id, label, detail, depth, expandable, expanded, icon, marks, data }`
are the tier-1 vocabulary. `depth` plus `expandable`/`expanded` describe a
tree without the module owning any widget; the host sends `module.row.activate`
with the row's opaque `data` and the `Gesture` (click, double click, context
menu, key).

### 10.1 Selection and scroll

`host.rows.set` carries no selection field, on purpose: a row list is the
module's whole state and a second channel for "and this one is current" would
be a second source of truth to keep in step. Instead the module puts the mark
`selected` on the row it wants shown, and the host scrolls the first row
carrying that mark into view whenever a new row set arrives. Marks the host
does not recognise are still passed through and ignored, so a module may carry
its own alongside it.

### 10.2 The filter box

Every module rail entry gets a host-owned filter text box above its rows. The
host does not filter anything itself — each change emits `rail.query` (§10.4)
to that entry's module, which answers with a new `host.rows.set`. A module that
did not subscribe to `rail.query` simply never sees the typing, and its rows
stand.

### 10.3 Reading the workspace: `host.fs.list` and `host.fs.read`

`host.fs.list { path } → { entries: [{ name, kind }] }` where `kind` is one of
`dir`, `file`, `symlink`, `other`. Hidden entries **are** listed — hiding them
is a view decision that belongs to the module — and the list is sorted by name.
A symlink is reported as `symlink` whatever it points at; the host does not
follow it to decide the kind.

`host.fs.read { path } → { text }` when the bytes are UTF-8, and
`{ bytes_b64 }` (standard base64) when they are not, so a module can read an
image or a binary without a second method. Reads are capped at **8 MiB**
(`rpc::MAX_READ`); a larger file is an `InvalidParams` error naming the size,
not a truncated answer.

Both methods need the `fs.read` capability and are **scoped to the open
workspace root**: the host canonicalises the root and the requested path and
refuses anything that lands outside, which covers `..` and a symlink whose
target escapes. The refusal is `CapabilityDenied` (-32001) and its message
names `fs.read_any`, the capability that lifts the scope entirely. A host with
no workspace open denies every scoped read; `fs.read_any` still works, because
a module holding it was never asking about the workspace.

The scope follows the human: `Host::activate` with a new `WorkspaceInfo`
retargets it for **every** running module at once, so a module that outlives a
workspace switch cannot keep reading the tree the user has left.

`host.fs.write` remains unsupported (`MethodNotFound`) — the capability exists
in the manifest vocabulary, but no host serves it yet.

### 10.4 Events: `host.events.subscribe` and `module.event`

`host.events.subscribe { kinds: [string] }` records the whole set for the
calling module and **replaces** any earlier set; subscribing to `[]` is how a
module goes quiet. Requires `events.subscribe`.

The host then sends `module.event` as a **notification** with
`{ kind, payload }` to each live module that named `kind`. Delivery is
best-effort and unordered with respect to other traffic; a module that is not
running is skipped rather than queued. An unknown `kind` must be **ignored**,
not answered with an error — a newer host has to be able to announce something
an older module never heard of.

The kinds live in `contract::methods::events`:

| Const | `kind` | Payload |
|---|---|---|
| `events::RAIL_QUERY` | `rail.query` | `{ entry: String, query: String }` |
| `events::FILES_REVEAL` | `files.reveal` | `{ path: String, line?: u32, col?: u32 }` |

`rail.query` is the filter box under a tier-1 rail entry (§10.2).
`files.reveal` is anything in the app that wants a path shown in a file tree —
a link click, a "reveal in files" menu item, a pane's working directory. If no
live module has subscribed, the app says so on a toast rather than silently
dropping it.

### 10.5 Opening and driving panes: `host.panes.spawn`, `host.panes.input`

`host.panes.spawn { kind, path?, surface? } → { pane_id }`. The **host** mints
the id (uuid v4) and answers immediately; the pane itself is opened later by
the app, which consumes `HostEvent::PaneSpawn { module, pane_id, kind, path,
surface }` off `Host::events()`. The module therefore has a usable id before
any window exists, and a slow or busy UI never blocks the module.

| `kind` | Meaning |
|---|---|
| `file` | open `path` in the host's own viewer |
| `shell` | a terminal, started in `path` — what a shell-tier module asks for |
| `module` | a pane owned by the module, showing `surface` (§11) |

A `kind` the app does not know becomes a toast, not a pane. Requires
`panes.spawn`.

`host.panes.input { pane_id, text } → {}` types into a pane the module opened,
and is the other half of shell tier: spawn a terminal, then drive it. It returns
as soon as the intent is on the event stream (`HostEvent::PaneInput { module,
pane_id, text }`) for the same reason `spawn` does — the pane lives on the UI
thread, and a module that waited for the keystroke to land would block its own
request loop.

An **unknown `pane_id` is not an error**. The host does not own the pane table;
answering "no such pane" would cost a synchronous round trip to the UI thread per
keystroke, and the app simply drops input for a pane it has closed. An *empty* id
is refused with `InvalidParams`, because that is a module bug rather than a race.

Requires `panes.input`, which is granted **separately** from `panes.spawn` on
purpose: opening a terminal and typing into somebody's shell are different
powers, and a module that may do the first should not silently acquire the
second.

### 10.6 The row menu: who draws a right-click

A `context` gesture on a row reaches the module in the ordinary way — a
`module.row.activate` with `gesture: "context"` — and the module may do whatever
it likes with it.

It does **not** have to draw a menu, and at tier 1 it cannot: a tier-1 module
sends rows and has no surface to put a popup on. So the **host** draws one as
well, from the row's own `data`:

* if `data.path` is a string, the app opens its own file menu over that path —
  the same rows a right-click on a filename inside a pane gets (open, open in a
  new pane, reveal in the OS file manager, copy path, and whatever else that
  build ships);
* if it is not, no host menu opens, and the gesture is still delivered.

A module gets the app's whole "Open in…" list, kept current by the app, by doing
nothing but putting a `path` in the row it already sends. Nothing about the menu
is negotiated: the host owns the vocabulary, and a module that wants verbs of its
own registers commands (§7) instead.

## 11. Pane kinds reserved for modules

`avada_core::tools::kind::PaneKind::Module(ModulePaneRef)` is the pane a
module opens through `host.panes.spawn`. Its workspace-file spelling is
`module:owner/repo#surface` with an optional `@semver` pin; a reference the
host cannot parse is kept verbatim as a tool id so an older build never drops
a newer file's pane. `Data`, `Table` and `Image` are the host's own viewer
panes (`view:data`, `view:table`, `view:image`).

## 12. Writing a module in Rust

```rust
use avada_module_sdk::{client, contract::methods, Capability, Manifest};
use serde_json::json;

fn main() -> Result<(), client::ClientError> {
    let manifest = Manifest::parse(include_str!("../avada.toml")).unwrap();
    let mut conn = client::from_env()?;            // AVADA_MODULE_FD / _PIPE
    let served = methods::MODULE_REQUIRED_V1.iter().map(|m| m.to_string()).collect();
    let host = conn.handshake(manifest, served)?;  // HostHello: granted caps, workspace
    if conn.has(Capability::UiRail) {
        conn.call(methods::HOST_RAIL_REGISTER, json!({ "entries": [/* RailEntry */] }))?;
    }
    let _ = host.workspace;
    while let Some(msg) = conn.recv()? {
        // dispatch on msg; answer requests with conn.respond(...)
    }
    Ok(())
}
```

Modules in other languages implement §4 and §5 directly; the SDK's JSON is the
contract, the Rust types are a convenience.
