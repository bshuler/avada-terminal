//! All HTTP route handlers (axum) — the byte-compatible surface the MCP server depends on.
//! Ports the route table from `src/main/control-server.ts` EXACTLY:
//!   GET  /health                 (the ONLY unauthenticated route)
//!   GET  /state                  scope-filtered windows tree (readmodel)
//!   POST /tokens                 mint scoped token (tokens + scope::check_mintable)
//!   GET  /panes/{id}/output      mode=screen|raw, tail, strip, since, waitForIdle/settleMs/timeoutMs
//!                                (control::output cores); cursor ALWAYS present
//!   POST /panes/{id}/input       allowInput gate (403); data|keys (control::input); submit; lock 423
//!   GET|POST /panes/{id}/messages durable inbox (control::inbox)
//!   POST|DELETE /panes/{id}/lock  advisory lock (control::lock)
//!   POST /command                dispatch
//!   GET  /events                 WS upgrade (token via header or ?token=)
//!   + 401 unauthorized / 404 {error,path} / 405 method-not-allowed fallbacks
//!
//! Bearer via `Authorization: Bearer` or `?token=` (WS only). Every body shape matches the TS
//! source (omit-when-unset; ordered structs where field order is observable).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use avada_module_sdk::caps::Capability;
use avada_module_sdk::descriptor::Verb;
use avada_module_sdk::manifest::ModuleId;
use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, Request, State};
use axum::handler::Handler;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::middleware::{from_fn_with_state, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, on, MethodFilter, MethodRouter};
use axum::{Json, Router};
use serde::Serialize;
use serde_json::{json, Value};

use crate::ansi_strip::strip_ansi;
use crate::control::descriptor_table;
use crate::control::dispatch;
use crate::control::dispatch::{capability_refusal, verb_capability};
use crate::control::events::ControlEvent;
use crate::control::input::{keys_to_bytes, submit_newlines, KeysResult, SUBMIT_DELAY_MS};
use crate::control::modules::{match_route, InvokeError, Match, RouteCall};
use crate::control::nudge;
use crate::control::output::{
    detect_awaiting_input, next_poll_delay, slice_since, wait_decision, WaitVerdict,
    DEFAULT_SETTLE_MS, DEFAULT_WAIT_TIMEOUT_MS,
};
use crate::control::readmodel::{Activity, PaneStatus, WindowOut};
use crate::control::scope::{check_mintable, coerce_scope, pane_in_scope, queue_in_scope, Scope};
use crate::control::server::{events_url, notify_state, now_ms, Shared};
use crate::control::tokens::TokenInfo;
use crate::control::uiops::UiOp;
use crate::control::work::{
    Counts, EnqueueOpts, LeaseOutcome, ListFilter, NackOpts, QueueSummary, Task, TaskState,
};
use crate::persistence::projects;

// ---- the router, built from the descriptor table -------------------------------------------
//
// `descriptor_table::core_routes()` is the one list of what this server serves: verb, path,
// capability, scope. This file only supplies a handler per described route (`handlers()`),
// and `router` joins the two. A described route without a handler, or a handler without a
// descriptor, is a startup panic that names the route — so `GET /schema` can never drift
// from what is actually mounted. The per-route capability check is a layer on the handler
// alone (not the whole `MethodRouter`), so the 405 fallback stays byte-exact.

/// A handler ready to be mounted: given the verb filter and (when the route names a
/// capability) the gate, produce the `MethodRouter` for its path.
pub(crate) type Mount = Box<dyn FnOnce(MethodFilter, Option<Gate>) -> MethodRouter<Arc<Shared>>>;

/// Pair a dotted method name from the descriptor table with its handler.
fn h<H, T>(method: &'static str, handler: H) -> (&'static str, Mount)
where
    H: Handler<T, Arc<Shared>>,
    T: 'static,
{
    (
        method,
        Box::new(move |filter, gate| match gate {
            Some(gate) => on(
                filter,
                handler.layer(from_fn_with_state(gate, capability_gate)),
            ),
            None => on(filter, handler),
        }),
    )
}

/// Every handler this file serves, keyed by the descriptor table's method name. Adding a
/// handler here without a descriptor in `descriptor_table::core_routes()` — or the reverse —
/// panics in [`router`] with the route's name.
pub(crate) fn handlers() -> Vec<(&'static str, Mount)> {
    vec![
        h("health", health),
        h("state", state),
        h("loops", loops_get),
        h("tokens.mint", tokens),
        h("devices.list", devices_list),
        h("devices.mint", devices_mint),
        h("devices.revoke", devices_revoke),
        h("command", command),
        h("projects.list", projects_list),
        h("projects.add", projects_add),
        h("projects.patch", projects_patch),
        h("projects.delete", projects_delete),
        h("panes.output", output),
        h("panes.input", input),
        h("panes.messages.list", messages_get),
        h("panes.messages.post", messages_post),
        h("panes.lock", lock_post),
        h("panes.unlock", lock_delete),
        // ---- work queue (worker-pool phase-2/3) ----
        h("queues.list", queues_list),
        h("queues.tasks.enqueue", task_enqueue),
        h("queues.tasks.list", tasks_list),
        h("queues.claim", task_claim),
        h("queues.purge", queue_purge),
        h("tasks.get", task_get),
        h("tasks.ack", task_ack),
        h("tasks.nack", task_nack),
        h("tasks.extend", task_extend),
        h("settings.get", settings_get),
        h("settings.patch", settings_patch),
        h("fs.read", fs_read),
        h("events", events_ws),
        // ---- track F2 marketplace (mirrors the fenced block in descriptor_table)
        h("marketplace.search", marketplace_search),
        h("marketplace.show", marketplace_show),
        h("marketplace.install", marketplace_install),
        h("marketplace.jobs", marketplace_jobs),
        h("marketplace.job", marketplace_job),
        h("marketplace.enable", marketplace_enable),
        h("marketplace.disable", marketplace_disable),
        h("marketplace.uninstall", marketplace_uninstall),
        h("marketplace.installed", marketplace_installed),
        h("marketplace.toolchain", marketplace_toolchain),
        h("marketplace.signin", marketplace_signin),
        h("marketplace.signin.poll", marketplace_signin_poll),
        h("schema", schema_get),
    ]
}

fn method_filter(verb: Verb) -> MethodFilter {
    match verb {
        Verb::Get => MethodFilter::GET,
        Verb::Post => MethodFilter::POST,
        Verb::Put => MethodFilter::PUT,
        Verb::Patch => MethodFilter::PATCH,
        Verb::Delete => MethodFilter::DELETE,
    }
}

/// Build the full router with the shared state baked in, one mount per descriptor-table
/// route. Panics (naming the route) when the table and [`handlers`] disagree.
#[tracing::instrument(level = "debug", ret, skip(shared))]
pub fn router(shared: Arc<Shared>) -> Router {
    let listed = handlers();
    let n = listed.len();
    let mut handlers: BTreeMap<&'static str, Mount> = listed.into_iter().collect();
    assert_eq!(
        handlers.len(),
        n,
        "routes::handlers() names the same method twice"
    );
    // Path → the MethodRouters to merge there, in table order.
    let mut by_path: Vec<(String, Vec<MethodRouter<Arc<Shared>>>)> = Vec::new();
    for desc in descriptor_table::core_routes() {
        let mount = handlers.remove(desc.method.as_str()).unwrap_or_else(|| {
            panic!(
                "control route {:?} ({:?} {}) is described in descriptor_table but has no \
                 handler in routes::handlers()",
                desc.method, desc.verb, desc.path
            )
        });
        let gate = desc.capability.map(|cap| Gate {
            shared: Arc::clone(&shared),
            cap,
        });
        let method_router = mount(method_filter(desc.verb), gate);
        let path = desc.mounted_path();
        match by_path.iter_mut().find(|(p, _)| *p == path) {
            Some((_, list)) => list.push(method_router),
            None => by_path.push((path, vec![method_router])),
        }
    }
    if !handlers.is_empty() {
        let stray: Vec<&str> = handlers.keys().copied().collect();
        panic!(
            "routes::handlers() mounts {stray:?} without a descriptor in \
             descriptor_table::core_routes()"
        );
    }
    let mut app = Router::new();
    for (path, routers) in by_path {
        let merged = routers
            .into_iter()
            .reduce(MethodRouter::merge)
            .expect("at least one method per path");
        app = app.route(&path, merged);
    }
    // Module routes are not in the table: they arrive at runtime through the schema
    // registry, so one wildcard per shape takes every verb and `module_route` does the
    // lookup, the gate and the 404/405 itself.
    app = app
        .route("/m/{owner}/{repo}", any(module_route))
        .route("/m/{owner}/{repo}/{*rest}", any(module_route));
    app.method_not_allowed_fallback(method_not_allowed)
        .fallback(not_found)
        .with_state(shared)
}

// ---- capability gate ----------------------------------------------------------------------

/// What a gated route needs: the resolver lives in `Shared`, the capability comes from the
/// route's descriptor.
#[derive(Clone)]
pub(crate) struct Gate {
    shared: Arc<Shared>,
    cap: Capability,
}

/// Refuse a caller whose token does not hold the route's capability with 403
/// `{"error":"capability","capability":"<name>"}`. Runs before the handler and only for a
/// token that is an identity (see [`identify`]) — an unknown or absent token falls through
/// so the handler's own 401 stays byte-exact. The token is read from the Bearer header, else
/// from `?token=` (the WebSocket route's way in); it is never logged.
async fn capability_gate(State(gate): State<Gate>, req: Request, next: Next) -> Response {
    let token = bearer_header(req.headers()).or_else(|| {
        Query::<HashMap<String, String>>::try_from_uri(req.uri())
            .ok()
            .and_then(|Query(q)| q.get("token").cloned())
    });
    if let Some(token) = token {
        let known = identify(&gate.shared, Some(&token)).is_some();
        if known {
            if let Err(cap) = gate.shared.caps.check(&token, gate.cap) {
                tracing::warn!(capability = cap.name(), "request refused: capability");
                let (code, body) = capability_refusal(cap);
                return jstatus(code, body);
            }
        }
    }
    next.run(req).await
}

// ---- response helpers ---------------------------------------------------------------------

#[tracing::instrument(level = "debug", ret)]
fn jstatus(code: u16, body: Value) -> Response {
    (
        StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(body),
    )
        .into_response()
}

#[tracing::instrument(level = "debug", ret, skip(body))]
fn ok_json<T: Serialize>(body: T) -> Response {
    Json(body).into_response()
}

// ---- auth ---------------------------------------------------------------------------------

#[tracing::instrument(level = "debug", skip_all)]
fn bearer_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::to_string)
}

/// Resolve the presented bearer (HTTP header only — `?token=` is WS-only, per TS). 401 on failure.
// pre-existing; deferred per repo lint policy (test.yml)
#[allow(clippy::result_large_err)]
#[tracing::instrument(level = "debug", skip_all)]
fn authorize(shared: &Arc<Shared>, headers: &HeaderMap) -> Result<TokenInfo, Response> {
    let token = bearer_header(headers);
    identify(shared, token.as_deref()).ok_or_else(|| {
        // The token value is a secret: log only whether one was presented at all.
        tracing::warn!(
            bearer_present = token.is_some(),
            "request rejected: unauthorized"
        );
        jstatus(401, json!({ "error": "unauthorized" }))
    })
}

/// Who is calling: the token store's answer (master, device, scoped) or, failing that, a
/// token an installed capability source claims — a module's token, which the module host
/// issues and the rights service vouches for. A module token is root-scoped (scope narrows
/// panes; a module is narrowed by its capabilities instead) and never expires here (the host
/// rotates it by restarting the module). `None` is the 401.
fn identify(shared: &Shared, token: Option<&str>) -> Option<TokenInfo> {
    let stored = shared.tokens.lock().unwrap().resolve(token, now_ms());
    stored.or_else(|| {
        token.filter(|t| shared.caps.claims(t)).map(|_| TokenInfo {
            scope: None,
            expires_at: None,
        })
    })
}

/// A resolved, in-scope pane: its session uid plus the canonical pane id (the caller may have
/// addressed it by an alias — see `ReadModel::resolve_pane_id`). 404 (no such pane) / 403 (out
/// of scope).
struct FoundPane {
    uid: String,
    pane_id: String,
}

// pre-existing; deferred per repo lint policy (test.yml)
#[allow(clippy::result_large_err)]
#[tracing::instrument(level = "debug", skip(shared))]
fn find_pane_scoped(
    shared: &Arc<Shared>,
    scope: Option<&Scope>,
    pane_id: &str,
) -> Result<FoundPane, Response> {
    let m = shared.model.lock().unwrap();
    let canonical = match m.resolve_pane_id(pane_id) {
        None => {
            return Err(jstatus(
                404,
                json!({ "error": "no such pane", "paneId": pane_id }),
            ))
        }
        Some(c) => c,
    };
    match m.coords_of(&canonical) {
        None => Err(jstatus(
            404,
            json!({ "error": "no such pane", "paneId": pane_id }),
        )),
        Some(coords) => {
            if !pane_in_scope(scope, &coords) {
                return Err(jstatus(
                    403,
                    json!({ "error": "pane out of scope", "paneId": pane_id }),
                ));
            }
            let uid = m
                .pane(&canonical)
                .map(|p| p.session_uid.clone())
                .unwrap_or_default();
            Ok(FoundPane {
                uid,
                pane_id: canonical,
            })
        }
    }
}

// ---- /health ------------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HealthOut {
    ok: bool,
    app: &'static str,
    pid: u32,
    version: String,
    allow_input: bool,
}

#[tracing::instrument(level = "debug", skip_all)]
async fn health(State(shared): State<Arc<Shared>>) -> Response {
    ok_json(HealthOut {
        ok: true,
        app: "avada",
        pid: shared.pid,
        version: shared.version.clone(),
        allow_input: shared.allow_input(),
    })
}

// ---- /state -------------------------------------------------------------------------------

/// `/state`'s top-level, additive `speech` field: the per-pane "talk" engine's status,
/// reported even before the engine has been lazily spawned.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SpeechOut {
    muted: bool,
    focused_only: bool,
    backend: String,
    speaking_pane: Option<String>,
}

/// `/state`'s top-level, additive `dictation` field: which halves of the STT pipeline this
/// machine actually has, and which panes have a live microphone right now. Recording state
/// lives here rather than on the pane, because the GUI republishes every pane wholesale each
/// sync tick and would stamp a per-pane flag straight back out.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DictationOut {
    recorder: String,
    transcriber: String,
    recording_panes: Vec<String>,
    /// Where finished recordings and their transcripts are kept. Reported so that
    /// "the transcript was wrong" has an answer that does not require knowing the
    /// layout of the state directory by heart.
    kept_in: String,
}

#[derive(Serialize)]
struct StateWithSpeechOut {
    windows: Vec<WindowOut>,
    speech: SpeechOut,
    dictation: DictationOut,
}

#[tracing::instrument(level = "debug", skip_all)]
async fn state(State(shared): State<Arc<Shared>>, headers: HeaderMap) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    let out = {
        let m = shared.model.lock().unwrap();
        m.state_for_scope_with_dims(info.scope.as_ref(), &|p| shared.compute_activity(p), &|p| {
            shared.sessions.dims(&p.session_uid)
        })
    };
    let status = shared.speech.status();
    let dictation = shared.dictation.status();
    ok_json(StateWithSpeechOut {
        windows: out.windows,
        speech: SpeechOut {
            muted: status.muted,
            focused_only: status.focused_only,
            backend: status.backend,
            speaking_pane: status.speaking_pane,
        },
        dictation: DictationOut {
            recorder: dictation.recorder,
            transcriber: dictation.transcriber,
            recording_panes: dictation.recording_panes,
            kept_in: dictation.kept_in,
        },
    })
}

// ---- /loops ---------------------------------------------------------------------------------

/// GET /loops — the two app-owned scheduler loops (status-check, monitored-agent restart):
/// whether each is enabled, its configured interval, and when it last/next fires.
///
/// Unlike `/settings`, this never 503s for a missing GUI: the read-model's default is an
/// honest "disabled, never fired" ([`LoopsInfo::default`]), which is a real answer, not a
/// placeholder. No scope check either — loops are app-wide, not per-pane, so there is nothing
/// for a scoped token to be excluded from. A later lane wires a GUI publisher to
/// `ReadModel::set_loops`; until then every caller sees the same honest default.
#[tracing::instrument(level = "debug", skip_all)]
async fn loops_get(State(shared): State<Arc<Shared>>, headers: HeaderMap) -> Response {
    if let Err(e) = authorize(&shared, &headers) {
        return e;
    }
    ok_json(shared.model.lock().unwrap().loops())
}

// ---- /tokens ------------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MintOut {
    ok: bool,
    token: String,
    scope: Scope,
    expires_at: Option<i64>,
    port: Option<u16>,
    events: Value,
}

#[tracing::instrument(level = "debug", skip_all)]
async fn tokens(State(shared): State<Arc<Shared>>, headers: HeaderMap, body: Bytes) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    let parsed: Option<Value> = serde_json::from_slice(&body).ok();
    let scope_val = parsed
        .as_ref()
        .and_then(|b| b.get("scope"))
        .cloned()
        .unwrap_or(Value::Null);
    let requested = match coerce_scope(&scope_val) {
        Some(s) => s,
        None => {
            return jstatus(
                400,
                json!({ "error": "expected { scope: { windowIds?|tabIds?|paneIds? }, ttlMs? }" }),
            )
        }
    };
    // No-escalation: the requested scope must sit within the minter's authority + name real ids.
    let problem = {
        let m = shared.model.lock().unwrap();
        check_mintable(info.scope.as_ref(), &requested, &*m)
    };
    if let Some(p) = problem {
        return jstatus(403, json!({ "error": p }));
    }
    let ttl = parsed
        .as_ref()
        .and_then(|b| b.get("ttlMs"))
        .and_then(Value::as_i64)
        .filter(|&t| t > 0);
    let (token, expires_at) = shared
        .tokens
        .lock()
        .unwrap()
        .mint(requested.clone(), ttl, now_ms());
    let port = shared.port();
    let (port_field, events) = if port != 0 {
        (
            Some(port),
            Value::String(events_url(&shared.advertised_host(), port, &token)),
        )
    } else {
        (None, Value::Null)
    };
    ok_json(MintOut {
        ok: true,
        token,
        scope: requested,
        expires_at,
        port: port_field,
        events,
    })
}

// ---- /devices (paired mobile clients: persisted, revocable, full-authority) ----------------
//
// A device token is unscoped (a phone is a full remote head, and a scope is a whitelist of
// TODAY's panes — it would go blind the moment a new pane opens), persisted to
// `device-tokens.json` so pairing survives a host restart, and individually revocable by label.
// `avada pair` POSTs here so the MASTER token never leaves the machine; `devices`/`revoke`
// list and drop. All three are MASTER-only — a scoped agent token must not mint device creds.

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DeviceMintOut {
    ok: bool,
    label: String,
    token: String,
    expires_at: Option<i64>,
    port: Option<u16>,
    events: Value,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DeviceListItem {
    label: String,
    expires_at: Option<i64>,
    /// The device's SSH public key, when it paired one. A public key is not a credential, so
    /// unlike the bearer token it is safe to echo back — `avada devices` shows which
    /// devices can also reach the embedded SSH server.
    #[serde(skip_serializing_if = "Option::is_none")]
    ssh_key: Option<String>,
}

/// Rewrite `device-tokens.json` from the live store (best-effort — the in-memory table is
/// authoritative for this process; the file only has to be right for the next start). Kept beside
/// the active `control.json` so tests/embeds with a custom control file stay self-consistent.
#[tracing::instrument(level = "debug", ret, skip(shared))]
fn persist_devices(shared: &Arc<Shared>) {
    let records: Vec<_> = shared
        .tokens
        .lock()
        .unwrap()
        .list_devices()
        .into_iter()
        .map(
            |(token, info)| crate::persistence::device_tokens::DeviceRecord {
                label: info.label,
                token,
                expires_at: info.expires_at,
                ssh_key: info.ssh_key,
            },
        )
        .collect();
    let path = shared.control_file.with_file_name("device-tokens.json");
    if let Err(e) = crate::persistence::device_tokens::save_to(&path, &records) {
        eprintln!("[control] persist device-tokens.json failed: {e}");
    }
}

/// POST /devices — mint a device token. Body: `{ label?, ttlMs?, sshKey? }` (label defaults to
/// `device`, ttl omitted = never expires). Returns the token + reachable port/events for the QR.
///
/// `sshKey` is an optional `authorized_keys`-form public key for the same device: it rides in the
/// same record so the embedded SSH server accepts that key, and so one `avada revoke <label>`
/// drops both doors at once. It is validated by the caller (`avada pair --ssh-key`); the
/// server only stores what it is given, exactly as it does the label.
#[tracing::instrument(level = "debug", skip_all)]
async fn devices_mint(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    if info.scope.is_some() {
        return jstatus(403, json!({ "error": "master token required" }));
    }
    let parsed: Option<Value> = serde_json::from_slice(&body).ok();
    let label = parsed
        .as_ref()
        .and_then(|b| b.get("label"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("device")
        .to_string();
    let ttl = parsed
        .as_ref()
        .and_then(|b| b.get("ttlMs"))
        .and_then(Value::as_i64)
        .filter(|&t| t > 0);
    let ssh_key = parsed
        .as_ref()
        .and_then(|b| b.get("sshKey"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let (token, expires_at) =
        shared
            .tokens
            .lock()
            .unwrap()
            .mint_device(label.clone(), ttl, now_ms(), ssh_key);
    persist_devices(&shared);
    let port = shared.port();
    let (port_field, events) = if port != 0 {
        (
            Some(port),
            Value::String(events_url(&shared.advertised_host(), port, &token)),
        )
    } else {
        (None, Value::Null)
    };
    ok_json(DeviceMintOut {
        ok: true,
        label,
        token,
        expires_at,
        port: port_field,
        events,
    })
}

/// GET /devices — list paired devices (label + expiry only; tokens are never echoed back).
#[tracing::instrument(level = "debug", skip_all)]
async fn devices_list(State(shared): State<Arc<Shared>>, headers: HeaderMap) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    if info.scope.is_some() {
        return jstatus(403, json!({ "error": "master token required" }));
    }
    let mut items: Vec<DeviceListItem> = shared
        .tokens
        .lock()
        .unwrap()
        .list_devices()
        .into_iter()
        .map(|(_token, info)| DeviceListItem {
            label: info.label,
            expires_at: info.expires_at,
            ssh_key: info.ssh_key,
        })
        .collect();
    items.sort_by(|a, b| a.label.cmp(&b.label));
    ok_json(json!({ "ok": true, "devices": items }))
}

/// DELETE /devices?label=<label> — revoke every device carrying `label` (a query param so labels
/// may hold spaces or slashes). `revoked` = how many were dropped.
#[tracing::instrument(level = "debug", skip_all)]
async fn devices_revoke(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    if info.scope.is_some() {
        return jstatus(403, json!({ "error": "master token required" }));
    }
    let Some(label) = q.get("label").filter(|l| !l.is_empty()) else {
        return jstatus(400, json!({ "error": "missing label" }));
    };
    let removed = shared.tokens.lock().unwrap().revoke_device(label);
    if removed > 0 {
        persist_devices(&shared);
    }
    ok_json(json!({ "ok": true, "revoked": removed }))
}

// ---- /panes/{id}/output -------------------------------------------------------------------

#[tracing::instrument(level = "debug", skip_all)]
async fn output(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    let found = match find_pane_scoped(&shared, info.scope.as_ref(), &id) {
        Ok(f) => f,
        Err(e) => return e,
    };
    let uid = found.uid;

    if q.get("waitForIdle").map(|v| v == "1").unwrap_or(false) {
        let settle = pos_num(q.get("settleMs")).unwrap_or(DEFAULT_SETTLE_MS);
        let timeout = pos_num(q.get("timeoutMs")).unwrap_or(DEFAULT_WAIT_TIMEOUT_MS);
        let since = non_neg_num(q.get("since"));
        let (settled, timed_out) = wait_for_quiet(&shared, &uid, settle, timeout, since).await;
        let mut body = build_output_body(&shared, &id, &uid, &q);
        if let Value::Object(map) = &mut body {
            map.insert("waited".into(), json!(true));
            map.insert("settled".into(), json!(settled));
            map.insert("timedOut".into(), json!(timed_out));
        }
        return ok_json(body);
    }
    ok_json(build_output_body(&shared, &id, &uid, &q))
}

// ---- /fs/read (mobile clickable paths) ------------------------------------------------------

/// Read a text file off the host disk for a remote viewer (the mobile app's tap-a-path
/// feature). MASTER token only: scoped tokens are for sandboxed agents and must not
/// grow a filesystem read primitive. Query: `path` (absolute or `~/...`), optional
/// `maxBytes` (default 256 KiB, capped 2 MiB). Non-UTF-8 content 415s rather than
/// mangling bytes; oversized files return the head with `truncated: true`.
#[tracing::instrument(level = "debug", skip_all)]
async fn fs_read(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    if info.scope.is_some() {
        return jstatus(403, json!({ "error": "master token required" }));
    }
    let Some(raw_path) = q.get("path").filter(|p| !p.is_empty()) else {
        return jstatus(400, json!({ "error": "missing path" }));
    };
    // `~` expansion matches the desktop clickable-paths behaviour.
    let path = if let Some(rest) = raw_path.strip_prefix("~/") {
        match std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            Some(home) => std::path::PathBuf::from(home).join(rest),
            None => return jstatus(400, json!({ "error": "cannot expand ~ (no HOME)" })),
        }
    } else {
        std::path::PathBuf::from(raw_path)
    };
    if !path.is_absolute() {
        return jstatus(400, json!({ "error": "path must be absolute (or ~/...)" }));
    }
    let max_bytes = q
        .get("maxBytes")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(256 * 1024)
        .min(2 * 1024 * 1024);
    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(_) => return jstatus(404, json!({ "error": "not found" })),
    };
    if !meta.is_file() {
        return jstatus(400, json!({ "error": "not a regular file" }));
    }
    let size = meta.len();
    let bytes = {
        use std::io::Read;
        let mut f = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(_) => return jstatus(403, json!({ "error": "cannot open" })),
        };
        let mut buf = vec![0u8; max_bytes.min(size as usize)];
        let mut read = 0;
        while read < buf.len() {
            match f.read(&mut buf[read..]) {
                Ok(0) => break,
                Ok(n) => read += n,
                Err(_) => return jstatus(500, json!({ "error": "read failed" })),
            }
        }
        buf.truncate(read);
        buf
    };
    let truncated = (bytes.len() as u64) < size;
    // Lop a torn trailing UTF-8 sequence off a truncated read before validating.
    let content = match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) if truncated => {
            let valid = e.utf8_error().valid_up_to();
            let mut b = e.into_bytes();
            b.truncate(valid);
            match String::from_utf8(b) {
                Ok(s) => s,
                Err(_) => return jstatus(415, json!({ "error": "not a text file" })),
            }
        }
        Err(_) => return jstatus(415, json!({ "error": "not a text file" })),
    };
    ok_json(json!({
        "path": path.display().to_string(),
        "size": size,
        "truncated": truncated,
        "content": content,
    }))
}

#[tracing::instrument(level = "debug", ret, skip(shared))]
fn pane_status_str(shared: &Arc<Shared>, pane_id: &str) -> &'static str {
    shared
        .model
        .lock()
        .unwrap()
        .pane(pane_id)
        .map(|p| p.status.as_str())
        .unwrap_or(PaneStatus::Exited.as_str())
}

#[tracing::instrument(level = "debug", ret, skip(shared))]
fn build_output_body(
    shared: &Arc<Shared>,
    pane_id: &str,
    uid: &str,
    q: &HashMap<String, String>,
) -> Value {
    if q.get("mode").map(|m| m == "screen").unwrap_or(false) {
        build_screen_body(shared, pane_id, uid, q)
    } else {
        read_output_body(shared, pane_id, uid, q)
    }
}

#[tracing::instrument(level = "debug", ret, skip(shared))]
fn read_output_body(
    shared: &Arc<Shared>,
    pane_id: &str,
    uid: &str,
    q: &HashMap<String, String>,
) -> Value {
    // Atomic pair: a torn replay/cursor read would drop or duplicate bytes when a
    // remote client splices the live `output` frame stream onto this snapshot.
    let (raw, total) = shared
        .sessions
        .replay_with_cursor(uid)
        .map(|(r, c)| (r, c as i64))
        .unwrap_or_default();
    let since = non_neg_num(q.get("since"));
    let mut text = raw.clone();
    let mut cursor = total;
    let mut truncated = false;
    if let Some(s) = since {
        let sl = slice_since(&raw, total, s);
        text = sl.output;
        cursor = sl.cursor;
        truncated = sl.truncated;
    }
    let strip = q.get("strip").map(|v| v == "1").unwrap_or(false);
    if strip {
        text = strip_ansi(&text);
    }
    if let Some(tail) = tail_num(q.get("tail")) {
        text = tail_lines(&text, tail);
    }
    let status = pane_status_str(shared, pane_id);
    let mut body = json!({
        "paneId": pane_id,
        "status": status,
        "stripped": strip,
        "output": text,
        "cursor": cursor,
    });
    if let Some(s) = since {
        if let Value::Object(map) = &mut body {
            map.insert("since".into(), json!(s));
            map.insert("truncated".into(), json!(truncated));
        }
    }
    body
}

#[tracing::instrument(level = "debug", ret, skip(shared))]
fn build_screen_body(
    shared: &Arc<Shared>,
    pane_id: &str,
    uid: &str,
    q: &HashMap<String, String>,
) -> Value {
    let cursor = shared
        .sessions
        .output_bytes(uid)
        .map(|b| b as i64)
        .unwrap_or(0);
    match shared.sessions.render_screen(uid) {
        Some(full) => {
            // Prompt detection runs on the FULL screen so a clipping `tail` can't hide a blocked
            // prompt above the tail window.
            let awaiting = detect_awaiting_input(&full);
            let mut text = full;
            if let Some(tail) = tail_num(q.get("tail")) {
                text = tail_lines(&text, tail);
            }
            json!({
                "paneId": pane_id,
                "status": pane_status_str(shared, pane_id),
                "mode": "screen",
                "output": text,
                "cursor": cursor,
                "awaitingInput": awaiting,
            })
        }
        None => {
            // No screen (pane gone / wedged): fall back to the raw replay, flagged.
            let mut body = read_output_body(shared, pane_id, uid, q);
            if let Value::Object(map) = &mut body {
                map.insert("mode".into(), json!("raw"));
                map.insert("screenUnavailable".into(), json!(true));
            }
            body
        }
    }
}

/// Block until the pane has been output-quiet for `settle_ms` (or `timeout_ms` elapses), driving
/// an adaptive poll over the live tracking maps (pure `wait_decision`/`next_poll_delay`).
#[tracing::instrument(level = "debug", ret, skip(shared))]
async fn wait_for_quiet(
    shared: &Arc<Shared>,
    uid: &str,
    settle_ms: i64,
    timeout_ms: i64,
    since: Option<i64>,
) -> (bool, bool) {
    let start = now_ms();
    loop {
        let now = now_ms();
        let last = shared.sessions.last_output_at(uid).map(|t| t as i64);
        let total = shared
            .sessions
            .output_bytes(uid)
            .map(|b| b as i64)
            .unwrap_or(0);
        match wait_decision(last, total, since, now, start, settle_ms, timeout_ms) {
            WaitVerdict::Settled => return (true, false),
            WaitVerdict::Timeout => return (false, true),
            WaitVerdict::Wait => {
                let d = next_poll_delay(last, now, start, settle_ms, timeout_ms).max(1) as u64;
                tokio::time::sleep(Duration::from_millis(d)).await;
            }
        }
    }
}

// ---- /panes/{id}/input --------------------------------------------------------------------

/// Turn a failed pty write into the response the caller deserves.
///
/// `409` rather than `500`: nothing malfunctioned here. The pane the caller named is simply
/// not there any more — its process exited, or the session daemon holding it did — and the
/// only wrong answer is the `200 {"ok": true}` this used to give.
#[tracing::instrument(level = "debug", ret)]
fn write_failed(pane_id: &str, e: &std::io::Error) -> Response {
    jstatus(
        409,
        json!({
            "error": "input not delivered",
            "paneId": pane_id,
            "detail": e.to_string(),
        }),
    )
}

#[tracing::instrument(level = "debug", skip_all)]
async fn input(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    let found = match find_pane_scoped(&shared, info.scope.as_ref(), &id) {
        Ok(f) => f,
        Err(e) => return e,
    };
    if !shared.allow_input() {
        return jstatus(403, json!({ "error": "input not allowed" }));
    }
    let b: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let has_data = b.get("data").and_then(Value::as_str).is_some();
    let has_keys = b.get("keys").map(Value::is_array).unwrap_or(false);
    if !has_data && !has_keys {
        return jstatus(
            400,
            json!({ "error": "expected { data: string } or { keys: string[] }" }),
        );
    }
    // Advisory write lock (H): if someone else holds it, refuse.
    let owner = b.get("owner").and_then(Value::as_str);
    let holder = shared.locks.lock().unwrap().holder(&id, now_ms());
    if let Some(h) = &holder {
        if Some(h.as_str()) != owner {
            return jstatus(423, json!({ "error": "pane locked", "owner": h }));
        }
    }
    let uid = found.uid;

    if has_keys {
        let keys: Vec<String> = b
            .get("keys")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        return match keys_to_bytes(&refs) {
            KeysResult::Ok { bytes } => match shared.sessions.write(&uid, &bytes) {
                Ok(()) => jstatus(200, json!({ "ok": true, "keys": keys })),
                Err(e) => write_failed(&found.pane_id, &e),
            },
            KeysResult::Err { unknown } => jstatus(
                400,
                json!({ "error": "unknown key(s)", "unknown": unknown }),
            ),
        };
    }

    let data = b.get("data").and_then(Value::as_str).unwrap_or("");
    let platform = if cfg!(windows) { "win32" } else { "linux" };
    if let Err(e) = shared
        .sessions
        .write(&uid, &submit_newlines(data, platform))
    {
        return write_failed(&found.pane_id, &e);
    }
    // submit (A1): a bare CR as a SEPARATE pty write a beat later, so bracketed-paste TUIs read it
    // as Enter, not pasted content.
    //
    // This second write is genuinely unreportable: it happens after the response has gone
    // out. What the 200 above now means is precisely "the text landed" — the caller learns
    // about a pane that died in the intervening beat by watching its output, as it would for
    // any input it sent by hand.
    if b.get("submit").and_then(Value::as_bool) == Some(true) {
        let delay = b
            .get("submitDelayMs")
            .and_then(Value::as_i64)
            .filter(|&d| d >= 0)
            .map(|d| d as u64)
            .unwrap_or(SUBMIT_DELAY_MS);
        let sessions = Arc::clone(&shared.sessions);
        let uid2 = uid.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(delay)).await;
            let _ = sessions.write(&uid2, "\r");
        });
    }
    jstatus(200, json!({ "ok": true }))
}

// ---- /panes/{id}/messages -----------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MessagesOut {
    pane_id: String,
    messages: Vec<crate::control::inbox::PaneMessage>,
    dropped: usize,
    latest_seq: u64,
}

#[tracing::instrument(level = "debug", skip_all)]
async fn messages_get(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    // Canonical id: an inbox is keyed by the read-model's pane id, so an alias-addressed read
    // (`pane-<uuid>` vs bare `<uuid>`) must land on the same queue the writer posted to.
    let id = match find_pane_scoped(&shared, info.scope.as_ref(), &id) {
        Ok(f) => f.pane_id,
        Err(e) => return e,
    };
    let after = non_neg_num(q.get("after"))
        .filter(|&a| a > 0)
        .map(|a| a as u64)
        .unwrap_or(0);
    let inbox = shared.inbox.lock().unwrap();
    let out = MessagesOut {
        messages: inbox.read(&id, after),
        dropped: inbox.dropped_count(&id),
        latest_seq: inbox.latest_seq(&id),
        pane_id: id,
    };
    ok_json(out)
}

#[tracing::instrument(level = "debug", skip_all)]
async fn messages_post(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    // Canonical id: the inbox is keyed by the read-model's pane id, so an alias-addressed post
    // (`pane-<uuid>` vs bare `<uuid>`) lands on the queue the target actually reads.
    let id = match find_pane_scoped(&shared, info.scope.as_ref(), &id) {
        Ok(f) => f.pane_id,
        Err(e) => return e,
    };
    let b: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let from = b
        .get("from")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let msg_body = match b.get("body").and_then(Value::as_str) {
        Some(s) => s.to_string(),
        None => return jstatus(400, json!({ "error": "expected { from?, body: string }" })),
    };
    let msg = shared
        .inbox
        .lock()
        .unwrap()
        .post(&id, &from, &msg_body, now_ms());
    // Wake an opted-in agent pane (goals org): the durable read stays the source of truth, but a
    // TUI agent parked at its prompt would never perform that read on its own.
    arm_inbox_nudge(&shared, &id, msg.seq);
    // Nudge live, in-scope clients (the durable read remains the source of truth).
    let coords = shared.model.lock().unwrap().coords_of(&id);
    shared.events.broadcast_for_pane(
        coords.as_ref(),
        &ControlEvent::Message {
            to: id,
            from,
            seq: msg.seq,
            body: msg_body,
        },
    );
    jstatus(200, json!({ "ok": true, "seq": msg.seq }))
}

/// Whether inbox nudges are enabled at all — `AVADA_MSG_NUDGE=0` (or `false`/`off`) in the
/// app's environment turns the whole mechanism off, leaving the bus pull-only as before.
#[tracing::instrument(level = "debug", ret)]
fn nudges_enabled() -> bool {
    !matches!(
        crate::compat::env_var("AVADA_MSG_NUDGE").as_deref(),
        Some("0") | Some("false") | Some("off")
    )
}

/// Arm (and, if needed, spawn) the waiter that types a one-line "you have mail" into `pane_id`
/// once it goes quiet. See `control::nudge` for the policy: role opt-in, coalescing,
/// never-mid-turn, rate limit.
#[tracing::instrument(level = "debug", ret, skip(shared))]
fn arm_inbox_nudge(shared: &Arc<Shared>, pane_id: &str, seq: u64) {
    if !nudges_enabled() {
        return;
    }
    let (wants, uid, status) = {
        let m = shared.model.lock().unwrap();
        match m.pane(pane_id) {
            None => return,
            Some(p) => (
                nudge::wants_nudge(p.meta.as_ref()),
                p.session_uid.clone(),
                p.status,
            ),
        }
    };
    if !wants || status == PaneStatus::Exited {
        return;
    }
    if !shared.nudges.lock().unwrap().arm(pane_id, seq, now_ms()) {
        return; // a waiter is already in flight for this pane; it will pick the batch up
    }
    let shared = Arc::clone(shared);
    let pane_id = pane_id.to_string();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(nudge::SETTLE_MS)).await;
        loop {
            // Busy = mid-turn (a running command, or output within the idle threshold): typing
            // now would land in a live prompt, so wait for the pane to come to rest.
            let busy = {
                let m = shared.model.lock().unwrap();
                match m.pane(&pane_id) {
                    None => return, // pane closed while we waited
                    Some(p) => shared.compute_activity(p) == Activity::Busy,
                }
            };
            let step = shared.nudges.lock().unwrap().poll(&pane_id, busy, now_ms());
            match step {
                nudge::Step::Stop => return,
                nudge::Step::Wait => {
                    tokio::time::sleep(Duration::from_millis(nudge::POLL_MS)).await;
                }
                nudge::Step::Send(text) => {
                    // Same cadence the goal/resume delivery uses: text, gap, CR, insurance CR —
                    // a bracketed-paste TUI reads text+CR in one read as a paste otherwise.
                    let sessions = Arc::clone(&shared.sessions);
                    // Nobody is waiting on this task, so a failure has no one to be
                    // reported to — but it still means the following CRs would be typed
                    // at a pane that is not there. Give up instead of pressing Enter into
                    // the void.
                    if sessions.write(&uid, &text).is_err() {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    let _ = sessions.write(&uid, "\r");
                    tokio::time::sleep(Duration::from_millis(600)).await;
                    let _ = sessions.write(&uid, "\r");
                    return;
                }
            }
        }
    });
}

// ---- /panes/{id}/lock ---------------------------------------------------------------------

#[tracing::instrument(level = "debug", skip_all)]
async fn lock_post(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    if let Err(e) = find_pane_scoped(&shared, info.scope.as_ref(), &id) {
        return e;
    }
    let b: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let owner = match b.get("owner").and_then(Value::as_str) {
        Some(o) => o,
        None => {
            return jstatus(
                400,
                json!({ "error": "expected { owner: string, ttlMs? }" }),
            )
        }
    };
    let ttl = b
        .get("ttlMs")
        .and_then(Value::as_i64)
        .filter(|&t| t > 0)
        .unwrap_or(30_000);
    let r = shared
        .locks
        .lock()
        .unwrap()
        .acquire(&id, owner, now_ms(), ttl);
    if r.ok {
        jstatus(
            200,
            json!({ "ok": true, "owner": r.owner, "expiresAt": r.expires_at }),
        )
    } else {
        jstatus(
            423,
            json!({ "ok": false, "owner": r.owner, "expiresAt": r.expires_at, "error": "held" }),
        )
    }
}

#[tracing::instrument(level = "debug", skip_all)]
async fn lock_delete(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    if let Err(e) = find_pane_scoped(&shared, info.scope.as_ref(), &id) {
        return e;
    }
    let b: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let owner = match b.get("owner").and_then(Value::as_str) {
        Some(o) => o,
        None => return jstatus(400, json!({ "error": "expected { owner: string }" })),
    };
    let ok = shared.locks.lock().unwrap().release(&id, owner, now_ms());
    if ok {
        jstatus(200, json!({ "ok": true }))
    } else {
        jstatus(423, json!({ "ok": false, "error": "not the lock holder" }))
    }
}

// ---- work queue: /queues + /tasks ---------------------------------------------------------
//
// Same spine as every existing route: `authorize → 401`; a queue-scope gate → 403
// (`queue_in_scope`, master = any); camelCase JSON via `ok_json`/`jstatus`; bodies parsed
// with `serde_json::from_slice(..).unwrap_or(Value::Null)` and validated with the same
// `expected { … }` 400 style. One NEW status — `409 Conflict` for a stale lease (a wrong
// `fencingToken`, the optimistic-concurrency failure) — distinct from the lock module's
// `423 Locked`. The serialized `Task` IS the canonical wire format (camelCase, epoch-ms,
// flattened lease fields claimedBy/fencingToken/visibilityDeadline), so claim/get return it
// verbatim and the controller presents the task's own `fencingToken` back on ack/nack/extend.

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EnqueueOut {
    ok: bool,
    id: String,
    seq: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ClaimOut {
    ok: bool,
    tasks: Vec<Task>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TasksListOut {
    queue: String,
    tasks: Vec<Task>,
    counts: Counts,
    latest_seq: u64,
}

#[derive(Serialize)]
struct QueuesOut {
    queues: Vec<QueueSummary>,
}

/// Map a lease-guarded outcome (`ack`/`nack`/`extend`) to the byte-exact response: the
/// `ok_body` closure builds the 200 body from the resulting task; `Conflict → 409` (stale
/// `fencingToken`); `NotFound → 404`.
#[tracing::instrument(level = "debug", skip_all)]
fn lease_response(
    outcome: LeaseOutcome,
    id: &str,
    ok_body: impl FnOnce(&Task) -> Value,
) -> Response {
    match outcome {
        LeaseOutcome::Ok(task) => jstatus(200, ok_body(&task)),
        LeaseOutcome::Conflict => jstatus(409, json!({ "error": "stale lease", "taskId": id })),
        LeaseOutcome::NotFound => jstatus(404, json!({ "error": "no such task", "taskId": id })),
    }
}

/// The shared queue-scope gate: 403 unless `queue_in_scope` (master passes).
#[tracing::instrument(level = "debug", ret)]
fn queue_scope_gate(scope: Option<&Scope>, queue: &str) -> Option<Response> {
    if queue_in_scope(scope, queue) {
        None
    } else {
        Some(jstatus(
            403,
            json!({ "error": "queue out of scope", "queue": queue }),
        ))
    }
}

// POST /queues/{queue}/tasks — enqueue
#[tracing::instrument(level = "debug", skip_all)]
async fn task_enqueue(
    State(shared): State<Arc<Shared>>,
    Path(queue): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    if let Some(e) = queue_scope_gate(info.scope.as_ref(), &queue) {
        return e;
    }
    let b: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let payload = match b.get("payload").and_then(Value::as_str) {
        Some(s) => s.to_string(),
        None => {
            return jstatus(
                400,
                json!({ "error": "expected { payload: string, kind?, title?, priority?, maxAttempts?, visibilityTimeoutMs?, delayMs?|availableAt?, dedupeKey?, goalId?, dependsOn?: string[] }" }),
            )
        }
    };
    let mut opts = EnqueueOpts::default();
    if let Some(k) = b.get("kind").and_then(Value::as_str) {
        opts.kind = k.to_string();
    }
    if let Some(t) = b.get("title").and_then(Value::as_str) {
        opts.title = t.to_string();
    }
    if let Some(p) = b.get("priority").and_then(Value::as_i64) {
        opts.priority = p;
    }
    if let Some(m) = b
        .get("maxAttempts")
        .and_then(Value::as_i64)
        .filter(|&m| m > 0)
    {
        opts.max_attempts = m as u32;
    }
    if let Some(v) = b
        .get("visibilityTimeoutMs")
        .and_then(Value::as_i64)
        .filter(|&v| v > 0)
    {
        opts.visibility_timeout_ms = v;
    }
    let now = now_ms();
    // `availableAt` (absolute ms) wins; else `delayMs` schedules `now + delay`.
    if let Some(a) = b.get("availableAt").and_then(Value::as_i64) {
        opts.available_at = Some(a);
    } else if let Some(d) = b.get("delayMs").and_then(Value::as_i64).filter(|&d| d > 0) {
        opts.available_at = Some(now + d);
    }
    if let Some(k) = b.get("dedupeKey").and_then(Value::as_str) {
        opts.dedupe_key = Some(k.to_string());
    }
    // Goals system: `goalId` tags the task to a goal; `dependsOn` (array of task ids) gates the
    // task's claim until every listed task is `done` (see WorkQueue::claim).
    if let Some(g) = b.get("goalId").and_then(Value::as_str) {
        opts.goal_id = Some(g.to_string());
    }
    if let Some(deps) = b.get("dependsOn").and_then(Value::as_array) {
        opts.depends_on = Some(
            deps.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
        );
    }
    let task = shared
        .work
        .lock()
        .unwrap()
        .enqueue(&queue, &payload, opts, now);
    ok_json(EnqueueOut {
        ok: true,
        id: task.id,
        seq: task.seq,
    })
}

// GET /queues/{queue}/tasks — list/inspect (cursor `after`, optional `state`, `limit`)
#[tracing::instrument(level = "debug", skip_all)]
async fn tasks_list(
    State(shared): State<Arc<Shared>>,
    Path(queue): Path<String>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    if let Some(e) = queue_scope_gate(info.scope.as_ref(), &queue) {
        return e;
    }
    let after = non_neg_num(q.get("after"))
        .filter(|&a| a > 0)
        .map(|a| a as u64)
        .unwrap_or(0);
    let limit = pos_num(q.get("limit")).unwrap_or(100).min(1000) as usize;
    // Only a state string that round-trips is a real filter (an unknown value is ignored,
    // never silently coerced to `queued`).
    let state = q.get("state").and_then(|s| {
        let st = TaskState::from_wire(s);
        (st.as_str() == s).then_some(st)
    });
    let wq = shared.work.lock().unwrap();
    let tasks = wq.list(&queue, ListFilter { state }, after, limit);
    let counts = wq.counts(&queue);
    let latest_seq = tasks.iter().map(|t| t.seq).max().unwrap_or(after);
    ok_json(TasksListOut {
        queue,
        tasks,
        counts,
        latest_seq,
    })
}

// POST /queues/{queue}/claim — claim the next task(s) (competing consumers)
#[tracing::instrument(level = "debug", skip_all)]
async fn task_claim(
    State(shared): State<Arc<Shared>>,
    Path(queue): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    if let Some(e) = queue_scope_gate(info.scope.as_ref(), &queue) {
        return e;
    }
    let b: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let worker = match b
        .get("worker")
        .and_then(Value::as_str)
        .filter(|w| !w.is_empty())
    {
        Some(w) => w.to_string(),
        None => {
            return jstatus(
                400,
                json!({ "error": "expected { worker: string, leaseMs?, count? }" }),
            )
        }
    };
    // `leaseMs <= 0` ⇒ the queue falls back to the task's own visibility timeout.
    let lease_ms = b
        .get("leaseMs")
        .and_then(Value::as_i64)
        .filter(|&v| v > 0)
        .unwrap_or(0);
    let count = b
        .get("count")
        .and_then(Value::as_i64)
        .filter(|&c| c > 0)
        .map(|c| c.min(100) as usize)
        .unwrap_or(1);
    let now = now_ms();
    let mut tasks = Vec::new();
    {
        let mut wq = shared.work.lock().unwrap();
        for _ in 0..count {
            match wq.claim(&queue, &worker, lease_ms, now) {
                Some(c) => tasks.push(c.task),
                None => break, // queue drained — an EMPTY claim is 200 {tasks:[]}, never 204
            }
        }
    }
    ok_json(ClaimOut { ok: true, tasks })
}

// POST /queues/{queue}/purge — drop terminal tasks (retention/cleanup)
#[tracing::instrument(level = "debug", skip_all)]
async fn queue_purge(
    State(shared): State<Arc<Shared>>,
    Path(queue): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    if let Some(e) = queue_scope_gate(info.scope.as_ref(), &queue) {
        return e;
    }
    let b: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let now = now_ms();
    // `olderThan` = absolute ms cutoff; `olderThanMs` = an age (cutoff = now - age);
    // neither ⇒ purge every currently-terminal task.
    let older_than = if let Some(abs) = b.get("olderThan").and_then(Value::as_i64) {
        abs
    } else if let Some(age) = b.get("olderThanMs").and_then(Value::as_i64) {
        now - age
    } else {
        now
    };
    let removed = shared.work.lock().unwrap().purge(&queue, older_than);
    jstatus(200, json!({ "ok": true, "removed": removed }))
}

// GET /queues — every queue + its depth (scope-filtered)
#[tracing::instrument(level = "debug", skip_all)]
async fn queues_list(State(shared): State<Arc<Shared>>, headers: HeaderMap) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    let all = shared.work.lock().unwrap().queues();
    let queues = all
        .into_iter()
        .filter(|qs| queue_in_scope(info.scope.as_ref(), &qs.queue))
        .collect();
    ok_json(QueuesOut { queues })
}

// GET /tasks/{id} — fetch one task (scope resolved from its queue)
#[tracing::instrument(level = "debug", skip_all)]
async fn task_get(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    let task = shared.work.lock().unwrap().get(&id);
    match task {
        None => jstatus(404, json!({ "error": "no such task", "taskId": id })),
        Some(task) => {
            if let Some(e) = queue_scope_gate(info.scope.as_ref(), &task.queue) {
                return e;
            }
            ok_json(task)
        }
    }
}

/// Resolve `fencingToken` + the task's queue-scope for a lease op, or the error response.
/// Returns `(fencing_token, body)` on success.
// pre-existing; deferred per repo lint policy (test.yml)
#[allow(clippy::result_large_err)]
#[tracing::instrument(level = "debug", skip_all)]
fn lease_op_preamble(
    shared: &Arc<Shared>,
    info: &TokenInfo,
    id: &str,
    body: &Bytes,
) -> Result<(u64, Value), Response> {
    let b: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let token = match b.get("fencingToken").and_then(Value::as_u64) {
        Some(t) => t,
        None => {
            return Err(jstatus(
                400,
                json!({ "error": "expected { fencingToken: number, … }" }),
            ))
        }
    };
    // Resolve the task → its queue for the scope gate; 404 if it's gone.
    let queue = match shared.work.lock().unwrap().get(id) {
        Some(t) => t.queue,
        None => {
            return Err(jstatus(
                404,
                json!({ "error": "no such task", "taskId": id }),
            ))
        }
    };
    if let Some(e) = queue_scope_gate(info.scope.as_ref(), &queue) {
        return Err(e);
    }
    Ok((token, b))
}

// POST /tasks/{id}/ack — complete a claimed task (lease-guarded)
#[tracing::instrument(level = "debug", skip_all)]
async fn task_ack(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    let (token, b) = match lease_op_preamble(&shared, &info, &id, &body) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let result = b.get("result").and_then(Value::as_str);
    let outcome = shared
        .work
        .lock()
        .unwrap()
        .ack(&id, token, result, now_ms());
    lease_response(
        outcome,
        &id,
        |t| json!({ "ok": true, "state": t.state.as_str() }),
    )
}

// POST /tasks/{id}/nack — fail/retry a claimed task (lease-guarded)
#[tracing::instrument(level = "debug", skip_all)]
async fn task_nack(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    let (token, b) = match lease_op_preamble(&shared, &info, &id, &body) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let opts = NackOpts {
        // default requeue=true (retry with backoff); requeue=false ⇒ give up now (Failed)
        requeue: b.get("requeue").and_then(Value::as_bool).unwrap_or(true),
        error: b.get("error").and_then(Value::as_str).map(str::to_string),
        delay_ms: b.get("delayMs").and_then(Value::as_i64),
    };
    let outcome = shared.work.lock().unwrap().nack(&id, token, opts, now_ms());
    lease_response(
        outcome,
        &id,
        |t| json!({ "ok": true, "state": t.state.as_str() }),
    )
}

// POST /tasks/{id}/extend — heartbeat: extend the lease (lease-guarded)
#[tracing::instrument(level = "debug", skip_all)]
async fn task_extend(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    let (token, b) = match lease_op_preamble(&shared, &info, &id, &body) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let extra_ms = match b.get("extraMs").and_then(Value::as_i64).filter(|&x| x > 0) {
        Some(x) => x,
        None => {
            return jstatus(
                400,
                json!({ "error": "expected { fencingToken: number, extraMs: number }" }),
            )
        }
    };
    let outcome = shared
        .work
        .lock()
        .unwrap()
        .extend(&id, token, extra_ms, now_ms());
    lease_response(
        outcome,
        &id,
        |t| json!({ "ok": true, "visibilityDeadline": t.visibility_deadline }),
    )
}

// ---- /command -----------------------------------------------------------------------------

#[tracing::instrument(level = "debug", skip_all)]
async fn command(State(shared): State<Arc<Shared>>, headers: HeaderMap, body: Bytes) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    let cmd: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    // The route's own gate checked `workspace.write`; verbs that spend more (spawn a pane,
    // read a screen, type, restart the host) are checked here, per verb. Master-path tokens
    // hold everything, so this only ever bites a capability-limited token.
    if let Some(cap) = cmd
        .get("type")
        .and_then(Value::as_str)
        .and_then(verb_capability)
    {
        if let Some(token) = bearer_header(&headers) {
            if let Err(cap) = shared.caps.check(&token, cap) {
                tracing::warn!(capability = cap.name(), "command refused: capability");
                let (code, body) = capability_refusal(cap);
                return jstatus(code, body);
            }
        }
    }
    // `restartApp` is handled here, not in dispatch: it needs `Shared` (the GUI host polls
    // the flag each tick and performs the teardown on the UI thread). An optional
    // `sessionId` + `prompt` pair pre-queues a speak-first message for a conversation the
    // restore will resurrect. Root scope only — this takes the whole app down.
    if cmd.get("type").and_then(Value::as_str) == Some("restartApp") {
        if info.scope.is_some() {
            return jstatus(
                403,
                serde_json::json!({"error": "restartApp needs a root token"}),
            );
        }
        let scope = match cmd.get("scope").and_then(Value::as_str).unwrap_or("gui") {
            "gui" => 1u8,
            "full" => 2u8,
            other => {
                return jstatus(
                    400,
                    serde_json::json!({"error": format!("unknown scope: {other}")}),
                )
            }
        };
        if let (Some(sid), Some(text)) = (
            cmd.get("sessionId").and_then(Value::as_str),
            cmd.get("prompt").and_then(Value::as_str),
        ) {
            if let Err(e) = crate::resume_queue::enqueue(sid, text) {
                return jstatus(400, serde_json::json!({"error": e}));
            }
        }
        shared
            .restart_app
            .store(scope, std::sync::atomic::Ordering::SeqCst);
        return jstatus(
            200,
            serde_json::json!({"ok": true, "scope": if scope == 1 {"gui"} else {"full"}}),
        );
    }
    // Tab verbs are handled here too, and for the same reason as `restartApp`: the GUI owns
    // the tab tree and republishes it wholesale every sync tick, so a tab edit written into
    // the read model would be overwritten a frame later. They are validated now and QUEUED for
    // the UI thread (`control::uiops`).
    if let Some(ty) = cmd.get("type").and_then(Value::as_str) {
        if TAB_VERBS.contains(&ty) {
            return tab_command(&shared, &info, ty, &cmd);
        }
    }
    // Dictation verbs are handled here, not in dispatch, for two reasons: they need `Shared`
    // (the STT service AND the pty write that delivers the transcript), and `stopDictation`
    // BLOCKS for as long as the transcriber takes — dispatch runs holding the read-model
    // mutex, which must never be held across a whisper run.
    if let Some(ty) = cmd.get("type").and_then(Value::as_str) {
        if DICTATION_VERBS.contains(&ty) {
            return dictation_command(&shared, &info, ty, &cmd).await;
        }
    }
    let control_file = shared.control_file.to_str().map(str::to_string);
    let result = {
        let mut m = shared.model.lock().unwrap();
        dispatch::handle_command(
            &mut m,
            &shared.sessions,
            control_file.as_deref(),
            info.scope.as_ref(),
            &cmd,
            &shared.speech,
        )
    };
    // `setLayout` is dispatched above like any other model command, but a layout only exists
    // for real in the GUI: `publish` rebuilds each tab from the GUI's own state every tick and
    // would snap the read-model change straight back. Mirror the successful ones into the UI
    // queue. Best-effort — a full queue here is not worth failing a command that already
    // succeeded, and the caller's next `setLayout` will land.
    if cmd.get("type").and_then(Value::as_str) == Some("setLayout") && result.status < 300 {
        if let (Some(tab_id), Some(layout)) = (
            cmd.get("tabId").and_then(Value::as_str),
            cmd.get("layout").and_then(Value::as_str),
        ) {
            let _ = shared.ui_ops.lock().unwrap().push(UiOp::SetTabLayout {
                tab_id: tab_id.to_string(),
                layout: layout.to_string(),
            });
        }
    }
    // A closed pane must not leave a microphone running: nothing would ever stop it, and the
    // pty its transcript was bound for is gone. Cancel discards the audio rather than
    // transcribing it into nowhere.
    if cmd.get("type").and_then(Value::as_str) == Some("closePane") && result.status < 300 {
        if let Some(pane_id) = cmd.get("paneId").and_then(Value::as_str) {
            shared.dictation.cancel(pane_id);
        }
    }
    // Phase-5: keep supervisor policies in lockstep with pane meta (setMeta flips
    // hp.supervise; newPane carries meta; closePane removes a pane). Cheap + idempotent.
    shared.reconcile_policies();
    if result.notify_state {
        notify_state(&shared);
    }
    // A project-opening newPane bumped the registry's recency off-thread → tell the GUI host.
    if result.projects_dirty {
        shared.mark_projects_dirty();
    }
    jstatus(result.status, result.body)
}

// ---- tabs + settings (queued for the UI thread) --------------------------------------------
// Both surfaces edit state the GUI owns rather than the read model, so both take the
// `control::uiops` path: validate here, apply on the next sync tick. See that module for why.

/// `/command` verbs that drive a pane's microphone.
const DICTATION_VERBS: &[&str] = &["startDictation", "stopDictation", "cancelDictation"];

/// Start / stop / cancel a pane's microphone.
///
/// `stopDictation` is the whole feature: it stops the recorder, waits for the transcriber,
/// and types the result into the pane. The wait happens on a blocking task — a cold whisper
/// model is seconds, and the async executor's threads are not ours to occupy. The transcript
/// is [sanitized](crate::control::dictation_service::sanitize_for_pane) before it reaches the
/// pty, and Enter is a SEPARATE delayed write exactly as in `/panes/{id}/input`, so a
/// bracketed-paste TUI reads it as a keypress rather than pasted content.
#[tracing::instrument(level = "debug", skip_all)]
async fn dictation_command(
    shared: &Arc<Shared>,
    info: &TokenInfo,
    ty: &str,
    cmd: &Value,
) -> Response {
    let pane_id = match cmd.get("paneId").and_then(Value::as_str) {
        Some(p) => p,
        None => return jstatus(400, json!({ "error": format!("{ty} needs a paneId") })),
    };
    let found = match find_pane_scoped(shared, info.scope.as_ref(), pane_id) {
        Ok(f) => f,
        Err(e) => return e,
    };
    // Dictation is input: it types into a pane, so it lives or dies with the same switch.
    if !shared.allow_input() {
        return jstatus(403, json!({ "error": "input not allowed" }));
    }

    match ty {
        "cancelDictation" => {
            shared.dictation.cancel(&found.pane_id);
            jstatus(200, json!({ "ok": true, "paneId": found.pane_id }))
        }
        "startDictation" => match crate::control::dictation_service::start_dictation(
            shared,
            &found.pane_id,
            &found.uid,
        ) {
            Ok(recorder) => jstatus(
                200,
                json!({ "ok": true, "paneId": found.pane_id, "recorder": recorder }),
            ),
            Err(e) => jstatus(400, json!({ "error": e, "paneId": found.pane_id })),
        },
        _ => {
            let s2 = Arc::clone(shared);
            let pane = found.pane_id.clone();
            let uid = found.uid.clone();
            let delivered = match tokio::task::spawn_blocking(move || {
                crate::control::dictation_service::stop_and_deliver(&s2, &pane, &uid)
            })
            .await
            {
                Ok(r) => r,
                Err(_) => Err("dictation task failed".to_string()),
            };
            match delivered {
                Ok(d) => jstatus(
                    200,
                    json!({
                        "ok": true,
                        "paneId": found.pane_id,
                        "text": d.text,
                        "backend": d.backend,
                        "submitted": d.submitted,
                        // Where the audio and this text were kept, so a caller that got a
                        // transcript it does not believe has something to go back to.
                        "kept": d.kept.as_ref().map(|p| p.display().to_string()),
                    }),
                ),
                Err(e) => jstatus(400, json!({ "error": e, "paneId": found.pane_id })),
            }
        }
    }
}

/// `/command` verbs that edit the tab tree.
const TAB_VERBS: &[&str] = &["newTab", "closeTab", "renameTab", "focusTab", "moveTab"];

/// Queue a UI op, turning a full queue into a 503 rather than a silent drop.
#[tracing::instrument(level = "debug", ret, skip(shared))]
fn queue_ui_op(shared: &Arc<Shared>, op: UiOp, body: Value) -> Response {
    if shared.ui_ops.lock().unwrap().push(op) {
        jstatus(202, body)
    } else {
        jstatus(
            503,
            json!({ "error": "the UI op queue is full — is the app running?" }),
        )
    }
}

/// Resolve a tab id against the read model, 404ing an id the model has never seen. Tab ids are
/// positional, so a stale one would otherwise silently address whatever tab now sits at that
/// index — the worst possible failure mode for `closeTab`.
// pre-existing convention; deferred per repo lint policy (test.yml)
#[allow(clippy::result_large_err)]
#[tracing::instrument(level = "debug", ret, skip(shared))]
fn find_tab(shared: &Arc<Shared>, cmd: &Value) -> Result<(String, i64), Response> {
    let tab_id = match cmd.get("tabId").and_then(Value::as_str) {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => {
            return Err(jstatus(
                400,
                json!({ "error": "missing string field: tabId" }),
            ))
        }
    };
    match shared.model.lock().unwrap().tab_window(&tab_id) {
        Some(wid) => Ok((tab_id, wid)),
        None => Err(jstatus(
            404,
            json!({ "error": "no such tab", "tabId": tab_id }),
        )),
    }
}

/// Handle one tab verb. Tab edits are structural and window-wide, so — like `restartApp` —
/// they need a root token; a scoped token is a grant over named panes, not over the workspace
/// those panes happen to sit in.
#[tracing::instrument(level = "debug", skip_all)]
fn tab_command(shared: &Arc<Shared>, info: &TokenInfo, ty: &str, cmd: &Value) -> Response {
    if info.scope.is_some() {
        return jstatus(403, json!({ "error": format!("{ty} needs a root token") }));
    }
    match ty {
        "newTab" => {
            let window_id = match cmd.get("windowId").and_then(Value::as_i64) {
                Some(w) => w,
                None => match shared.model.lock().unwrap().first_window_id() {
                    Some(w) => w,
                    None => return jstatus(503, json!({ "error": "no window" })),
                },
            };
            // The id the tab WILL have: ids are positional and only the UI thread appends.
            let index = match shared.model.lock().unwrap().tab_count(window_id) {
                Some(n) => n,
                None => {
                    return jstatus(
                        404,
                        json!({ "error": "no such window", "windowId": window_id }),
                    )
                }
            };
            let title = cmd
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_string)
                .filter(|t| !t.is_empty());
            let cwd = cmd
                .get("cwd")
                .and_then(Value::as_str)
                .map(str::to_string)
                .filter(|c| !c.is_empty());
            queue_ui_op(
                shared,
                UiOp::NewTab {
                    window_id,
                    title,
                    cwd,
                },
                json!({ "ok": true, "queued": true, "tabId": format!("{window_id}:{index}") }),
            )
        }
        "closeTab" => match find_tab(shared, cmd) {
            Err(e) => e,
            Ok((tab_id, _)) => {
                // Refuse an app-owned tab here rather than queueing it. `State::close_tab`
                // would drop the op on the UI thread anyway, but by then we have already
                // answered 202 — and a caller told "queued" has no way to learn the close
                // never happened. 409: the tab exists, its state forbids the verb.
                let system = shared
                    .model
                    .lock()
                    .unwrap()
                    .tab(&tab_id)
                    .is_some_and(|t| t.system);
                if system {
                    jstatus(
                        409,
                        json!({
                            "error": "the app owns this tab; the close is refused",
                            "tabId": tab_id,
                        }),
                    )
                } else {
                    queue_ui_op(
                        shared,
                        UiOp::CloseTab {
                            tab_id: tab_id.clone(),
                        },
                        json!({ "ok": true, "queued": true, "tabId": tab_id }),
                    )
                }
            }
        },
        "renameTab" => {
            let title = match cmd.get("title").and_then(Value::as_str) {
                Some(t) => t.to_string(),
                None => return jstatus(400, json!({ "error": "missing string field: title" })),
            };
            match find_tab(shared, cmd) {
                Err(e) => e,
                Ok((tab_id, _)) => queue_ui_op(
                    shared,
                    UiOp::RenameTab {
                        tab_id: tab_id.clone(),
                        title,
                    },
                    json!({ "ok": true, "queued": true, "tabId": tab_id }),
                ),
            }
        }
        "focusTab" => match find_tab(shared, cmd) {
            Err(e) => e,
            Ok((tab_id, _)) => queue_ui_op(
                shared,
                UiOp::FocusTab {
                    tab_id: tab_id.clone(),
                },
                json!({ "ok": true, "queued": true, "tabId": tab_id }),
            ),
        },
        "moveTab" => {
            let to = match cmd.get("to").and_then(Value::as_i64).filter(|&t| t >= 0) {
                Some(t) => t as usize,
                None => {
                    return jstatus(
                        400,
                        json!({ "error": "missing non-negative integer field: to" }),
                    )
                }
            };
            match find_tab(shared, cmd) {
                Err(e) => e,
                Ok((tab_id, _)) => queue_ui_op(
                    shared,
                    UiOp::MoveTab {
                        tab_id: tab_id.clone(),
                        to,
                    },
                    json!({ "ok": true, "queued": true, "tabId": tab_id, "to": to }),
                ),
            }
        }
        _ => jstatus(400, json!({ "error": format!("unknown command: {ty}") })),
    }
}

/// The app's live preferences. Published by the GUI each sync tick — absent (503) when no GUI
/// is attached, rather than a defaults blob that would not describe anything real.
#[tracing::instrument(level = "debug", skip_all)]
async fn settings_get(State(shared): State<Arc<Shared>>, headers: HeaderMap) -> Response {
    if let Err(e) = authorize(&shared, &headers) {
        return e;
    }
    match shared.settings.lock().unwrap().clone() {
        Some(v) => ok_json(json!({ "settings": v })),
        None => jstatus(
            503,
            json!({ "error": "settings unavailable (no GUI attached)" }),
        ),
    }
}

/// Merge a camelCase patch into the app preferences. Root token only: preferences are global,
/// and some of them (`keepAlive`, `browserMode`) change what happens to every pane in the app.
/// Unknown keys are rejected up front — a typo that silently does nothing is worse than a 400.
#[tracing::instrument(level = "debug", skip_all)]
async fn settings_patch(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    if info.scope.is_some() {
        return jstatus(403, json!({ "error": "settings needs a root token" }));
    }
    let patch: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return jstatus(400, json!({ "error": "expected a JSON object" })),
    };
    let Some(obj) = patch.as_object() else {
        return jstatus(400, json!({ "error": "expected a JSON object" }));
    };
    if obj.is_empty() {
        return jstatus(400, json!({ "error": "expected at least one setting" }));
    }
    // Validate keys against the blob the GUI published — that is the live shape of `Settings`,
    // so this stays correct as fields are added without core having to know them.
    let published = shared.settings.lock().unwrap().clone();
    let Some(known) = published.as_ref().and_then(Value::as_object) else {
        return jstatus(
            503,
            json!({ "error": "settings unavailable (no GUI attached)" }),
        );
    };
    let unknown: Vec<&str> = obj
        .keys()
        .filter(|k| !known.contains_key(*k))
        .map(String::as_str)
        .collect();
    if !unknown.is_empty() {
        return jstatus(400, json!({ "error": "unknown settings", "keys": unknown }));
    }
    let keys: Vec<String> = obj.keys().cloned().collect();
    // Unlike the tab ops, a settings patch is answered with its OUTCOME: the preferences
    // layer validates values (palette index, effect token, log level, ...) on the UI thread,
    // and a caller told `ok` for a value that was then thrown away has no way to find out.
    // So queue the op with a reply handle and wait — bounded, so a headless embedder (nothing
    // drains the queue) still gets the historical `202 queued` instead of hanging.
    let (reply, rx) = crate::control::uiops::PatchReply::new();
    let queued = shared.ui_ops.lock().unwrap().push(UiOp::PatchSettings {
        patch: patch.clone(),
        reply: Some(reply),
    });
    if !queued {
        return jstatus(
            503,
            json!({ "error": "the UI op queue is full — is the app running?" }),
        );
    }
    match tokio::time::timeout(std::time::Duration::from_secs(5), rx).await {
        Ok(Ok(Ok(()))) => ok_json(json!({ "ok": true, "applied": true, "keys": keys })),
        Ok(Ok(Err(e))) => jstatus(400, json!({ "error": e, "keys": keys })),
        // The host dropped the handle without answering, or nothing drained the queue in time.
        _ => jstatus(202, json!({ "ok": true, "queued": true, "keys": keys })),
    }
}

// ---- /projects ----------------------------------------------------------------------------
// The project registry (`projects.json`): the directories the app remembers, shared by the
// GUI sidebar rail. These are global, not pane-scoped — any authorized token may use them. The
// `core::persistence::projects` layer owns the file; a write marks the GUI host's dirty flag so
// the rail refreshes live (`ControlHost::sync`).

#[tracing::instrument(level = "debug", skip_all)]
async fn projects_list(State(shared): State<Arc<Shared>>, headers: HeaderMap) -> Response {
    if let Err(e) = authorize(&shared, &headers) {
        return e;
    }
    ok_json(json!({ "projects": projects::list_projects() }))
}

#[tracing::instrument(level = "debug", skip_all)]
async fn projects_add(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(e) = authorize(&shared, &headers) {
        return e;
    }
    let b: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let dir = match b.get("dir").and_then(Value::as_str) {
        Some(d) if !d.trim().is_empty() => d.trim(),
        _ => return jstatus(400, json!({ "error": "expected { dir: string }" })),
    };
    // Mirror the GUI Add-Project dialog: the path must exist and be a directory (a git repo is
    // NOT required — `add_project_explicit` happily tracks any folder).
    if !std::path::Path::new(dir).is_dir() {
        return jstatus(
            400,
            json!({ "error": "path doesn't exist or isn't a directory", "dir": dir }),
        );
    }
    let (project, added) = projects::add_project_explicit(dir);
    shared.mark_projects_dirty();
    ok_json(json!({ "ok": true, "added": added, "project": project }))
}

#[tracing::instrument(level = "debug", skip_all)]
async fn projects_patch(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(e) = authorize(&shared, &headers) {
        return e;
    }
    let b: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let name = b
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let color = b
        .get("color")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    if name.is_none() && color.is_none() {
        return jstatus(
            400,
            json!({ "error": "expected { name?: string, color?: string }" }),
        );
    }
    if let Some(name) = name {
        projects::rename_project(&id, name);
    }
    if let Some(color) = color {
        projects::set_project_color(&id, color);
    }
    shared.mark_projects_dirty();
    ok_json(json!({ "ok": true }))
}

#[tracing::instrument(level = "debug", skip_all)]
async fn projects_delete(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = authorize(&shared, &headers) {
        return e;
    }
    projects::remove_project(&id);
    shared.mark_projects_dirty();
    ok_json(json!({ "ok": true }))
}

// ---- /events (WebSocket) ------------------------------------------------------------------

#[tracing::instrument(level = "debug", skip_all)]
async fn events_ws(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> Response {
    // Token from the `?token=` query (WS clients can't set Authorization reliably) or a Bearer header.
    let token = q.get("token").cloned().or_else(|| bearer_header(&headers));
    let Some(info) = identify(&shared, token.as_deref()) else {
        return (StatusCode::UNAUTHORIZED, "").into_response();
    };
    let scope = info.scope;
    let shared2 = Arc::clone(&shared);
    ws.on_upgrade(move |socket| handle_ws(shared2, socket, scope))
}

#[tracing::instrument(level = "debug", ret, skip(shared))]
async fn handle_ws(shared: Arc<Shared>, mut socket: WebSocket, scope: Option<Scope>) {
    let (id, mut rx) = shared.events.add_client(scope);
    // Greet first (this frame is queued ahead of any fan-out).
    shared.events.send_to(
        id,
        &ControlEvent::Hello {
            pid: shared.pid,
            version: shared.version.clone(),
        },
    );
    loop {
        tokio::select! {
            maybe = rx.recv() => match maybe {
                Some(text) => {
                    if socket.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                None => break,
            },
            inbound = socket.recv() => match inbound {
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(_)) => break,
            },
        }
    }
    shared.events.remove_client(id);
}

// ---- fallbacks ----------------------------------------------------------------------------

/// `GET /schema`: the descriptor table as a `SchemaDocument` — every route, RPC and module
/// the host serves, sorted, so two calls on an unchanged host are byte-identical. Any
/// authenticated token may read it (it is the API's own description, not data).
#[tracing::instrument(level = "debug", skip_all)]
async fn schema_get(State(shared): State<Arc<Shared>>, headers: HeaderMap) -> Response {
    if let Err(e) = authorize(&shared, &headers) {
        return e;
    }
    ok_json(shared.schema.document(&shared.version))
}

#[tracing::instrument(level = "debug", ret)]
async fn method_not_allowed() -> Response {
    jstatus(405, json!({ "error": "method not allowed" }))
}

#[tracing::instrument(level = "debug", ret)]
async fn not_found(uri: Uri) -> Response {
    jstatus(404, json!({ "error": "not found", "path": uri.path() }))
}

// ---- marketplace: /marketplace/... (track F2) ---------------------------------------------

/// The installed marketplace, or the 503 every marketplace route answers until the app
/// installs one (`Shared::install_marketplace`). Same contract as the module host: the
/// route is listed, the service is simply not reachable yet.
// deferred per repo lint policy (test.yml): the error arm is a full Response
#[allow(clippy::result_large_err)]
fn marketplace_of(shared: &Arc<Shared>) -> Result<Arc<crate::marketplace::Marketplace>, Response> {
    let mp = shared.marketplace.read().unwrap().clone();
    mp.ok_or_else(|| jstatus(503, json!({ "error": "marketplace unavailable" })))
}

/// Identity first (401 before a stranger learns whether a marketplace exists), then the
/// service (503). The capability gate (403) already ran in the router.
#[allow(clippy::result_large_err)]
fn marketplace_for(
    shared: &Arc<Shared>,
    headers: &HeaderMap,
) -> Result<Arc<crate::marketplace::Marketplace>, Response> {
    authorize(shared, headers)?;
    marketplace_of(shared)
}

/// A marketplace refusal as a response: the status is the error's own
/// (`MarketplaceError::http_status`), the body its message. A missing toolchain carries
/// the install guide under its own key so a client can show it verbatim.
fn marketplace_error(e: crate::marketplace::MarketplaceError) -> Response {
    use crate::marketplace::MarketplaceError;
    let status = e.http_status();
    let body = match &e {
        MarketplaceError::Toolchain(guide) => json!({
            "error": "toolchain missing",
            "guide": guide,
        }),
        _ => json!({ "error": e.to_string() }),
    };
    tracing::debug!(status, error = %e, "marketplace refusal");
    jstatus(status, body)
}

/// An optional JSON object body: nothing is `{}`, unparsable is 400. Bodies are read raw
/// so the identity and service checks run before axum's own 415/422 would.
#[allow(clippy::result_large_err)]
fn marketplace_body(body: &Bytes) -> Result<Value, Response> {
    if body.is_empty() {
        return Ok(json!({}));
    }
    match serde_json::from_slice::<Value>(body) {
        Ok(v) if v.is_object() => Ok(v),
        Ok(_) => Err(jstatus(
            400,
            json!({ "error": "bad request", "message": "body must be a JSON object" }),
        )),
        Err(e) => Err(jstatus(
            400,
            json!({ "error": "bad request", "message": e.to_string() }),
        )),
    }
}

fn body_str(body: &Value, key: &str) -> Option<String> {
    body.get(key).and_then(Value::as_str).map(str::to_string)
}

#[tracing::instrument(level = "debug", skip_all)]
async fn marketplace_search(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let mp = match marketplace_for(&shared, &headers) {
        Ok(m) => m,
        Err(r) => return r,
    };
    let words = q.get("q").cloned().unwrap_or_default();
    match mp.search(&words).await {
        Ok(hits) => ok_json(json!({ "modules": hits })),
        Err(e) => marketplace_error(e),
    }
}

#[tracing::instrument(level = "debug", skip_all)]
async fn marketplace_show(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
) -> Response {
    let mp = match marketplace_for(&shared, &headers) {
        Ok(m) => m,
        Err(r) => return r,
    };
    match mp.show(&owner, &repo).await {
        Ok(view) => ok_json(view),
        Err(e) => marketplace_error(e),
    }
}

/// `{module, tag?, accepted?, workspace?, commit?}` → 202 `{job}`: the pipeline runs in
/// the background and `marketplace.job` shows it move.
#[tracing::instrument(level = "debug", skip_all)]
async fn marketplace_install(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let mp = match marketplace_for(&shared, &headers) {
        Ok(m) => m,
        Err(r) => return r,
    };
    let body = match marketplace_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let Some(module) = body_str(&body, "module").filter(|m| !m.is_empty()) else {
        return jstatus(400, json!({ "error": "missing module" }));
    };
    let accepted = match body.get("accepted") {
        None | Some(Value::Null) => None,
        Some(v) => match serde_json::from_value::<BTreeSet<Capability>>(v.clone()) {
            Ok(set) => Some(set),
            Err(e) => {
                return jstatus(
                    400,
                    json!({ "error": "bad request", "message": format!("accepted: {e}") }),
                )
            }
        },
    };
    let mut req = crate::marketplace::InstallRequest::new(&module);
    req.tag = body_str(&body, "tag");
    req.accepted = accepted;
    req.workspace = body_str(&body, "workspace");
    req.expected_commit = body_str(&body, "commit");
    match mp.install(req) {
        Ok(job) => jstatus(202, json!({ "job": job })),
        Err(e) => marketplace_error(e),
    }
}

#[tracing::instrument(level = "debug", skip_all)]
async fn marketplace_jobs(State(shared): State<Arc<Shared>>, headers: HeaderMap) -> Response {
    let mp = match marketplace_for(&shared, &headers) {
        Ok(m) => m,
        Err(r) => return r,
    };
    ok_json(json!({ "jobs": mp.jobs() }))
}

#[tracing::instrument(level = "debug", skip_all)]
async fn marketplace_job(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let mp = match marketplace_for(&shared, &headers) {
        Ok(m) => m,
        Err(r) => return r,
    };
    match mp.job(&id) {
        Ok(job) => ok_json(job),
        Err(e) => marketplace_error(e),
    }
}

/// Shared body of enable/disable: `{workspace}` → `{module, enabled: {workspace: bool}}`.
async fn marketplace_set_enabled(
    shared: Arc<Shared>,
    headers: HeaderMap,
    owner: String,
    repo: String,
    body: Bytes,
    enabled: bool,
) -> Response {
    let mp = match marketplace_for(&shared, &headers) {
        Ok(m) => m,
        Err(r) => return r,
    };
    let body = match marketplace_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let Some(workspace) = body_str(&body, "workspace").filter(|w| !w.is_empty()) else {
        return jstatus(400, json!({ "error": "missing workspace" }));
    };
    let module = format!("{owner}/{repo}");
    match mp.set_enabled(&workspace, &module, enabled) {
        Ok(map) => ok_json(json!({ "module": module, "enabled": map })),
        Err(e) => marketplace_error(e),
    }
}

#[tracing::instrument(level = "debug", skip_all)]
async fn marketplace_enable(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    marketplace_set_enabled(shared, headers, owner, repo, body, true).await
}

#[tracing::instrument(level = "debug", skip_all)]
async fn marketplace_disable(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    marketplace_set_enabled(shared, headers, owner, repo, body, false).await
}

#[tracing::instrument(level = "debug", skip_all)]
async fn marketplace_uninstall(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    Path((owner, repo, version)): Path<(String, String, String)>,
) -> Response {
    let mp = match marketplace_for(&shared, &headers) {
        Ok(m) => m,
        Err(r) => return r,
    };
    let module = format!("{owner}/{repo}");
    match mp.uninstall(&module, &version) {
        Ok(()) => ok_json(json!({ "ok": true, "module": module, "version": version })),
        Err(e) => marketplace_error(e),
    }
}

#[tracing::instrument(level = "debug", skip_all)]
async fn marketplace_installed(State(shared): State<Arc<Shared>>, headers: HeaderMap) -> Response {
    let mp = match marketplace_for(&shared, &headers) {
        Ok(m) => m,
        Err(r) => return r,
    };
    match mp.installed() {
        Ok(list) => ok_json(json!({ "modules": list })),
        Err(e) => marketplace_error(e),
    }
}

/// The readiness report: the toolchain, whether a free build can run, the guide when it
/// cannot, and whether a GitHub token is stored (never the token).
#[tracing::instrument(level = "debug", skip_all)]
async fn marketplace_toolchain(State(shared): State<Arc<Shared>>, headers: HeaderMap) -> Response {
    let mp = match marketplace_for(&shared, &headers) {
        Ok(m) => m,
        Err(r) => return r,
    };
    let tc = mp.toolchain();
    ok_json(json!({
        "ready": tc.ready(),
        "missing": tc.missing(),
        "guide": tc.guide(),
        "toolchain": tc,
        "signed_in": mp.signed_in(),
    }))
}

#[tracing::instrument(level = "debug", skip_all)]
async fn marketplace_signin(State(shared): State<Arc<Shared>>, headers: HeaderMap) -> Response {
    let mp = match marketplace_for(&shared, &headers) {
        Ok(m) => m,
        Err(r) => return r,
    };
    match mp.signin_start().await {
        Ok(view) => ok_json(view),
        Err(e) => marketplace_error(e),
    }
}

#[tracing::instrument(level = "debug", skip_all)]
async fn marketplace_signin_poll(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let mp = match marketplace_for(&shared, &headers) {
        Ok(m) => m,
        Err(r) => return r,
    };
    match mp.signin_poll(&id).await {
        Ok(view) => ok_json(view),
        Err(e) => marketplace_error(e),
    }
}

// ---- module routes: /m/<owner>/<repo>/... ---------------------------------------------------

fn verb_of(method: &Method) -> Option<Verb> {
    match *method {
        Method::GET => Some(Verb::Get),
        Method::POST => Some(Verb::Post),
        Method::PUT => Some(Verb::Put),
        Method::PATCH => Some(Verb::Patch),
        Method::DELETE => Some(Verb::Delete),
        _ => None,
    }
}

/// Every request under `/m/`. Order matters and mirrors the core routes: identity first
/// (401 before a stranger learns which modules exist), then the registry lookup (404 /
/// 405 with the fallback bodies), then the descriptor's capability (403), then the
/// forward. A listed route whose module is not reachable is 503, a module that fails
/// mid-call is 502, and the module's own JSON-RPC error comes back as 400 with its
/// code and message — the client sent something the module refused.
#[tracing::instrument(level = "debug", skip_all, fields(method = %method, path = %uri.path()))]
async fn module_route(
    State(shared): State<Arc<Shared>>,
    Path(caps): Path<HashMap<String, String>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    body: Bytes,
) -> Response {
    let token = match authorize(&shared, &headers) {
        Ok(_) => bearer_header(&headers).unwrap_or_default(),
        Err(r) => return r,
    };
    let owner = caps.get("owner").cloned().unwrap_or_default();
    let repo = caps.get("repo").cloned().unwrap_or_default();
    let rest = caps.get("rest").cloned().unwrap_or_default();
    let Ok(module) = ModuleId::new(&format!("{owner}/{repo}")) else {
        return jstatus(404, json!({ "error": "not found", "path": uri.path() }));
    };
    let Some(verb) = verb_of(&method) else {
        return method_not_allowed().await;
    };
    let sub = format!("/{rest}");
    let matched = shared
        .schema
        .with(|reg| match_route(reg.routes(), &module, verb, &sub));
    let (desc, mut params) = match matched {
        Match::Route(desc, params) => (desc, params),
        Match::WrongVerb => return method_not_allowed().await,
        Match::NoRoute => return jstatus(404, json!({ "error": "not found", "path": uri.path() })),
    };
    if let Some(cap) = desc.capability {
        if let Err(cap) = shared.caps.check(&token, cap) {
            tracing::warn!(capability = cap.name(), "module route refused: capability");
            let (code, body) = capability_refusal(cap);
            return jstatus(code, body);
        }
    }
    for (k, v) in query {
        params.entry(k).or_insert(Value::String(v));
    }
    let body = if body.is_empty() {
        None
    } else {
        match serde_json::from_slice::<Value>(&body) {
            Ok(v) => Some(v),
            Err(e) => {
                return jstatus(
                    400,
                    json!({ "error": "bad request", "message": format!("body is not JSON: {e}") }),
                )
            }
        }
    };
    let invoker = shared.modules.read().unwrap().clone();
    let Some(invoker) = invoker else {
        return jstatus(
            503,
            json!({ "error": "module unavailable", "module": module.as_str() }),
        );
    };
    let call = RouteCall {
        module: module.clone(),
        route: desc.method.clone(),
        params,
        body,
    };
    match invoker.invoke(call).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(InvokeError::Unavailable(message)) => jstatus(
            503,
            json!({ "error": "module unavailable", "module": module.as_str(), "message": message }),
        ),
        Err(InvokeError::Module {
            code,
            message,
            data,
        }) => jstatus(
            400,
            json!({ "error": "module", "code": code, "message": message, "data": data }),
        ),
        Err(InvokeError::Failed(message)) => jstatus(
            502,
            json!({ "error": "module failed", "module": module.as_str(), "message": message }),
        ),
    }
}

// ---- query-param parsing (mirrors the TS posNum / nonNegNum / Number(tail)) ----------------

#[tracing::instrument(level = "debug", ret)]
fn pos_num(v: Option<&String>) -> Option<i64> {
    v.and_then(|s| s.parse::<f64>().ok())
        .filter(|n| n.is_finite() && *n > 0.0)
        .map(|n| n as i64)
}

#[tracing::instrument(level = "debug", ret)]
fn non_neg_num(v: Option<&String>) -> Option<i64> {
    v.and_then(|s| s.parse::<f64>().ok())
        .filter(|n| n.is_finite() && *n >= 0.0)
        .map(|n| n as i64)
}

#[tracing::instrument(level = "debug", ret)]
fn tail_num(v: Option<&String>) -> Option<usize> {
    v.and_then(|s| s.parse::<f64>().ok())
        .filter(|n| n.is_finite() && *n > 0.0)
        .map(|n| n as usize)
}

#[tracing::instrument(level = "debug", ret)]
fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn num_parsers_match_ts_semantics() {
        assert_eq!(pos_num(Some(&"600".to_string())), Some(600));
        assert_eq!(pos_num(Some(&"0".to_string())), None);
        assert_eq!(pos_num(Some(&"-5".to_string())), None);
        assert_eq!(pos_num(None), None);
        assert_eq!(non_neg_num(Some(&"0".to_string())), Some(0));
        assert_eq!(non_neg_num(Some(&"-1".to_string())), None);
        assert_eq!(tail_num(Some(&"3".to_string())), Some(3));
        assert_eq!(tail_num(Some(&"0".to_string())), None);
    }

    #[test]
    fn tail_lines_keeps_last_n() {
        assert_eq!(tail_lines("a\nb\nc\nd", 2), "c\nd");
        assert_eq!(tail_lines("a\nb", 5), "a\nb");
    }
}

/// Golden-JSON parity: boot the REAL axum stack and assert byte-exact response bodies for every
/// route + error shape (modulo port/token/pid), against the `src/main/control-server.ts` oracle.
/// Runs the full socket round-trip with `reqwest` (a lib dependency, available to unit tests).
#[cfg(test)]
mod golden {
    use super::router;
    use crate::control::readmodel::{PaneInfo, PaneStatus, TabInfo, WindowInfo};
    use crate::control::server::Shared;
    use crate::control::uiops::UiOp;
    use crate::session_manager::SessionManager;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    pub(super) struct Server {
        pub(super) shared: Arc<Shared>,
        pub(super) base: String,
        pub(super) token: String,
    }

    async fn boot(allow_input: bool) -> Server {
        boot_with_control_tag(allow_input, "golden").await
    }

    /// Same server, but with its `control.json` in its own directory — and therefore its own
    /// `device-tokens.json`, which lives beside it (`persist_devices` derives the path with
    /// `with_file_name`, so only a distinct *directory* isolates it). Any test that touches the
    /// device table needs this: the tests run in parallel, and a shared table means one test
    /// revoking — or two atomic rewrites racing over the same scratch file — is another test's
    /// flake.
    pub(super) async fn boot_with_control_tag(allow_input: bool, tag: &str) -> Server {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let sessions = Arc::new(SessionManager::new(tx));
        let dir = std::env::temp_dir().join(format!("hp-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("test scratch dir");
        let control_file = dir.join("control.json");
        let speech_settings = std::env::temp_dir().join("hp-golden-speech.json");
        let shared = Shared::new(
            sessions,
            allow_input,
            "0.1.8",
            control_file,
            speech_settings,
        );
        shared.model.lock().unwrap().add_window(WindowInfo {
            keyboard_focus_pane: None,
            window_id: 1,
            active_tab_id: Some("t1".into()),
            tabs: vec![TabInfo {
                system: false,
                id: "t1".into(),
                title: "Tab 1".into(),
                layout: "auto".into(),
                panes: vec![],
            }],
        });
        let token = "tok-master".to_string();
        shared.tokens.lock().unwrap().set_master(token.clone());
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        shared.port.store(port, Ordering::SeqCst);
        let app = router(Arc::clone(&shared));
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Server {
            shared,
            base: format!("http://127.0.0.1:{port}"),
            token,
        }
    }

    fn pane(id: &str, uid: &str) -> PaneInfo {
        PaneInfo {
            id: id.into(),
            session_uid: uid.into(),
            label: "shell".into(),
            subtitle: None,
            color: "#3b82f6".into(),
            command: None,
            args: None,
            cwd: None,
            shell: None,
            status: PaneStatus::Running,
            exit_code: None,
            meta: None,
            talk: false,
            kind: crate::tools::PaneKind::Terminal,
        }
    }

    pub(super) fn client() -> reqwest::Client {
        reqwest::Client::new()
    }

    #[tokio::test]
    async fn fs_read_serves_text_master_only() {
        let s = boot(true).await;
        let dir = std::env::temp_dir().join(format!("hp-fsread-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("hello.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let path = file.display().to_string();

        // Master token reads the file.
        let r = client()
            .get(format!("{}/fs/read?path={}", s.base, path))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["content"], json!("fn main() {}\n"));
        assert_eq!(v["truncated"], json!(false));

        // Missing file → 404; relative path → 400; no auth → 401.
        let r = client()
            .get(format!(
                "{}/fs/read?path={}/nope.txt",
                s.base,
                dir.display()
            ))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 404);
        let r = client()
            .get(format!("{}/fs/read?path=relative.txt", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 400);
        let r = client()
            .get(format!("{}/fs/read?path={}", s.base, path))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 401);

        // A scoped token is refused outright.
        let mint: Value = client()
            .post(format!("{}/tokens", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .json(&json!({"scope": {"tabIds": ["t1"]}}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let scoped = mint["token"].as_str().unwrap();
        let r = client()
            .get(format!("{}/fs/read?path={}", s.base, path))
            .header("authorization", format!("Bearer {scoped}"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 403);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn devices_mint_list_revoke_full_authority_and_master_only() {
        let s = boot_with_control_tag(true, "devices-crud").await;

        // Master mints a device token; it comes back and carries full (unscoped) authority.
        let mint: Value = client()
            .post(format!("{}/devices", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .json(&json!({ "label": "iphone" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(mint["ok"], json!(true));
        assert_eq!(mint["label"], json!("iphone"));
        let device = mint["token"].as_str().unwrap().to_string();

        // The device token authenticates like the master (e.g. reads /state).
        let r = client()
            .get(format!("{}/state", s.base))
            .header("authorization", format!("Bearer {device}"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);

        // GET /devices lists it by label and NEVER echoes the token back.
        let list: Value = client()
            .get(format!("{}/devices", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(list["devices"][0]["label"], json!("iphone"));
        assert!(list["devices"][0].get("token").is_none());

        // A scoped token can neither mint nor list device credentials.
        let scoped = client()
            .post(format!("{}/tokens", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .json(&json!({ "scope": { "tabIds": ["t1"] } }))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()["token"]
            .as_str()
            .unwrap()
            .to_string();
        for method in ["post", "get"] {
            let req = if method == "post" {
                client().post(format!("{}/devices", s.base))
            } else {
                client().get(format!("{}/devices", s.base))
            };
            let r = req
                .header("authorization", format!("Bearer {scoped}"))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status().as_u16(), 403);
        }

        // Revoke by label drops it; the device token is then rejected, and the list is empty.
        let revoked: Value = client()
            .delete(format!("{}/devices?label=iphone", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(revoked["revoked"], json!(1));
        let r = client()
            .get(format!("{}/state", s.base))
            .header("authorization", format!("Bearer {device}"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 401);
        let list: Value = client()
            .get(format!("{}/devices", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(list["devices"].as_array().unwrap().len(), 0);

        let _ = std::fs::remove_file(s.shared.control_file.with_file_name("device-tokens.json"));
    }

    #[tokio::test]
    async fn a_paired_ssh_key_persists_with_the_device_and_dies_with_it() {
        let s = boot_with_control_tag(true, "devices-sshkey").await;
        let key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIH0lTeStPhoNe phone";
        let path = s.shared.control_file.with_file_name("device-tokens.json");
        let _ = std::fs::remove_file(&path);

        let mint: Value = client()
            .post(format!("{}/devices", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .json(&json!({ "label": "iphone", "sshKey": key }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(mint["ok"], json!(true));

        // The key lands in the same on-disk record as the bearer token — that file IS the SSH
        // server's per-device key list, so this is the whole of "one pairing, both doors".
        let records = crate::persistence::device_tokens::load_from(&path);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].label, "iphone");
        assert_eq!(records[0].ssh_key.as_deref(), Some(key));

        // A public key is not a credential, so the listing may echo it (the token still may not).
        let list: Value = client()
            .get(format!("{}/devices", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(list["devices"][0]["sshKey"], json!(key));
        assert!(list["devices"][0].get("token").is_none());

        // One revocation closes both doors: the record, and with it the key, leaves the file.
        let revoked: Value = client()
            .delete(format!("{}/devices?label=iphone", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(revoked["revoked"], json!(1));
        assert!(crate::persistence::device_tokens::load_from(&path).is_empty());

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn health_is_byte_exact() {
        let s = boot(true).await;
        let r = client()
            .get(format!("{}/health", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
        let body = r.text().await.unwrap();
        let pid = std::process::id();
        assert_eq!(
            body,
            format!(
                r#"{{"ok":true,"app":"avada","pid":{pid},"version":"0.1.8","allowInput":true}}"#
            )
        );
    }

    #[tokio::test]
    async fn health_needs_no_auth() {
        let s = boot(false).await;
        let r = client()
            .get(format!("{}/health", s.base))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
        assert!(r.text().await.unwrap().contains(r#""allowInput":false"#));
    }

    #[tokio::test]
    async fn state_empty_window_is_byte_exact() {
        let s = boot(true).await;
        let r = client()
            .get(format!("{}/state", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
        let body: Value = serde_json::from_str(&r.text().await.unwrap()).unwrap();
        // `windows` stays byte-exact (the frozen legacy shape); `speech` is additive — its
        // `backend` is environment-dependent (whatever TTS the test machine has on PATH),
        // so only its shape is asserted, not the exact backend name.
        assert_eq!(
            body["windows"],
            json!([{"windowId":1,"activeTabId":"t1","keyboardFocusPaneId":null,"tabs":[{"id":"t1","title":"Tab 1","layout":"auto","panes":[]}]}])
        );
        assert_eq!(body["speech"]["muted"], json!(false));
        assert_eq!(body["speech"]["focusedOnly"], json!(false));
        assert!(body["speech"]["backend"].is_string());
        assert_eq!(body["speech"]["speakingPane"], Value::Null);
        // `dictation` is additive too, and equally environment-dependent — a machine with no
        // recorder still reports the block, so a client can tell "nothing installed" apart
        // from "old server".
        assert!(body["dictation"]["recorder"].is_string());
        assert!(body["dictation"]["transcriber"].is_string());
        assert_eq!(body["dictation"]["recordingPanes"], json!([]));
    }

    #[tokio::test]
    async fn loops_with_no_gui_publisher_is_honest_defaults_not_503() {
        // Unlike `/settings`, a missing GUI publisher is not an error here — nobody has called
        // `ReadModel::set_loops` yet (that wiring is a later lane), and "both loops disabled,
        // never fired" is the truthful answer to give, not a 503.
        let s = boot(true).await;
        let r = client()
            .get(format!("{}/loops", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
        assert_eq!(
            r.text().await.unwrap(),
            r#"{"status":{"enabled":false,"intervalSecs":0,"lastFiredAt":null,"nextFireAt":null},"restart":{"enabled":false,"intervalSecs":0,"lastFiredAt":null,"nextFireAt":null}}"#
        );
    }

    #[tokio::test]
    async fn loops_needs_auth() {
        let s = boot(true).await;
        let r = client()
            .get(format!("{}/loops", s.base))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 401);
    }

    // ---- dictation ------------------------------------------------------------------------

    async fn dictate(s: &Server, ty: &str, pane: &str) -> (u16, Value) {
        let r = client()
            .post(format!("{}/command", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .json(&json!({ "type": ty, "paneId": pane }))
            .send()
            .await
            .unwrap();
        let status = r.status().as_u16();
        (
            status,
            serde_json::from_str(&r.text().await.unwrap()).unwrap(),
        )
    }

    #[tokio::test]
    async fn dictation_into_a_pane_that_does_not_exist_is_404() {
        let s = boot(true).await;
        let (status, body) = dictate(&s, "startDictation", "ghost").await;
        assert_eq!(status, 404);
        assert_eq!(body["error"], json!("no such pane"));
    }

    #[tokio::test]
    async fn dictation_needs_a_pane_id() {
        let s = boot(true).await;
        let r = client()
            .post(format!("{}/command", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .json(&json!({ "type": "startDictation" }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 400);
        assert!(r.text().await.unwrap().contains("needs a paneId"));
    }

    #[tokio::test]
    async fn dictation_is_input_and_obeys_the_input_switch() {
        // A read-only token holder must not be able to type into a pane by speaking into it.
        let s = boot(false).await;
        s.shared
            .model
            .lock()
            .unwrap()
            .insert_pane(1, pane("p1", "u1"));
        let (status, body) = dictate(&s, "startDictation", "p1").await;
        assert_eq!(status, 403);
        assert_eq!(body["error"], json!("input not allowed"));
    }

    #[tokio::test]
    async fn stopping_a_pane_that_never_recorded_is_an_error_not_a_hang() {
        let s = boot(true).await;
        s.shared
            .model
            .lock()
            .unwrap()
            .insert_pane(1, pane("p1", "u1"));
        let (status, body) = dictate(&s, "stopDictation", "p1").await;
        assert_eq!(status, 400);
        assert!(body["error"].as_str().unwrap().contains("not recording"));
    }

    #[tokio::test]
    async fn cancelling_a_pane_that_never_recorded_is_silent() {
        // Cancel is the teardown path — a pane closing mid-recording, a window going away.
        // It must never fail, or teardown would have to care whether a mic was live.
        let s = boot(true).await;
        s.shared
            .model
            .lock()
            .unwrap()
            .insert_pane(1, pane("p1", "u1"));
        let (status, body) = dictate(&s, "cancelDictation", "p1").await;
        assert_eq!(status, 200);
        assert_eq!(body["ok"], json!(true));
    }

    #[tokio::test]
    async fn state_with_pane_omits_unset_optionals_and_keeps_field_order() {
        let s = boot(true).await;
        s.shared
            .model
            .lock()
            .unwrap()
            .insert_pane(1, pane("p1", "u1"));
        let body = client()
            .get(format!("{}/state", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        // A never-output running pane reads `busy`; optionals omitted; field order preserved.
        assert!(body.contains(
            r##""panes":[{"id":"p1","sessionUid":"u1","label":"shell","color":"#3b82f6","status":"running","activity":"busy"}]"##
        ));
    }

    #[tokio::test]
    async fn unauthorized_is_401_exact() {
        let s = boot(true).await;
        let r = client()
            .get(format!("{}/state", s.base))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 401);
        assert_eq!(r.text().await.unwrap(), r#"{"error":"unauthorized"}"#);
    }

    #[tokio::test]
    async fn no_such_pane_is_404_exact() {
        let s = boot(true).await;
        let r = client()
            .get(format!("{}/panes/ghost/output", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 404);
        assert_eq!(
            r.text().await.unwrap(),
            r#"{"error":"no such pane","paneId":"ghost"}"#
        );
    }

    #[tokio::test]
    async fn unknown_path_is_404_not_found_with_path() {
        let s = boot(true).await;
        let r = client()
            .get(format!("{}/nope", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 404);
        assert_eq!(
            r.text().await.unwrap(),
            r#"{"error":"not found","path":"/nope"}"#
        );
    }

    #[tokio::test]
    async fn output_of_a_sessionless_pane_is_byte_exact() {
        let s = boot(true).await;
        s.shared
            .model
            .lock()
            .unwrap()
            .insert_pane(1, pane("p1", "u1"));
        let r = client()
            .get(format!("{}/panes/p1/output", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
        // serde_json::Value object → keys sorted; cursor ALWAYS present.
        assert_eq!(
            r.text().await.unwrap(),
            r#"{"cursor":0,"output":"","paneId":"p1","status":"running","stripped":false}"#
        );
    }

    #[tokio::test]
    async fn input_to_a_pane_with_no_live_session_is_409_not_200() {
        // The pane row exists — the GUI published it — but nothing is behind `u1`. This used to
        // answer `200 {"ok":true}` for keystrokes that reached no process at all, which is how a
        // control-API caller came to believe it had a terminal to work in.
        let s = boot(true).await;
        s.shared
            .model
            .lock()
            .unwrap()
            .insert_pane(1, pane("p1", "u1"));
        let r = client()
            .post(format!("{}/panes/p1/input", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .header("content-type", "application/json")
            .body(r#"{"data":"hi"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 409);
        let body: serde_json::Value = r.json().await.unwrap();
        assert_eq!(body["error"], "input not delivered");
        assert_eq!(body["paneId"], "p1");
        assert!(
            body["detail"].as_str().unwrap().contains("u1"),
            "the detail names the session that is missing, got {body}"
        );
    }

    #[tokio::test]
    async fn keys_to_a_pane_with_no_live_session_is_409_not_200() {
        // Same hole on the `keys` branch, which takes a different path to the same `write`.
        let s = boot(true).await;
        s.shared
            .model
            .lock()
            .unwrap()
            .insert_pane(1, pane("p1", "u1"));
        let r = client()
            .post(format!("{}/panes/p1/input", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .header("content-type", "application/json")
            .body(r#"{"keys":["enter"]}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 409);
        assert_eq!(
            r.json::<serde_json::Value>().await.unwrap()["error"],
            "input not delivered"
        );
    }

    #[tokio::test]
    async fn output_of_an_exited_pane_reports_exited_not_running() {
        // Companion to the two 409 tests above, on the read side of the same crash mode. Those
        // cover a *write* to a dead pane no longer lying `200 ok`; this covers a *read*: once
        // something upstream — `heal_lost_panes`'s re-check, or the activity ticker's
        // `reconcile_exits` — has recorded a pane as `Exited` because its session is gone, the
        // API has to actually say so, byte for byte, rather than a stale `running` surviving the
        // trip through `pane_status_str` and `PaneOut` serialization. No live session exists
        // behind `u1` here either, matching the two tests above — the difference under test is
        // only which status the model already holds.
        let s = boot(true).await;
        s.shared.model.lock().unwrap().insert_pane(
            1,
            PaneInfo {
                status: PaneStatus::Exited,
                ..pane("p1", "u1")
            },
        );
        let r = client()
            .get(format!("{}/panes/p1/output", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
        assert_eq!(
            r.text().await.unwrap(),
            r#"{"cursor":0,"output":"","paneId":"p1","status":"exited","stripped":false}"#
        );
    }

    #[tokio::test]
    async fn input_blocked_when_allow_input_off_is_403() {
        let s = boot(false).await;
        s.shared
            .model
            .lock()
            .unwrap()
            .insert_pane(1, pane("p1", "u1"));
        let r = client()
            .post(format!("{}/panes/p1/input", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .header("content-type", "application/json")
            .body(r#"{"data":"hi"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 403);
        assert_eq!(r.text().await.unwrap(), r#"{"error":"input not allowed"}"#);
    }

    #[tokio::test]
    async fn messages_post_then_get_roundtrip() {
        let s = boot(true).await;
        s.shared
            .model
            .lock()
            .unwrap()
            .insert_pane(1, pane("p1", "u1"));
        let post = client()
            .post(format!("{}/panes/p1/messages", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .header("content-type", "application/json")
            .body(r#"{"from":"mgr","body":"go"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(post.text().await.unwrap(), r#"{"ok":true,"seq":1}"#);
        let get: serde_json::Value = client()
            .get(format!("{}/panes/p1/messages", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        // ts is wall-clock; assert every other field byte-for-byte via structural compare.
        assert_eq!(get["paneId"], serde_json::json!("p1"));
        assert_eq!(get["dropped"], serde_json::json!(0));
        assert_eq!(get["latestSeq"], serde_json::json!(1));
        let m = &get["messages"][0];
        assert_eq!(m["seq"], serde_json::json!(1));
        assert_eq!(m["to"], serde_json::json!("p1"));
        assert_eq!(m["from"], serde_json::json!("mgr"));
        assert_eq!(m["body"], serde_json::json!("go"));
        assert!(m["ts"].is_number());
    }

    /// The two pane-id spellings in the wild (`pane-<uuid>` from the app, bare `<uuid>` from the
    /// control API) must address the SAME inbox: an agent handed one form and reconstructing the
    /// other used to get `404 no such pane`, silently breaking the reply direction of the bus.
    #[tokio::test]
    async fn messages_are_addressable_by_either_pane_id_spelling() {
        let s = boot(true).await;
        s.shared
            .model
            .lock()
            .unwrap()
            .insert_pane(1, pane("pane-abc", "u1"));
        // Post to the bare-uuid alias…
        let post = client()
            .post(format!("{}/panes/abc/messages", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .header("content-type", "application/json")
            .body(r#"{"from":"impl","body":"done"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(post.status().as_u16(), 200);
        // …and read it back under the canonical id.
        let get: serde_json::Value = client()
            .get(format!("{}/panes/pane-abc/messages", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(get["paneId"], serde_json::json!("pane-abc"));
        assert_eq!(get["messages"][0]["body"], serde_json::json!("done"));
        // The reverse spelling reads the same queue and reports the canonical id.
        let alias: serde_json::Value = client()
            .get(format!("{}/panes/abc/messages", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(alias["paneId"], serde_json::json!("pane-abc"));
        assert_eq!(alias["messages"][0]["body"], serde_json::json!("done"));
    }

    #[tokio::test]
    async fn lock_acquire_then_nonowner_input_is_423() {
        let s = boot(true).await;
        s.shared
            .model
            .lock()
            .unwrap()
            .insert_pane(1, pane("p1", "u1"));
        let lock: serde_json::Value = client()
            .post(format!("{}/panes/p1/lock", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .header("content-type", "application/json")
            .body(r#"{"owner":"mgrA","ttlMs":60000}"#)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(lock["ok"], serde_json::json!(true));
        assert_eq!(lock["owner"], serde_json::json!("mgrA"));
        assert!(lock["expiresAt"].is_number());
        // A different writer is refused 423 with the holder named.
        let blocked = client()
            .post(format!("{}/panes/p1/input", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .header("content-type", "application/json")
            .body(r#"{"data":"x","owner":"mgrB"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(blocked.status().as_u16(), 423);
        assert_eq!(
            blocked.text().await.unwrap(),
            r#"{"error":"pane locked","owner":"mgrA"}"#
        );
    }

    #[tokio::test]
    async fn tokens_mint_and_no_escalation() {
        let s = boot(true).await;
        s.shared
            .model
            .lock()
            .unwrap()
            .insert_pane(1, pane("p1", "u1"));
        // Master mints a pane-scoped token.
        let minted: serde_json::Value = client()
            .post(format!("{}/tokens", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .header("content-type", "application/json")
            .body(r#"{"scope":{"paneIds":["p1"]}}"#)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(minted["ok"], serde_json::json!(true));
        let scoped = minted["token"].as_str().unwrap().to_string();
        assert_eq!(minted["token"].as_str().unwrap().len(), 64);
        assert_eq!(minted["scope"], serde_json::json!({ "paneIds": ["p1"] }));
        assert!(minted["events"]
            .as_str()
            .unwrap()
            .contains("/events?token="));
        // The scoped token cannot escalate to the whole window → 403.
        let esc = client()
            .post(format!("{}/tokens", s.base))
            .header("authorization", format!("Bearer {}", scoped))
            .header("content-type", "application/json")
            .body(r#"{"scope":{"windowIds":[1]}}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(esc.status().as_u16(), 403);
        assert!(esc
            .text()
            .await
            .unwrap()
            .contains("outside the minting token's scope"));
    }

    // ---- /projects -----------------------------------------------------------------------
    // Only the side-effect-FREE paths are golden-tested here: the persistence layer writes the
    // real `projects.json` (no store injection), so exercising add/patch/delete over HTTP would
    // clobber the dev machine's actual registry and race parallel tests. The write paths are
    // covered by `persistence::projects` unit tests (`add_project_explicit_in`, …); these assert
    // auth + validation + the read-only GET contract the MCP depends on.

    #[tokio::test]
    async fn projects_list_is_authorized_and_returns_an_array() {
        let s = boot(true).await;
        let r = client()
            .get(format!("{}/projects", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
        let body: serde_json::Value = r.json().await.unwrap();
        assert!(
            body["projects"].is_array(),
            "expected a projects array, got {body}"
        );
    }

    #[tokio::test]
    async fn projects_list_unauthorized_is_401() {
        let s = boot(true).await;
        let r = client()
            .get(format!("{}/projects", s.base))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 401);
        assert_eq!(r.text().await.unwrap(), r#"{"error":"unauthorized"}"#);
    }

    #[tokio::test]
    async fn projects_add_without_dir_is_400() {
        let s = boot(true).await;
        let r = client()
            .post(format!("{}/projects", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .header("content-type", "application/json")
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 400);
        assert_eq!(
            r.text().await.unwrap(),
            r#"{"error":"expected { dir: string }"}"#
        );
    }

    #[tokio::test]
    async fn projects_add_nonexistent_dir_is_400() {
        let s = boot(true).await;
        // A path guaranteed not to exist → 400 before any registry write.
        let dir = format!("/nonexistent-hp-test-{}/repo", std::process::id());
        let r = client()
            .post(format!("{}/projects", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .header("content-type", "application/json")
            .body(format!(r#"{{"dir":"{dir}"}}"#))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 400);
        let body: serde_json::Value = r.json().await.unwrap();
        assert_eq!(
            body["error"],
            serde_json::json!("path doesn't exist or isn't a directory")
        );
    }

    #[tokio::test]
    async fn projects_patch_without_fields_is_400() {
        let s = boot(true).await;
        let r = client()
            .patch(format!("{}/projects/whatever", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .header("content-type", "application/json")
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 400);
        assert_eq!(
            r.text().await.unwrap(),
            r#"{"error":"expected { name?: string, color?: string }"}"#
        );
    }

    #[tokio::test]
    async fn new_pane_command_returns_id_and_lands_in_state() {
        let s = boot(true).await;
        let resp: serde_json::Value = client()
            .post(format!("{}/command", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .header("content-type", "application/json")
            .body(
                r#"{"type":"newPane","windowId":1,"pane":{"label":"worker","command":"echo hi"}}"#,
            )
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(resp["ok"], serde_json::json!(true));
        let pane_id = resp["result"].as_str().unwrap().to_string();
        // The pane is immediately present in /state (no debounce — synchronous in-process).
        let state = client()
            .get(format!("{}/state", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(state.contains(&format!(r#""id":"{pane_id}""#)));
        assert!(state.contains(r#""label":"worker""#));
        // Clean up the spawned pty.
        let _ = client()
            .post(format!("{}/command", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .header("content-type", "application/json")
            .body(format!(r#"{{"type":"closePane","paneId":"{pane_id}"}}"#))
            .send()
            .await;
    }

    #[tokio::test]
    async fn close_tab_refuses_the_app_owned_tab_rather_than_queueing_it() {
        let s = boot(true).await;
        // Two tabs on a second window: one the app owns (the always-on "Hyperpane"), one not.
        // The pair is the point — the refusal has to be about THIS tab, not about the verb.
        s.shared.model.lock().unwrap().add_window(WindowInfo {
            window_id: 2,
            active_tab_id: Some("sys".into()),
            keyboard_focus_pane: None,
            tabs: vec![
                TabInfo {
                    id: "sys".into(),
                    title: "Hyperpane".into(),
                    layout: "auto".into(),
                    panes: vec![],
                    system: true,
                },
                TabInfo {
                    id: "ordinary".into(),
                    title: "Tab 2".into(),
                    layout: "auto".into(),
                    panes: vec![],
                    system: false,
                },
            ],
        });

        let r = post(
            &s,
            "/command",
            &s.token,
            r#"{"type":"closeTab","tabId":"sys"}"#,
        )
        .await;
        // 409, not 202: the tab exists, its state forbids the verb. Answering 202 would tell
        // the caller "queued" for a close the UI thread then silently drops — and a caller told
        // "queued" has no way to learn it never happened.
        assert_eq!(r.status().as_u16(), 409);
        let body: Value = r.json().await.unwrap();
        assert_eq!(body["tabId"], json!("sys"));
        assert!(body["error"].as_str().unwrap().contains("refused"));

        let r = post(
            &s,
            "/command",
            &s.token,
            r#"{"type":"closeTab","tabId":"ordinary"}"#,
        )
        .await;
        assert_eq!(r.status().as_u16(), 202);
        assert_eq!(r.json::<Value>().await.unwrap()["queued"], json!(true));
    }

    // ---- work queue routes -----------------------------------------------------------------

    use serde_json::{json, Value};

    async fn post(s: &Server, path: &str, token: &str, body: &str) -> reqwest::Response {
        client()
            .post(format!("{}{}", s.base, path))
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .unwrap()
    }

    /// Enqueue one task into `queue` and claim it as `worker`; return `(id, fencingToken)`.
    async fn enqueue_and_claim(s: &Server, queue: &str, worker: &str) -> (String, u64) {
        let enq: Value = post(
            s,
            &format!("/queues/{queue}/tasks"),
            &s.token,
            r#"{"payload":"{}"}"#,
        )
        .await
        .json()
        .await
        .unwrap();
        let id = enq["id"].as_str().unwrap().to_string();
        let claim: Value = post(
            s,
            &format!("/queues/{queue}/claim"),
            &s.token,
            &format!(r#"{{"worker":"{worker}"}}"#),
        )
        .await
        .json()
        .await
        .unwrap();
        let fencing = claim["tasks"][0]["fencingToken"].as_u64().unwrap();
        (id, fencing)
    }

    #[tokio::test]
    async fn queue_enqueue_claim_ack_happy_path() {
        let s = boot(true).await;
        let enq: Value = post(
            &s,
            "/queues/build/tasks",
            &s.token,
            r#"{"payload":"{\"prompt\":\"do it\"}","kind":"manual","title":"T","priority":7}"#,
        )
        .await
        .json()
        .await
        .unwrap();
        assert_eq!(enq["ok"], json!(true));
        assert_eq!(enq["seq"], json!(1));
        let id = enq["id"].as_str().unwrap().to_string();

        // claim → a canonical Task carrying the fencing token + opaque payload verbatim
        let claim: Value = post(&s, "/queues/build/claim", &s.token, r#"{"worker":"wkr-1"}"#)
            .await
            .json()
            .await
            .unwrap();
        let t = &claim["tasks"][0];
        assert_eq!(t["id"], json!(id));
        assert_eq!(t["state"], json!("claimed"));
        assert_eq!(t["claimedBy"], json!("wkr-1"));
        assert_eq!(t["attempts"], json!(1));
        assert_eq!(t["fencingToken"], json!(1));
        assert_eq!(t["payload"], json!(r#"{"prompt":"do it"}"#));
        let fencing = t["fencingToken"].as_u64().unwrap();

        // ack with the fencing token → done
        let ack: Value = post(
            &s,
            &format!("/tasks/{id}/ack"),
            &s.token,
            &format!(r#"{{"fencingToken":{fencing},"result":"artifact://x"}}"#),
        )
        .await
        .json()
        .await
        .unwrap();
        assert_eq!(ack, json!({ "ok": true, "state": "done" }));

        // GET reflects the terminal state + recorded result
        let got: Value = client()
            .get(format!("{}/tasks/{id}", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(got["state"], json!("done"));
        assert_eq!(got["result"], json!("artifact://x"));
    }

    #[tokio::test]
    async fn queue_claim_empty_is_200_empty_tasks_byte_exact() {
        let s = boot(true).await;
        let r = post(&s, "/queues/nothing/claim", &s.token, r#"{"worker":"w"}"#).await;
        assert_eq!(r.status().as_u16(), 200);
        assert_eq!(r.text().await.unwrap(), r#"{"ok":true,"tasks":[]}"#);
    }

    #[tokio::test]
    async fn queue_ack_with_stale_fencing_token_is_409() {
        let s = boot(true).await;
        let (id, fencing) = enqueue_and_claim(&s, "build", "wkr-1").await;
        let r = post(
            &s,
            &format!("/tasks/{id}/ack"),
            &s.token,
            &format!(r#"{{"fencingToken":{}}}"#, fencing + 1),
        )
        .await;
        assert_eq!(r.status().as_u16(), 409);
        assert_eq!(
            r.text().await.unwrap(),
            format!(r#"{{"error":"stale lease","taskId":"{id}"}}"#)
        );
    }

    #[tokio::test]
    async fn queue_task_wire_shape_is_camelcase_with_flattened_lease() {
        let s = boot(true).await;
        let (id, _fencing) = enqueue_and_claim(&s, "build", "wkr").await;
        let body = client()
            .get(format!("{}/tasks/{id}", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        // camelCase columns + epoch-ms numbers
        assert!(body.contains(r#""maxAttempts":5"#));
        assert!(body.contains(r#""availableAt":"#));
        assert!(body.contains(r#""createdAt":"#));
        // FLATTENED lease fields (claimedBy/fencingToken/visibilityDeadline) — never a nested object
        assert!(body.contains(r#""claimedBy":"wkr""#));
        assert!(body.contains(r#""fencingToken":1"#));
        assert!(body.contains(r#""visibilityDeadline":"#));
        assert!(!body.contains(r#""lease""#)); // not nested
        assert!(!body.contains("claimed_by")); // not snake_case
    }

    #[tokio::test]
    async fn queue_nack_requeue_then_extend_heartbeat() {
        let s = boot(true).await;
        let (id, fencing) = enqueue_and_claim(&s, "build", "wkr").await;
        // extend (heartbeat) bumps the visibility deadline
        let ext: Value = post(
            &s,
            &format!("/tasks/{id}/extend"),
            &s.token,
            &format!(r#"{{"fencingToken":{fencing},"extraMs":5000}}"#),
        )
        .await
        .json()
        .await
        .unwrap();
        assert_eq!(ext["ok"], json!(true));
        assert!(ext["visibilityDeadline"].is_number());
        // nack(requeue=true) returns it to queued
        let nack: Value = post(
            &s,
            &format!("/tasks/{id}/nack"),
            &s.token,
            &format!(r#"{{"fencingToken":{fencing},"requeue":true,"error":"boom"}}"#),
        )
        .await
        .json()
        .await
        .unwrap();
        assert_eq!(nack, json!({ "ok": true, "state": "queued" }));
    }

    #[tokio::test]
    async fn queue_enqueue_requires_auth_401() {
        let s = boot(true).await;
        let r = client()
            .post(format!("{}/queues/build/tasks", s.base))
            .header("content-type", "application/json")
            .body(r#"{"payload":"x"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 401);
        assert_eq!(r.text().await.unwrap(), r#"{"error":"unauthorized"}"#);
    }

    #[tokio::test]
    async fn queue_scoped_token_is_gated_to_its_queue() {
        let s = boot(true).await;
        // master mints a token scoped to queue "build" only
        let minted: Value = post(
            &s,
            "/tokens",
            &s.token,
            r#"{"scope":{"queueIds":["build"]}}"#,
        )
        .await
        .json()
        .await
        .unwrap();
        assert_eq!(minted["ok"], json!(true));
        assert_eq!(minted["scope"], json!({ "queueIds": ["build"] }));
        let scoped = minted["token"].as_str().unwrap().to_string();
        // it CAN enqueue to its own queue
        let ok = post(&s, "/queues/build/tasks", &scoped, r#"{"payload":"x"}"#).await;
        assert_eq!(ok.status().as_u16(), 200);
        // but a foreign queue is 403
        let denied = post(&s, "/queues/deploy/tasks", &scoped, r#"{"payload":"x"}"#).await;
        assert_eq!(denied.status().as_u16(), 403);
        assert_eq!(
            denied.text().await.unwrap(),
            r#"{"error":"queue out of scope","queue":"deploy"}"#
        );
    }

    // ---- /settings: `keepAlive` + `editorCommand` -------------------------------------------
    // Both settings are global and both are wired end to end, but the seam between the store,
    // this route and the behavior they drive had no test. `keepAlive`'s own effect lives in the
    // app crate (the quit path reads the persisted preference), so what is testable HERE is the
    // route contract: a valid value round-trips into `GET /settings`, a malformed one is a 400
    // rather than a silent no-op, and neither reaches the settings at all without a root token.

    /// Publish the blob a GUI would publish each sync tick. `PATCH /settings` validates its
    /// keys against exactly this, so the fixture IS the contract the route enforces.
    fn publish_settings(s: &Server, v: Value) {
        *s.shared.settings.lock().unwrap() = Some(v);
    }

    fn settings_fixture() -> Value {
        json!({ "keepAlive": true, "editorCommand": "", "clickablePaths": true })
    }

    fn json_type(v: &Value) -> &'static str {
        match v {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        }
    }

    /// The GUI's `State::apply_settings_patch`, in miniature: merge onto the published blob and
    /// refuse a value whose JSON type is not the one that key already holds — which is what
    /// serde does when the real patch is deserialized back into `Settings`.
    fn merge_published_settings(shared: &Arc<Shared>, patch: &Value) -> Result<(), String> {
        let Some(mut blob) = shared.settings.lock().unwrap().clone() else {
            return Err("settings unavailable".to_string());
        };
        let obj = patch
            .as_object()
            .ok_or_else(|| "settings patch must be a JSON object".to_string())?;
        {
            let dst = blob
                .as_object_mut()
                .ok_or_else(|| "settings are not an object".to_string())?;
            for (k, v) in obj {
                let cur = dst
                    .get(k)
                    .ok_or_else(|| format!("unknown setting: {k}"))?
                    .clone();
                if json_type(&cur) != json_type(v) {
                    return Err(format!(
                        "bad settings patch: {k} expects a {}",
                        json_type(&cur)
                    ));
                }
                dst.insert(k.clone(), v.clone());
            }
        }
        *shared.settings.lock().unwrap() = Some(blob);
        Ok(())
    }

    /// Stand in for the GUI's UI thread: drain `ui_ops`, apply each `PatchSettings`, and answer
    /// the reply handle the route is waiting on. Without a drainer every patch times out into
    /// the headless `202 queued` and nothing would be asserted about the outcome.
    fn spawn_gui_settings_thread(s: &Server) {
        let shared = Arc::clone(&s.shared);
        tokio::spawn(async move {
            loop {
                let ops = shared.ui_ops.lock().unwrap().drain();
                for op in ops {
                    if let UiOp::PatchSettings { patch, reply } = op {
                        let outcome = merge_published_settings(&shared, &patch);
                        if let Some(reply) = reply {
                            reply.send(outcome);
                        }
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
        });
    }

    async fn patch_settings(s: &Server, token: &str, body: &str) -> (u16, Value) {
        let r = client()
            .patch(format!("{}/settings", s.base))
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .unwrap();
        let status = r.status().as_u16();
        (status, r.json().await.unwrap())
    }

    async fn get_settings(s: &Server, token: &str) -> (u16, Value) {
        let r = client()
            .get(format!("{}/settings", s.base))
            .header("authorization", format!("Bearer {token}"))
            .send()
            .await
            .unwrap();
        let status = r.status().as_u16();
        (status, r.json().await.unwrap())
    }

    #[tokio::test]
    async fn a_keep_alive_patch_is_applied_and_shows_up_in_the_next_settings_get() {
        let s = boot(true).await;
        publish_settings(&s, settings_fixture());
        spawn_gui_settings_thread(&s);

        let (status, body) = patch_settings(&s, &s.token, r#"{"keepAlive":false}"#).await;
        assert_eq!(status, 200);
        assert_eq!(
            body,
            json!({ "ok": true, "applied": true, "keys": ["keepAlive"] })
        );

        // The answer is the OUTCOME, not an acknowledgement: the read side agrees with it.
        let (status, body) = get_settings(&s, &s.token).await;
        assert_eq!(status, 200);
        assert_eq!(body["settings"]["keepAlive"], json!(false));

        // And back on, so the round trip is not a one-way latch.
        let (status, _) = patch_settings(&s, &s.token, r#"{"keepAlive":true}"#).await;
        assert_eq!(status, 200);
        let (_, body) = get_settings(&s, &s.token).await;
        assert_eq!(body["settings"]["keepAlive"], json!(true));
    }

    #[tokio::test]
    async fn a_malformed_keep_alive_is_400_rather_than_silently_ignored() {
        let s = boot(true).await;
        publish_settings(&s, settings_fixture());
        spawn_gui_settings_thread(&s);

        // A string where a bool belongs: the preferences layer refuses it, and the route
        // reports that refusal instead of answering `ok` for a value it threw away.
        let (status, body) = patch_settings(&s, &s.token, r#"{"keepAlive":"yes"}"#).await;
        assert_eq!(status, 400);
        assert_eq!(body["keys"], json!(["keepAlive"]));
        assert!(body["error"].as_str().unwrap().contains("keepAlive"));
        // …and the live value is untouched.
        let (_, body) = get_settings(&s, &s.token).await;
        assert_eq!(body["settings"]["keepAlive"], json!(true));

        for bad in [r#"{"keepAlive":1}"#, r#"{"keepAlive":null}"#] {
            let (status, _) = patch_settings(&s, &s.token, bad).await;
            assert_eq!(status, 400, "{bad} must not be accepted as a boolean");
        }
        // A misspelled key is refused up front, naming the key — a typo that quietly does
        // nothing is the failure this route exists to prevent.
        let (status, body) = patch_settings(&s, &s.token, r#"{"keepalive":false}"#).await;
        assert_eq!(status, 400);
        assert_eq!(
            body,
            json!({ "error": "unknown settings", "keys": ["keepalive"] })
        );
        // Not-an-object bodies never reach the queue either.
        for bad in [r#"["keepAlive"]"#, "{}", "not json at all"] {
            let (status, _) = patch_settings(&s, &s.token, bad).await;
            assert_eq!(status, 400, "{bad} must be refused");
        }
        let (_, body) = get_settings(&s, &s.token).await;
        assert_eq!(body["settings"]["keepAlive"], json!(true));
    }

    #[tokio::test]
    async fn an_editor_command_round_trips_and_the_stored_value_drives_the_open_plan() {
        let s = boot(true).await;
        publish_settings(&s, settings_fixture());
        spawn_gui_settings_thread(&s);

        let (status, body) = patch_settings(
            &s,
            &s.token,
            r#"{"editorCommand":"subl {path}:{line}:{col}"}"#,
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(
            body,
            json!({ "ok": true, "applied": true, "keys": ["editorCommand"] })
        );

        let (_, body) = get_settings(&s, &s.token).await;
        let stored = body["settings"]["editorCommand"].as_str().unwrap();
        assert_eq!(stored, "subl {path}:{line}:{col}");

        // The value that came back out is the value the open path consumes — the seam this
        // test exists for. It wins over a VS Code that is on PATH…
        let code = Some("/usr/bin/code");
        assert_eq!(
            crate::paths::plan_open(false, "/a b/c.ts", Some(12), Some(4), stored, code),
            crate::paths::OpenPlan::Editor
        );
        // …and builds the argv with the spaced path kept whole.
        assert_eq!(
            crate::paths::editor_command_line(stored, "/a b/c.ts", Some(12), Some(4)),
            format!(
                "{} {}",
                crate::paths::quote("subl"),
                crate::paths::quote("/a b/c.ts:12:4")
            )
        );
    }

    #[tokio::test]
    async fn a_blank_editor_command_round_trips_and_still_falls_back() {
        let s = boot(true).await;
        publish_settings(&s, settings_fixture());
        spawn_gui_settings_thread(&s);

        // Blank is a legitimate value (it MEANS "auto-detect"), so the route stores it — the
        // fallback is decided at open time, not by refusing the setting.
        let (status, _) = patch_settings(&s, &s.token, r#"{"editorCommand":"   "}"#).await;
        assert_eq!(status, 200);
        let (_, body) = get_settings(&s, &s.token).await;
        let stored = body["settings"]["editorCommand"].as_str().unwrap();
        assert_eq!(stored, "   ");
        assert_eq!(
            crate::paths::plan_open(false, "/x/y.ts", None, None, stored, None),
            crate::paths::OpenPlan::OsDefault
        );
        assert_eq!(
            crate::paths::editor_command_line(stored, "/x/y.ts", None, None),
            ""
        );
    }

    #[tokio::test]
    async fn a_non_string_editor_command_is_400_and_leaves_the_setting_alone() {
        let s = boot(true).await;
        publish_settings(&s, settings_fixture());
        spawn_gui_settings_thread(&s);

        for bad in [
            r#"{"editorCommand":42}"#,
            r#"{"editorCommand":true}"#,
            r#"{"editorCommand":["subl","{path}"]}"#,
        ] {
            let (status, body) = patch_settings(&s, &s.token, bad).await;
            assert_eq!(status, 400, "{bad} must be refused");
            assert_eq!(body["keys"], json!(["editorCommand"]));
        }
        let (_, body) = get_settings(&s, &s.token).await;
        assert_eq!(body["settings"]["editorCommand"], json!(""));
    }

    #[tokio::test]
    async fn settings_reads_need_auth_and_writes_need_a_root_token() {
        let s = boot(true).await;
        publish_settings(&s, settings_fixture());
        spawn_gui_settings_thread(&s);

        // No token at all: both verbs are 401, byte-exact like every other route.
        let r = client()
            .get(format!("{}/settings", s.base))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 401);
        assert_eq!(r.text().await.unwrap(), r#"{"error":"unauthorized"}"#);
        let r = client()
            .patch(format!("{}/settings", s.base))
            .header("content-type", "application/json")
            .body(r#"{"keepAlive":false}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 401);
        assert_eq!(r.text().await.unwrap(), r#"{"error":"unauthorized"}"#);

        // A scoped token may READ the preferences…
        let minted: Value = post(&s, "/tokens", &s.token, r#"{"scope":{"tabIds":["t1"]}}"#)
            .await
            .json()
            .await
            .unwrap();
        let scoped = minted["token"].as_str().unwrap().to_string();
        let (status, body) = get_settings(&s, &scoped).await;
        assert_eq!(status, 200);
        assert_eq!(body["settings"]["keepAlive"], json!(true));

        // …but not write them: a scope is a grant over named panes, and `keepAlive` changes
        // what happens to every pane in the app.
        let (status, body) = patch_settings(&s, &scoped, r#"{"keepAlive":false}"#).await;
        assert_eq!(status, 403);
        assert_eq!(body, json!({ "error": "settings needs a root token" }));
        let (status, body) = patch_settings(&s, &scoped, r#"{"editorCommand":"subl"}"#).await;
        assert_eq!(status, 403);
        assert_eq!(body, json!({ "error": "settings needs a root token" }));

        // The refusals changed nothing.
        let (_, body) = get_settings(&s, &s.token).await;
        assert_eq!(body["settings"]["keepAlive"], json!(true));
        assert_eq!(body["settings"]["editorCommand"], json!(""));
    }

    #[tokio::test]
    async fn settings_with_no_gui_attached_are_503_rather_than_an_invented_default() {
        // Nothing published: `keepAlive` has a default in the app, but answering with it here
        // would describe a GUI that isn't there — and a PATCH would have nowhere to land.
        let s = boot(true).await;
        let (status, body) = get_settings(&s, &s.token).await;
        assert_eq!(status, 503);
        assert_eq!(
            body,
            json!({ "error": "settings unavailable (no GUI attached)" })
        );
        let (status, body) = patch_settings(&s, &s.token, r#"{"keepAlive":false}"#).await;
        assert_eq!(status, 503);
        assert_eq!(
            body,
            json!({ "error": "settings unavailable (no GUI attached)" })
        );
    }
}

/// The descriptor table IS the router (plan track H3): every described route answers on the
/// wire, `GET /schema` lists exactly the table, and a token that holds no capability is
/// refused with the documented 403 on a capability route while the master token is never
/// refused. Same in-process axum stack as `golden`.
#[cfg(test)]
mod table {
    use super::golden::{boot_with_control_tag, client, Server};
    use crate::control::descriptor_table::core_routes;
    use crate::control::dispatch::CapabilitySource;
    use avada_module_sdk::caps::Capability;
    use avada_module_sdk::descriptor::{SchemaDocument, Verb};
    use serde_json::{json, Value};
    use std::collections::BTreeSet;
    use std::sync::Arc;

    /// A capability source that claims one token and answers a fixed set for it.
    struct Fixed {
        token: String,
        caps: BTreeSet<Capability>,
    }

    impl CapabilitySource for Fixed {
        fn caps_for(&self, token: &str) -> Option<BTreeSet<Capability>> {
            (token == self.token).then(|| self.caps.clone())
        }
    }

    /// Register a device token the token store accepts and pin its capabilities.
    fn limited(s: &Server, token: &str, caps: &[Capability]) {
        s.shared
            .tokens
            .lock()
            .unwrap()
            .add_device(token.to_string(), "limited".into(), None, None);
        s.shared.caps.install(Arc::new(Fixed {
            token: token.to_string(),
            caps: caps.iter().copied().collect(),
        }));
    }

    fn concrete(path: &str) -> String {
        path.replace("{id}", "p1").replace("{queue}", "q1")
    }

    async fn call(s: &Server, verb: Verb, path: &str, token: &str) -> (u16, String) {
        let url = format!("{}{}", s.base, concrete(path));
        let c = client();
        let req = match verb {
            Verb::Get => c.get(&url),
            Verb::Post => c.post(&url),
            Verb::Put => c.put(&url),
            Verb::Patch => c.patch(&url),
            Verb::Delete => c.delete(&url),
        };
        let r = req
            .header("authorization", format!("Bearer {token}"))
            .send()
            .await
            .unwrap();
        let status = r.status().as_u16();
        (status, r.text().await.unwrap())
    }

    #[tokio::test]
    async fn every_described_route_is_mounted_and_only_those() {
        let s = boot_with_control_tag(true, "table-mounted").await;
        for desc in core_routes() {
            let (status, body) = call(&s, desc.verb, &desc.mounted_path(), &s.token).await;
            let err = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string));
            assert_ne!(status, 405, "{} is described but not mounted", desc.method);
            assert_ne!(
                (status, err.as_deref()),
                (404, Some("not found")),
                "{} is described but not mounted",
                desc.method
            );
            assert_ne!(status, 401, "{} refused the master token", desc.method);
            assert_ne!(status, 403, "{} refused the master token", desc.method);
        }
        // A verb the table does not describe for a described path is the 405 fallback, and
        // a path the table does not describe is the 404 fallback — nothing is mounted on the
        // side.
        let (status, body) = call(&s, Verb::Delete, "/health", &s.token).await;
        assert_eq!(
            (status, body.as_str()),
            (405, r#"{"error":"method not allowed"}"#)
        );
        let (status, body) = call(&s, Verb::Get, "/m/acme/files/tree", &s.token).await;
        assert_eq!(
            (status, body.as_str()),
            (404, r#"{"error":"not found","path":"/m/acme/files/tree"}"#)
        );
    }

    #[tokio::test]
    async fn schema_lists_the_table_and_is_byte_identical_across_calls() {
        let s = boot_with_control_tag(true, "table-schema").await;
        let (status, first) = call(&s, Verb::Get, "/schema", &s.token).await;
        assert_eq!(status, 200);
        let (status, second) = call(&s, Verb::Get, "/schema", &s.token).await;
        assert_eq!(status, 200);
        assert_eq!(first, second, "two GET /schema calls differ");
        let doc: SchemaDocument = serde_json::from_str(&first).unwrap();
        assert_eq!(
            doc.contract_version,
            avada_module_sdk::contract::CONTRACT_VERSION
        );
        assert_eq!(doc.product, avada_module_sdk::PRODUCT_NAME);
        assert_eq!(doc.host_version, "0.1.8");
        let listed: BTreeSet<String> = doc.routes.iter().map(|r| r.method.clone()).collect();
        let table: BTreeSet<String> = core_routes().into_iter().map(|r| r.method).collect();
        assert_eq!(listed, table);
        assert!(listed.contains("schema"), "the schema lists itself");
        let methods: Vec<&str> = doc.routes.iter().map(|r| r.method.as_str()).collect();
        let mut sorted = methods.clone();
        sorted.sort_unstable();
        assert_eq!(methods, sorted, "routes are sorted by method");
        assert!(!doc.rpcs.is_empty());
        assert!(doc.modules.is_empty());
        // Any authenticated token may read it; no token is the byte-exact 401.
        let r = client()
            .get(format!("{}/schema", s.base))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 401);
        assert_eq!(r.text().await.unwrap(), r#"{"error":"unauthorized"}"#);
    }

    #[tokio::test]
    async fn a_token_holding_nothing_is_refused_only_on_capability_routes() {
        let s = boot_with_control_tag(true, "table-caps-none").await;
        limited(&s, "tok-limited-none", &[]);
        let (status, body) = call(&s, Verb::Get, "/settings", "tok-limited-none").await;
        assert_eq!(
            (status, body.as_str()),
            (
                403,
                r#"{"capability":"settings.read","error":"capability"}"#
            )
        );
        let (status, body) = call(&s, Verb::Get, "/state", "tok-limited-none").await;
        assert_eq!(
            (status, body.as_str()),
            (
                403,
                r#"{"capability":"workspace.read","error":"capability"}"#
            )
        );
        // The WebSocket route takes its token from the query too; the gate sees it there.
        let r = client()
            .get(format!("{}/events?token=tok-limited-none", s.base))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 403);
        assert_eq!(
            r.text().await.unwrap(),
            r#"{"capability":"events.subscribe","error":"capability"}"#
        );
        // Unrestricted routes answer as for any token.
        let (status, _) = call(&s, Verb::Get, "/health", "tok-limited-none").await;
        assert_eq!(status, 200);
        let (status, _) = call(&s, Verb::Get, "/schema", "tok-limited-none").await;
        assert_eq!(status, 200);
        // Every capability route refuses this token, with its own capability named.
        for desc in core_routes() {
            let Some(cap) = desc.capability else { continue };
            let (status, body) =
                call(&s, desc.verb, &desc.mounted_path(), "tok-limited-none").await;
            assert_eq!(
                (status, body),
                (
                    403,
                    json!({ "error": "capability", "capability": cap.name() }).to_string()
                ),
                "{}",
                desc.method
            );
        }
        // An unknown token is still the handler's 401, never a 403 from the gate.
        let (status, body) = call(&s, Verb::Get, "/settings", "tok-nobody").await;
        assert_eq!(
            (status, body.as_str()),
            (401, r#"{"error":"unauthorized"}"#)
        );
    }

    #[tokio::test]
    async fn command_verbs_spend_their_own_capability_on_top_of_the_route() {
        let s = boot_with_control_tag(true, "table-caps-verb").await;
        limited(&s, "tok-limited-ww", &[Capability::WorkspaceWrite]);
        let post = |body: Value, token: &str| {
            client()
                .post(format!("{}/command", s.base))
                .header("authorization", format!("Bearer {token}"))
                .json(&body)
                .send()
        };
        // The route floor (workspace.write) passes; the verb's own capability is refused.
        let r = post(
            json!({ "type": "readScreen", "paneId": "p1" }),
            "tok-limited-ww",
        )
        .await
        .unwrap();
        assert_eq!(r.status().as_u16(), 403);
        assert_eq!(
            r.text().await.unwrap(),
            r#"{"capability":"panes.output","error":"capability"}"#
        );
        let r = post(json!({ "type": "newPane" }), "tok-limited-ww")
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 403);
        assert_eq!(
            r.text().await.unwrap(),
            r#"{"capability":"panes.spawn","error":"capability"}"#
        );
        // A verb whose floor is enough reaches dispatch (whatever it answers, not 403).
        let r = post(
            json!({ "type": "focusPane", "paneId": "p1" }),
            "tok-limited-ww",
        )
        .await
        .unwrap();
        assert_ne!(r.status().as_u16(), 403);
        // The master token is never refused for a capability: same requests, no 403.
        let r = post(json!({ "type": "readScreen", "paneId": "p1" }), &s.token)
            .await
            .unwrap();
        assert_ne!(r.status().as_u16(), 403);
        let r = post(json!({ "type": "newPane" }), &s.token).await.unwrap();
        assert_ne!(r.status().as_u16(), 403);
    }

    #[tokio::test]
    async fn master_token_is_never_refused_for_a_capability() {
        let s = boot_with_control_tag(true, "table-master").await;
        for desc in core_routes() {
            let (status, body) = call(&s, desc.verb, &desc.mounted_path(), &s.token).await;
            let err = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string));
            assert_ne!(
                err.as_deref(),
                Some("capability"),
                "{} refused the master token ({status})",
                desc.method
            );
        }
    }

    /// A module token comes from the module host, not the token store. The rights service
    /// (an installed capability source) claims it, and that claim is an identity: the token
    /// clears the 401 on every route and meets each route's capability gate on its own
    /// merits. Nothing else changes — an unclaimed token is still nobody.
    #[tokio::test]
    async fn a_module_token_is_an_identity_gated_only_by_its_rights() {
        let s = boot_with_control_tag(true, "table-module-token").await;
        // Not registered with the token store — claimed by a source alone.
        s.shared.caps.install(Arc::new(Fixed {
            token: "tok-module".to_string(),
            caps: [Capability::WorkspaceRead, Capability::EventsSubscribe]
                .into_iter()
                .collect(),
        }));
        // Identity without a capability: an authorized route that gates nothing.
        let (status, _) = call(&s, Verb::Get, "/schema", "tok-module").await;
        assert_eq!(status, 200, "a claimed token is no longer 401");
        // A held capability: through.
        let (status, _) = call(&s, Verb::Get, "/state", "tok-module").await;
        assert_eq!(status, 200);
        // An unheld capability: refused by name, not as a stranger.
        let (status, body) = call(&s, Verb::Get, "/settings", "tok-module").await;
        assert_eq!(
            (status, body.as_str()),
            (
                403,
                r#"{"capability":"settings.read","error":"capability"}"#
            )
        );
        // The WebSocket route takes the same identity from the query.
        let r = client()
            .get(format!("{}/events?token=tok-module", s.base))
            .header("connection", "Upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
            .send()
            .await
            .unwrap();
        assert_eq!(
            r.status().as_u16(),
            101,
            "module token upgrades the event stream"
        );
        // A token nobody claims is still nobody: 401 everywhere, never 403.
        let (status, body) = call(&s, Verb::Get, "/schema", "tok-stranger").await;
        assert_eq!(
            (status, body.as_str()),
            (401, r#"{"error":"unauthorized"}"#)
        );
        let (status, body) = call(&s, Verb::Get, "/state", "tok-stranger").await;
        assert_eq!(
            (status, body.as_str()),
            (401, r#"{"error":"unauthorized"}"#)
        );
        // (Upgrade headers so the 401 is the handler's, not the extractor's 400.)
        let r = client()
            .get(format!("{}/events?token=tok-stranger", s.base))
            .header("connection", "Upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 401);
    }

    // ---- module routes: /m/<owner>/<repo>/... -----------------------------------------------

    use crate::control::modules::{InvokeError, InvokeFuture, RouteCall, RouteInvoker};
    use avada_module_sdk::descriptor::{Param, ParamLocation, RouteDescriptor, Scope};
    use avada_module_sdk::manifest::ModuleId;
    use std::sync::Mutex;

    /// An invoker that remembers every call and answers with a fixed result.
    struct Recorder {
        calls: Mutex<Vec<RouteCall>>,
        reply: Mutex<Result<Value, InvokeError>>,
    }

    impl Recorder {
        fn install(s: &Server, reply: Result<Value, InvokeError>) -> Arc<Recorder> {
            let r = Arc::new(Recorder {
                calls: Mutex::new(vec![]),
                reply: Mutex::new(reply),
            });
            s.shared.install_route_invoker(r.clone());
            r
        }
        fn calls(&self) -> Vec<RouteCall> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl RouteInvoker for Recorder {
        fn invoke(&self, call: RouteCall) -> InvokeFuture<'_> {
            self.calls.lock().unwrap().push(call);
            let reply = self.reply.lock().unwrap().clone();
            Box::pin(async move { reply })
        }
    }

    fn files() -> ModuleId {
        ModuleId::new("acme/files").unwrap()
    }

    fn module_desc(method: &str, verb: Verb, path: &str, cap: Capability) -> RouteDescriptor {
        RouteDescriptor {
            method: method.into(),
            path: path.into(),
            verb,
            capability: Some(cap),
            summary: method.into(),
            // The registry insists every `{name}` capture is declared.
            params: path
                .split('/')
                .filter_map(|seg| seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')))
                .map(|name| Param {
                    name: name.to_string(),
                    location: ParamLocation::Path,
                    kind: "string".into(),
                    required: true,
                    summary: String::new(),
                })
                .collect(),
            scope: Scope::Token,
            module: Some(files()),
            response: None,
        }
    }

    /// `acme/files` publishes `GET /tree` (workspace.read) and `POST /files/{id}` (fs.write).
    fn register_files(s: &Server) {
        s.shared
            .schema
            .register_module_routes(
                &files(),
                vec![
                    module_desc("tree", Verb::Get, "/tree", Capability::WorkspaceRead),
                    module_desc("open", Verb::Post, "/files/{id}", Capability::FsWrite),
                ],
            )
            .unwrap();
    }

    async fn send(
        s: &Server,
        verb: Verb,
        path: &str,
        token: &str,
        body: Option<&str>,
    ) -> (u16, Value) {
        let url = format!("{}{}", s.base, path);
        let c = client();
        let mut req = match verb {
            Verb::Get => c.get(&url),
            Verb::Post => c.post(&url),
            Verb::Put => c.put(&url),
            Verb::Patch => c.patch(&url),
            Verb::Delete => c.delete(&url),
        }
        .header("authorization", format!("Bearer {token}"));
        if let Some(b) = body {
            req = req
                .header("content-type", "application/json")
                .body(b.to_string());
        }
        let r = req.send().await.unwrap();
        let status = r.status().as_u16();
        let text = r.text().await.unwrap();
        let v = serde_json::from_str::<Value>(&text)
            .unwrap_or_else(|_| panic!("non-JSON body {text:?} (status {status})"));
        (status, v)
    }

    #[tokio::test]
    async fn a_registered_module_route_reaches_the_module_with_path_and_query_params() {
        let s = boot_with_control_tag(true, "mroute-ok").await;
        register_files(&s);
        let rec = Recorder::install(&s, Ok(json!({ "entries": ["a", "b"] })));

        let (status, body) = send(
            &s,
            Verb::Get,
            "/m/acme/files/tree?depth=2&tree=root",
            &s.token,
            None,
        )
        .await;
        assert_eq!((status, body), (200, json!({ "entries": ["a", "b"] })));

        let (status, body) = send(
            &s,
            Verb::Post,
            "/m/acme/files/files/f%201?id=shadowed&mode=rw",
            &s.token,
            Some(r#"{"lines":[1,2]}"#),
        )
        .await;
        assert_eq!((status, body), (200, json!({ "entries": ["a", "b"] })));

        let calls = rec.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].module, files());
        assert_eq!(calls[0].route, "tree");
        assert_eq!(
            Value::Object(calls[0].params.clone()),
            json!({ "depth": "2", "tree": "root" })
        );
        assert_eq!(calls[0].body, None);
        // The path capture is decoded and wins over a query key of the same name.
        assert_eq!(calls[1].route, "open");
        assert_eq!(
            Value::Object(calls[1].params.clone()),
            json!({ "id": "f 1", "mode": "rw" })
        );
        assert_eq!(calls[1].body, Some(json!({ "lines": [1, 2] })));
    }

    #[tokio::test]
    async fn a_module_route_is_gated_by_the_descriptor_capability_not_the_verb() {
        let s = boot_with_control_tag(true, "mroute-cap").await;
        register_files(&s);
        let rec = Recorder::install(&s, Ok(json!(null)));
        limited(&s, "tok-reader", &[Capability::WorkspaceRead]);

        let (status, body) = send(&s, Verb::Get, "/m/acme/files/tree", "tok-reader", None).await;
        assert_eq!((status, body), (200, json!(null)));
        let (status, body) = send(
            &s,
            Verb::Post,
            "/m/acme/files/files/f1",
            "tok-reader",
            Some("{}"),
        )
        .await;
        assert_eq!(
            (status, body),
            (
                403,
                json!({ "error": "capability", "capability": "fs.write" })
            )
        );
        // A module token (claimed by a source, unknown to the token store) works the same way.
        s.shared.caps.install(Arc::new(Fixed {
            token: "tok-module".into(),
            caps: [Capability::FsWrite].into_iter().collect(),
        }));
        let (status, _) = send(
            &s,
            Verb::Post,
            "/m/acme/files/files/f1",
            "tok-module",
            Some("{}"),
        )
        .await;
        assert_eq!(status, 200);
        let (status, _) = send(&s, Verb::Get, "/m/acme/files/tree", "tok-module", None).await;
        assert_eq!(status, 403);
        // Only the three allowed calls reached the module.
        assert_eq!(
            rec.calls()
                .iter()
                .map(|c| c.route.as_str())
                .collect::<Vec<_>>(),
            ["tree", "open"]
        );
    }

    #[tokio::test]
    async fn a_stranger_gets_401_before_learning_whether_a_module_route_exists() {
        let s = boot_with_control_tag(true, "mroute-401").await;
        register_files(&s);
        Recorder::install(&s, Ok(json!(null)));
        let (status, body) = send(&s, Verb::Get, "/m/acme/files/tree", "tok-stranger", None).await;
        assert_eq!((status, body), (401, json!({ "error": "unauthorized" })));
        let (status, body) = send(&s, Verb::Get, "/m/acme/files/nope", "tok-stranger", None).await;
        assert_eq!((status, body), (401, json!({ "error": "unauthorized" })));
    }

    #[tokio::test]
    async fn an_unknown_module_path_is_404_and_a_wrong_verb_is_405_with_the_fallback_bodies() {
        let s = boot_with_control_tag(true, "mroute-404").await;
        register_files(&s);
        Recorder::install(&s, Ok(json!(null)));
        let (status, body) = send(&s, Verb::Get, "/m/acme/files/nope", &s.token, None).await;
        assert_eq!(
            (status, body),
            (
                404,
                json!({ "error": "not found", "path": "/m/acme/files/nope" })
            )
        );
        // The shape must match exactly: an extra segment is a different path.
        let (status, _) = send(&s, Verb::Get, "/m/acme/files/tree/deeper", &s.token, None).await;
        assert_eq!(status, 404);
        // Another module's prefix knows nothing about acme/files' routes.
        let (status, _) = send(&s, Verb::Get, "/m/other/files/tree", &s.token, None).await;
        assert_eq!(status, 404);
        // An owner/repo that cannot be a module id is 404 too, not a 500.
        let (status, _) = send(&s, Verb::Get, "/m/AC%20ME/files/tree", &s.token, None).await;
        assert_eq!(status, 404);
        let (status, body) = send(&s, Verb::Delete, "/m/acme/files/tree", &s.token, None).await;
        assert_eq!(
            (status, body),
            (405, json!({ "error": "method not allowed" }))
        );
    }

    #[tokio::test]
    async fn a_module_route_registers_and_unregisters_live_without_a_router_rebuild() {
        let s = boot_with_control_tag(true, "mroute-live").await;
        Recorder::install(&s, Ok(json!("hi")));
        let (status, _) = send(&s, Verb::Get, "/m/acme/files/tree", &s.token, None).await;
        assert_eq!(status, 404);
        register_files(&s);
        let (status, body) = send(&s, Verb::Get, "/m/acme/files/tree", &s.token, None).await;
        assert_eq!((status, body), (200, json!("hi")));
        s.shared
            .schema
            .with(|reg| reg.unregister_module_routes(&files()));
        let (status, _) = send(&s, Verb::Get, "/m/acme/files/tree", &s.token, None).await;
        assert_eq!(status, 404);
    }

    #[tokio::test]
    async fn a_listed_route_without_a_reachable_module_is_503_and_module_errors_map_to_400_502() {
        let s = boot_with_control_tag(true, "mroute-err").await;
        register_files(&s);
        // No invoker installed yet: the app has not started the module host.
        let (status, body) = send(&s, Verb::Get, "/m/acme/files/tree", &s.token, None).await;
        assert_eq!(
            (status, body),
            (
                503,
                json!({ "error": "module unavailable", "module": "acme/files" })
            )
        );
        let rec = Recorder::install(&s, Err(InvokeError::Unavailable("not running".into())));
        let (status, body) = send(&s, Verb::Get, "/m/acme/files/tree", &s.token, None).await;
        assert_eq!(status, 503);
        assert_eq!(body["message"], "not running");
        *rec.reply.lock().unwrap() = Err(InvokeError::Module {
            code: -32602,
            message: "depth must be a number".into(),
            data: Some(json!({ "param": "depth" })),
        });
        let (status, body) = send(&s, Verb::Get, "/m/acme/files/tree", &s.token, None).await;
        assert_eq!(
            (status, body),
            (
                400,
                json!({
                    "error": "module",
                    "code": -32602,
                    "message": "depth must be a number",
                    "data": { "param": "depth" }
                })
            )
        );
        *rec.reply.lock().unwrap() = Err(InvokeError::Failed("module did not answer".into()));
        let (status, body) = send(&s, Verb::Get, "/m/acme/files/tree", &s.token, None).await;
        assert_eq!(status, 502);
        assert_eq!(body["error"], "module failed");
        assert_eq!(body["message"], "module did not answer");
    }

    #[tokio::test]
    async fn a_body_that_is_not_json_is_refused_before_the_module_sees_it() {
        let s = boot_with_control_tag(true, "mroute-body").await;
        register_files(&s);
        let rec = Recorder::install(&s, Ok(json!(null)));
        let (status, body) = send(
            &s,
            Verb::Post,
            "/m/acme/files/files/f1",
            &s.token,
            Some("not json"),
        )
        .await;
        assert_eq!(status, 400);
        assert_eq!(body["error"], "bad request");
        assert!(rec.calls().is_empty());
    }
}

/// Track F2: the `/marketplace/...` routes over the real axum stack, with a marketplace
/// built from the fake GitHub, the local bare-repo fixtures and the fake `cargo`
/// (`crate::marketplace::testing`). No network, no real toolchain, no real HOME.
#[cfg(test)]
mod marketplace_routes {
    use super::golden::{boot_with_control_tag, client, Server};
    use crate::control::descriptor_table::core_routes;
    use crate::control::dispatch::CapabilitySource;
    use crate::marketplace::job::Phase;
    use crate::marketplace::testing::{
        files_state, manifest_for, reopen, rig, scratch, DeviceOutcome, FakeCargo,
        FAKE_ACCESS_TOKEN, FILES,
    };
    use avada_module_sdk::caps::Capability;
    use avada_module_sdk::descriptor::{ParamLocation, Verb};
    use serde_json::{json, Value};
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use std::time::Duration;

    struct Fixed {
        token: String,
        caps: BTreeSet<Capability>,
    }

    impl CapabilitySource for Fixed {
        fn caps_for(&self, token: &str) -> Option<BTreeSet<Capability>> {
            (token == self.token).then(|| self.caps.clone())
        }
    }

    fn limited(s: &Server, token: &str, caps: &[Capability]) {
        s.shared
            .tokens
            .lock()
            .unwrap()
            .add_device(token.to_string(), "limited".into(), None, None);
        s.shared.caps.install(Arc::new(Fixed {
            token: token.to_string(),
            caps: caps.iter().copied().collect(),
        }));
    }

    async fn send(
        s: &Server,
        verb: Verb,
        path: &str,
        token: Option<&str>,
        body: Option<&str>,
    ) -> (u16, Value, String) {
        let url = format!("{}{}", s.base, path);
        let c = client();
        let mut req = match verb {
            Verb::Get => c.get(&url),
            Verb::Post => c.post(&url),
            Verb::Put => c.put(&url),
            Verb::Patch => c.patch(&url),
            Verb::Delete => c.delete(&url),
        };
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        if let Some(b) = body {
            req = req
                .header("content-type", "application/json")
                .body(b.to_string());
        }
        let r = req.send().await.unwrap();
        let status = r.status().as_u16();
        let text = r.text().await.unwrap();
        let v = serde_json::from_str::<Value>(&text).unwrap_or(Value::Null);
        (status, v, text)
    }

    async fn get(s: &Server, path: &str) -> (u16, Value) {
        let (st, v, _) = send(s, Verb::Get, path, Some(&s.token), None).await;
        (st, v)
    }

    async fn post(s: &Server, path: &str, body: &str) -> (u16, Value) {
        let (st, v, _) = send(s, Verb::Post, path, Some(&s.token), Some(body)).await;
        (st, v)
    }

    fn marketplace_routes() -> Vec<avada_module_sdk::descriptor::RouteDescriptor> {
        core_routes()
            .into_iter()
            .filter(|r| r.method.starts_with("marketplace."))
            .collect()
    }

    /// A path with every `{param}` replaced by something concrete.
    fn concrete(path: &str) -> String {
        path.replace("{owner}", "acme")
            .replace("{repo}", "avada-files")
            .replace("{version}", "1.0.0")
            .replace("{id}", "nope")
    }

    #[test]
    fn the_table_lists_twelve_gated_routes_with_their_path_params_declared() {
        let routes = marketplace_routes();
        let methods: Vec<&str> = routes.iter().map(|r| r.method.as_str()).collect();
        assert_eq!(
            methods,
            [
                "marketplace.search",
                "marketplace.show",
                "marketplace.install",
                "marketplace.jobs",
                "marketplace.job",
                "marketplace.enable",
                "marketplace.disable",
                "marketplace.uninstall",
                "marketplace.installed",
                "marketplace.toolchain",
                "marketplace.signin",
                "marketplace.signin.poll",
            ]
        );
        for r in &routes {
            assert_eq!(
                r.capability,
                Some(Capability::MarketplaceManage),
                "{}",
                r.method
            );
            assert!(r.path.starts_with("/marketplace/"), "{}", r.path);
            // Every `{x}` in the path is a declared Path param and vice versa.
            let in_path: BTreeSet<String> = r
                .path
                .split('/')
                .filter_map(|seg| seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')))
                .map(str::to_string)
                .collect();
            let declared: BTreeSet<String> = r
                .params
                .iter()
                .filter(|p| p.location == ParamLocation::Path)
                .map(|p| p.name.clone())
                .collect();
            assert_eq!(in_path, declared, "{}", r.method);
            for p in r
                .params
                .iter()
                .filter(|p| p.location == ParamLocation::Path)
            {
                assert!(
                    p.required,
                    "{}: path param {} must be required",
                    r.method, p.name
                );
            }
        }
        assert_eq!(
            routes
                .iter()
                .filter(|r| matches!(r.verb, Verb::Post))
                .count(),
            4
        );
        assert_eq!(
            routes
                .iter()
                .filter(|r| matches!(r.verb, Verb::Delete))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn every_route_is_401_then_403_then_503_until_a_marketplace_is_installed() {
        let s = boot_with_control_tag(true, "mp-gates").await;
        limited(&s, "tok-nothing", &[]);
        limited(&s, "tok-mp", &[Capability::MarketplaceManage]);
        for r in marketplace_routes() {
            let path = concrete(&r.path);
            let (st, v, _) = send(&s, r.verb, &path, None, None).await;
            assert_eq!(st, 401, "{}", r.method);
            assert_eq!(v["error"], "unauthorized", "{}", r.method);
            let (st, _, _) = send(&s, r.verb, &path, Some("tok-nothing"), None).await;
            assert_eq!(st, 403, "{} let an empty token through", r.method);
            for tok in ["tok-mp", s.token.as_str()] {
                let (st, v, _) = send(&s, r.verb, &path, Some(tok), None).await;
                assert_eq!(st, 503, "{} with {tok}", r.method);
                assert_eq!(v["error"], "marketplace unavailable", "{}", r.method);
            }
        }
        // Installing takes effect on the next request; no router rebuild.
        let r = rig(
            "routes-gates",
            files_state(),
            FakeCargo::Builds,
            Duration::from_secs(300),
        )
        .await;
        s.shared.install_marketplace(Arc::clone(&r.mp));
        let (st, v) = get(&s, "/marketplace/installed").await;
        assert_eq!(st, 200, "{v}");
        assert_eq!(v["modules"], json!([]));
        let (st, v, _) = send(
            &s,
            Verb::Get,
            "/marketplace/installed",
            Some("tok-nothing"),
            None,
        )
        .await;
        assert_eq!(st, 403, "{v}");
    }

    #[tokio::test]
    async fn install_enable_disable_uninstall_through_the_routes() {
        let s = boot_with_control_tag(true, "mp-install").await;
        let r = rig(
            "routes-install",
            files_state(),
            FakeCargo::Builds,
            Duration::from_secs(300),
        )
        .await;
        let sha = r.fixtures.repo(
            FILES,
            "v1.0.0",
            &manifest_for(FILES, "1.0.0", "kind = \"source\"", ""),
        );
        r.github
            .state
            .lock()
            .unwrap()
            .tags
            .insert(FILES.into(), vec![("v1.0.0".into(), sha.clone())]);
        s.shared.install_marketplace(Arc::clone(&r.mp));

        // Search and show go through the fake GitHub; the install goes through the
        // local git fixture.
        let (st, v) = get(&s, "/marketplace/search?q=files").await;
        assert_eq!(st, 200, "{v}");
        let names: Vec<&str> = v["modules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["full_name"].as_str().unwrap())
            .collect();
        assert_eq!(names, [FILES]);
        let (st, v) = get(&s, "/marketplace/modules/acme/avada-files").await;
        assert_eq!(st, 200, "{v}");
        assert_eq!(v["module"], FILES);
        assert_eq!(v["newest_tag"], "v1.0.0");
        assert_eq!(v["tags"][0]["commit"], sha);
        assert_eq!(v["installed"], json!([]));

        // Install: 202 with the job, then the job moves under GET /marketplace/jobs/{id}.
        let (st, v) = post(
            &s,
            "/marketplace/install",
            &json!({ "module": FILES, "workspace": "ws1", "commit": sha }).to_string(),
        )
        .await;
        assert_eq!(st, 202, "{v}");
        let id = v["job"]["id"].as_str().unwrap().to_string();
        assert_eq!(v["job"]["module"], FILES);
        assert_eq!(v["job"]["phase"], json!(Phase::Fetch));
        let mut seen_phases = BTreeSet::new();
        let mut done = Value::Null;
        for _ in 0..1500 {
            let (st, v) = get(&s, &format!("/marketplace/jobs/{id}")).await;
            assert_eq!(st, 200, "{v}");
            seen_phases.insert(v["phase"].as_str().unwrap().to_string());
            let phase: Phase = serde_json::from_value(v["phase"].clone()).unwrap();
            if phase.is_terminal() {
                done = v;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(done["phase"], json!(Phase::Done), "{done}");
        assert_eq!(done["version"], "1.0.0");
        assert_eq!(done["tag"], "v1.0.0");
        assert_eq!(done["progress"], 100);
        assert!(seen_phases.contains("done"), "{seen_phases:?}");
        let (st, v) = get(&s, "/marketplace/jobs").await;
        assert_eq!(st, 200);
        assert_eq!(v["jobs"][0]["id"], id);
        assert_eq!(v["jobs"].as_array().unwrap().len(), 1);

        // The install record and workspace state are visible.
        let (st, v) = get(&s, "/marketplace/installed").await;
        assert_eq!(st, 200, "{v}");
        let m = &v["modules"][0];
        assert_eq!(m["module"], FILES);
        assert_eq!(m["version"], "1.0.0");
        assert_eq!(m["active"], true);
        assert_eq!(m["tag"], "v1.0.0");
        assert_eq!(m["commit"], sha);
        assert!(m["sha256"].as_str().unwrap().len() == 64, "{m}");
        assert_eq!(m["enabled"], json!({ "ws1": true }));
        assert_eq!(m["broken"], Value::Null);
        let (_, v) = get(&s, "/marketplace/modules/acme/avada-files").await;
        assert_eq!(v["installed"], json!(["1.0.0"]));
        assert_eq!(v["active"], "1.0.0");

        // Disable, enable, and the refusals around them.
        let (st, v) = post(
            &s,
            "/marketplace/modules/acme/avada-files/disable",
            r#"{"workspace":"ws1"}"#,
        )
        .await;
        assert_eq!(st, 200, "{v}");
        assert_eq!(v["enabled"], json!({ "ws1": false }));
        let (st, v) = post(
            &s,
            "/marketplace/modules/acme/avada-files/enable",
            r#"{"workspace":"ws2"}"#,
        )
        .await;
        assert_eq!(st, 200, "{v}");
        assert_eq!(v["enabled"], json!({ "ws1": false, "ws2": true }));
        let (st, v) = post(&s, "/marketplace/modules/acme/avada-files/enable", "").await;
        assert_eq!(st, 400, "{v}");
        assert_eq!(v["error"], "missing workspace");
        let (st, v) = post(
            &s,
            "/marketplace/modules/acme/avada-files/enable",
            r#"{"workspace":"../x"}"#,
        )
        .await;
        assert_eq!(st, 400, "{v}");
        let (st, v) = post(
            &s,
            "/marketplace/modules/acme/avada-git/enable",
            r#"{"workspace":"ws1"}"#,
        )
        .await;
        assert_eq!(st, 404, "{v}");
        assert_ne!(
            v["error"], "not found",
            "must not look like an unmounted route"
        );
        let (st, v) = post(
            &s,
            "/marketplace/modules/acme/avada-files/enable",
            "{not json",
        )
        .await;
        assert_eq!(st, 400, "{v}");
        assert_eq!(v["error"], "bad request");

        // Uninstall the only version; the second try is 404, and the state is gone.
        let (st, v, _) = send(
            &s,
            Verb::Delete,
            "/marketplace/modules/acme/avada-files/1.0.0",
            Some(&s.token),
            None,
        )
        .await;
        assert_eq!(st, 200, "{v}");
        assert_eq!(v["ok"], true);
        let (st, v, _) = send(
            &s,
            Verb::Delete,
            "/marketplace/modules/acme/avada-files/1.0.0",
            Some(&s.token),
            None,
        )
        .await;
        assert_eq!(st, 404, "{v}");
        assert_ne!(v["error"], "not found");
        let (st, v, _) = send(
            &s,
            Verb::Delete,
            "/marketplace/modules/acme/avada-files/not-a-version",
            Some(&s.token),
            None,
        )
        .await;
        assert_eq!(
            st, 404,
            "an unparsable version is simply not installed: {v}"
        );
        let (_, v) = get(&s, "/marketplace/installed").await;
        assert_eq!(v["modules"], json!([]));
        let (_, v) = get(&s, "/marketplace/modules/acme/avada-files").await;
        assert_eq!(v["enabled"], json!({}));
        let (st, v) = get(&s, "/marketplace/jobs/nope").await;
        assert_eq!(st, 404, "{v}");
        assert_eq!(v["error"], "no job `nope`");
    }

    #[tokio::test]
    async fn install_refusals_are_statuses_and_a_failed_build_is_a_failed_job() {
        let s = boot_with_control_tag(true, "mp-refuse").await;
        let r = rig(
            "routes-refuse",
            files_state(),
            FakeCargo::Fails,
            Duration::from_secs(300),
        )
        .await;
        r.fixtures.repo(
            FILES,
            "v1.0.0",
            &manifest_for(FILES, "1.0.0", "kind = \"source\"", ""),
        );
        r.fixtures.repo(
            "acme/avada-git",
            "v2.0.0",
            &manifest_for("acme/avada-git", "2.0.0", "kind = \"binary\"", ""),
        );
        s.shared.install_marketplace(Arc::clone(&r.mp));

        for (body, status, error) in [
            ("{oops", 400, "bad request"),
            ("[]", 400, "bad request"),
            ("{}", 400, "missing module"),
            (r#"{"module":""}"#, 400, "missing module"),
            (
                r#"{"module":"nope"}"#,
                400,
                "`nope` is not an owner/repo module id",
            ),
            (
                r#"{"module":"acme/avada-files","workspace":"a b"}"#,
                400,
                "`a b` is not a usable workspace key",
            ),
            (
                r#"{"module":"acme/avada-files","accepted":["not.a.cap"]}"#,
                400,
                "bad request",
            ),
        ] {
            let (st, v) = post(&s, "/marketplace/install", body).await;
            assert_eq!(st, status, "{body}: {v}");
            assert_eq!(v["error"], error, "{body}: {v}");
        }
        assert!(
            r.mp.jobs().is_empty(),
            "a malformed request must not start a job"
        );

        // Refusals the pipeline finds while fetching or verifying — a binary module, a tag
        // that does not exist, a tag naming a different commit — are failed jobs: the
        // route answers 202 once the request is well-formed, and the job says why.
        for (body, needle) in [
            (
                r#"{"module":"acme/avada-git","tag":"v2.0.0"}"#,
                "prebuilt binary",
            ),
            (
                r#"{"module":"acme/avada-files","tag":"v9.9.9"}"#,
                "no tag `v9.9.9`",
            ),
            (
                r#"{"module":"acme/avada-files","commit":"0000000000000000000000000000000000000000"}"#,
                "names commit",
            ),
        ] {
            let (st, v) = post(&s, "/marketplace/install", body).await;
            assert_eq!(st, 202, "{body}: {v}");
            let id = v["job"]["id"].as_str().unwrap().to_string();
            let done = crate::marketplace::testing::wait(&r.mp, &id).await;
            assert_eq!(done.phase, Phase::Failed, "{body}: {done:?}");
            let (_, v) = get(&s, &format!("/marketplace/jobs/{id}")).await;
            assert_eq!(v["phase"], "failed", "{body}: {v}");
            let e = v["error"].as_str().unwrap();
            assert!(e.starts_with("refused: "), "{body}: {e}");
            assert!(e.contains(needle), "{body}: {e}");
        }
        let (_, v) = get(&s, "/marketplace/installed").await;
        assert_eq!(v["modules"], json!([]), "refusals install nothing");

        // The build fails: the job starts (202) and ends Failed, with the compiler's tail.
        let (st, v) = post(
            &s,
            "/marketplace/install",
            r#"{"module":"acme/avada-files","accepted":["fs.read"]}"#,
        )
        .await;
        assert_eq!(st, 202, "{v}");
        let id = v["job"]["id"].as_str().unwrap().to_string();
        let done = crate::marketplace::testing::wait(&r.mp, &id).await;
        assert_eq!(done.phase, Phase::Failed);
        let (st, v) = get(&s, &format!("/marketplace/jobs/{id}")).await;
        assert_eq!(st, 200);
        assert_eq!(v["phase"], json!(Phase::Failed));
        assert!(v["error"].as_str().unwrap().contains("build"), "{v}");
        assert!(
            v["log_tail"]
                .as_array()
                .unwrap()
                .iter()
                .any(|l| l.as_str().unwrap().contains("E0425")),
            "{v}"
        );
        let (_, v) = get(&s, "/marketplace/installed").await;
        assert_eq!(v["modules"], json!([]), "a failed build installs nothing");

        // The toolchain report, and the 412 with the guide when the toolchain is gone.
        let (st, v) = get(&s, "/marketplace/toolchain").await;
        assert_eq!(st, 200, "{v}");
        assert_eq!(v["ready"], true);
        assert_eq!(v["missing"], json!([]));
        assert_eq!(v["guide"], Value::Null);
        assert_eq!(v["signed_in"], false);
        let bare = reopen(
            &r.root,
            &r.fixtures,
            &r.github,
            Arc::clone(&r.tokens),
            Some(scratch("routes-no-tools")),
            Duration::from_secs(300),
        );
        s.shared.install_marketplace(bare);
        let (st, v) = get(&s, "/marketplace/toolchain").await;
        assert_eq!(st, 200, "{v}");
        assert_eq!(v["ready"], false);
        assert!(v["guide"].as_str().unwrap().contains("rustup"), "{v}");
        let (st, v) = post(
            &s,
            "/marketplace/install",
            r#"{"module":"acme/avada-files"}"#,
        )
        .await;
        assert_eq!(st, 412, "{v}");
        assert_eq!(v["error"], "toolchain missing");
        assert!(v["guide"].as_str().unwrap().contains("rustup"), "{v}");
    }

    #[tokio::test]
    async fn sign_in_routes_show_the_user_code_and_never_the_token() {
        let s = boot_with_control_tag(true, "mp-signin").await;
        let r = rig(
            "routes-signin",
            files_state(),
            FakeCargo::Builds,
            Duration::from_secs(300),
        )
        .await;
        s.shared.install_marketplace(Arc::clone(&r.mp));
        let (st, v, text) = send(&s, Verb::Post, "/marketplace/signin", Some(&s.token), None).await;
        assert_eq!(st, 200, "{v}");
        let id = v["id"].as_str().unwrap().to_string();
        assert!(!v["user_code"].as_str().unwrap().is_empty());
        assert!(v["verification_uri"]
            .as_str()
            .unwrap()
            .contains("/login/device"));
        assert_eq!(v["status"], "pending");
        assert!(!text.contains("device_code"), "{text}");
        let (st, v, text) = send(
            &s,
            Verb::Get,
            &format!("/marketplace/signin/{id}"),
            Some(&s.token),
            None,
        )
        .await;
        assert_eq!(st, 200, "{v}");
        assert_eq!(v["status"], "pending");
        assert!(!text.contains(FAKE_ACCESS_TOKEN));
        r.github.state.lock().unwrap().outcome = DeviceOutcome::Approved;
        let (st, v, text) = send(
            &s,
            Verb::Get,
            &format!("/marketplace/signin/{id}"),
            Some(&s.token),
            None,
        )
        .await;
        assert_eq!(st, 200, "{v}");
        assert_eq!(v["status"], "done");
        assert!(
            !text.contains(FAKE_ACCESS_TOKEN),
            "the token must never leave the store"
        );
        assert!(r.mp.signed_in());
        let (_, v) = get(&s, "/marketplace/toolchain").await;
        assert_eq!(v["signed_in"], true);
        let (st, v) = get(&s, "/marketplace/signin/nope").await;
        assert_eq!(st, 404, "{v}");
        assert_eq!(v["error"], "no sign-in `nope`");
    }
}
