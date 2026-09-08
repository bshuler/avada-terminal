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
use avada_module_sdk::contract::methods::{self, required_capability};
use avada_module_sdk::contract::{ErrorCode, Notification, Request, Response, RpcError};
use avada_module_sdk::rail::{validate_entry, RegisterRail, SetRows};
use avada_module_sdk::ModuleId;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The `host.*` methods this host serves; advertised in the host hello.
pub const SERVED: &[&str] = &[
    methods::HOST_RAIL_REGISTER,
    methods::HOST_ROWS_SET,
    methods::HOST_COMMAND_REGISTER,
    methods::HOST_PREFS_DECLARE,
    methods::HOST_PREFS_GET,
    methods::HOST_TOAST,
];

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
            methods::HOST_PANES_SPAWN
            | methods::HOST_PANES_INPUT
            | methods::HOST_FS_READ
            | methods::HOST_FS_WRITE
            | methods::HOST_FS_LIST
            | methods::HOST_EVENTS_SUBSCRIBE
            | methods::HOST_ROUTES_REGISTER
            | methods::HOST_KEYCHAIN_GET
            | methods::HOST_KEYCHAIN_SET => Err(unsupported(method)),
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
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::module::gate::DeclaredOnly;
    use crate::module::testkit;
    use avada_module_sdk::caps::Capability;
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
        for m in [
            methods::HOST_PANES_SPAWN,
            methods::HOST_PANES_INPUT,
            methods::HOST_FS_READ,
            methods::HOST_FS_WRITE,
            methods::HOST_FS_LIST,
            methods::HOST_EVENTS_SUBSCRIBE,
            methods::HOST_ROUTES_REGISTER,
            methods::HOST_KEYCHAIN_GET,
            methods::HOST_KEYCHAIN_SET,
        ] {
            let e = rig.d.call(m, &Value::Null).unwrap_err();
            assert_eq!(e.kind(), ErrorCode::MethodNotFound, "{m}");
            assert_eq!(e.data, Some(json!({ "unsupported": true })), "{m}");
        }
        // Without the capability the gate answers first.
        let bare = super::tests::rig(&[]);
        let e = bare
            .d
            .call(methods::HOST_FS_READ, &Value::Null)
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
}
