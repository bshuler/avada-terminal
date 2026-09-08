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
| `host.routes.register` | `{ routes: [RouteDescriptor] }` |
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
