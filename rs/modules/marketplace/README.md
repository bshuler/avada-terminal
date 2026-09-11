# avada-marketplace

The **Marketplace** rail entry for [Avada Terminal](https://avada.to): find modules
on GitHub, install them from source, enable or disable them per workspace, remove
them, and sign in to GitHub for a higher API rate limit.

It is a tier-1 module: one rail entry, a list of rows, eight commands. Everything
it shows comes from the host's `/marketplace/...` control routes; the module never
talks to GitHub, git or cargo itself.

## What it renders

| Row | From | Open gesture |
|---|---|---|
| `toolchain` | `GET /marketplace/toolchain` | shows the per-OS install guide when something is missing |
| `github` | `toolchain.signed_in`, `POST /marketplace/signin`, `GET /marketplace/signin/{id}` | starts or polls the GitHub device flow |
| `installed` / `installed-N` | `GET /marketplace/installed` | `POST /marketplace/modules/{owner}/{repo}/enable` or `.../disable` for the active workspace |
| `search` / `result-N` | `GET /marketplace/search?q=` | `POST /marketplace/install` |
| `jobs` / `job-ID` | `POST /marketplace/install` (202), `GET /marketplace/jobs/{id}` | polls the job; a finished job refreshes the installed list |
| `notice` | any non-2xx answer | — |

Commands (`ui.commands`), all tier 1:

| Command | Args | Route |
|---|---|---|
| `search` | `{q}` | `GET /marketplace/search?q=` |
| `install` | `{module, tag?}` | `POST /marketplace/install {module, tag?, workspace}` |
| `enable` / `disable` | `{module}` | `POST /marketplace/modules/{owner}/{repo}/(enable\|disable) {workspace}` |
| `uninstall` | `{module, version?}` (default: the active version) | `DELETE /marketplace/modules/{owner}/{repo}/{version}` |
| `job` | `{id?}` (default: the newest job this UI started) | `GET /marketplace/jobs/{id}` |
| `signin` | — | `POST /marketplace/signin`, then `GET /marketplace/signin/{id}` while pending |
| `refresh` | — | `GET /marketplace/toolchain`, `GET /marketplace/installed`, `GET /marketplace/jobs` |

Route failures come back as JSON-RPC errors: 401/403 → capability denied, 400/404
→ invalid params, anything else → `-32000 - status` (a 412 is `-32412`, with the
toolchain guide in the message). The same text is shown as the `notice` row and,
with `ui.toast`, as a toast.

## Capabilities

`avada.toml` requests `marketplace.manage` (an escape hatch: it is never granted
by default, so the user has to accept it at install), `ui.rail`, `ui.pane`,
`ui.commands` and `ui.toast`. Without `marketplace.manage` the rail shows one
explanatory row and no route is ever called; without `ui.rail` nothing is drawn
but the commands still work.

The module reaches the control server with the per-run `token` and `control_url`
from `host.hello`. The token lives only in the HTTP client and is never logged
(its `Debug` prints `<redacted>`).

## How the marketplace installs it

This repo carries the `avada-module` topic and a root `avada.toml` with
`distribution.kind = "source"`, so the host's own marketplace can install it:

```
POST /marketplace/install {"module": "bshuler/avada-marketplace", "tag": "v0.1.0"}
```

The host clones the tag, checks the manifest, runs `cargo build --release --locked`
and copies the binary into its install store. `avada-marketplace --manifest`
prints the embedded manifest so the installer can compare it with the repo's.

## Building and testing

The crate depends on `avada-module-sdk` by git rev. Fetch once with network, then
everything runs offline and touches nothing beyond loopback:

```sh
cargo fetch
cargo test --offline
cargo clippy --all-targets --offline -- -D warnings
cargo fmt --check
```

- `src/app.rs`, `src/rows.rs`, `src/control.rs` — unit tests: every command and row
  action against a recording fake of the control server, row derivation from route
  JSON, HTTP response parsing. `every_declared_command_is_dispatched` fails if a
  command in `avada.toml` loses its handler.
- `tests/e2e.rs` — spawns the real binary with a socketpair in `AVADA_MODULE_FD`
  (the way the host does), plays the host side of the contract, and answers the
  marketplace routes from a std `TcpListener` with canned JSON. One test per
  command, plus row activation, the `--manifest` flag, capability gating and
  `module.shutdown`.

## License

MIT OR Apache-2.0.
