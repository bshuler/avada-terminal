//! `avada ctl …` — the workspace's own command line over the running control API.
//!
//! This is the tool surface the always-on **Hyperpane** tab hands its agent (see
//! `resources/claude/hyperpane/`). The MCP server (`avada-mcp`) is a separate npm package
//! and can't grow with this repo; a subcommand of the binary itself always matches the app it
//! is talking to, needs no install step, and is reachable from any shell — including one inside
//! a pane.
//!
//! Every verb is a thin, honest wrapper over an HTTP route: the named ones exist because an
//! agent should not have to remember JSON shapes, and the raw `get`/`post`/`patch`/`command`
//! passthroughs exist so a route this file never learned about is still reachable. Machine
//! verbs print JSON; the three browsing verbs (`tabs`, `panes`, `read`) print text, because
//! their whole job is to be read.
//!
//! Exit codes: 0 ok · 1 the server said no (or isn't running) · 2 usage.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use avada_core::cli::invoke::Request;
use avada_core::cli::{cache, clap, complete, invoke, schema_cli, SchemaDocument, Verb};

use crate::control_cli::{self, Conn};

#[tracing::instrument(level = "debug", ret)]
pub fn wants_ctl(argv: &[String]) -> bool {
    argv.get(1).map(|a| a == "ctl").unwrap_or(false)
}

const USAGE: &str = "\
usage: avada ctl <verb> [args]

  Discovery
    health                          is the control API up, and what does it allow
    state                           the whole windows→tabs→panes tree, as JSON
    tabs                            that tree as an outline, one line per tab and pane
    panes                           one line per pane: id, tab, status, label
    settings                        the app's preferences, as JSON
    loops                           the status / restart schedules: on, every, last, next

  Terminals
    read <pane> [--tail N] [--raw] [--screen] [--wait]
    send <pane> <text…>             type text, no Enter
    submit <pane> <text…>           type text, then Enter
    keys <pane> <key>…              named keys, e.g. enter escape ctrl+c up

  Panes
    new-pane [--cwd D] [--cmd C] [--label L] [--color #rrggbb] [--shell S]
             [--project P] [--window N]        (lands in that window's active tab)
    close-pane <pane>
    restart-pane <pane>
    focus-pane <pane>
    rename-pane <pane> <title>
    recolor-pane <pane> <#rrggbb>
    layout <tab> <name>             auto | single | columns | rows | grid |
                                    main-stack | grid-<cols>x<rows>

  Tabs
    new-tab [--window N] [--title T] [--cwd D]
    close-tab <tab>
    rename-tab <tab> <title>
    focus-tab <tab>
    move-tab <tab> <index>

  Preferences
    set <key> <value>               one setting; value is JSON if it parses, else a string
    set-json <json>                 a whole patch object

  Raw (anything the verbs above don't cover)
    get <path>
    post <path> [json]
    patch <path> [json]
    command <json>                  POST /command with a verb object

Ids: a pane id comes from `panes`; a tab id is \"{window}:{index}\" and comes from `tabs`.
Tab ids are POSITIONAL — re-read `tabs` after anything that reorders or closes one.";

#[tracing::instrument(level = "debug", ret)]
pub fn run(argv: &[String]) -> std::io::Result<()> {
    let verb = argv.get(2).map(String::as_str).unwrap_or("");
    if verb.is_empty() || verb == "help" || verb == "--help" || verb == "-h" {
        println!("{USAGE}");
        return Ok(());
    }
    let args: Vec<String> = argv[3..].to_vec();
    let conn = control_cli::connect().unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
    match verb {
        // ---- discovery ----
        "health" => print_json(get(&conn, "/health")?),
        "state" => print_json(get(&conn, "/state")?),
        "settings" => print_json(get(&conn, "/settings")?),
        "tabs" => print_outline(&get(&conn, "/state")?),
        "panes" => print_panes(&get(&conn, "/state")?),
        "loops" => print_loops(&get(&conn, "/loops")?),

        // ---- terminals ----
        "read" => {
            let (pane, flags) =
                split_flags(&args, "read <pane> [--tail N] [--raw] [--screen] [--wait]");
            let mut q: Vec<String> = Vec::new();
            // Stripping ANSI is the default here and nowhere else: an agent reading a pane wants
            // the words, not the cursor choreography. `--raw` opts back into the bytes.
            if !flags.contains_key("raw") {
                q.push("strip=1".into());
            }
            if flags.contains_key("screen") {
                q.push("mode=screen".into());
            }
            if flags.contains_key("wait") {
                q.push("waitForIdle=1".into());
            }
            if let Some(n) = flags.get("tail") {
                q.push(format!("tail={n}"));
            }
            let body = get(&conn, &format!("/panes/{pane}/output?{}", q.join("&")))?;
            // The text is the point; print it bare and put the metadata on stderr so a pipe
            // gets exactly the terminal's content.
            eprintln!(
                "[{} · {}]",
                pane,
                body.get("status").and_then(Value::as_str).unwrap_or("?")
            );
            print!(
                "{}",
                body.get("output").and_then(Value::as_str).unwrap_or("")
            );
        }
        "send" | "submit" => {
            let pane = need(args.first(), "send <pane> <text…>");
            let text = args[1..].join(" ");
            if text.is_empty() {
                usage("send <pane> <text…>");
            }
            print_json(post(
                &conn,
                &format!("/panes/{pane}/input"),
                json!({ "data": text, "submit": verb == "submit" }),
            )?);
        }
        "keys" => {
            let pane = need(args.first(), "keys <pane> <key>…");
            let keys: Vec<&str> = args[1..].iter().map(String::as_str).collect();
            if keys.is_empty() {
                usage("keys <pane> <key>…");
            }
            print_json(post(
                &conn,
                &format!("/panes/{pane}/input"),
                json!({ "keys": keys }),
            )?);
        }

        // ---- panes ----
        "new-pane" => {
            let (_, flags) = split_flags_optional(&args);
            // The spawn spec goes UNDER `pane` — a flat one is rejected by the server, which is
            // the right call: a typo'd top-level `command` would otherwise silently spawn a
            // default shell. The pane lands in the window's active tab.
            let mut spec = json!({});
            put_str(&mut spec, "cwd", flags.get("cwd"));
            put_str(&mut spec, "command", flags.get("cmd"));
            put_str(&mut spec, "label", flags.get("label"));
            put_str(&mut spec, "color", flags.get("color"));
            put_str(&mut spec, "shell", flags.get("shell"));
            put_str(&mut spec, "project", flags.get("project"));
            let mut cmd = json!({ "type": "newPane", "pane": spec });
            if let Some(w) = flags.get("window").and_then(|w| w.parse::<i64>().ok()) {
                cmd["windowId"] = json!(w);
            }
            print_json(post(&conn, "/command", cmd)?);
        }
        "close-pane" => print_json(pane_verb(&conn, "closePane", &args, "close-pane <pane>")?),
        "restart-pane" => print_json(pane_verb(
            &conn,
            "restartPane",
            &args,
            "restart-pane <pane>",
        )?),
        "focus-pane" => print_json(pane_verb(&conn, "focusPane", &args, "focus-pane <pane>")?),
        "rename-pane" => {
            let pane = need(args.first(), "rename-pane <pane> <title>");
            let title = args[1..].join(" ");
            if title.is_empty() {
                usage("rename-pane <pane> <title>");
            }
            print_json(post(
                &conn,
                "/command",
                json!({ "type": "renamePane", "paneId": pane, "label": title }),
            )?);
        }
        "recolor-pane" => {
            let pane = need(args.first(), "recolor-pane <pane> <#rrggbb>");
            let color = need(args.get(1), "recolor-pane <pane> <#rrggbb>");
            print_json(post(
                &conn,
                "/command",
                json!({ "type": "recolorPane", "paneId": pane, "color": color }),
            )?);
        }
        "layout" => {
            let tab = need(args.first(), "layout <tab> <name>");
            let name = need(args.get(1), "layout <tab> <name>");
            print_json(post(
                &conn,
                "/command",
                json!({ "type": "setLayout", "tabId": tab, "layout": name }),
            )?);
        }

        // ---- tabs ----
        "new-tab" => {
            let (_, flags) = split_flags_optional(&args);
            let mut cmd = json!({ "type": "newTab" });
            if let Some(w) = flags.get("window").and_then(|w| w.parse::<i64>().ok()) {
                cmd["windowId"] = json!(w);
            }
            put_str(&mut cmd, "title", flags.get("title"));
            put_str(&mut cmd, "cwd", flags.get("cwd"));
            print_json(post(&conn, "/command", cmd)?);
        }
        "close-tab" => print_json(tab_verb(&conn, "closeTab", &args, "close-tab <tab>")?),
        "focus-tab" => print_json(tab_verb(&conn, "focusTab", &args, "focus-tab <tab>")?),
        "rename-tab" => {
            let tab = need(args.first(), "rename-tab <tab> <title>");
            let title = args[1..].join(" ");
            if title.is_empty() {
                usage("rename-tab <tab> <title>");
            }
            print_json(post(
                &conn,
                "/command",
                json!({ "type": "renameTab", "tabId": tab, "title": title }),
            )?);
        }
        "move-tab" => {
            let tab = need(args.first(), "move-tab <tab> <index>");
            let to: usize = need(args.get(1), "move-tab <tab> <index>")
                .parse()
                .unwrap_or_else(|_| usage("move-tab <tab> <index>   (index is a number)"));
            print_json(post(
                &conn,
                "/command",
                json!({ "type": "moveTab", "tabId": tab, "to": to }),
            )?);
        }

        // ---- preferences ----
        "set" => {
            let key = need(args.first(), "set <key> <value>");
            let raw = args[1..].join(" ");
            if raw.is_empty() {
                usage("set <key> <value>");
            }
            // `set fontPx 15` should send a number and `set defaultShell zsh` a string, without
            // the caller having to know which is which — so try JSON first and fall back to the
            // literal text. (`set editorCommand "code -w"` therefore stays a string.)
            let value = serde_json::from_str::<Value>(&raw).unwrap_or_else(|_| json!(raw));
            print_json(patch(&conn, "/settings", json!({ key: value }))?);
        }
        "set-json" => {
            let body = parse_json(args.first(), "set-json <json>");
            print_json(patch(&conn, "/settings", body)?);
        }

        // ---- raw ----
        "get" => print_json(get(&conn, &path_arg(args.first(), "get <path>"))?),
        "post" => {
            let p = path_arg(args.first(), "post <path> [json]");
            let body = args.get(1).map_or(json!({}), |s| {
                serde_json::from_str(s).unwrap_or_else(|e| usage(&format!("bad json: {e}")))
            });
            print_json(post(&conn, &p, body)?);
        }
        "patch" => {
            let p = path_arg(args.first(), "patch <path> [json]");
            let body = args.get(1).map_or(json!({}), |s| {
                serde_json::from_str(s).unwrap_or_else(|e| usage(&format!("bad json: {e}")))
            });
            print_json(patch(&conn, &p, body)?);
        }
        "command" => {
            let body = parse_json(args.first(), "command <json>");
            print_json(post(&conn, "/command", body)?);
        }

        other => {
            // Not a hand-written verb: the schema-generated tree fills in the rest
            // (`avada ctl tokens mint …` is `avada tokens mint …`).
            let doc = load_document(Some(&conn.base), &cache::default_dir());
            match classify_ctl_verb(other, &doc) {
                Verdict::Schema => {
                    let mut without_ctl: Vec<String> = argv.to_vec();
                    without_ctl.remove(1);
                    return run_schema(&without_ctl);
                }
                Verdict::Hand | Verdict::Unknown => {
                    eprintln!("unknown verb '{other}'\n\n{USAGE}");
                    std::process::exit(2);
                }
            }
        }
    }
    Ok(())
}

// ---- schema-generated verbs ----------------------------------------------------------------
//
// `avada <verb> [args]` with the verb set read from the instance's `GET /schema`
// (docs/cli.md). Everything that decides is in `avada_core::cli`; this section only
// supplies the I/O — the connection, the cache directory, stdin/stdout — and maps the
// outcome to an exit code.

/// The hand-written `ctl` verbs, which win over a schema route of the same name
/// (`avada ctl health` is the hand-written one; `avada health` the generated one).
pub const HAND_VERBS: &[&str] = &[
    "health",
    "state",
    "settings",
    "tabs",
    "panes",
    "loops",
    "read",
    "send",
    "submit",
    "keys",
    "new-pane",
    "close-pane",
    "restart-pane",
    "focus-pane",
    "rename-pane",
    "recolor-pane",
    "layout",
    "new-tab",
    "close-tab",
    "rename-tab",
    "focus-tab",
    "move-tab",
    "set",
    "set-json",
    "get",
    "post",
    "patch",
    "command",
];

/// Who answers a `ctl` verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// A verb in [`HAND_VERBS`]: the match arm above.
    Hand,
    /// A top-level name of the schema tree: the generated CLI.
    Schema,
    /// Neither: usage error.
    Unknown,
}

/// Dispatch order for `avada ctl <verb>`: hand-written first, schema second.
pub fn classify_ctl_verb(verb: &str, doc: &SchemaDocument) -> Verdict {
    if HAND_VERBS.contains(&verb) {
        return Verdict::Hand;
    }
    if top_level_names(doc).iter().any(|n| n == verb) {
        return Verdict::Schema;
    }
    Verdict::Unknown
}

/// Every first word the generated tree accepts: the CLI's own names plus the first segment
/// of each placeable route.
pub fn top_level_names(doc: &SchemaDocument) -> Vec<String> {
    let mut names: Vec<String> = schema_cli::RESERVED.iter().map(|s| s.to_string()).collect();
    for r in doc.routes.iter().filter(|r| schema_cli::is_placeable(r)) {
        if r.module.is_none() {
            if let Some(first) = r.method.split('.').next() {
                names.push(first.to_string());
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

/// `avada <verb> …` where `<verb>` is a generated one. Reads the cache (or the built-in
/// table) and never the network, so the launcher's dispatch stays cheap.
pub fn wants_schema_cli(argv: &[String]) -> bool {
    let Some(first) = argv.get(1) else {
        return false;
    };
    if first.starts_with('-') {
        return false;
    }
    let doc = load_document(None, &cache::default_dir());
    top_level_names(&doc).iter().any(|n| n == first)
}

/// Entry point for `avada <verb> …`: connect (or not), run, exit with the mapped code.
pub fn run_schema(argv: &[String]) -> std::io::Result<()> {
    let conn = control_cli::connect();
    let no_instance = conn
        .as_ref()
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    let live = conn.as_ref().ok().map(|c| Live { conn: c });
    let instance: Result<&dyn Instance, &str> = match &live {
        Some(l) => Ok(l),
        None => Err(no_instance.as_str()),
    };
    let mut stdin = || {
        let mut s = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)
            .map(|_| s)
            .map_err(|e| e.to_string())
    };
    let outcome = schema_main(
        argv,
        instance,
        &cache::default_dir(),
        &mut stdin,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    );
    match exit_code(&outcome) {
        0 => Ok(()),
        code => std::process::exit(code),
    }
}

/// How a generated verb ended. Mapped to the process exit code by [`exit_code`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Printed what was asked.
    Ok,
    /// The command line was wrong (clap error, bad JSON, not a route).
    Usage(String),
    /// The request could not be made or the server said no (including "no instance").
    Failed(String),
}

/// 0 ok · 1 request failed (or no instance) · 2 usage — the same contract as `ctl`.
pub fn exit_code(outcome: &Outcome) -> i32 {
    match outcome {
        Outcome::Ok => 0,
        Outcome::Failed(_) => 1,
        Outcome::Usage(_) => 2,
    }
}

/// The running instance, as much of it as the generated CLI needs. `Live` is the real one;
/// tests supply a fake and count the calls.
pub trait Instance {
    /// Base URL of the control API, the cache key.
    fn control_url(&self) -> String;
    /// The cheap freshness probe: `GET /health` → `version`.
    fn host_version(&self) -> Result<String, String>;
    /// `GET /schema`, authenticated.
    fn fetch_schema(&self) -> Result<SchemaDocument, String>;
    /// One request; `Err` is the message to print (status and detail already in it).
    fn send(&self, req: &Request) -> Result<Value, String>;
}

struct Live<'a> {
    conn: &'a Conn,
}

impl Live<'_> {
    fn exchange(
        &self,
        method: reqwest::Method,
        req_path: &str,
        body: Option<&Value>,
    ) -> Result<Value, String> {
        let mut r = self
            .conn
            .client
            .request(method, format!("{}{req_path}", self.conn.base))
            .bearer_auth(&self.conn.token);
        if let Some(b) = body {
            r = r.json(b);
        }
        let resp = r.send().map_err(|e| format!("{req_path}: {e}"))?;
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        if !status.is_success() {
            let detail = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
                .unwrap_or(text);
            return Err(format!("{req_path}: {status} — {detail}"));
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }
}

impl Instance for Live<'_> {
    fn control_url(&self) -> String {
        self.conn.base.clone()
    }
    fn host_version(&self) -> Result<String, String> {
        let v = self.exchange(reqwest::Method::GET, "/health", None)?;
        v.get("version")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| "/health: no version in reply".to_string())
    }
    fn fetch_schema(&self) -> Result<SchemaDocument, String> {
        let v = self.exchange(reqwest::Method::GET, "/schema", None)?;
        serde_json::from_value(v).map_err(|e| format!("/schema: not a schema document: {e}"))
    }
    fn send(&self, req: &Request) -> Result<Value, String> {
        let method = match req.method {
            Verb::Get => reqwest::Method::GET,
            Verb::Post => reqwest::Method::POST,
            Verb::Put => reqwest::Method::PUT,
            Verb::Patch => reqwest::Method::PATCH,
            Verb::Delete => reqwest::Method::DELETE,
        };
        self.exchange(method, &req.path_and_query(), req.body.as_ref())
    }
}

/// The document the tree is built from, in order of preference: the cache file for this
/// control URL, the most recent cache file at all, this binary's own table. A cached
/// document that fails validation is skipped, not trusted.
pub fn load_document(control_url: Option<&str>, cache_dir: &std::path::Path) -> SchemaDocument {
    let cached = control_url
        .and_then(|u| cache::load(&cache::file_for(cache_dir, u)))
        .or_else(|| cache::latest(cache_dir));
    match cached {
        Some(c) if schema_cli::validate(&c.schema).is_ok() => c.schema,
        _ => schema_cli::builtin_document(env!("CARGO_PKG_VERSION")),
    }
}

/// Ask the instance whether the document in hand is still its own; when not (or when
/// `force`), fetch and cache the new one. `Ok(true)` when the document changed.
fn refresh_if_stale(
    instance: &dyn Instance,
    cache_dir: &std::path::Path,
    doc: &mut SchemaDocument,
    force: bool,
) -> Result<bool, String> {
    let url = instance.control_url();
    let version = instance.host_version()?;
    let file = cache::file_for(cache_dir, &url);
    let fresh = cache::load(&file)
        .map(|c| cache::is_fresh(&c, &url, &version) && c.schema == *doc)
        .unwrap_or(false);
    if fresh && !force {
        return Ok(false);
    }
    let fetched = instance.fetch_schema()?;
    schema_cli::validate(&fetched).map_err(|e| e.to_string())?;
    let entry = cache::Cached {
        control_url: url,
        host_version: fetched.host_version.clone(),
        schema: fetched,
    };
    // A cache that cannot be written is a slower CLI, not a failed command.
    let _ = cache::store(&file, &entry);
    let changed = entry.schema != *doc;
    *doc = entry.schema;
    Ok(changed)
}

/// The generated CLI, with every side channel injected so tests drive it in-process.
/// `argv[0]` is the program; `argv[1..]` the words after `avada`.
pub fn schema_main(
    argv: &[String],
    instance: Result<&dyn Instance, &str>,
    cache_dir: &std::path::Path,
    stdin: &mut dyn FnMut() -> Result<String, String>,
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
) -> Outcome {
    let words = argv.get(1..).unwrap_or(&[]);
    let mut doc = load_document(instance.ok().map(|i| i.control_url()).as_deref(), cache_dir);

    // Completion never touches the network: it answers from what is on disk.
    if words.first().map(String::as_str) == Some(complete::COMPLETE_VERB) {
        let rest = words.get(2..).unwrap_or(&[]);
        let rest = rest.strip_prefix(&["--".to_string()][..]).unwrap_or(rest);
        let tree = schema_cli::command_tree(&doc);
        for c in complete::candidates(&tree, rest) {
            let _ = writeln!(out, "{c}");
        }
        return Outcome::Ok;
    }

    let mut parsed = parse(&doc, argv, out, err);
    if let Err(Some(_)) = &parsed {
        // An unknown verb may be one the instance grew since the cache was written.
        if let Ok(i) = instance {
            if refresh_if_stale(i, cache_dir, &mut doc, false).unwrap_or(false) {
                parsed = parse(&doc, argv, out, err);
            }
        }
    }
    let matches = match parsed {
        Ok(m) => m,
        Err(None) => return Outcome::Ok,
        Err(Some(msg)) => {
            let _ = write!(err, "{msg}");
            return Outcome::Usage(msg);
        }
    };

    let (path, leaf) = invoke::matched_path(&matches);
    if path.first().map(String::as_str) == Some("completions") {
        let shell = leaf.get_one::<String>("shell").cloned().unwrap_or_default();
        return match complete::shim(&shell) {
            Some(s) => {
                let _ = write!(out, "{s}");
                Outcome::Ok
            }
            None => {
                let msg = format!("no completion shim for `{shell}`\n");
                let _ = write!(err, "{msg}");
                Outcome::Usage(msg)
            }
        };
    }

    let mut request = match invoke::build(&doc, &matches, stdin) {
        Ok(r) => r,
        Err(invoke::InvokeError::Stdin(e)) => {
            let msg = format!("reading --json from stdin: {e}\n");
            let _ = write!(err, "{msg}");
            return Outcome::Failed(msg);
        }
        Err(e) => {
            let msg = format!("{e}\n");
            let _ = write!(err, "{msg}");
            return Outcome::Usage(msg);
        }
    };

    let instance = match instance {
        Ok(i) => i,
        Err(no_instance) => {
            let msg = format!("{no_instance}\n");
            let _ = write!(err, "{msg}");
            return Outcome::Failed(msg);
        }
    };

    // The verb is about to run against a live instance: make sure it is that instance's verb.
    match refresh_if_stale(instance, cache_dir, &mut doc, request.refresh) {
        Ok(true) => {
            // The tree moved under us: parse again against the instance's own document.
            let matches = match parse(&doc, argv, out, err) {
                Ok(m) => m,
                Err(None) => return Outcome::Ok,
                Err(Some(msg)) => {
                    let _ = write!(err, "{msg}");
                    return Outcome::Usage(msg);
                }
            };
            request = match invoke::build(&doc, &matches, stdin) {
                Ok(r) => r,
                Err(e) => {
                    let msg = format!("{e}\n");
                    let _ = write!(err, "{msg}");
                    return Outcome::Usage(msg);
                }
            };
        }
        Ok(false) => {}
        Err(e) => {
            let msg = format!("{e}\n");
            let _ = write!(err, "{msg}");
            return Outcome::Failed(msg);
        }
    }

    match instance.send(&request) {
        Ok(v) => {
            let _ = writeln!(
                out,
                "{}",
                serde_json::to_string_pretty(&v).unwrap_or_default()
            );
            Outcome::Ok
        }
        Err(msg) => {
            let msg = format!("{msg}\n");
            let _ = write!(err, "{msg}");
            Outcome::Failed(msg)
        }
    }
}

/// clap's parse with its three outcomes made explicit: matches; `Err(None)` when help or
/// the version was printed (a success); `Err(Some(text))` for a usage error.
fn parse(
    doc: &SchemaDocument,
    argv: &[String],
    out: &mut dyn std::io::Write,
    _err: &mut dyn std::io::Write,
) -> Result<clap::ArgMatches, Option<String>> {
    let tree = schema_cli::command_tree(doc);
    match tree.try_get_matches_from(argv) {
        Ok(m) => Ok(m),
        Err(e) => {
            use clap::error::ErrorKind;
            match e.kind() {
                ErrorKind::DisplayHelp
                | ErrorKind::DisplayVersion
                | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
                    let _ = write!(out, "{e}");
                    Err(None)
                }
                _ => Err(Some(e.to_string())),
            }
        }
    }
}

// ---- HTTP ----------------------------------------------------------------------------------

#[tracing::instrument(level = "debug", ret, skip(conn))]
fn get(conn: &Conn, path: &str) -> std::io::Result<Value> {
    send(conn.client.get(format!("{}{path}", conn.base)), conn, path)
}

#[tracing::instrument(level = "debug", ret, skip(conn))]
fn post(conn: &Conn, path: &str, body: Value) -> std::io::Result<Value> {
    send(
        conn.client.post(format!("{}{path}", conn.base)).json(&body),
        conn,
        path,
    )
}

#[tracing::instrument(level = "debug", ret, skip(conn))]
fn patch(conn: &Conn, path: &str, body: Value) -> std::io::Result<Value> {
    send(
        conn.client
            .patch(format!("{}{path}", conn.base))
            .json(&body),
        conn,
        path,
    )
}

/// Send, and turn anything that isn't a 2xx into a message on stderr plus exit 1 — an agent
/// reading stdout should never have to tell a successful response from an error object.
#[tracing::instrument(level = "debug", ret, skip(conn))]
fn send(req: reqwest::blocking::RequestBuilder, conn: &Conn, path: &str) -> std::io::Result<Value> {
    let resp = req
        .bearer_auth(&conn.token)
        .send()
        .map_err(|e| std::io::Error::other(format!("{path}: {e}")))?;
    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    if !status.is_success() {
        let detail = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
            .unwrap_or(text);
        eprintln!("{path}: {status} — {detail}");
        std::process::exit(1);
    }
    Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
}

// ---- output --------------------------------------------------------------------------------

#[tracing::instrument(level = "debug", ret)]
fn print_json(v: Value) {
    println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
}

/// `/state` as an outline. The point is that one screenful answers "what is open, and what is
/// each thing's id" — the two questions every other verb needs answered first.
#[tracing::instrument(level = "debug", ret)]
fn print_outline(state: &Value) {
    for w in state
        .get("windows")
        .and_then(Value::as_array)
        .map_or(&[][..], |a| a)
    {
        let wid = w.get("windowId").and_then(Value::as_i64).unwrap_or(-1);
        let active = w.get("activeTabId").and_then(Value::as_str).unwrap_or("");
        println!("window {wid}");
        for t in w
            .get("tabs")
            .and_then(Value::as_array)
            .map_or(&[][..], |a| a)
        {
            let id = t.get("id").and_then(Value::as_str).unwrap_or("?");
            let mark = if id == active { "*" } else { " " };
            println!(
                "{mark} {id}  {}  [{}]",
                t.get("title").and_then(Value::as_str).unwrap_or(""),
                t.get("layout").and_then(Value::as_str).unwrap_or("")
            );
            for p in t
                .get("panes")
                .and_then(Value::as_array)
                .map_or(&[][..], |a| a)
            {
                println!(
                    "      {}  {}  ({})",
                    p.get("id").and_then(Value::as_str).unwrap_or("?"),
                    p.get("label").and_then(Value::as_str).unwrap_or(""),
                    p.get("status").and_then(Value::as_str).unwrap_or("?")
                );
            }
        }
    }
}

/// Every pane, flat, with the tab it sits in — the listing to grep when you know a pane by its
/// title and need its id.
#[tracing::instrument(level = "debug", ret)]
fn print_panes(state: &Value) {
    for w in state
        .get("windows")
        .and_then(Value::as_array)
        .map_or(&[][..], |a| a)
    {
        for t in w
            .get("tabs")
            .and_then(Value::as_array)
            .map_or(&[][..], |a| a)
        {
            let tab = t.get("id").and_then(Value::as_str).unwrap_or("?");
            for p in t
                .get("panes")
                .and_then(Value::as_array)
                .map_or(&[][..], |a| a)
            {
                println!(
                    "{}\t{}\t{}\t{}",
                    p.get("id").and_then(Value::as_str).unwrap_or("?"),
                    tab,
                    p.get("status").and_then(Value::as_str).unwrap_or("?"),
                    p.get("label").and_then(Value::as_str).unwrap_or("")
                );
            }
        }
    }
}

/// The two scheduler loops, one line each. The verb exists because the alternative to
/// "when did the restart loop last run" is reading the log or waiting out the clock: an
/// agent inside a pane, or a human over ssh with no GUI, can answer it in one command.
/// Times are shown relative to now — the absolute epoch seconds are in `ctl get /loops` for
/// anything that wants to compute with them.
#[tracing::instrument(level = "debug", ret)]
fn print_loops(v: &Value) {
    let now = crate::loops::unix_now();
    let null = Value::Null;
    println!(
        "{:<9}{:<5}{:<7}{:<13}{}",
        "loop", "on", "every", "last fired", "next fire"
    );
    for name in ["status", "restart"] {
        let l = v.get(name).unwrap_or(&null);
        let enabled = l.get("enabled").and_then(Value::as_bool).unwrap_or(false);
        let interval = l.get("intervalSecs").and_then(Value::as_u64).unwrap_or(0);
        let last = l.get("lastFiredAt").and_then(Value::as_u64);
        let next = l.get("nextFireAt").and_then(Value::as_u64);
        println!(
            "{:<9}{:<5}{:<7}{:<13}{}",
            name,
            if enabled { "yes" } else { "no" },
            if interval == 0 {
                "-".to_string()
            } else {
                span(interval)
            },
            relative(last, now),
            match next {
                Some(_) => relative(next, now),
                // A loop that is on but has not been scheduled yet (the app is still inside
                // its 60s startup grace) is "soon", not "never".
                None if enabled => "soon".to_string(),
                None => "-".to_string(),
            },
        );
    }
}

/// A duration in seconds as one coarse unit — `45s`, `15m`, `24h`, `3d`. Coarse on purpose:
/// this column answers "roughly when", and a loop's whole point is that its exact second
/// does not matter.
#[tracing::instrument(level = "debug", ret)]
fn span(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3_600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3_600),
        s => format!("{}d", s / 86_400),
    }
}

/// A unix timestamp read against `now`: `in 13m` ahead, `8m ago` behind, `never` for `None`.
/// A time that is already past while the loop is still armed reads `0s ago` rather than
/// going negative — that is the honest picture of a firing that is due and has not landed.
#[tracing::instrument(level = "debug", ret)]
fn relative(at: Option<u64>, now: u64) -> String {
    match at {
        None => "never".to_string(),
        Some(t) if t > now => format!("in {}", span(t - now)),
        Some(t) => format!("{} ago", span(now - t)),
    }
}

// ---- argument plumbing ---------------------------------------------------------------------

fn usage(msg: &str) -> ! {
    eprintln!("usage: avada ctl {msg}");
    std::process::exit(2);
}

#[tracing::instrument(level = "debug", ret)]
fn need<'a>(v: Option<&'a String>, msg: &str) -> &'a str {
    match v.map(String::as_str).filter(|s| !s.is_empty()) {
        Some(s) => s,
        None => usage(msg),
    }
}

#[tracing::instrument(level = "debug", ret)]
fn path_arg(v: Option<&String>, msg: &str) -> String {
    let p = need(v, msg);
    if p.starts_with('/') {
        p.to_string()
    } else {
        format!("/{p}")
    }
}

#[tracing::instrument(level = "debug", ret)]
fn parse_json(v: Option<&String>, msg: &str) -> Value {
    let raw = need(v, msg);
    serde_json::from_str(raw).unwrap_or_else(|e| usage(&format!("{msg}   (bad json: {e})")))
}

/// Split `<positional> [--flag value | --flag]` into the first positional and a flag map.
#[tracing::instrument(level = "debug", ret)]
fn split_flags(args: &[String], msg: &str) -> (String, BTreeMap<String, String>) {
    let (pos, flags) = split_flags_optional(args);
    match pos.into_iter().next() {
        Some(p) => (p, flags),
        None => usage(msg),
    }
}

/// The same split with no required positional. A `--flag` with no value is recorded as present
/// with an empty value, so `flags.contains_key("raw")` is the test for a bare switch.
#[tracing::instrument(level = "debug", ret)]
fn split_flags_optional(args: &[String]) -> (Vec<String>, BTreeMap<String, String>) {
    let mut pos = Vec::new();
    let mut flags = BTreeMap::new();
    let mut i = 0;
    while i < args.len() {
        if let Some(name) = args[i].strip_prefix("--") {
            let takes_value = args
                .get(i + 1)
                .is_some_and(|v| !v.starts_with("--") || name == "cmd");
            if takes_value {
                flags.insert(name.to_string(), args[i + 1].clone());
                i += 2;
            } else {
                flags.insert(name.to_string(), String::new());
                i += 1;
            }
        } else {
            pos.push(args[i].clone());
            i += 1;
        }
    }
    (pos, flags)
}

#[tracing::instrument(level = "debug", ret)]
fn put_str(cmd: &mut Value, key: &str, val: Option<&String>) {
    if let Some(v) = val.filter(|v| !v.is_empty()) {
        cmd[key] = json!(v);
    }
}

#[tracing::instrument(level = "debug", ret, skip(conn))]
fn pane_verb(conn: &Conn, ty: &str, args: &[String], msg: &str) -> std::io::Result<Value> {
    let pane = need(args.first(), msg);
    post(conn, "/command", json!({ "type": ty, "paneId": pane }))
}

#[tracing::instrument(level = "debug", ret, skip(conn))]
fn tab_verb(conn: &Conn, ty: &str, args: &[String], msg: &str) -> std::io::Result<Value> {
    let tab = need(args.first(), msg);
    post(conn, "/command", json!({ "type": ty, "tabId": tab }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn wants_ctl_only_matches_the_subcommand() {
        assert!(wants_ctl(&s(&["avada", "ctl", "panes"])));
        assert!(!wants_ctl(&s(&["avada"])));
        assert!(!wants_ctl(&s(&["avada", "-c", "ctl"])));
    }

    #[test]
    fn flags_split_from_positionals_and_bare_switches_are_present_but_empty() {
        let (pos, flags) = split_flags_optional(&s(&["p1", "--tail", "40", "--raw"]));
        assert_eq!(pos, vec!["p1".to_string()]);
        assert_eq!(flags.get("tail").map(String::as_str), Some("40"));
        assert_eq!(flags.get("raw").map(String::as_str), Some(""));
    }

    #[test]
    fn a_switch_followed_by_another_switch_does_not_swallow_it() {
        // `--wait --tail 5`: `wait` must not eat `--tail` as its value.
        let (_, flags) = split_flags_optional(&s(&["p1", "--wait", "--tail", "5"]));
        assert_eq!(flags.get("wait").map(String::as_str), Some(""));
        assert_eq!(flags.get("tail").map(String::as_str), Some("5"));
    }

    #[test]
    fn cmd_takes_its_value_even_when_the_value_looks_like_a_flag() {
        // `--cmd "--version"` is a real thing to want to run.
        let (_, flags) = split_flags_optional(&s(&["--cmd", "--version"]));
        assert_eq!(flags.get("cmd").map(String::as_str), Some("--version"));
    }

    #[test]
    fn a_span_is_shown_in_one_coarse_unit() {
        assert_eq!(span(0), "0s");
        assert_eq!(span(59), "59s");
        assert_eq!(span(900), "15m");
        assert_eq!(span(3_599), "59m");
        assert_eq!(span(86_400), "1d");
        assert_eq!(span(7_200), "2h");
    }

    #[test]
    fn a_loop_time_reads_relative_to_now_and_never_goes_negative() {
        assert_eq!(relative(None, 1_000), "never");
        assert_eq!(relative(Some(1_780), 1_000), "in 13m");
        assert_eq!(relative(Some(520), 1_000), "8m ago");
        // Due but not yet fired: past, not a negative future.
        assert_eq!(relative(Some(1_000), 1_000), "0s ago");
    }

    #[test]
    fn a_raw_path_gets_its_leading_slash() {
        assert_eq!(path_arg(Some(&"queues".to_string()), "x"), "/queues");
        assert_eq!(path_arg(Some(&"/queues".to_string()), "x"), "/queues");
    }

    // ---- schema-generated verbs ----

    use std::cell::{Cell, RefCell};
    use std::path::PathBuf;

    /// An instance that answers from memory and counts what the CLI asked of it.
    struct Fake {
        url: String,
        version: String,
        doc: SchemaDocument,
        reply: Result<Value, String>,
        sent: RefCell<Vec<Request>>,
        probes: Cell<usize>,
        fetches: Cell<usize>,
    }

    impl Fake {
        fn new(doc: SchemaDocument) -> Self {
            Fake {
                url: "http://127.0.0.1:4041".into(),
                version: doc.host_version.clone(),
                doc,
                reply: Ok(json!({ "ok": true })),
                sent: RefCell::new(Vec::new()),
                probes: Cell::new(0),
                fetches: Cell::new(0),
            }
        }
    }

    impl Instance for Fake {
        fn control_url(&self) -> String {
            self.url.clone()
        }
        fn host_version(&self) -> Result<String, String> {
            self.probes.set(self.probes.get() + 1);
            Ok(self.version.clone())
        }
        fn fetch_schema(&self) -> Result<SchemaDocument, String> {
            self.fetches.set(self.fetches.get() + 1);
            Ok(self.doc.clone())
        }
        fn send(&self, req: &Request) -> Result<Value, String> {
            self.sent.borrow_mut().push(req.clone());
            self.reply.clone()
        }
    }

    fn builtin() -> SchemaDocument {
        schema_cli::builtin_document("1.0.0")
    }

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("avada-ctl-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn no_stdin() -> impl FnMut() -> Result<String, String> {
        || Err("no stdin in tests".to_string())
    }

    /// Drive `schema_main` with an in-memory instance (or none) and an empty cache dir.
    fn drive(
        argv: &[&str],
        instance: Result<&dyn Instance, &str>,
        dir: &std::path::Path,
    ) -> (Outcome, String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut stdin = no_stdin();
        let outcome = schema_main(&s(argv), instance, dir, &mut stdin, &mut out, &mut err);
        (
            outcome,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn every_hand_verb_is_documented_in_usage() {
        for v in HAND_VERBS {
            assert!(
                USAGE.contains(&format!("    {v}")) || USAGE.contains(&format!("    {v} ")),
                "{v} is a hand-written verb but USAGE does not list it"
            );
        }
    }

    #[test]
    fn hand_written_verbs_win_over_schema_ones() {
        let doc = builtin();
        // `health` exists on both sides: the hand-written one answers.
        assert!(doc.routes.iter().any(|r| r.method == "health"));
        assert_eq!(classify_ctl_verb("health", &doc), Verdict::Hand);
        assert_eq!(classify_ctl_verb("panes", &doc), Verdict::Hand);
        // Schema-only names fill in the rest.
        assert!(doc.routes.iter().any(|r| r.method.starts_with("tokens.")));
        assert_eq!(classify_ctl_verb("tokens", &doc), Verdict::Schema);
        assert_eq!(classify_ctl_verb("m", &doc), Verdict::Schema);
        assert_eq!(classify_ctl_verb("completions", &doc), Verdict::Schema);
        assert_eq!(classify_ctl_verb("no-such-verb", &doc), Verdict::Unknown);
    }

    #[test]
    fn wants_schema_cli_claims_generated_verbs_only() {
        assert!(wants_schema_cli(&s(&["avada", "tokens", "mint"])));
        assert!(wants_schema_cli(&s(&["avada", "completions", "zsh"])));
        assert!(!wants_schema_cli(&s(&["avada", "ctl", "health"])));
        assert!(!wants_schema_cli(&s(&["avada", "--help"])));
        assert!(!wants_schema_cli(&s(&["avada"])));
        assert!(!wants_schema_cli(&s(&["avada", "/some/dir"])));
    }

    #[test]
    fn a_schema_verb_builds_the_request_and_prints_the_reply_pretty() {
        let fake = Fake::new(builtin());
        let dir = tmp("send");
        let (outcome, out, err) = drive(
            &["avada", "panes", "output", "p 1", "--tail", "3"],
            Ok(&fake),
            &dir,
        );
        assert_eq!(outcome, Outcome::Ok, "stderr: {err}");
        let sent = fake.sent.borrow();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].method, Verb::Get);
        assert_eq!(sent[0].path_and_query(), "/panes/p%201/output?tail=3");
        assert_eq!(out, "{\n  \"ok\": true\n}\n");
        assert_eq!(exit_code(&outcome), 0);
        // The instance's schema is now cached for the next run.
        let cached = cache::load(&cache::file_for(&dir, &fake.url)).expect("cached");
        assert_eq!(cached.host_version, "1.0.0");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn exit_codes_follow_the_ctl_contract() {
        assert_eq!(exit_code(&Outcome::Ok), 0);
        assert_eq!(exit_code(&Outcome::Failed("x".into())), 1);
        assert_eq!(exit_code(&Outcome::Usage("x".into())), 2);

        let dir = tmp("exit");
        // usage: a verb that is not there, a flag that is not there, JSON that is not JSON
        let fake = Fake::new(builtin());
        let (o, _, err) = drive(&["avada", "no-such-verb"], Ok(&fake), &dir);
        assert!(matches!(o, Outcome::Usage(_)), "{err}");
        assert_eq!(exit_code(&o), 2);
        let (o, _, _) = drive(&["avada", "health", "--bogus"], Ok(&fake), &dir);
        assert!(matches!(o, Outcome::Usage(_)));
        let (o, _, err) = drive(
            &["avada", "tokens", "mint", "--json", "{not json"],
            Ok(&fake),
            &dir,
        );
        assert!(matches!(o, Outcome::Usage(_)), "{err}");
        assert!(fake.sent.borrow().is_empty(), "usage errors send nothing");

        // request failed: the server said no
        let mut refused = Fake::new(builtin());
        refused.reply = Err("/health: 403 Forbidden — scope".into());
        let (o, _, err) = drive(&["avada", "health"], Ok(&refused), &dir);
        assert_eq!(
            o,
            Outcome::Failed("/health: 403 Forbidden — scope\n".into())
        );
        assert_eq!(exit_code(&o), 1);
        assert_eq!(err, "/health: 403 Forbidden — scope\n");

        // no instance: a route fails with the connect message, exit 1
        let (o, _, err) = drive(&["avada", "health"], Err("no running control API"), &dir);
        assert!(matches!(o, Outcome::Failed(_)));
        assert_eq!(exit_code(&o), 1);
        assert!(err.contains("no running control API"), "{err}");

        // ok
        let (o, _, _) = drive(&["avada", "health"], Ok(&fake), &dir);
        assert_eq!(exit_code(&o), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn help_and_completions_work_with_no_instance_and_never_probe_one() {
        let dir = tmp("help");
        let (o, out, _) = drive(&["avada", "--help"], Err("down"), &dir);
        assert_eq!(o, Outcome::Ok);
        assert!(out.contains("tokens"), "{out}");
        let (o, out, _) = drive(&["avada", "completions", "zsh"], Err("down"), &dir);
        assert_eq!(o, Outcome::Ok);
        assert!(out.starts_with("#compdef avada"));
        let (o, out, _) = drive(
            &["avada", "__complete", "bash", "--", "to"],
            Err("down"),
            &dir,
        );
        assert_eq!(o, Outcome::Ok);
        assert_eq!(out, "tokens\n");
        let (o, _, err) = drive(&["avada", "completions", "powershell"], Err("down"), &dir);
        assert!(matches!(o, Outcome::Usage(_)), "{err}");

        // With an instance present, neither help nor completion asks it anything.
        let fake = Fake::new(builtin());
        drive(&["avada", "panes", "--help"], Ok(&fake), &dir);
        drive(&["avada", "completions", "fish"], Ok(&fake), &dir);
        drive(
            &["avada", "__complete", "fish", "--", "panes", ""],
            Ok(&fake),
            &dir,
        );
        assert_eq!(fake.probes.get(), 0);
        assert_eq!(fake.fetches.get(), 0);
        assert!(fake.sent.borrow().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_builtin_route_is_visible_in_generated_help() {
        // Fails when a route lands in `core_routes()` but the generated `--help` never
        // shows it: the parent's help must list the leaf.
        let dir = tmp("coverage");
        let doc = builtin();
        let mut checked = 0;
        for r in doc.routes.iter().filter(|r| schema_cli::is_placeable(r)) {
            let path = schema_cli::command_path(r);
            let (parent, leaf) = path.split_at(path.len() - 1);
            let mut argv: Vec<&str> = vec!["avada"];
            argv.extend(parent.iter().map(String::as_str));
            argv.push("--help");
            let (o, out, err) = drive(&argv, Err("down"), &dir);
            assert_eq!(o, Outcome::Ok, "{}: {err}", r.method);
            assert!(
                out.lines().any(|l| l.trim_start().starts_with(&leaf[0])),
                "{} is in core_routes() but `avada {} --help` does not list `{}`:\n{out}",
                r.method,
                parent.join(" "),
                leaf[0]
            );
            checked += 1;
        }
        assert!(
            checked > 10,
            "the core table has shrunk to {checked} routes"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_moved_host_version_refreshes_the_cache_and_a_still_one_does_not() {
        let dir = tmp("refresh");
        let mut old = builtin();
        old.host_version = "1.0.0".into();
        let mut new = builtin();
        new.host_version = "2.0.0".into();

        let fake = Fake::new(new.clone());
        cache::store(
            &cache::file_for(&dir, &fake.url),
            &cache::Cached {
                control_url: fake.url.clone(),
                host_version: "1.0.0".into(),
                schema: old.clone(),
            },
        )
        .unwrap();
        let (o, _, err) = drive(&["avada", "health"], Ok(&fake), &dir);
        assert_eq!(o, Outcome::Ok, "{err}");
        assert_eq!(fake.probes.get(), 1, "one cheap probe before the request");
        assert_eq!(fake.fetches.get(), 1, "a moved version fetches the schema");
        let cached = cache::load(&cache::file_for(&dir, &fake.url)).unwrap();
        assert_eq!(cached.host_version, "2.0.0");
        assert_eq!(cached.schema, new);

        // Same version, same document: served from the cache, no fetch.
        let (o, _, _) = drive(&["avada", "health"], Ok(&fake), &dir);
        assert_eq!(o, Outcome::Ok);
        assert_eq!(fake.fetches.get(), 1);
        assert_eq!(fake.probes.get(), 2);

        // `schema --refresh` fetches regardless.
        let (o, _, _) = drive(&["avada", "schema", "--refresh"], Ok(&fake), &dir);
        assert_eq!(o, Outcome::Ok);
        assert_eq!(fake.fetches.get(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn json_body_flags_merge_over_a_json_object() {
        let fake = Fake::new(builtin());
        let dir = tmp("json");
        let (o, _, err) = drive(
            &[
                "avada",
                "tokens",
                "mint",
                "--json",
                r#"{"scope":"panes:read","note":"x"}"#,
            ],
            Ok(&fake),
            &dir,
        );
        assert_eq!(o, Outcome::Ok, "{err}");
        let sent = fake.sent.borrow();
        assert_eq!(sent[0].method, Verb::Post);
        assert_eq!(sent[0].path, "/tokens");
        let body = sent[0].body.as_ref().unwrap();
        assert_eq!(body["scope"], "panes:read");
        assert_eq!(body["note"], "x");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
