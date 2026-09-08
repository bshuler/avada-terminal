//! The HTTP face of a module's routes: `/m/<owner>/<repo>/...`.
//!
//! A module registers [`RouteDescriptor`]s with the schema registry; the control server
//! mounts one wildcard under `/m/` and, per request, looks the path up in that registry,
//! gates it on the descriptor's capability and forwards it to the module as a
//! `module.route.invoke` request (`docs/module-contract.md`). This file holds the seam
//! between the two: the server knows a [`RouteInvoker`], not the module host, so the
//! control plane can be tested with a fake and the host can be wired in later by the app.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use avada_module_sdk::contract::methods::MODULE_ROUTE_INVOKE;
use avada_module_sdk::descriptor::RouteDescriptor;
use avada_module_sdk::manifest::ModuleId;
use serde_json::{json, Map, Value};

use crate::control::schema::SchemaState;
use crate::control::server::Shared;
use crate::module::{Host, HostError, HostEvent};

/// One matched request on its way to a module.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteCall {
    /// Which module owns the route.
    pub module: ModuleId,
    /// The descriptor's `method` — the name the module registered the route under.
    pub route: String,
    /// Path captures (`{name}` segments) merged with the query string; path wins on a clash.
    pub params: Map<String, Value>,
    /// The request body when one was sent, already parsed as JSON.
    pub body: Option<Value>,
}

impl RouteCall {
    /// The `module.route.invoke` params exactly as the contract lists them.
    pub fn rpc_params(&self) -> Value {
        let mut v = json!({ "route": self.route, "params": self.params });
        if let Some(body) = &self.body {
            v["body"] = body.clone();
        }
        v
    }
}

/// Why a forwarded call produced no answer.
#[derive(Debug, Clone, PartialEq)]
pub enum InvokeError {
    /// The module is not installed or not running: the route is listed but nobody is home.
    /// Answered as 503 so a client retries later rather than treating it as a bad request.
    Unavailable(String),
    /// The module answered with a JSON-RPC error; `code`/`message`/`data` are its own.
    Module {
        /// The module's error code.
        code: i64,
        /// The module's message.
        message: String,
        /// Whatever structured detail the module attached.
        data: Option<Value>,
    },
    /// The host could not complete the exchange (timeout, closed pipe): the module's fault
    /// or the host's, but not the client's. Answered as 502.
    Failed(String),
}

impl From<HostError> for InvokeError {
    fn from(e: HostError) -> Self {
        match e {
            HostError::NotInstalled(_) | HostError::NotRunning(_) => {
                InvokeError::Unavailable(e.to_string())
            }
            HostError::Rpc(err) => InvokeError::Module {
                code: err.code,
                message: err.message,
                data: err.data,
            },
            other => InvokeError::Failed(other.to_string()),
        }
    }
}

/// A boxed future so the trait stays object-safe (it lives behind `Arc<dyn RouteInvoker>`
/// in the server's shared state).
pub type InvokeFuture<'a> = Pin<Box<dyn Future<Output = Result<Value, InvokeError>> + Send + 'a>>;

/// Whoever can turn a [`RouteCall`] into the module's answer. The module host implements
/// it; tests install a recorder.
pub trait RouteInvoker: Send + Sync {
    /// Forward the call and return the JSON body the module produced.
    fn invoke(&self, call: RouteCall) -> InvokeFuture<'_>;
}

impl RouteInvoker for Host {
    /// [`Host::call`] blocks on the module's answer, so it runs on the blocking pool — an
    /// axum worker must never wait on a child process in place.
    fn invoke(&self, call: RouteCall) -> InvokeFuture<'_> {
        let host = self.clone();
        Box::pin(async move {
            let id = call.module.clone();
            let params = call.rpc_params();
            tokio::task::spawn_blocking(move || host.call(&id, MODULE_ROUTE_INVOKE, params))
                .await
                .map_err(|e| InvokeError::Failed(format!("route invoke task failed: {e}")))?
                .map_err(InvokeError::from)
        })
    }
}

/// Fold one host event into the schema registry: a module's `host.routes.register`
/// becomes its route set (replacing the previous one), and a status that is no longer
/// live (crashed, disabled, broken, shut down) takes the set down again — the module
/// re-registers when it comes back, since registration is part of its startup. A set the
/// registry refuses (a method name that clashes with the core's or another module's, or a
/// capability the module does not hold) is logged and the module's previous set stands;
/// the module itself already got an `InvalidParams` for anything the host could check
/// alone. Every other event is somebody else's business.
pub fn apply_host_event(schema: &SchemaState, event: &HostEvent) {
    match event {
        HostEvent::Routes { module, routes } => {
            if let Err(e) = schema.register_module_routes(module, routes.clone()) {
                tracing::warn!(module = %module, error = %e, "module routes refused");
            }
        }
        HostEvent::Status { module, status } if !status.is_live() => {
            schema.with(|r| r.unregister_module_routes(module));
        }
        _ => {}
    }
}

/// Wire a module host into the control server: the host answers `/m/...` calls, and a
/// thread named `module-routes` keeps the schema registry in step with the host's
/// events (see [`apply_host_event`]). The thread owns only an event receiver, so it ends
/// on its own once the host is dropped; the handle is returned for tests that want to
/// join it.
pub fn attach_host(shared: Arc<Shared>, host: &Host) -> std::thread::JoinHandle<()> {
    shared.install_route_invoker(Arc::new(host.clone()));
    let events = host.events();
    std::thread::Builder::new()
        .name("module-routes".into())
        .spawn(move || {
            while let Ok(event) = events.recv() {
                apply_host_event(&shared.schema, &event);
            }
        })
        .expect("spawn module-routes thread")
}

/// What the registry says about a request path under one module.
#[derive(Debug, PartialEq)]
pub enum Match {
    /// A descriptor's path and verb both fit; here it is, with its path captures.
    Route(Box<RouteDescriptor>, Map<String, Value>),
    /// Some descriptor's path fits but none with this verb — 405, not 404.
    WrongVerb,
    /// Nothing the module registered looks like this path.
    NoRoute,
}

/// Find the descriptor for `path` (module-relative, leading `/`) and `verb` among `routes`,
/// honouring `{name}` captures segment by segment. Descriptors that belong to another
/// module (or to the core) are skipped.
pub fn match_route(
    routes: &[RouteDescriptor],
    module: &ModuleId,
    verb: avada_module_sdk::descriptor::Verb,
    path: &str,
) -> Match {
    let mut path_hit = false;
    for desc in routes.iter().filter(|d| d.module.as_ref() == Some(module)) {
        let Some(captures) = capture(&desc.path, path) else {
            continue;
        };
        if desc.verb == verb {
            return Match::Route(Box::new(desc.clone()), captures);
        }
        path_hit = true;
    }
    if path_hit {
        Match::WrongVerb
    } else {
        Match::NoRoute
    }
}

/// Segment-wise match of a request path against a descriptor path; `{name}` segments
/// capture, everything else must be equal. `None` when the shapes differ.
fn capture(pattern: &str, path: &str) -> Option<Map<String, Value>> {
    let want: Vec<&str> = pattern.trim_end_matches('/').split('/').collect();
    let got: Vec<&str> = path.trim_end_matches('/').split('/').collect();
    if want.len() != got.len() {
        return None;
    }
    let mut out = Map::new();
    for (w, g) in want.iter().zip(got.iter()) {
        match w.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
            Some(name) => {
                if g.is_empty() {
                    return None;
                }
                out.insert(name.to_string(), Value::String((*g).to_string()));
            }
            None if w == g => {}
            None => return None,
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use avada_module_sdk::caps::Capability;
    use avada_module_sdk::descriptor::{Scope, Verb};

    fn desc(module: &ModuleId, method: &str, verb: Verb, path: &str) -> RouteDescriptor {
        RouteDescriptor {
            method: method.into(),
            path: path.into(),
            verb,
            capability: Some(Capability::WorkspaceRead),
            summary: method.into(),
            params: vec![],
            scope: Scope::Token,
            module: Some(module.clone()),
            response: None,
        }
    }

    #[test]
    fn a_capture_names_each_braced_segment_and_refuses_a_different_shape() {
        let got = capture("/files/{id}/lines/{n}", "/files/f1/lines/42").unwrap();
        assert_eq!(got.get("id"), Some(&json!("f1")));
        assert_eq!(got.get("n"), Some(&json!("42")));
        assert!(capture("/files/{id}", "/files").is_none());
        assert!(capture("/files/{id}", "/files/f1/extra").is_none());
        assert!(capture("/files/{id}", "/dirs/f1").is_none());
        assert!(capture("/files/{id}", "/files/").is_none());
        assert_eq!(capture("/tree", "/tree/").unwrap().len(), 0);
    }

    #[test]
    fn a_match_tells_a_wrong_verb_from_a_missing_route() {
        let acme = ModuleId::new("acme/files").unwrap();
        let other = ModuleId::new("other/files").unwrap();
        let routes = vec![
            desc(&acme, "tree", Verb::Get, "/tree"),
            desc(&acme, "open", Verb::Post, "/files/{id}"),
            desc(&other, "stat", Verb::Get, "/stat"),
        ];
        match match_route(&routes, &acme, Verb::Post, "/files/f1") {
            Match::Route(d, caps) => {
                assert_eq!(d.method, "open");
                assert_eq!(caps.get("id"), Some(&json!("f1")));
            }
            m => panic!("expected a route, got {m:?}"),
        }
        assert_eq!(
            match_route(&routes, &acme, Verb::Delete, "/tree"),
            Match::WrongVerb
        );
        assert_eq!(
            match_route(&routes, &acme, Verb::Get, "/nope"),
            Match::NoRoute
        );
        // Another module's route is invisible under this module's prefix.
        assert_eq!(
            match_route(&routes, &acme, Verb::Get, "/stat"),
            Match::NoRoute
        );
    }

    #[test]
    fn host_events_put_routes_in_the_schema_and_a_dead_module_takes_them_out() {
        use crate::module::ModuleStatus;
        let schema = SchemaState::new();
        let acme = ModuleId::new("acme/files").unwrap();
        let mounted = |schema: &SchemaState| -> Vec<String> {
            schema.with(|r| {
                r.routes()
                    .iter()
                    .filter(|d| d.module.as_ref() == Some(&acme))
                    .map(|d| d.mounted_path())
                    .collect()
            })
        };
        apply_host_event(
            &schema,
            &HostEvent::Routes {
                module: acme.clone(),
                routes: vec![desc(&acme, "acme.tree", Verb::Get, "/tree")],
            },
        );
        assert_eq!(mounted(&schema), vec!["/m/acme/files/tree"]);
        assert!(schema
            .document("0")
            .routes
            .iter()
            .any(|d| d.method == "acme.tree"));
        // A second registration replaces the first.
        apply_host_event(
            &schema,
            &HostEvent::Routes {
                module: acme.clone(),
                routes: vec![desc(&acme, "acme.stat", Verb::Get, "/stat")],
            },
        );
        assert_eq!(mounted(&schema), vec!["/m/acme/files/stat"]);
        // Live statuses leave the set alone; a crash removes it.
        for live in [ModuleStatus::Starting, ModuleStatus::Running] {
            apply_host_event(
                &schema,
                &HostEvent::Status {
                    module: acme.clone(),
                    status: live,
                },
            );
            assert_eq!(mounted(&schema).len(), 1);
        }
        apply_host_event(
            &schema,
            &HostEvent::Status {
                module: acme.clone(),
                status: ModuleStatus::Crashed { restarts: 1 },
            },
        );
        assert!(mounted(&schema).is_empty());
        // Events that are not about routes are ignored.
        apply_host_event(
            &schema,
            &HostEvent::Toast {
                module: acme.clone(),
                text: "hi".into(),
                level: "info".into(),
            },
        );
        assert!(mounted(&schema).is_empty());
    }

    #[test]
    fn a_set_the_registry_refuses_leaves_the_previous_set_standing() {
        let schema = SchemaState::new();
        let acme = ModuleId::new("acme/files").unwrap();
        let core_method = schema.with(|r| {
            r.routes()
                .iter()
                .find(|d| d.module.is_none())
                .map(|d| d.method.clone())
                .expect("the core table has routes")
        });
        let before = schema.with(|r| r.routes().len());
        apply_host_event(
            &schema,
            &HostEvent::Routes {
                module: acme.clone(),
                routes: vec![desc(&acme, "acme.tree", Verb::Get, "/tree")],
            },
        );
        assert_eq!(schema.with(|r| r.routes().len()), before + 1);
        // A method name the core already owns is refused; the registry is unchanged.
        apply_host_event(
            &schema,
            &HostEvent::Routes {
                module: acme.clone(),
                routes: vec![desc(&acme, &core_method, Verb::Get, "/clash")],
            },
        );
        let after: Vec<String> = schema.with(|r| {
            r.routes()
                .iter()
                .filter(|d| d.module.as_ref() == Some(&acme))
                .map(|d| d.method.clone())
                .collect()
        });
        assert_eq!(after, vec!["acme.tree"]);
        assert_eq!(schema.with(|r| r.routes().len()), before + 1);
    }

    #[test]
    fn rpc_params_carry_body_only_when_one_was_sent() {
        let call = RouteCall {
            module: ModuleId::new("acme/files").unwrap(),
            route: "tree".into(),
            params: Map::new(),
            body: None,
        };
        assert_eq!(call.rpc_params(), json!({ "route": "tree", "params": {} }));
        let with = RouteCall {
            body: Some(json!({ "depth": 2 })),
            ..call
        };
        assert_eq!(
            with.rpc_params(),
            json!({ "route": "tree", "params": {}, "body": { "depth": 2 } })
        );
    }

    #[test]
    fn host_errors_sort_into_the_three_client_facing_kinds() {
        let id = ModuleId::new("acme/files").unwrap();
        assert!(matches!(
            InvokeError::from(HostError::NotRunning(id.clone())),
            InvokeError::Unavailable(_)
        ));
        assert!(matches!(
            InvokeError::from(HostError::NotInstalled(id)),
            InvokeError::Unavailable(_)
        ));
        assert!(matches!(
            InvokeError::from(HostError::Timeout),
            InvokeError::Failed(_)
        ));
        let rpc = avada_module_sdk::contract::RpcError {
            code: -32601,
            message: "no such route".into(),
            data: Some(json!({ "route": "tree" })),
        };
        assert_eq!(
            InvokeError::from(HostError::Rpc(rpc)),
            InvokeError::Module {
                code: -32601,
                message: "no such route".into(),
                data: Some(json!({ "route": "tree" })),
            }
        );
    }
}
