//! Host-side method dispatch: what happens when a module calls `host.*`.
//!
//! Every method in `contract::methods::HOST_REQUIRED_V1` is named here, so a typo is a
//! compile error and a missing arm is a test failure ([`tests::every_contract_method_is_
//! dispatched`]). The gate is consulted first, with the capability the contract assigns
//! to the method; only then does the arm run. Methods this host does not serve yet
//! answer `MethodNotFound` with `data.unsupported = true` — distinct from an unknown
//! name, which has no `data`.

use super::gate::{CapabilityGate, Decision};
use super::host::HostEvent;
use super::rail::{FanOut, RailEvent, RailState};
use avada_module_sdk::caps::Capability;
use avada_module_sdk::contract::methods::{self, required_capability};
use avada_module_sdk::contract::{ErrorCode, Notification, Request, Response, RpcError};
use avada_module_sdk::descriptor::{validate_table, RouteDescriptor, Scope};
use avada_module_sdk::rail::{validate_entry, RegisterRail, SetRows};
use avada_module_sdk::ModuleId;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The `host.*` methods this host serves; advertised in the host hello.
pub const SERVED: &[&str] = &[
    methods::HOST_RAIL_REGISTER,
    methods::HOST_ROWS_SET,
    methods::HOST_COMMAND_REGISTER,
    methods::HOST_PREFS_DECLARE,
    methods::HOST_PREFS_GET,
    methods::HOST_TOAST,
    methods::HOST_ROUTES_REGISTER,
    methods::HOST_FS_LIST,
    methods::HOST_FS_READ,
    methods::HOST_FS_WRITE,
    methods::HOST_PANES_SPAWN,
    methods::HOST_PANES_INPUT,
    methods::HOST_EVENTS_SUBSCRIBE,
];

/// The largest file `host.fs.read` will hand back. A module that wants a gigabyte of
/// video does not want it as one JSON string, and the host would buffer all of it twice
/// (bytes, then base64) before the writer ever saw a line.
pub const MAX_READ: u64 = 8 * 1024 * 1024;

/// The largest body `host.fs.write` will accept, deliberately the same number as
/// [`MAX_READ`]. A module that can read a file it wrote is the least surprising rule, and
/// an asymmetric pair would mean a module could produce a file it can never open again.
pub const MAX_WRITE: usize = MAX_READ as usize;

/// One command a module registered (`host.command.register`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSpec {
    /// Module-scoped id; the host sends it back in `module.command.invoke`.
    pub id: String,
    /// Palette label.
    pub label: String,
    /// Optional key chord.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chord: Option<String>,
}

#[derive(Deserialize)]
struct RegisterCommands {
    commands: Vec<CommandSpec>,
}

#[derive(Deserialize)]
struct ToastParams {
    text: String,
    #[serde(default = "default_level")]
    level: String,
}

fn default_level() -> String {
    "info".to_string()
}

#[derive(Deserialize)]
struct DeclarePrefs {
    page: Value,
}

/// A module's preference values, persisted as `prefs.json` in its data directory.
#[derive(Debug)]
pub struct Prefs {
    path: PathBuf,
    /// The page the module declared, if it has.
    pub page: Option<Value>,
    /// Current values.
    pub values: Map<String, Value>,
}

impl Prefs {
    /// Load from `data_dir/prefs.json`; missing or unreadable means empty.
    pub fn load(data_dir: &Path) -> Prefs {
        let path = data_dir.join("prefs.json");
        let values = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|v| v.get("values").cloned())
            .and_then(|v| match v {
                Value::Object(m) => Some(m),
                _ => None,
            })
            .unwrap_or_default();
        Prefs {
            path,
            page: None,
            values,
        }
    }

    /// Write the values back.
    pub fn save(&self) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let body = serde_json::to_vec_pretty(&json!({ "values": self.values }))?;
        std::fs::write(&self.path, body)
    }

    /// `{ "values": {...} }`, the shape of `host.prefs.get`'s result and of the
    /// `module.prefs.changed` notification.
    pub fn as_result(&self) -> Value {
        json!({ "values": self.values })
    }
}

/// What every module's dispatcher shares: the gate and the event fan-outs.
pub(crate) struct Shared {
    pub gate: Arc<dyn CapabilityGate>,
    pub rail_events: FanOut<RailEvent>,
    pub events: FanOut<HostEvent>,
    /// The open workspace's root, when it has one. Every `host.fs.*` path is resolved
    /// against it unless the module holds `fs.read_any`. Held here rather than read off
    /// `HostConfig` because the root changes when the human switches workspace, and a
    /// module that outlives the switch must not keep reading the old tree.
    pub workspace_root: Mutex<Option<PathBuf>>,
}

/// Per-module dispatch state.
pub(crate) struct Dispatcher {
    module: ModuleId,
    shared: Arc<Shared>,
    /// What the module put on the rail.
    pub rail: Mutex<RailState>,
    /// Registered commands.
    pub commands: Mutex<Vec<CommandSpec>>,
    /// Preferences.
    pub prefs: Mutex<Prefs>,
    /// Control-plane routes the module registered, stamped with its id.
    pub routes: Mutex<Vec<RouteDescriptor>>,
    /// Event kinds the module asked for (`host.events.subscribe`). The whole set;
    /// subscribing again replaces it.
    pub subscriptions: Mutex<BTreeSet<String>>,
}

/// `host.routes.register` params.
#[derive(Deserialize)]
struct RegisterRoutes {
    routes: Vec<RouteDescriptor>,
}

/// `host.fs.list` and `host.fs.read` params.
#[derive(Deserialize)]
struct PathParams {
    path: String,
}

/// `host.fs.write` params.
#[derive(Deserialize)]
struct WriteParams {
    path: String,
    text: String,
}

/// `host.panes.spawn` params.
#[derive(Deserialize)]
struct SpawnPane {
    kind: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    surface: Option<String>,
}

/// `host.panes.input` params.
#[derive(Deserialize)]
struct PaneInput {
    pane_id: String,
    text: String,
}

/// `host.events.subscribe` params.
#[derive(Deserialize)]
struct Subscribe {
    kinds: Vec<String>,
}

/// What a scope check concluded before any path resolution happened.
enum Scoped {
    /// The module holds the `_any` capability: no boundary applies.
    Anywhere,
    /// The canonicalised workspace root every resolved path must sit inside.
    Within(PathBuf),
}

fn invalid_params(e: impl std::fmt::Display) -> RpcError {
    RpcError::new(ErrorCode::InvalidParams, e.to_string())
}

fn unsupported(method: &str) -> RpcError {
    let mut e = RpcError::new(
        ErrorCode::MethodNotFound,
        format!("`{method}` is not served by this host yet"),
    );
    e.data = Some(json!({ "unsupported": true }));
    e
}

impl Dispatcher {
    pub(crate) fn new(module: ModuleId, shared: Arc<Shared>, data_dir: &Path) -> Self {
        Dispatcher {
            module,
            shared,
            rail: Mutex::new(RailState::default()),
            commands: Mutex::new(Vec::new()),
            prefs: Mutex::new(Prefs::load(data_dir)),
            routes: Mutex::new(Vec::new()),
            subscriptions: Mutex::new(BTreeSet::new()),
        }
    }

    /// Handle one request end to end, producing the response to write back.
    pub(crate) fn handle(&self, req: &Request) -> Response {
        match self.call(&req.method, &req.params) {
            Ok(v) => req.ok(v),
            Err(e) => req.err(e),
        }
    }

    /// A notification from the module. Nothing in contract v1 is host-bound as a
    /// notification, so this only logs.
    pub(crate) fn notification(&self, n: &Notification) {
        tracing::debug!(module = %self.module, method = %n.method, "module notification");
    }

    /// The gate check every arm goes through.
    fn gate(&self, method: &str) -> Result<(), RpcError> {
        let Some(cap) = required_capability(method) else {
            return Ok(());
        };
        match self.shared.gate.check(&self.module, cap) {
            Decision::Allow => Ok(()),
            Decision::Deny | Decision::Ask => Err(RpcError::new(
                ErrorCode::CapabilityDenied,
                format!("`{}` was not granted to this module", cap.name()),
            )),
        }
    }

    /// Dispatch by name. Public within the crate so the host can call it for tests and
    /// for its own `set_prefs`.
    pub(crate) fn call(&self, method: &str, params: &Value) -> Result<Value, RpcError> {
        self.gate(method)?;
        match method {
            methods::HOST_RAIL_REGISTER => self.rail_register(params),
            methods::HOST_ROWS_SET => self.rows_set(params),
            methods::HOST_COMMAND_REGISTER => self.command_register(params),
            methods::HOST_PREFS_DECLARE => self.prefs_declare(params),
            methods::HOST_PREFS_GET => Ok(self.lock_prefs().as_result()),
            methods::HOST_TOAST => self.toast(params),
            methods::HOST_ROUTES_REGISTER => self.routes_register(params),
            methods::HOST_FS_LIST => self.fs_list(params),
            methods::HOST_FS_READ => self.fs_read(params),
            methods::HOST_FS_WRITE => self.fs_write(params),
            methods::HOST_PANES_SPAWN => self.panes_spawn(params),
            methods::HOST_PANES_INPUT => self.panes_input(params),
            methods::HOST_EVENTS_SUBSCRIBE => self.events_subscribe(params),
            methods::HOST_KEYCHAIN_GET | methods::HOST_KEYCHAIN_SET => Err(unsupported(method)),
            other => Err(RpcError::new(
                ErrorCode::MethodNotFound,
                format!("unknown method `{other}`"),
            )),
        }
    }

    fn lock_prefs(&self) -> std::sync::MutexGuard<'_, Prefs> {
        self.prefs.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn rail_register(&self, params: &Value) -> Result<Value, RpcError> {
        let RegisterRail { mut entries } =
            serde_json::from_value(params.clone()).map_err(invalid_params)?;
        for e in &mut entries {
            validate_entry(e).map_err(invalid_params)?;
            match &e.module {
                Some(m) if *m != self.module => {
                    return Err(invalid_params(format!(
                        "rail entry `{}` claims module `{m}`",
                        e.id
                    )));
                }
                _ => e.module = Some(self.module.clone()),
            }
        }
        let mut ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        ids.sort_unstable();
        if ids.windows(2).any(|w| w[0] == w[1]) {
            return Err(invalid_params("duplicate rail entry id"));
        }
        self.rail
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .register(entries.clone());
        self.shared.rail_events.send(RailEvent::Registered {
            module: self.module.clone(),
            entries,
        });
        Ok(Value::Null)
    }

    fn rows_set(&self, params: &Value) -> Result<Value, RpcError> {
        let SetRows { entry, rows } =
            serde_json::from_value(params.clone()).map_err(invalid_params)?;
        self.rail
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_rows(&entry, rows.clone())
            .map_err(invalid_params)?;
        self.shared.rail_events.send(RailEvent::Rows {
            module: self.module.clone(),
            entry,
            rows,
        });
        Ok(Value::Null)
    }

    fn command_register(&self, params: &Value) -> Result<Value, RpcError> {
        let RegisterCommands { commands } =
            serde_json::from_value(params.clone()).map_err(invalid_params)?;
        for c in &commands {
            if c.id.trim().is_empty() || c.label.trim().is_empty() {
                return Err(invalid_params("command id and label must not be empty"));
            }
        }
        *self.commands.lock().unwrap_or_else(|e| e.into_inner()) = commands.clone();
        self.shared.events.send(HostEvent::Commands {
            module: self.module.clone(),
            commands,
        });
        Ok(Value::Null)
    }

    fn prefs_declare(&self, params: &Value) -> Result<Value, RpcError> {
        let DeclarePrefs { page } =
            serde_json::from_value(params.clone()).map_err(invalid_params)?;
        self.lock_prefs().page = Some(page.clone());
        self.shared.events.send(HostEvent::PrefsDeclared {
            module: self.module.clone(),
            page,
        });
        Ok(Value::Null)
    }

    fn toast(&self, params: &Value) -> Result<Value, RpcError> {
        let ToastParams { text, level } =
            serde_json::from_value(params.clone()).map_err(invalid_params)?;
        if text.trim().is_empty() {
            return Err(invalid_params("toast text must not be empty"));
        }
        self.shared.events.send(HostEvent::Toast {
            module: self.module.clone(),
            text,
            level,
        });
        Ok(Value::Null)
    }

    /// `host.routes.register`: the whole set at once (registering again replaces). Every
    /// descriptor is stamped with this module's id — one that names another module is
    /// refused, as is one without a capability (the control server would refuse it
    /// later, silently from the module's point of view) or with a scope other than
    /// `token` (a module cannot open a route to the world or restrict one to the
    /// master token). The batch is validated as a table here so the module gets an
    /// `InvalidParams` naming the route; cross-module collisions are the registry's
    /// call and are logged by the bridge.
    fn routes_register(&self, params: &Value) -> Result<Value, RpcError> {
        let RegisterRoutes { mut routes } =
            serde_json::from_value(params.clone()).map_err(invalid_params)?;
        for r in &mut routes {
            if let Some(other) = &r.module {
                if other != &self.module {
                    return Err(invalid_params(format!(
                        "route `{}` names module `{other}`; this module is `{}`",
                        r.method, self.module
                    )));
                }
            }
            r.module = Some(self.module.clone());
            if r.capability.is_none() {
                return Err(invalid_params(format!(
                    "route `{}` names no capability; every module route must",
                    r.method
                )));
            }
            if r.scope != Scope::Token {
                return Err(invalid_params(format!(
                    "route `{}` asks for scope `{}`; module routes are token-scoped",
                    r.method,
                    serde_json::to_value(r.scope)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_string))
                        .unwrap_or_default()
                )));
            }
        }
        validate_table(&routes).map_err(invalid_params)?;
        *self.routes.lock().unwrap_or_else(|e| e.into_inner()) = routes.clone();
        self.shared.events.send(HostEvent::Routes {
            module: self.module.clone(),
            routes,
        });
        Ok(Value::Null)
    }

    /// Resolve a `host.fs.*` path.
    ///
    /// The scope is the workspace root, canonicalised on both sides so a `..` segment or
    /// a symlink pointing out of the tree is caught by the same `starts_with` — checking
    /// the spelling of the path a module sent would only catch the honest mistakes.
    /// `fs.read_any` lifts the scope entirely; that is what the capability *is*, and the
    /// human granted it at install time.
    fn scoped(&self, path: &str) -> Result<PathBuf, RpcError> {
        let root = match self.scope_root(path, Capability::FsReadAny, "read")? {
            Scoped::Anywhere => return Ok(PathBuf::from(path)),
            Scoped::Within(root) => root,
        };
        let real = PathBuf::from(path)
            .canonicalize()
            .map_err(|e| invalid_params(format!("`{path}`: {e}")))?;
        if !real.starts_with(&root) {
            return Err(RpcError::new(
                ErrorCode::CapabilityDenied,
                format!("`{path}` is outside the workspace root; `fs.read_any` reads outside it"),
            ));
        }
        Ok(real)
    }

    /// Resolve a `host.fs.write` path, which — unlike a read — is usually a file that does
    /// not exist yet.
    ///
    /// `canonicalize` cannot answer for a path with no inode, so the scope check runs
    /// against the **deepest ancestor that does exist** and the tail is rejoined
    /// afterwards. That is not a weaker check: every symlink on the way to the leaf is
    /// resolved by canonicalising that ancestor, so a link out of the tree is caught
    /// exactly as it is on a read, and the unresolved tail may only be plain names — a
    /// `..` after the existing prefix is refused rather than normalised, because
    /// normalising it here is how a scope check gets talked out of its own answer.
    fn scoped_write(&self, path: &str) -> Result<PathBuf, RpcError> {
        let root = match self.scope_root(path, Capability::FsWriteAny, "write")? {
            Scoped::Anywhere => return Ok(PathBuf::from(path)),
            Scoped::Within(root) => root,
        };
        let raw = PathBuf::from(path);
        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        let mut probe = raw.as_path();
        let real = loop {
            match probe.canonicalize() {
                Ok(real) => break real,
                Err(e) => {
                    let (Some(parent), Some(name)) = (probe.parent(), probe.file_name()) else {
                        return Err(invalid_params(format!("`{path}`: {e}")));
                    };
                    if !matches!(probe.components().next_back(), Some(Component::Normal(_))) {
                        return Err(invalid_params(format!(
                            "`{path}` names no file the host can create"
                        )));
                    }
                    tail.push(name.to_os_string());
                    probe = parent;
                }
            }
        };
        if !real.starts_with(&root) {
            return Err(RpcError::new(
                ErrorCode::CapabilityDenied,
                format!("`{path}` is outside the workspace root; `fs.write_any` writes outside it"),
            ));
        }
        let mut out = real;
        for name in tail.into_iter().rev() {
            out.push(name);
        }
        Ok(out)
    }

    /// The shared first half of both scope checks: the "any" capability short-circuits it,
    /// otherwise the active workspace root is the boundary. `verb` only shapes the message
    /// a human reads when there is no workspace open at all.
    fn scope_root(&self, path: &str, any: Capability, verb: &str) -> Result<Scoped, RpcError> {
        if path.trim().is_empty() {
            return Err(invalid_params("path must not be empty"));
        }
        if matches!(self.shared.gate.check(&self.module, any), Decision::Allow) {
            return Ok(Scoped::Anywhere);
        }
        let root = self
            .shared
            .workspace_root
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(root) = root else {
            return Err(RpcError::new(
                ErrorCode::CapabilityDenied,
                format!(
                    "no workspace root to {verb} within; `{}` is needed to {verb} outside one",
                    any.name()
                ),
            ));
        };
        root.canonicalize().map(Scoped::Within).map_err(|e| {
            RpcError::new(
                ErrorCode::CapabilityDenied,
                format!("the workspace root cannot be resolved: {e}"),
            )
        })
    }

    /// `host.fs.list { path } -> { entries: [{ name, kind }] }`, sorted by name.
    ///
    /// Hidden entries are listed. Whether a dotfile belongs on screen is the module's
    /// question — an explorer that hid them would be lying about what is on disk, and a
    /// host that hid them would take that decision away from every module at once.
    fn fs_list(&self, params: &Value) -> Result<Value, RpcError> {
        let PathParams { path } = serde_json::from_value(params.clone()).map_err(invalid_params)?;
        let dir = self.scoped(&path)?;
        let rd = std::fs::read_dir(&dir).map_err(|e| invalid_params(format!("`{path}`: {e}")))?;
        let mut entries: Vec<(String, &'static str)> = Vec::new();
        for ent in rd.flatten() {
            // `file_type` does not follow the link: a symlink is its own kind, so the
            // module can decide whether to descend rather than being walked into a loop.
            let kind = match ent.file_type() {
                Ok(t) if t.is_symlink() => "symlink",
                Ok(t) if t.is_dir() => "dir",
                Ok(t) if t.is_file() => "file",
                _ => "other",
            };
            entries.push((ent.file_name().to_string_lossy().into_owned(), kind));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let entries: Vec<Value> = entries
            .into_iter()
            .map(|(name, kind)| json!({ "name": name, "kind": kind }))
            .collect();
        Ok(json!({ "entries": entries }))
    }

    /// `host.fs.read { path } -> { text } | { bytes_b64 }`.
    ///
    /// Text when the bytes are UTF-8, base64 when they are not, so a module that only
    /// wanted to show source never has to think about encodings and one that wanted an
    /// image still gets the bytes.
    fn fs_read(&self, params: &Value) -> Result<Value, RpcError> {
        let PathParams { path } = serde_json::from_value(params.clone()).map_err(invalid_params)?;
        let file = self.scoped(&path)?;
        let len = std::fs::metadata(&file)
            .map_err(|e| invalid_params(format!("`{path}`: {e}")))?
            .len();
        if len > MAX_READ {
            return Err(invalid_params(format!(
                "`{path}` is {len} bytes; `host.fs.read` stops at {MAX_READ}"
            )));
        }
        let bytes = std::fs::read(&file).map_err(|e| invalid_params(format!("`{path}`: {e}")))?;
        match String::from_utf8(bytes) {
            Ok(text) => Ok(json!({ "text": text })),
            Err(e) => Ok(json!({
                "bytes_b64": base64::engine::general_purpose::STANDARD.encode(e.as_bytes()),
            })),
        }
    }

    /// `host.fs.write { path, text } -> {}`.
    ///
    /// Text only, and whole-file only. A module that has to say where in a file its bytes
    /// go would need the host to arbitrate concurrent edits to a file the human may also
    /// have open; replacing the file is the operation whose meaning does not depend on
    /// what happened between the read and the write.
    ///
    /// Missing parent directories are created, because the file a module most wants to
    /// write is the first one in a directory that does not exist yet (`.avada/` in a fresh
    /// checkout) and refusing that would only push a `mkdir` method onto the contract. They
    /// are created *after* the scope check, never before it.
    fn fs_write(&self, params: &Value) -> Result<Value, RpcError> {
        let WriteParams { path, text } =
            serde_json::from_value(params.clone()).map_err(invalid_params)?;
        if text.len() > MAX_WRITE {
            return Err(invalid_params(format!(
                "`{path}` is {} bytes; `host.fs.write` stops at {MAX_WRITE}",
                text.len()
            )));
        }
        let file = self.scoped_write(&path)?;
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| invalid_params(format!("`{path}`: {e}")))?;
        }
        std::fs::write(&file, text.as_bytes())
            .map_err(|e| invalid_params(format!("`{path}`: {e}")))?;
        Ok(json!({}))
    }

    /// `host.panes.spawn { kind, path?, surface? } -> { pane_id }`.
    ///
    /// The id is minted here and returned at once; opening the pane is the app's job and
    /// happens off the event stream. A module that had to wait for a window to exist
    /// would block its own request loop on the UI thread's next frame.
    fn panes_spawn(&self, params: &Value) -> Result<Value, RpcError> {
        let SpawnPane {
            kind,
            path,
            surface,
        } = serde_json::from_value(params.clone()).map_err(invalid_params)?;
        if kind.trim().is_empty() {
            return Err(invalid_params("pane kind must not be empty"));
        }
        let pane_id = uuid::Uuid::new_v4().to_string();
        self.shared.events.send(HostEvent::PaneSpawn {
            module: self.module.clone(),
            pane_id: pane_id.clone(),
            kind,
            path,
            surface,
        });
        Ok(json!({ "pane_id": pane_id }))
    }

    /// `host.panes.input` `{ pane_id, text }` -> `{}`.
    ///
    /// Typing into a pane the module opened, which is the whole point of a shell-tier
    /// module: it spawns a terminal and then drives it. Like `panes.spawn` this returns
    /// the moment the intent is on the event stream — the pane lives on the UI thread and
    /// a module that waited for the keystroke to land would block its own request loop.
    ///
    /// An unknown `pane_id` is not an error here. The host does not own the pane table (the
    /// app does), and answering "no such pane" would mean a synchronous round trip to the
    /// UI thread for every keystroke; the app drops input for a pane it has closed.
    /// An *empty* id is refused, because that is a module bug rather than a race.
    fn panes_input(&self, params: &Value) -> Result<Value, RpcError> {
        let PaneInput { pane_id, text } =
            serde_json::from_value(params.clone()).map_err(invalid_params)?;
        if pane_id.trim().is_empty() {
            return Err(invalid_params("pane_id must not be empty"));
        }
        self.shared.events.send(HostEvent::PaneInput {
            module: self.module.clone(),
            pane_id,
            text,
        });
        Ok(json!({}))
    }

    /// `host.events.subscribe { kinds }`: the whole set, replacing any earlier one. An
    /// empty set unsubscribes.
    fn events_subscribe(&self, params: &Value) -> Result<Value, RpcError> {
        let Subscribe { kinds } = serde_json::from_value(params.clone()).map_err(invalid_params)?;
        if let Some(bad) = kinds.iter().find(|k| k.trim().is_empty()) {
            let _ = bad;
            return Err(invalid_params("event kind must not be empty"));
        }
        *self.subscriptions.lock().unwrap_or_else(|e| e.into_inner()) = kinds.into_iter().collect();
        Ok(Value::Null)
    }

    /// Whether the module asked for `kind`; consulted by [`super::host::Host::emit`].
    pub(crate) fn subscribed(&self, kind: &str) -> bool {
        self.subscriptions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(kind)
    }

    /// Replace the stored values (the host's side of "prefs set"), persist them, and
    /// hand back the `module.prefs.changed` params to send.
    pub(crate) fn set_prefs(&self, values: Map<String, Value>) -> std::io::Result<Value> {
        let mut prefs = self.lock_prefs();
        prefs.values = values;
        prefs.save()?;
        Ok(prefs.as_result())
    }

    /// Everything the module registered, for the placeholder and the palette.
    pub(crate) fn commands(&self) -> Vec<CommandSpec> {
        self.commands
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The control-plane routes the module registered, stamped with its id.
    pub(crate) fn routes(&self) -> Vec<RouteDescriptor> {
        self.routes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::module::gate::DeclaredOnly;
    use crate::module::testkit;
    use avada_module_sdk::caps::Capability;
    use avada_module_sdk::contract::methods::events as contract_events;
    use std::sync::mpsc::Receiver;

    pub(crate) struct Rig {
        pub d: Dispatcher,
        pub rail: Receiver<RailEvent>,
        pub events: Receiver<HostEvent>,
        _dir: tempdir::Dir,
    }

    pub(crate) mod tempdir {
        pub struct Dir(pub std::path::PathBuf);
        impl Dir {
            pub fn new(tag: &str) -> Dir {
                let p = std::env::temp_dir().join(format!(
                    "avada-module-{tag}-{}-{}",
                    std::process::id(),
                    crate::module::token::Token::mint()
                        .expose()
                        .get(..8)
                        .unwrap_or("x")
                ));
                std::fs::create_dir_all(&p).unwrap();
                Dir(p)
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    pub(crate) fn rig(caps: &[Capability]) -> Rig {
        let record = testkit::record(caps);
        let shared = Arc::new(Shared {
            gate: Arc::new(DeclaredOnly::from_record(&record)),
            rail_events: FanOut::default(),
            events: FanOut::default(),
            workspace_root: Mutex::new(None),
        });
        let rail = shared.rail_events.subscribe();
        let events = shared.events.subscribe();
        let dir = tempdir::Dir::new("rpc");
        let d = Dispatcher::new(record.module_id.clone(), shared, &dir.0);
        Rig {
            d,
            rail,
            events,
            _dir: dir,
        }
    }

    fn all_caps() -> Vec<Capability> {
        methods::HOST_REQUIRED_V1
            .iter()
            .filter_map(|m| required_capability(m))
            .collect()
    }

    #[test]
    fn every_contract_method_is_dispatched() {
        let rig = rig(&all_caps());
        for m in methods::HOST_REQUIRED_V1 {
            let r = rig.d.call(m, &Value::Null);
            let unknown = matches!(&r, Err(e) if e.message.starts_with("unknown method"));
            assert!(!unknown, "{m} fell through to the unknown-method arm");
        }
        assert!(matches!(
            rig.d.call("host.nope", &Value::Null),
            Err(e) if e.kind() == ErrorCode::MethodNotFound && e.data.is_none()
        ));
    }

    #[test]
    fn served_methods_are_a_subset_of_the_contract() {
        for m in SERVED {
            assert!(
                methods::HOST_REQUIRED_V1.contains(m),
                "{m} is not in the contract"
            );
        }
    }

    #[test]
    fn unsupported_methods_say_so_after_the_gate() {
        let rig = rig(&all_caps());
        for m in [methods::HOST_KEYCHAIN_GET, methods::HOST_KEYCHAIN_SET] {
            let e = rig.d.call(m, &Value::Null).unwrap_err();
            assert_eq!(e.kind(), ErrorCode::MethodNotFound, "{m}");
            assert_eq!(e.data, Some(json!({ "unsupported": true })), "{m}");
        }
        // Without the capability the gate answers first.
        let bare = super::tests::rig(&[]);
        let e = bare
            .d
            .call(methods::HOST_PANES_INPUT, &Value::Null)
            .unwrap_err();
        assert_eq!(e.kind(), ErrorCode::CapabilityDenied);
    }

    #[test]
    fn rail_register_validates_stamps_module_and_emits() {
        let rig = rig(&[Capability::UiRail]);
        let params = json!({ "entries": [
            { "id": "files", "label": "Files", "tier": 1 },
        ]});
        rig.d.call(methods::HOST_RAIL_REGISTER, &params).unwrap();
        match rig.rail.recv().unwrap() {
            RailEvent::Registered { module, entries } => {
                assert_eq!(module, testkit::module_id());
                assert_eq!(entries[0].module.as_ref(), Some(&testkit::module_id()));
            }
            other => panic!("{other:?}"),
        }
        // Bad id → InvalidParams, nothing emitted.
        let bad = json!({ "entries": [{ "id": "Files", "label": "x", "tier": 1 }] });
        let e = rig.d.call(methods::HOST_RAIL_REGISTER, &bad).unwrap_err();
        assert_eq!(e.kind(), ErrorCode::InvalidParams);
        // Claiming another module's id is refused.
        let theft = json!({ "entries": [{ "id": "files", "label": "x", "tier": 1, "module": "other/mod" }] });
        assert_eq!(
            rig.d
                .call(methods::HOST_RAIL_REGISTER, &theft)
                .unwrap_err()
                .kind(),
            ErrorCode::InvalidParams
        );
        let dup = json!({ "entries": [
            { "id": "files", "label": "x", "tier": 1 },
            { "id": "files", "label": "y", "tier": 1 },
        ]});
        assert_eq!(
            rig.d
                .call(methods::HOST_RAIL_REGISTER, &dup)
                .unwrap_err()
                .kind(),
            ErrorCode::InvalidParams
        );
        assert!(rig.rail.try_recv().is_err());
    }

    #[test]
    fn rows_set_needs_a_registered_entry() {
        let rig = rig(&[Capability::UiRail]);
        let rows = json!({ "entry": "files", "rows": [{ "id": "a", "label": "A" }] });
        let e = rig.d.call(methods::HOST_ROWS_SET, &rows).unwrap_err();
        assert_eq!(e.kind(), ErrorCode::InvalidParams);
        rig.d
            .call(
                methods::HOST_RAIL_REGISTER,
                &json!({ "entries": [{ "id": "files", "label": "Files", "tier": 1 }] }),
            )
            .unwrap();
        let _ = rig.rail.recv().unwrap();
        rig.d.call(methods::HOST_ROWS_SET, &rows).unwrap();
        match rig.rail.recv().unwrap() {
            RailEvent::Rows { entry, rows, .. } => {
                assert_eq!(entry, "files");
                assert_eq!(rows[0].id, "a");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn missing_capability_is_refused_with_the_contract_code() {
        let rig = rig(&[Capability::UiToast]);
        let e = rig
            .d
            .call(methods::HOST_RAIL_REGISTER, &json!({ "entries": [] }))
            .unwrap_err();
        assert_eq!(e.kind(), ErrorCode::CapabilityDenied);
        assert_eq!(e.code, -32001);
        assert!(rig.rail.try_recv().is_err());
    }

    #[test]
    fn toast_and_commands_reach_the_event_stream() {
        let rig = rig(&[Capability::UiToast, Capability::UiCommands]);
        rig.d
            .call(methods::HOST_TOAST, &json!({ "text": "hello" }))
            .unwrap();
        assert!(matches!(
            rig.events.recv().unwrap(),
            HostEvent::Toast { text, level, .. } if text == "hello" && level == "info"
        ));
        assert_eq!(
            rig.d
                .call(methods::HOST_TOAST, &json!({ "text": "  " }))
                .unwrap_err()
                .kind(),
            ErrorCode::InvalidParams
        );
        rig.d
            .call(
                methods::HOST_COMMAND_REGISTER,
                &json!({ "commands": [{ "id": "reveal", "label": "Reveal" }] }),
            )
            .unwrap();
        assert!(matches!(
            rig.events.recv().unwrap(),
            HostEvent::Commands { commands, .. } if commands[0].id == "reveal"
        ));
        assert_eq!(rig.d.commands().len(), 1);
    }

    fn route(method: &str, path: &str) -> Value {
        let params: Vec<Value> = path
            .split('/')
            .filter_map(|seg| seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')))
            .map(|name| json!({ "name": name, "location": "path", "kind": "string", "required": true, "summary": "" }))
            .collect();
        json!({
            "method": method,
            "path": path,
            "verb": "GET",
            "capability": "workspace.read",
            "summary": "",
            "params": params,
            "scope": "token",
        })
    }

    #[test]
    fn routes_register_stamps_the_module_and_reaches_the_event_stream() {
        let rig = rig(&[Capability::ControlRoute]);
        rig.d
            .call(
                methods::HOST_ROUTES_REGISTER,
                &json!({ "routes": [route("acme.tree", "/tree"), route("acme.open", "/files/{id}")] }),
            )
            .unwrap();
        let routes = match rig.events.recv().unwrap() {
            HostEvent::Routes { module, routes } => {
                assert_eq!(module.as_str(), "acme/avada-files");
                routes
            }
            other => panic!("{other:?}"),
        };
        assert_eq!(routes.len(), 2);
        assert!(
            routes
                .iter()
                .all(|r| r.module.as_ref().map(|m| m.as_str()) == Some("acme/avada-files")),
            "the host fills in the owner"
        );
        assert_eq!(routes[1].mounted_path(), "/m/acme/avada-files/files/{id}");
        assert_eq!(rig.d.routes(), routes);
        // Registering again replaces the set; an empty set is a valid way to withdraw.
        rig.d
            .call(methods::HOST_ROUTES_REGISTER, &json!({ "routes": [] }))
            .unwrap();
        assert!(matches!(
            rig.events.recv().unwrap(),
            HostEvent::Routes { routes, .. } if routes.is_empty()
        ));
        assert!(rig.d.routes().is_empty());
    }

    #[test]
    fn routes_register_refuses_what_the_control_server_would_and_says_which_route() {
        let rig = rig(&[Capability::ControlRoute]);
        let refused = |routes: Value| -> String {
            let e = rig
                .d
                .call(methods::HOST_ROUTES_REGISTER, &json!({ "routes": routes }))
                .unwrap_err();
            assert_eq!(e.kind(), ErrorCode::InvalidParams);
            e.message
        };
        // Another module's id.
        let mut foreign = route("acme.tree", "/tree");
        foreign["module"] = json!("other/widget");
        let msg = refused(json!([foreign]));
        assert!(
            msg.contains("acme.tree") && msg.contains("other/widget"),
            "{msg}"
        );
        // Its own id spelled out is fine.
        let mut own = route("acme.tree", "/tree");
        own["module"] = json!("acme/avada-files");
        rig.d
            .call(methods::HOST_ROUTES_REGISTER, &json!({ "routes": [own] }))
            .unwrap();
        rig.events.recv().unwrap();
        // No capability.
        let mut bare = route("acme.tree", "/tree");
        bare["capability"] = Value::Null;
        let msg = refused(json!([bare]));
        assert!(msg.contains("no capability"), "{msg}");
        // Master or public scope.
        let mut master = route("acme.tree", "/tree");
        master["scope"] = json!("master");
        let msg = refused(json!([master]));
        assert!(
            msg.contains("`master`") && msg.contains("token-scoped"),
            "{msg}"
        );
        // A path capture that is not declared as a param, and a duplicate method.
        let mut undeclared = route("acme.open", "/files/{id}");
        undeclared["params"] = json!([]);
        refused(json!([undeclared]));
        refused(json!([
            route("acme.tree", "/tree"),
            route("acme.tree", "/other")
        ]));
        // Nothing refused reached the event stream, and the last good set stands.
        assert!(rig.events.try_recv().is_err());
        assert_eq!(rig.d.routes().len(), 1);
        // Without `control.route` the gate answers first.
        let bare = super::tests::rig(&[]);
        let e = bare
            .d
            .call(methods::HOST_ROUTES_REGISTER, &json!({ "routes": [] }))
            .unwrap_err();
        assert_eq!(e.kind(), ErrorCode::CapabilityDenied);
    }

    #[test]
    fn prefs_declare_get_and_host_set_persist() {
        let rig = rig(&[Capability::UiPrefs]);
        rig.d
            .call(
                methods::HOST_PREFS_DECLARE,
                &json!({ "page": { "title": "Files" } }),
            )
            .unwrap();
        assert!(matches!(
            rig.events.recv().unwrap(),
            HostEvent::PrefsDeclared { .. }
        ));
        assert_eq!(
            rig.d.call(methods::HOST_PREFS_GET, &Value::Null).unwrap(),
            json!({ "values": {} })
        );
        let mut values = Map::new();
        values.insert("depth".into(), json!(3));
        let changed = rig.d.set_prefs(values).unwrap();
        assert_eq!(changed, json!({ "values": { "depth": 3 } }));
        // A fresh load from the same directory sees the persisted value.
        let again = Prefs::load(rig._dir.0.as_path());
        assert_eq!(again.values.get("depth"), Some(&json!(3)));
    }

    #[test]
    fn handle_wraps_ok_and_err_into_responses() {
        let rig = rig(&[Capability::UiToast]);
        let ok = rig.d.handle(&Request::new(
            7,
            methods::HOST_TOAST,
            json!({ "text": "x" }),
        ));
        assert_eq!(ok.result, Some(Value::Null));
        assert!(ok.error.is_none());
        let err = rig
            .d
            .handle(&Request::new(8, methods::HOST_RAIL_REGISTER, Value::Null));
        assert_eq!(err.error.unwrap().kind(), ErrorCode::CapabilityDenied);
    }

    /// A rig whose `host.fs.*` scope is `dir`.
    fn fs_rig(caps: &[Capability], dir: &Path) -> Rig {
        let rig = rig(caps);
        *rig.d.shared.workspace_root.lock().unwrap() = Some(dir.to_path_buf());
        rig
    }

    fn list_names(v: &Value) -> Vec<String> {
        v["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn fs_list_sorts_by_name_keeps_hidden_entries_and_names_the_kind() {
        let dir = tempdir::Dir::new("fs-list");
        let root = &dir.0;
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(root.join("b.txt"), "b").unwrap();
        std::fs::write(root.join(".hidden"), "h").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("b.txt"), root.join("link")).unwrap();
        let rig = fs_rig(&[Capability::FsRead], root);
        let v = rig
            .d
            .call(
                methods::HOST_FS_LIST,
                &json!({ "path": root.to_string_lossy() }),
            )
            .unwrap();
        let names = list_names(&v);
        assert!(
            names.contains(&".hidden".to_string()),
            "hidden entries are the module's call, not the host's: {names:?}"
        );
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "entries come back sorted by name");
        let kind = |n: &str| {
            v["entries"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["name"] == n)
                .map(|e| e["kind"].as_str().unwrap().to_string())
                .unwrap()
        };
        assert_eq!(kind("src"), "dir");
        assert_eq!(kind("b.txt"), "file");
        #[cfg(unix)]
        assert_eq!(
            kind("link"),
            "symlink",
            "a link is its own kind, not the thing it points at"
        );
    }

    #[test]
    fn fs_read_is_scoped_to_the_workspace_root() {
        let dir = tempdir::Dir::new("fs-scope");
        let root = &dir.0;
        let outside = tempdir::Dir::new("fs-outside");
        std::fs::write(root.join("in.txt"), "inside").unwrap();
        std::fs::write(outside.0.join("out.txt"), "outside").unwrap();
        let rig = fs_rig(&[Capability::FsRead], root);
        let read = |p: PathBuf| {
            rig.d.call(
                methods::HOST_FS_READ,
                &json!({ "path": p.to_string_lossy() }),
            )
        };
        assert_eq!(
            read(root.join("in.txt")).unwrap(),
            json!({ "text": "inside" })
        );
        let e = read(outside.0.join("out.txt")).unwrap_err();
        assert_eq!(e.kind(), ErrorCode::CapabilityDenied);
        assert!(e.message.contains("fs.read_any"), "{}", e.message);
        // `..` back out of the root is the same refusal, not a path-spelling check.
        let e = read(
            root.join("..")
                .join(outside.0.file_name().unwrap())
                .join("out.txt"),
        )
        .unwrap_err();
        assert_eq!(e.kind(), ErrorCode::CapabilityDenied);
    }

    #[test]
    fn fs_write_creates_the_file_and_the_directories_above_it() {
        let dir = tempdir::Dir::new("fs-write");
        let root = &dir.0;
        let rig = fs_rig(&[Capability::FsWrite, Capability::FsRead], root);
        let target = root.join(".avada").join("project.json");
        assert_eq!(
            rig.d
                .call(
                    methods::HOST_FS_WRITE,
                    &json!({ "path": target.to_string_lossy(), "text": "{}\n" }),
                )
                .unwrap(),
            json!({}),
            "a write answers with an empty object, not a byte count"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "{}\n");
        // The module can read back exactly what it wrote — the point of MAX_WRITE == MAX_READ.
        assert_eq!(
            rig.d
                .call(
                    methods::HOST_FS_READ,
                    &json!({ "path": target.to_string_lossy() }),
                )
                .unwrap(),
            json!({ "text": "{}\n" })
        );
        // A second write replaces rather than appends.
        rig.d
            .call(
                methods::HOST_FS_WRITE,
                &json!({ "path": target.to_string_lossy(), "text": "second" }),
            )
            .unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "second");
    }

    #[test]
    fn fs_write_is_scoped_and_write_any_is_what_lifts_it() {
        let dir = tempdir::Dir::new("fs-write-scope");
        let outside = tempdir::Dir::new("fs-write-out");
        let root = &dir.0;
        let target = outside.0.join("new.txt");
        let params = json!({ "path": target.to_string_lossy(), "text": "x" });

        let rig = fs_rig(&[Capability::FsWrite], root);
        let e = rig.d.call(methods::HOST_FS_WRITE, &params).unwrap_err();
        assert_eq!(e.kind(), ErrorCode::CapabilityDenied);
        assert!(
            e.message.contains("fs.write_any"),
            "the refusal names the WRITE escape hatch, not the read one: {}",
            e.message
        );
        assert!(!target.exists(), "nothing was written");

        // `fs.read_any` must NOT lift a write scope.
        let reader = fs_rig(&[Capability::FsWrite, Capability::FsReadAny], root);
        assert_eq!(
            reader
                .d
                .call(methods::HOST_FS_WRITE, &params)
                .unwrap_err()
                .kind(),
            ErrorCode::CapabilityDenied
        );

        let any = fs_rig(&[Capability::FsWrite, Capability::FsWriteAny], root);
        any.d.call(methods::HOST_FS_WRITE, &params).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "x");
    }

    #[test]
    fn fs_write_refuses_a_tail_that_climbs_out_and_a_body_over_the_cap() {
        let dir = tempdir::Dir::new("fs-write-tail");
        let root = &dir.0;
        let rig = fs_rig(&[Capability::FsWrite], root);
        // `<root>/nope/../../escape`: the existing prefix is the root, and the unresolved
        // tail is refused rather than normalised.
        let sneaky = root.join("nope").join("..").join("..").join("escape");
        let e = rig
            .d
            .call(
                methods::HOST_FS_WRITE,
                &json!({ "path": sneaky.to_string_lossy(), "text": "x" }),
            )
            .unwrap_err();
        assert!(
            matches!(
                e.kind(),
                ErrorCode::InvalidParams | ErrorCode::CapabilityDenied
            ),
            "{e:?}"
        );
        assert!(
            !root.parent().unwrap().join("escape").exists(),
            "nothing landed above the root"
        );

        let e = rig
            .d
            .call(
                methods::HOST_FS_WRITE,
                &json!({
                    "path": root.join("big").to_string_lossy(),
                    "text": "a".repeat(MAX_WRITE + 1),
                }),
            )
            .unwrap_err();
        assert_eq!(e.kind(), ErrorCode::InvalidParams);
        assert!(e.message.contains("stops at"), "{}", e.message);
        assert!(
            !root.join("big").exists(),
            "the cap is checked before the write"
        );
    }

    #[cfg(unix)]
    #[test]
    fn fs_write_will_not_follow_a_symlink_out_of_the_workspace() {
        let dir = tempdir::Dir::new("fs-write-link");
        let outside = tempdir::Dir::new("fs-write-link-out");
        std::os::unix::fs::symlink(&outside.0, dir.0.join("escape")).unwrap();
        let rig = fs_rig(&[Capability::FsWrite], &dir.0);
        let e = rig
            .d
            .call(
                methods::HOST_FS_WRITE,
                &json!({
                    "path": dir.0.join("escape").join("stolen").to_string_lossy(),
                    "text": "x",
                }),
            )
            .unwrap_err();
        assert_eq!(
            e.kind(),
            ErrorCode::CapabilityDenied,
            "the existing prefix canonicalises through the link, so it lands outside"
        );
        assert!(!outside.0.join("stolen").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_workspace_is_refused_where_it_points() {
        let dir = tempdir::Dir::new("fs-link");
        let outside = tempdir::Dir::new("fs-link-out");
        std::fs::write(outside.0.join("secret"), "s").unwrap();
        std::os::unix::fs::symlink(outside.0.join("secret"), dir.0.join("escape")).unwrap();
        let rig = fs_rig(&[Capability::FsRead], &dir.0);
        let e = rig
            .d
            .call(
                methods::HOST_FS_READ,
                &json!({ "path": dir.0.join("escape").to_string_lossy() }),
            )
            .unwrap_err();
        assert_eq!(
            e.kind(),
            ErrorCode::CapabilityDenied,
            "both sides are canonicalised, so the link resolves outside the root"
        );
    }

    #[test]
    fn without_a_root_fs_read_is_denied_and_read_any_lifts_the_scope() {
        let outside = tempdir::Dir::new("fs-noroot");
        std::fs::write(outside.0.join("f"), "x").unwrap();
        let path = json!({ "path": outside.0.join("f").to_string_lossy() });
        // No workspace root at all.
        let bare = rig(&[Capability::FsRead]);
        let e = bare.d.call(methods::HOST_FS_READ, &path).unwrap_err();
        assert_eq!(e.kind(), ErrorCode::CapabilityDenied);
        assert!(e.message.contains("no workspace root"), "{}", e.message);
        // `fs.read_any` reads anywhere, root or no root.
        let any = rig(&[Capability::FsRead, Capability::FsReadAny]);
        assert_eq!(
            any.d.call(methods::HOST_FS_READ, &path).unwrap(),
            json!({ "text": "x" })
        );
    }

    #[test]
    fn fs_read_caps_the_size_and_base64s_bytes_that_are_not_text() {
        let dir = tempdir::Dir::new("fs-bytes");
        std::fs::write(dir.0.join("bin"), [0xff, 0xfe, 0x00]).unwrap();
        std::fs::write(dir.0.join("big"), vec![b'a'; (MAX_READ + 1) as usize]).unwrap();
        let rig = fs_rig(&[Capability::FsRead], &dir.0);
        let v = rig
            .d
            .call(
                methods::HOST_FS_READ,
                &json!({ "path": dir.0.join("bin").to_string_lossy() }),
            )
            .unwrap();
        assert!(v.get("text").is_none());
        assert_eq!(v["bytes_b64"], json!("//4A"));
        let e = rig
            .d
            .call(
                methods::HOST_FS_READ,
                &json!({ "path": dir.0.join("big").to_string_lossy() }),
            )
            .unwrap_err();
        assert_eq!(e.kind(), ErrorCode::InvalidParams);
        assert!(e.message.contains("stops at"), "{}", e.message);
    }

    #[test]
    fn panes_spawn_mints_an_id_answers_it_and_announces_the_pane() {
        let rig = rig(&[Capability::PanesSpawn]);
        let v = rig
            .d
            .call(
                methods::HOST_PANES_SPAWN,
                &json!({ "kind": "file", "path": "/w/a.rs" }),
            )
            .unwrap();
        let id = v["pane_id"].as_str().unwrap().to_string();
        assert_eq!(id.len(), 36, "a uuid, not a counter: {id}");
        match rig.events.recv().unwrap() {
            HostEvent::PaneSpawn {
                module,
                pane_id,
                kind,
                path,
                surface,
            } => {
                assert_eq!(module, testkit::module_id());
                assert_eq!(
                    pane_id, id,
                    "the app opens the pane the module was told about"
                );
                assert_eq!(kind, "file");
                assert_eq!(path.as_deref(), Some("/w/a.rs"));
                assert_eq!(surface, None);
            }
            other => panic!("{other:?}"),
        }
        // An empty kind is refused before anything is announced.
        assert_eq!(
            rig.d
                .call(methods::HOST_PANES_SPAWN, &json!({ "kind": " " }))
                .unwrap_err()
                .kind(),
            ErrorCode::InvalidParams
        );
        assert!(rig.events.try_recv().is_err());
        // Without `panes.spawn` the gate answers first.
        assert_eq!(
            super::tests::rig(&[])
                .d
                .call(methods::HOST_PANES_SPAWN, &json!({ "kind": "file" }))
                .unwrap_err()
                .kind(),
            ErrorCode::CapabilityDenied
        );
    }

    #[test]
    fn panes_input_announces_the_keystrokes_and_needs_its_own_capability() {
        let rig = rig(&[Capability::PanesInput]);
        assert_eq!(
            rig.d
                .call(
                    methods::HOST_PANES_INPUT,
                    &json!({ "pane_id": "p-1", "text": "ls\r" })
                )
                .unwrap(),
            json!({})
        );
        match rig.events.recv().unwrap() {
            HostEvent::PaneInput {
                module,
                pane_id,
                text,
            } => {
                assert_eq!(module, testkit::module_id());
                assert_eq!(pane_id, "p-1");
                assert_eq!(text, "ls\r");
            }
            other => panic!("{other:?}"),
        }
        // An empty id is a module bug and is refused before anything is announced; an
        // unknown one is a race with a closed pane and is not.
        assert_eq!(
            rig.d
                .call(
                    methods::HOST_PANES_INPUT,
                    &json!({ "pane_id": " ", "text": "x" })
                )
                .unwrap_err()
                .kind(),
            ErrorCode::InvalidParams
        );
        assert!(rig.events.try_recv().is_err());
        // `panes.spawn` does not imply `panes.input`: spawning a pane and typing into one
        // are separately granted.
        assert_eq!(
            super::tests::rig(&[Capability::PanesSpawn])
                .d
                .call(
                    methods::HOST_PANES_INPUT,
                    &json!({ "pane_id": "p-1", "text": "x" })
                )
                .unwrap_err()
                .kind(),
            ErrorCode::CapabilityDenied
        );
    }

    #[test]
    fn events_subscribe_records_the_whole_set_and_replaces_it() {
        let rig = rig(&[Capability::EventsSubscribe]);
        assert!(!rig.d.subscribed(contract_events::RAIL_QUERY));
        rig.d
            .call(
                methods::HOST_EVENTS_SUBSCRIBE,
                &json!({ "kinds": [contract_events::RAIL_QUERY, contract_events::FILES_REVEAL] }),
            )
            .unwrap();
        assert!(rig.d.subscribed(contract_events::RAIL_QUERY));
        assert!(rig.d.subscribed(contract_events::FILES_REVEAL));
        assert!(!rig.d.subscribed("something.else"));
        // Subscribing again replaces rather than adds.
        rig.d
            .call(
                methods::HOST_EVENTS_SUBSCRIBE,
                &json!({ "kinds": [contract_events::FILES_REVEAL] }),
            )
            .unwrap();
        assert!(!rig.d.subscribed(contract_events::RAIL_QUERY));
        // An empty set unsubscribes; an empty *name* is a mistake.
        rig.d
            .call(methods::HOST_EVENTS_SUBSCRIBE, &json!({ "kinds": [] }))
            .unwrap();
        assert!(!rig.d.subscribed(contract_events::FILES_REVEAL));
        assert_eq!(
            rig.d
                .call(methods::HOST_EVENTS_SUBSCRIBE, &json!({ "kinds": [" "] }))
                .unwrap_err()
                .kind(),
            ErrorCode::InvalidParams
        );
        // Without `events.subscribe` the gate answers first.
        assert_eq!(
            super::tests::rig(&[])
                .d
                .call(methods::HOST_EVENTS_SUBSCRIBE, &json!({ "kinds": [] }))
                .unwrap_err()
                .kind(),
            ErrorCode::CapabilityDenied
        );
    }
}
