//! `SchemaDocument` → clap `Command` tree (plan §2: "CLI generated from `GET /schema`").
//!
//! Pure: no I/O, no globals. The rules, so `avada --help` and the completion shim agree
//! with the docs (`docs/cli.md`):
//!
//! - a route's dotted `method` is its command path: `tokens.mint` → `avada tokens mint`;
//! - a module route (`module: Some(id)`) lives under `avada m <owner>/<repo> <method…>`,
//!   so a module can never shadow a core verb;
//! - every `Path` param is a required positional, in path order; every `Query` or `Body`
//!   param is a `--kebab-name` flag (`ttlMs` → `--ttl-ms`) whose value is typed by `kind`
//!   (`integer` parses as `i64`, `boolean` is a bare switch, anything else is text that
//!   `object`/`array` later parse as JSON); the descriptor `summary` is the help line;
//! - a `string` param whose summary reads `a | b | c` is an enum: those are its only values;
//! - a non-GET route also takes `--json <text>` (`-` reads stdin) for an arbitrary body;
//! - the [`RESERVED`] top-level names are the CLI's own; a route whose first segment is
//!   one of them is dropped rather than merged (the reserved command wins);
//! - `schema` (the route) also takes `--refresh` so the disk cache can be busted.

use avada_module_sdk::descriptor::{
    validate_table, DescriptorError, Param, ParamLocation, RouteDescriptor, SchemaDocument, Verb,
};
use clap::builder::PossibleValuesParser;
use clap::{Arg, ArgAction, Command};
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

/// clap's builder wants `&'static str` names (core's `clap` is declared without the
/// `string` feature, and Cargo.toml is frozen this wave), and ours come from a runtime
/// document. Each distinct name is leaked exactly once, however many trees a process
/// builds: a few hundred short strings for the life of a CLI run.
pub fn intern(s: &str) -> &'static str {
    static POOL: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let mut pool = POOL
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(hit) = pool.get(s) {
        return hit;
    }
    let leaked: &'static str = Box::leak(s.to_string().into_boxed_str());
    pool.insert(leaked);
    leaked
}

/// Top-level names the CLI defines itself; a schema route cannot claim one.
pub const RESERVED: &[&str] = &["m", "completions", "__complete"];

/// The `m` namespace: `avada m <owner>/<repo> …`.
pub const MODULE_NS: &str = "m";
/// Arg id of the `--json` body override.
pub const JSON_ARG: &str = "__json";
/// Arg id of `schema --refresh`.
pub const REFRESH_ARG: &str = "__refresh";
/// Command name of the schema route/verb.
pub const SCHEMA_VERB: &str = "schema";

/// Why a document is unusable by the generator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaCliError {
    /// A route in the document fails the SDK's own rules (bad method, duplicate, ...).
    Descriptor(DescriptorError),
    /// The document names another product.
    WrongProduct(String),
}

impl std::fmt::Display for SchemaCliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SchemaCliError::Descriptor(e) => write!(f, "schema: {e}"),
            SchemaCliError::WrongProduct(p) => write!(
                f,
                "schema describes `{p}`, not {}",
                avada_module_sdk::PRODUCT_NAME
            ),
        }
    }
}
impl std::error::Error for SchemaCliError {}

/// The document this binary would serve itself: the core table, no modules. What the CLI
/// falls back to when there is neither a cache file nor an instance to ask.
pub fn builtin_document(host_version: &str) -> SchemaDocument {
    crate::control::schema::SchemaRegistry::core().document(host_version)
}

/// Reject a document the generator cannot trust: every route must pass the SDK's table
/// rules and the product must be ours. A document from a stranger's server, or a corrupt
/// cache file that still deserialised, stops here.
pub fn validate(doc: &SchemaDocument) -> Result<(), SchemaCliError> {
    if doc.product != avada_module_sdk::PRODUCT_NAME {
        return Err(SchemaCliError::WrongProduct(doc.product.clone()));
    }
    validate_table(&doc.routes).map_err(SchemaCliError::Descriptor)
}

/// The command path a route is reachable at: `["tokens", "mint"]`, or
/// `["m", "acme/files", "files", "tree"]` for a module route.
pub fn command_path(route: &RouteDescriptor) -> Vec<String> {
    let mut path = Vec::new();
    if let Some(id) = &route.module {
        path.push(MODULE_NS.to_string());
        path.push(id.as_str().to_string());
    }
    path.extend(route.method.split('.').map(str::to_string));
    path
}

/// Whether a route lands in the tree at all (see [`RESERVED`]).
pub fn is_placeable(route: &RouteDescriptor) -> bool {
    route.module.is_some() || !RESERVED.contains(&route.method.split('.').next().unwrap_or(""))
}

/// `ttlMs` → `ttl-ms`, `waitForIdle` → `wait-for-idle`, `path` → `path`.
pub fn flag_name(param: &str) -> String {
    let mut out = String::with_capacity(param.len() + 4);
    for c in param.chars() {
        if c.is_ascii_uppercase() {
            out.push('-');
            out.push(c.to_ascii_lowercase());
        } else if c == '_' {
            out.push('-');
        } else {
            out.push(c);
        }
    }
    out
}

/// The enum values a `string` param admits, read from a summary of the form `a | b | c`.
pub fn enum_values(param: &Param) -> Option<Vec<String>> {
    if param.kind != "string" {
        return None;
    }
    let parts: Vec<&str> = param.summary.split('|').map(str::trim).collect();
    if parts.len() < 2 {
        return None;
    }
    let word = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    };
    if parts.iter().all(|p| word(p)) {
        Some(parts.into_iter().map(str::to_string).collect())
    } else {
        None
    }
}

/// Whether the route takes `--json`.
pub fn takes_json(route: &RouteDescriptor) -> bool {
    route.verb != Verb::Get
}

/// One node of the tree while it is being assembled.
#[derive(Default)]
struct Node {
    route: Option<RouteDescriptor>,
    children: Vec<(String, Node)>,
}

impl Node {
    fn child(&mut self, name: &str) -> &mut Node {
        if let Some(i) = self.children.iter().position(|(n, _)| n == name) {
            &mut self.children[i].1
        } else {
            self.children.push((name.to_string(), Node::default()));
            &mut self.children.last_mut().unwrap().1
        }
    }

    fn insert(&mut self, path: &[String], route: RouteDescriptor) {
        match path {
            [] => {
                // First declaration wins; the SDK already refuses duplicates.
                if self.route.is_none() {
                    self.route = Some(route);
                }
            }
            [head, rest @ ..] => self.child(head).insert(rest, route),
        }
    }
}

/// Build the whole tree. The root is named `avada`, carries the host version, and has one
/// subcommand per top-level segment plus the reserved ones. Call [`validate`] first if the
/// document came from anywhere but this process.
pub fn command_tree(doc: &SchemaDocument) -> Command {
    let mut root = Node::default();
    for route in doc.routes.iter().filter(|r| is_placeable(r)) {
        root.insert(&command_path(route), route.clone());
    }
    let mut cmd = Command::new("avada")
        .about("Drive the running avada instance over its control API")
        .long_about(format!(
            "Drive the running avada instance over its control API.\n\n\
             Verbs come from the instance's GET /schema (host {}, contract v{}); \
             `avada schema --refresh` re-reads it, `avada completions <shell>` prints a \
             completion shim.",
            doc.host_version, doc.contract_version
        ))
        .version(intern(&doc.host_version))
        .disable_help_subcommand(true)
        .subcommand_required(true)
        .arg_required_else_help(true);
    for (name, node) in root.children {
        cmd = cmd.subcommand(to_command(&name, node));
    }
    cmd = cmd.subcommand(
        Command::new("completions")
            .about("Print a shell completion shim (source it, or install it)")
            .arg(
                Arg::new("shell")
                    .required(true)
                    .value_parser(PossibleValuesParser::new(["bash", "zsh", "fish"]))
                    .help("bash | zsh | fish"),
            ),
    );
    cmd = cmd.subcommand(
        Command::new("__complete")
            .hide(true)
            .about("Answer a completion request from the shim")
            .arg(Arg::new("shell").required(true))
            .arg(
                Arg::new("words")
                    .num_args(0..)
                    .trailing_var_arg(true)
                    .allow_hyphen_values(true),
            ),
    );
    cmd.build();
    cmd
}

fn to_command(name: &str, node: Node) -> Command {
    let mut cmd = Command::new(intern(name)).disable_help_subcommand(true);
    if name == MODULE_NS {
        cmd = cmd.about("Verbs of an installed module: m <owner>/<repo> <verb…>");
    }
    let has_children = !node.children.is_empty();
    match node.route {
        Some(route) => {
            cmd = cmd.about(route.summary.clone());
            cmd = add_route_args(cmd, &route);
            if has_children {
                // `a` is a verb and `a b` is another: args and subcommands must not mix.
                cmd = cmd.args_conflicts_with_subcommands(true);
            }
        }
        None => {
            if has_children {
                cmd = cmd.subcommand_required(true).arg_required_else_help(true);
            }
        }
    }
    for (child, sub) in node.children {
        cmd = cmd.subcommand(to_command(&child, sub));
    }
    cmd
}

fn add_route_args(mut cmd: Command, route: &RouteDescriptor) -> Command {
    let mut seen: Vec<&str> = Vec::new();
    // Positionals in PATH order, not declaration order: `avada queues tasks enqueue <queue>`
    // reads the way the path does.
    for name in route.path_params() {
        if let Some(p) = route
            .params
            .iter()
            .find(|p| p.location == ParamLocation::Path && p.name == name)
        {
            if seen.contains(&p.name.as_str()) {
                continue;
            }
            seen.push(&p.name);
            cmd = cmd.arg(
                Arg::new(intern(&p.name))
                    .required(true)
                    .value_name(intern(&p.name.to_ascii_uppercase()))
                    .help(p.summary.clone()),
            );
        }
    }
    for p in route
        .params
        .iter()
        .filter(|p| p.location != ParamLocation::Path)
    {
        if seen.contains(&p.name.as_str()) || p.name == JSON_ARG || p.name == REFRESH_ARG {
            continue;
        }
        seen.push(&p.name);
        let mut arg = Arg::new(intern(&p.name))
            .long(intern(&flag_name(&p.name)))
            .help(p.summary.clone());
        // A required body member may arrive inside `--json` instead of as its own flag; a
        // required query parameter has no such alternative.
        let require = |arg: Arg| -> Arg {
            if !p.required {
                arg
            } else if p.location == ParamLocation::Body && takes_json(route) {
                arg.required_unless_present(JSON_ARG)
            } else {
                arg.required(true)
            }
        };
        arg = match p.kind.as_str() {
            "boolean" => arg.action(ArgAction::SetTrue),
            "integer" => require(arg.value_parser(clap::value_parser!(i64)).value_name("N")),
            _ => {
                let arg = require(arg);
                match enum_values(p) {
                    Some(values) => arg
                        .value_parser(PossibleValuesParser::new(
                            values.iter().map(|v| intern(v)).collect::<Vec<_>>(),
                        ))
                        .value_name("VALUE"),
                    None => arg.value_name(match p.kind.as_str() {
                        "object" | "array" => "JSON",
                        _ => "TEXT",
                    }),
                }
            }
        };
        cmd = cmd.arg(arg);
    }
    if takes_json(route) {
        cmd = cmd.arg(
            Arg::new(JSON_ARG)
                .long("json")
                .value_name("TEXT")
                .help("Request body as JSON (`-` reads stdin); flags override its members"),
        );
    }
    if route.module.is_none() && route.method == SCHEMA_VERB {
        cmd = cmd.arg(
            Arg::new(REFRESH_ARG)
                .long("refresh")
                .action(ArgAction::SetTrue)
                .help("Re-read the schema from the instance and rewrite the disk cache"),
        );
    }
    cmd
}

/// Walk the tree by command path; `None` when a segment is missing.
pub fn find<'a>(root: &'a Command, path: &[String]) -> Option<&'a Command> {
    let mut node = root;
    for seg in path {
        node = node.find_subcommand(seg)?;
    }
    Some(node)
}

/// The route a command path names, if that path is a leaf (or an interior verb).
pub fn route_at<'a>(doc: &'a SchemaDocument, path: &[String]) -> Option<&'a RouteDescriptor> {
    doc.routes
        .iter()
        .filter(|r| is_placeable(r))
        .find(|r| command_path(r) == path)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use avada_module_sdk::caps::Capability;
    use avada_module_sdk::descriptor::Scope;
    use avada_module_sdk::manifest::ModuleId;

    pub(crate) fn p(
        name: &str,
        location: ParamLocation,
        kind: &str,
        required: bool,
        s: &str,
    ) -> Param {
        Param {
            name: name.into(),
            location,
            kind: kind.into(),
            required,
            summary: s.into(),
        }
    }

    pub(crate) fn route(
        method: &str,
        path: &str,
        verb: Verb,
        params: Vec<Param>,
    ) -> RouteDescriptor {
        RouteDescriptor {
            method: method.into(),
            path: path.into(),
            verb,
            capability: Some(Capability::FsRead),
            summary: format!("summary of {method}"),
            params,
            scope: Scope::Token,
            module: None,
            response: None,
        }
    }

    pub(crate) fn module_route(id: &str, method: &str, path: &str) -> RouteDescriptor {
        let mut r = route(method, path, Verb::Get, vec![]);
        r.module = Some(ModuleId::new(id).unwrap());
        r
    }

    pub(crate) fn doc(routes: Vec<RouteDescriptor>) -> SchemaDocument {
        SchemaDocument {
            contract_version: 1,
            product: avada_module_sdk::PRODUCT_NAME.into(),
            host_version: "9.9.9".into(),
            routes,
            rpcs: vec![],
            modules: vec![],
        }
    }

    /// A small document exercising every rule: path/query/body params, nested dots, an
    /// interior verb with a child, a module, an enum, and a boolean.
    pub(crate) fn sample() -> SchemaDocument {
        use ParamLocation::{Body, Path, Query};
        doc(vec![
            route("health", "/health", Verb::Get, vec![]),
            route(
                "panes.output",
                "/panes/{id}/output",
                Verb::Get,
                vec![
                    p("id", Path, "string", true, "Pane id"),
                    p("mode", Query, "string", false, "screen | raw"),
                    p("tail", Query, "integer", false, "Trailing lines"),
                    p("waitForIdle", Query, "boolean", false, "Block"),
                ],
            ),
            route(
                "tokens.mint",
                "/tokens",
                Verb::Post,
                vec![
                    p(
                        "scope",
                        Body,
                        "object",
                        true,
                        "windowIds | tabIds | paneIds",
                    ),
                    p("ttlMs", Body, "integer", false, "Lifetime"),
                ],
            ),
            route(
                "queues.tasks.enqueue",
                "/queues/{queue}/tasks",
                Verb::Post,
                vec![
                    p("queue", Path, "string", true, "Queue"),
                    p("payload", Body, "object", true, "Task body"),
                ],
            ),
            route("schema", "/schema", Verb::Get, vec![]),
            // `a` is a verb and `a.b` too.
            route("state", "/state", Verb::Get, vec![]),
            route("state.tree", "/state/tree", Verb::Get, vec![]),
            module_route("acme/files", "files.tree", "/tree"),
            module_route("acme/files", "files.blame", "/blame"),
            module_route("acme/git", "log", "/log"),
        ])
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn dots_become_nested_subcommands_and_modules_sit_under_m() {
        let tree = command_tree(&sample());
        assert!(find(&tree, &s(&["health"])).is_some());
        assert!(find(&tree, &s(&["panes", "output"])).is_some());
        assert!(find(&tree, &s(&["queues", "tasks", "enqueue"])).is_some());
        assert!(find(&tree, &s(&["m", "acme/files", "files", "tree"])).is_some());
        assert!(find(&tree, &s(&["m", "acme/files", "files", "blame"])).is_some());
        assert!(find(&tree, &s(&["m", "acme/git", "log"])).is_some());
        assert!(
            find(&tree, &s(&["files"])).is_none(),
            "module verbs are namespaced"
        );
        assert!(find(&tree, &s(&["completions"])).is_some());
        let hidden = find(&tree, &s(&["__complete"])).unwrap();
        assert!(hidden.is_hide_set());
    }

    #[test]
    fn path_params_are_positionals_and_the_rest_are_typed_flags() {
        let tree = command_tree(&sample());
        let out = find(&tree, &s(&["panes", "output"])).unwrap();
        let args: Vec<_> = out.get_arguments().collect();
        let id = args.iter().find(|a| a.get_id() == "id").unwrap();
        assert!(id.is_positional() && id.is_required_set());
        let tail = args.iter().find(|a| a.get_id() == "tail").unwrap();
        assert_eq!(tail.get_long(), Some("tail"));
        let wait = args.iter().find(|a| a.get_id() == "waitForIdle").unwrap();
        assert_eq!(wait.get_long(), Some("wait-for-idle"));
        assert!(!wait.get_action().takes_values(), "boolean is a switch");
        let mode = args.iter().find(|a| a.get_id() == "mode").unwrap();
        let values: Vec<String> = mode
            .get_possible_values()
            .iter()
            .map(|v| v.get_name().to_string())
            .collect();
        assert_eq!(values, ["screen", "raw"]);
        assert!(
            args.iter().all(|a| a.get_id() != JSON_ARG),
            "GET takes no --json"
        );
        // Help text is the descriptor summary.
        assert_eq!(
            id.get_help().map(|h| h.to_string()).as_deref(),
            Some("Pane id")
        );

        let mint = find(&tree, &s(&["tokens", "mint"])).unwrap();
        let args: Vec<_> = mint.get_arguments().collect();
        assert!(args.iter().any(|a| a.get_id() == JSON_ARG));
        let ttl = args.iter().find(|a| a.get_id() == "ttlMs").unwrap();
        assert_eq!(ttl.get_long(), Some("ttl-ms"));
        let scope = args.iter().find(|a| a.get_id() == "scope").unwrap();
        // A required body member: its own flag or `--json` must supply it.
        assert!(tree
            .clone()
            .try_get_matches_from(s(&["avada", "tokens", "mint"]))
            .is_err());
        assert!(tree
            .clone()
            .try_get_matches_from(s(&["avada", "tokens", "mint", "--scope", "{}"]))
            .is_ok());
        assert!(tree
            .clone()
            .try_get_matches_from(s(&["avada", "tokens", "mint", "--json", "{}"]))
            .is_ok());
        assert!(
            scope.get_possible_values().is_empty(),
            "an object summary is not an enum"
        );
        // The typed integer flag rejects text and accepts a number.
        assert!(tree
            .clone()
            .try_get_matches_from(s(&[
                "avada", "tokens", "mint", "--scope", "{}", "--ttl-ms", "x"
            ]))
            .is_err());
        assert!(tree
            .clone()
            .try_get_matches_from(s(&[
                "avada", "tokens", "mint", "--scope", "{}", "--ttl-ms", "5"
            ]))
            .is_ok());
    }

    #[test]
    fn schema_verb_takes_refresh_and_reserved_names_win() {
        let tree = command_tree(&sample());
        let schema = find(&tree, &s(&["schema"])).unwrap();
        assert!(schema.get_arguments().any(|a| a.get_id() == REFRESH_ARG));
        // A route that claims a reserved name is dropped; the CLI's command survives.
        let mut d = sample();
        d.routes.push(route("completions", "/x", Verb::Get, vec![]));
        d.routes.push(route("m.thing", "/y", Verb::Get, vec![]));
        let tree = command_tree(&d);
        let c = find(&tree, &s(&["completions"])).unwrap();
        assert!(c.get_arguments().any(|a| a.get_id() == "shell"));
        assert!(find(&tree, &s(&["m", "thing"])).is_none());
        assert!(route_at(&d, &s(&["completions"])).is_none());
    }

    #[test]
    fn an_interior_verb_keeps_its_children_and_colliding_params_do_not_panic() {
        let tree = command_tree(&sample());
        let state = find(&tree, &s(&["state"])).unwrap();
        assert!(state.find_subcommand("tree").is_some());
        assert!(tree
            .clone()
            .try_get_matches_from(s(&["avada", "state"]))
            .is_ok());
        assert!(tree
            .clone()
            .try_get_matches_from(s(&["avada", "state", "tree"]))
            .is_ok());
        // Same name in path and body: the positional wins, the flag is skipped, no panic.
        use ParamLocation::{Body, Path};
        let d = doc(vec![route(
            "x.y",
            "/x/{id}",
            Verb::Post,
            vec![
                p("id", Path, "string", true, "a"),
                p("id", Body, "string", false, "b"),
                p("__json", Body, "string", false, "c"),
            ],
        )]);
        let tree = command_tree(&d);
        let y = find(&tree, &s(&["x", "y"])).unwrap();
        assert_eq!(y.get_arguments().filter(|a| a.get_id() == "id").count(), 1);
        assert_eq!(
            y.get_arguments().filter(|a| a.get_id() == JSON_ARG).count(),
            1
        );
    }

    #[test]
    fn malformed_documents_are_rejected() {
        let mut d = sample();
        assert_eq!(validate(&d), Ok(()));
        d.routes.push(route("health", "/dup", Verb::Get, vec![]));
        assert!(matches!(
            validate(&d),
            Err(SchemaCliError::Descriptor(DescriptorError::Duplicate(_)))
        ));
        let mut d = sample();
        d.routes.push(route("Bad-Name", "/z", Verb::Get, vec![]));
        assert!(matches!(
            validate(&d),
            Err(SchemaCliError::Descriptor(DescriptorError::BadMethod(_)))
        ));
        let mut d = sample();
        d.product = "other".into();
        assert!(matches!(validate(&d), Err(SchemaCliError::WrongProduct(_))));
        // A body that is not a document at all.
        assert!(serde_json::from_str::<SchemaDocument>(r#"{"routes": 3}"#).is_err());
    }

    #[test]
    fn flag_names_and_enums() {
        assert_eq!(flag_name("ttlMs"), "ttl-ms");
        assert_eq!(flag_name("waitForIdle"), "wait-for-idle");
        assert_eq!(flag_name("snake_case"), "snake-case");
        assert_eq!(flag_name("path"), "path");
        assert_eq!(
            enum_values(&p(
                "m",
                ParamLocation::Query,
                "string",
                false,
                "screen | raw"
            )),
            Some(vec!["screen".into(), "raw".into()])
        );
        assert_eq!(
            enum_values(&p(
                "m",
                ParamLocation::Query,
                "string",
                false,
                "Trailing lines"
            )),
            None
        );
        assert_eq!(
            enum_values(&p("m", ParamLocation::Query, "integer", false, "a | b")),
            None
        );
        assert_eq!(
            enum_values(&p("m", ParamLocation::Query, "string", false, "a | b c")),
            None
        );
    }

    #[test]
    fn every_core_route_is_placed_and_visible_in_help() {
        let doc = crate::control::schema::SchemaRegistry::core().document("0.0.1");
        assert_eq!(validate(&doc), Ok(()));
        let tree = command_tree(&doc);
        for r in &doc.routes {
            let path = command_path(r);
            let node = find(&tree, &path).unwrap_or_else(|| panic!("{} missing", r.method));
            assert_eq!(
                node.get_about().map(|a| a.to_string()),
                Some(r.summary.clone())
            );
            // The parent's help lists it by name.
            let parent = find(&tree, &path[..path.len() - 1]).unwrap();
            let help = parent.clone().render_long_help().to_string();
            assert!(
                help.contains(path.last().unwrap()),
                "{} not in help of {:?}:\n{help}",
                r.method,
                &path[..path.len() - 1]
            );
        }
    }
}
