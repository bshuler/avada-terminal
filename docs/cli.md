# `avada` command line — schema-generated verbs

`avada <verb> [args]` talks to the running instance's control API. The verb set is not
hand-written: it is generated from `GET /schema` (the `SchemaDocument` every route is
described in), cached on disk, and completed by the shell from that cache. The
hand-written `avada ctl …` verbs stay exactly as they were.

## Verb generation

Every `RouteDescriptor` in the schema becomes one clap subcommand:

| descriptor | command line |
|---|---|
| `method: "health"` | `avada health` |
| `method: "tokens.mint"` (dots nest) | `avada tokens mint` |
| `method: "queues.tasks.enqueue"` | `avada queues tasks enqueue` |
| a module route `files.tree` owned by `acme/files` | `avada m acme/files files tree` |
| `params[].location == "path"` | required positional, in path order (`avada panes output <ID>`) |
| `params[].location == "query"` or `"body"` | `--flag` (`camelCase` → `--kebab-case`) |
| `kind: "boolean"` | a switch (`--wait-for-idle`) |
| `kind: "integer"` | `--tail <N>`, parsed as an integer |
| `kind: "string"` whose summary reads `a \| b \| c` | `--mode <VALUE>` limited to those values |
| `kind: "object"` / `"array"` | `--scope <JSON>` |
| any non-`GET` route | `--json <TEXT>` — the whole body; `--json -` reads stdin; typed flags override its members |

A route's `summary` is its help text. A route whose method is an interior name of
another (`state` next to `state.tree`) is still callable: `avada state` runs the route,
`avada state tree` the child. A required body member is satisfied either by its flag or
by `--json`. `GET` replies are printed as pretty JSON; so is everything else.

The tree carries two verbs of its own: `avada completions <shell>` and the hidden
`avada __complete <shell> -- <words…>` that the completion shims call.

## Precedence

`avada ctl <verb>` answers with the hand-written verb when there is one (`ctl_cli.rs`,
`HAND_VERBS`); a verb that is not hand-written but is a top-level name of the schema
tree runs the generated command (`avada ctl tokens mint …` ≡ `avada tokens mint …`).
Anything else is a usage error. At the top level, `avada <word>` is dispatched to the
generated CLI only when `<word>` is a top-level verb of the cached (or built-in) tree, so
`avada <dir>`, `avada pair`, `avada devices …` and the launch flags are unaffected.

## Cache

Location: `<data dir>/cli-schema/<control url with every byte outside [A-Za-z0-9.-] as _>.json`
(macOS: `~/Library/Application Support/avada/cli-schema/http___127.0.0.1_4041.json`),
one file per instance the CLI has talked to. Each file holds the control URL, the
`host_version` the instance reported, and the document.

Freshness: before a verb is sent, the CLI asks `GET /health` (cheap, unauthenticated)
for `version`. The cached document is used only when both the control URL and that
version match; otherwise — or on `avada schema --refresh` — it fetches `GET /schema`,
validates it, rewrites the file and rebuilds the command tree before parsing the verb
again. A verb clap does not know may be one the instance grew since the cache was
written: an unknown-verb error triggers the same probe once before it is reported.

With no instance running, the file for the last control URL (else the most recent file,
else this binary's built-in table) still drives `--help` and completions; a verb that
needs the instance fails with the connect error and exit 1. A corrupt or invalid cache
file is ignored, never trusted.

## Shell completions

```sh
# bash — add to ~/.bashrc
source <(avada completions bash)
# zsh — once, into a directory on $fpath, then restart the shell (compinit)
avada completions zsh > "${fpath[1]}/_avada"
# fish
avada completions fish > ~/.config/fish/completions/avada.fish
```

Each shim forwards the words typed so far to `avada __complete <shell> -- <words…>`,
which answers from the cache: subcommand names at every depth (including after
`m <owner>/<repo>`), `--flags` when the current word starts with `-`, and the allowed
values of an enumerated flag. Positionals are free-form. Completion never contacts the
instance.

## Exit codes

| code | meaning |
|---|---|
| 0 | the request succeeded (or `--help`, `--version`, `completions`, `__complete`) |
| 1 | the request could not be made or the server refused it — including "no instance" |
| 2 | usage: unknown verb or flag, a missing required argument, `--json` that is not JSON |

These are the same codes `avada ctl` uses. The reason for a 1 or a 2 goes to stderr.

## Where it lives

- `rs/crates/core/src/cli/schema_cli.rs` — schema → clap `Command` tree (pure).
- `rs/crates/core/src/cli/invoke.rs` — `ArgMatches` → `Request { method, path, query, body }`.
- `rs/crates/core/src/cli/cache.rs` — the cache file format and freshness rule.
- `rs/crates/core/src/cli/complete.rs` — completion candidates and the three shims.
- `rs/crates/app/src/ctl_cli.rs` — `schema_main`: connects, refreshes, sends, maps the exit code.
