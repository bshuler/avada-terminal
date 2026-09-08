//! clap `ArgMatches` → a [`Request`] the app executes over `control_cli::Conn`.
//!
//! Pure: the only side channel is the `read_stdin` closure a caller supplies for
//! `--json -`, so tests can feed a string and the binary can read the pipe.

use avada_module_sdk::descriptor::{ParamLocation, RouteDescriptor, SchemaDocument, Verb};
use clap::ArgMatches;
use serde_json::{Map, Value};

use super::schema_cli::{is_placeable, takes_json, JSON_ARG, REFRESH_ARG};

/// One HTTP call against the control API, still to be sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// HTTP verb of the route.
    pub method: Verb,
    /// Path with `{params}` substituted (percent-encoded), module routes already mounted
    /// under `/m/<owner>/<repo>`.
    pub path: String,
    /// Query pairs in flag order (values raw; the sender encodes them).
    pub query: Vec<(String, String)>,
    /// JSON body, when the route is not a GET and something was given.
    pub body: Option<Value>,
    /// `schema --refresh` was asked for.
    pub refresh: bool,
}

impl Request {
    /// Path plus encoded query string, ready to append to the base URL.
    pub fn path_and_query(&self) -> String {
        if self.query.is_empty() {
            return self.path.clone();
        }
        let q: Vec<String> = self
            .query
            .iter()
            .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
            .collect();
        format!("{}?{}", self.path, q.join("&"))
    }
}

/// Why matches could not be turned into a request. Every variant is a usage error
/// (exit 2) except `Stdin`, which is an I/O failure (exit 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvokeError {
    /// The matched command path is not a route (an interior namespace, or a CLI-own verb).
    NotARoute(Vec<String>),
    /// A `--json` value or an `object`/`array` flag did not parse as JSON.
    BadJson { arg: String, detail: String },
    /// `--json` was given something other than an object, so flags cannot merge into it.
    BodyNotObject,
    /// `--json -` could not be read.
    Stdin(String),
}

impl std::fmt::Display for InvokeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InvokeError::NotARoute(path) => {
                write!(f, "`{}` is not a request; pick a verb", path.join(" "))
            }
            InvokeError::BadJson { arg, detail } => write!(f, "--{arg}: not JSON: {detail}"),
            InvokeError::BodyNotObject => {
                write!(f, "--json must be an object when other flags are given")
            }
            InvokeError::Stdin(e) => write!(f, "reading --json from stdin: {e}"),
        }
    }
}
impl std::error::Error for InvokeError {}

/// The command path the matches took: every nested subcommand name.
pub fn matched_path(matches: &ArgMatches) -> (Vec<String>, &ArgMatches) {
    let mut path = Vec::new();
    let mut m = matches;
    while let Some((name, sub)) = m.subcommand() {
        path.push(name.to_string());
        m = sub;
    }
    (path, m)
}

/// Build the request for the verb the user matched. `read_stdin` is consulted once, only
/// for `--json -`.
pub fn build(
    doc: &SchemaDocument,
    matches: &ArgMatches,
    read_stdin: &mut dyn FnMut() -> Result<String, String>,
) -> Result<Request, InvokeError> {
    let (path, leaf) = matched_path(matches);
    let route = super::schema_cli::route_at(doc, &path)
        .filter(|r| is_placeable(r))
        .ok_or(InvokeError::NotARoute(path))?;
    build_for(route, leaf, read_stdin)
}

/// Same as [`build`] with the route already known.
pub fn build_for(
    route: &RouteDescriptor,
    leaf: &ArgMatches,
    read_stdin: &mut dyn FnMut() -> Result<String, String>,
) -> Result<Request, InvokeError> {
    let mut path = route.mounted_path();
    let mut query = Vec::new();
    let mut fields = Map::new();
    let mut seen: Vec<&str> = Vec::new();
    for p in &route.params {
        if seen.contains(&p.name.as_str()) {
            continue;
        }
        seen.push(&p.name);
        if !leaf.try_contains_id(&p.name).unwrap_or(false) {
            continue;
        }
        match p.location {
            ParamLocation::Path => {
                if let Some(v) = leaf.get_one::<String>(&p.name) {
                    path = path.replace(&format!("{{{}}}", p.name), &percent_encode(v));
                }
            }
            ParamLocation::Query => {
                if let Some(v) = typed_value(leaf, &p.name, &p.kind)? {
                    let text = match v {
                        Value::String(s) => s,
                        other => other.to_string(),
                    };
                    query.push((p.name.clone(), text));
                }
            }
            ParamLocation::Body => {
                if let Some(v) = typed_value(leaf, &p.name, &p.kind)? {
                    fields.insert(p.name.clone(), v);
                }
            }
        }
    }
    let mut body: Option<Value> = None;
    if takes_json(route) && leaf.try_contains_id(JSON_ARG).unwrap_or(false) {
        if let Some(text) = leaf.get_one::<String>(JSON_ARG) {
            let text = if text == "-" {
                read_stdin().map_err(InvokeError::Stdin)?
            } else {
                text.clone()
            };
            let v: Value = serde_json::from_str(&text).map_err(|e| InvokeError::BadJson {
                arg: "json".into(),
                detail: e.to_string(),
            })?;
            body = Some(v);
        }
    }
    if !fields.is_empty() {
        body = Some(match body {
            None => Value::Object(fields),
            Some(Value::Object(mut base)) => {
                base.extend(fields);
                Value::Object(base)
            }
            Some(_) => return Err(InvokeError::BodyNotObject),
        });
    }
    let refresh = leaf.try_contains_id(REFRESH_ARG).unwrap_or(false) && leaf.get_flag(REFRESH_ARG);
    Ok(Request {
        method: route.verb,
        path,
        query,
        body,
        refresh,
    })
}

/// The JSON value a flag carries, or `None` when the flag was not given (or a boolean
/// switch is off).
fn typed_value(leaf: &ArgMatches, name: &str, kind: &str) -> Result<Option<Value>, InvokeError> {
    Ok(match kind {
        "boolean" => {
            if leaf.get_flag(name) {
                Some(Value::Bool(true))
            } else {
                None
            }
        }
        "integer" => leaf.get_one::<i64>(name).map(|n| Value::from(*n)),
        "object" | "array" => match leaf.get_one::<String>(name) {
            None => None,
            Some(text) => Some(
                serde_json::from_str(text).map_err(|e| InvokeError::BadJson {
                    arg: super::schema_cli::flag_name(name),
                    detail: e.to_string(),
                })?,
            ),
        },
        _ => leaf
            .get_one::<String>(name)
            .map(|s| Value::String(s.clone())),
    })
}

/// RFC 3986 unreserved characters pass; everything else is `%XX`-encoded, so a pane id
/// or a file path is safe inside a path segment or a query value.
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::schema_cli::tests::sample;
    use crate::cli::schema_cli::{command_path, command_tree};

    fn argv(v: &[&str]) -> Vec<String> {
        std::iter::once("avada".to_string())
            .chain(v.iter().map(|x| x.to_string()))
            .collect()
    }

    fn no_stdin() -> Result<String, String> {
        Err("no stdin in this test".into())
    }

    fn req(doc: &SchemaDocument, v: &[&str]) -> Result<Request, InvokeError> {
        let m = command_tree(doc).try_get_matches_from(argv(v)).unwrap();
        build(doc, &m, &mut no_stdin)
    }

    #[test]
    fn path_query_and_body_params_land_where_the_descriptor_says() {
        let doc = sample();
        let r = req(
            &doc,
            &[
                "panes",
                "output",
                "p/1",
                "--mode",
                "raw",
                "--tail",
                "40",
                "--wait-for-idle",
            ],
        )
        .unwrap();
        assert_eq!(r.method, Verb::Get);
        assert_eq!(r.path, "/panes/p%2F1/output");
        assert_eq!(
            r.query,
            vec![
                ("mode".to_string(), "raw".to_string()),
                ("tail".to_string(), "40".to_string()),
                ("waitForIdle".to_string(), "true".to_string()),
            ]
        );
        assert_eq!(r.body, None);
        assert_eq!(
            r.path_and_query(),
            "/panes/p%2F1/output?mode=raw&tail=40&waitForIdle=true"
        );

        let r = req(
            &doc,
            &[
                "tokens",
                "mint",
                "--scope",
                r#"{"paneIds":["a"]}"#,
                "--ttl-ms",
                "5",
            ],
        )
        .unwrap();
        assert_eq!(r.method, Verb::Post);
        assert_eq!(r.path, "/tokens");
        assert_eq!(
            r.body,
            Some(serde_json::json!({"scope": {"paneIds": ["a"]}, "ttlMs": 5}))
        );
        assert!(!r.refresh);
    }

    #[test]
    fn json_body_merges_under_typed_flags_and_dash_reads_stdin() {
        let doc = sample();
        let r = req(
            &doc,
            &[
                "queues",
                "tasks",
                "enqueue",
                "q one",
                "--json",
                r#"{"payload":{"a":1},"extra":true}"#,
                "--payload",
                r#"{"b":2}"#,
            ],
        )
        .unwrap();
        assert_eq!(r.path, "/queues/q%20one/tasks");
        assert_eq!(
            r.body,
            Some(serde_json::json!({"payload": {"b": 2}, "extra": true})),
            "flags override --json members"
        );

        let m = command_tree(&doc)
            .try_get_matches_from(argv(&["tokens", "mint", "--scope", "{}", "--json", "-"]))
            .unwrap();
        let mut fed = || Ok(r#"{"ttlMs": 7}"#.to_string());
        let r = build(&doc, &m, &mut fed).unwrap();
        assert_eq!(r.body, Some(serde_json::json!({"ttlMs": 7, "scope": {}})));
        let mut broken = || Err("closed".to_string());
        assert_eq!(
            build(&doc, &m, &mut broken),
            Err(InvokeError::Stdin("closed".into()))
        );
    }

    #[test]
    fn bad_json_and_non_object_bodies_are_usage_errors() {
        let doc = sample();
        assert!(matches!(
            req(&doc, &["tokens", "mint", "--scope", "nope"]),
            Err(InvokeError::BadJson { ref arg, .. }) if arg == "scope"
        ));
        assert!(matches!(
            req(&doc, &["tokens", "mint", "--scope", "{}", "--json", "["]),
            Err(InvokeError::BadJson { ref arg, .. }) if arg == "json"
        ));
        assert_eq!(
            req(&doc, &["tokens", "mint", "--scope", "{}", "--json", "[1]"]),
            Err(InvokeError::BodyNotObject)
        );
        // A GET with only --json is impossible (no such flag), an interior namespace
        // matched alone is not a route.
        let m = command_tree(&doc)
            .try_get_matches_from(argv(&["completions", "bash"]))
            .unwrap();
        assert_eq!(
            build(&doc, &m, &mut no_stdin),
            Err(InvokeError::NotARoute(vec!["completions".into()]))
        );
    }

    #[test]
    fn schema_refresh_and_module_routes() {
        let doc = sample();
        let r = req(&doc, &["schema", "--refresh"]).unwrap();
        assert!(r.refresh);
        assert_eq!(r.path, "/schema");
        let r = req(&doc, &["m", "acme/files", "files", "tree"]).unwrap();
        assert_eq!(r.path, "/m/acme/files/tree");
        let r = req(&doc, &["state", "tree"]).unwrap();
        assert_eq!(r.path, "/state/tree");
        let r = req(&doc, &["state"]).unwrap();
        assert_eq!(r.path, "/state");
    }

    #[test]
    fn percent_encoding() {
        assert_eq!(percent_encode("abc-_.~09"), "abc-_.~09");
        assert_eq!(percent_encode("a b/c?d=é"), "a%20b%2Fc%3Fd%3D%C3%A9");
    }

    /// Every real route builds a request whose path is the descriptor's path with each
    /// `{param}` substituted, and whose query/body carry every non-path param.
    #[test]
    fn every_core_route_round_trips() {
        let doc = crate::control::schema::SchemaRegistry::core().document("0.0.1");
        let tree = command_tree(&doc);
        assert!(!doc.routes.is_empty());
        for route in &doc.routes {
            let mut words = command_path(route);
            let mut expected_path = route.path.clone();
            let mut seen: Vec<&str> = Vec::new();
            for name in route.path_params() {
                words.push(format!("v-{name}"));
                expected_path = expected_path.replace(&format!("{{{name}}}"), &format!("v-{name}"));
            }
            let mut expect_query = Vec::new();
            let mut expect_body = Map::new();
            for p in &route.params {
                if p.location == ParamLocation::Path || seen.contains(&p.name.as_str()) {
                    continue;
                }
                seen.push(&p.name);
                let flag = format!("--{}", crate::cli::schema_cli::flag_name(&p.name));
                let value: Value = match p.kind.as_str() {
                    "boolean" => {
                        words.push(flag);
                        Value::Bool(true)
                    }
                    "integer" => {
                        words.push(flag);
                        words.push("3".into());
                        Value::from(3)
                    }
                    "object" => {
                        words.push(flag);
                        words.push(r#"{"k":1}"#.into());
                        serde_json::json!({"k": 1})
                    }
                    "array" => {
                        words.push(flag);
                        words.push("[1]".into());
                        serde_json::json!([1])
                    }
                    _ => {
                        let v = crate::cli::schema_cli::enum_values(p)
                            .map(|e| e[0].clone())
                            .unwrap_or_else(|| "txt".into());
                        words.push(flag);
                        words.push(v.clone());
                        Value::String(v)
                    }
                };
                match p.location {
                    ParamLocation::Query => {
                        let text = match &value {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        expect_query.push((p.name.clone(), text));
                    }
                    ParamLocation::Body => {
                        expect_body.insert(p.name.clone(), value);
                    }
                    ParamLocation::Path => unreachable!(),
                }
            }
            let m = tree
                .clone()
                .try_get_matches_from(argv(&words.iter().map(String::as_str).collect::<Vec<_>>()))
                .unwrap_or_else(|e| panic!("{}: {e}", route.method));
            let r =
                build(&doc, &m, &mut no_stdin).unwrap_or_else(|e| panic!("{}: {e}", route.method));
            assert_eq!(r.method, route.verb, "{}", route.method);
            assert_eq!(r.path, expected_path, "{}", route.method);
            assert!(
                !r.path.contains('{'),
                "{}: unsubstituted param",
                route.method
            );
            assert_eq!(r.query, expect_query, "{}", route.method);
            if expect_body.is_empty() {
                assert_eq!(r.body, None, "{}", route.method);
            } else {
                assert_eq!(r.body, Some(Value::Object(expect_body)), "{}", route.method);
            }
        }
    }

    #[test]
    fn a_route_with_no_params_is_the_bare_path() {
        let doc = sample();
        let r = req(&doc, &["health"]).unwrap();
        assert_eq!(
            r,
            Request {
                method: Verb::Get,
                path: "/health".into(),
                query: vec![],
                body: None,
                refresh: false
            }
        );
    }
}
