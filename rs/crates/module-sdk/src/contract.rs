//! The wire contract between host and module: a newline-delimited JSON-RPC 2.0
//! stream over the inherited socket (see [`crate::client`]).
//!
//! The first line each side writes is a hello. The module speaks first
//! ([`ModuleHello`]); the host answers ([`HostHello`]) with the negotiated contract
//! version or closes the socket. After the hellos, either side may send a
//! [`Request`] and must answer every request it receives with exactly one
//! [`Response`] carrying the same id. [`Notification`]s carry no id and get no answer.
//!
//! Method names are namespaced by who serves them: `host.*` methods are served by
//! the host and called by the module; `module.*` methods are served by the module.
//! Unknown methods answer with [`ErrorCode::MethodNotFound`].

use crate::manifest::Manifest;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

/// The contract version this SDK implements. Bumped only for incompatible changes to
/// the wire format or the meaning of an existing method; new methods are additive and
/// discovered through [`HostHello::methods`].
pub const CONTRACT_VERSION: u32 = 1;

/// The first line the module writes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModuleHello {
    /// Always `"module.hello"`.
    #[serde(rename = "type")]
    pub kind: HelloKind,
    /// The manifest the module was built with. The host compares it to the install
    /// record and refuses the module on any difference.
    pub manifest: Manifest,
    /// Lowest contract version the module can speak.
    pub contract_min: u32,
    /// Highest contract version the module can speak.
    pub contract_max: u32,
    /// Methods this module serves besides the required `module.*` set.
    #[serde(default)]
    pub methods: Vec<String>,
    /// SDK version string, for diagnostics only.
    #[serde(default)]
    pub sdk_version: String,
}

/// The host's answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostHello {
    /// Always `"host.hello"`.
    #[serde(rename = "type")]
    pub kind: HelloKind,
    /// The contract version both sides will speak.
    pub contract_version: u32,
    /// Host product version (`x.y.z`).
    pub host_version: String,
    /// Product name (`Avada Terminal`).
    pub product: String,
    /// Capabilities the user accepted for this module. Anything not listed is refused
    /// at the host boundary; the module should hide the affected UI.
    pub granted: Vec<crate::caps::Capability>,
    /// `host.*` methods this host serves.
    pub methods: Vec<String>,
    /// Absolute path of the module's private data directory.
    pub data_dir: String,
    /// The workspace currently active, if any, so the module can render before the
    /// first `module.activate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceInfo>,
    /// Per-run bearer token the module presents on the control server (`/m/...`
    /// routes and `host.*` calls that leave the pipe). Minted by the host at every
    /// spawn, never persisted, never logged. Absent from a host older than this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// Base URL of the control server (`http://127.0.0.1:<port>`), so a module can call
    /// HTTP routes with its `token`. Absent when the host has no control server running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_url: Option<String>,
}

/// Discriminator on the hello lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HelloKind {
    /// Written by the module.
    #[serde(rename = "module.hello")]
    Module,
    /// Written by the host.
    #[serde(rename = "host.hello")]
    Host,
}

/// What a module knows about a workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    /// Stable id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Root directory, if the workspace has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
}

/// Pick the contract version both sides speak, or `None` if the ranges do not meet.
pub fn negotiate(module_min: u32, module_max: u32, host_min: u32, host_max: u32) -> Option<u32> {
    let lo = module_min.max(host_min);
    let hi = module_max.min(host_max);
    (lo <= hi).then_some(hi)
}

/// A JSON-RPC 2.0 request id. Numbers are what this SDK emits; strings are accepted.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    /// Numeric id.
    Num(u64),
    /// String id.
    Str(String),
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Id::Num(n) => write!(f, "{n}"),
            Id::Str(s) => f.write_str(s),
        }
    }
}

/// A request that expects exactly one response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// Always `"2.0"`.
    pub jsonrpc: Jsonrpc,
    /// Correlates the response.
    pub id: Id,
    /// `host.*` or `module.*`.
    pub method: String,
    /// Method parameters; an object, or absent.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub params: Value,
}

/// A one-way message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    /// Always `"2.0"`.
    pub jsonrpc: Jsonrpc,
    /// Method name.
    pub method: String,
    /// Parameters; an object, or absent.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub params: Value,
}

/// The answer to a [`Request`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    /// Always `"2.0"`.
    pub jsonrpc: Jsonrpc,
    /// The request's id.
    pub id: Id,
    /// Present on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Present on failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

/// The literal `"2.0"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Jsonrpc {
    /// The only value.
    #[default]
    #[serde(rename = "2.0")]
    V2,
}

/// Error codes. Standard JSON-RPC codes plus the contract's own in `-32000..=-32099`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// Not valid JSON.
    ParseError,
    /// Not a request object.
    InvalidRequest,
    /// No such method.
    MethodNotFound,
    /// Bad params.
    InvalidParams,
    /// Server-side failure.
    Internal,
    /// The capability needed for this method was not granted at install.
    CapabilityDenied,
    /// The user answered "no" to an `ask` right this time.
    UserDenied,
    /// The module or host is shutting down.
    ShuttingDown,
    /// A workspace-scoped call arrived with no active workspace.
    NoWorkspace,
    /// Anything else; the code is carried verbatim.
    Other(i64),
}

impl ErrorCode {
    /// Numeric code on the wire.
    pub fn code(self) -> i64 {
        match self {
            ErrorCode::ParseError => -32700,
            ErrorCode::InvalidRequest => -32600,
            ErrorCode::MethodNotFound => -32601,
            ErrorCode::InvalidParams => -32602,
            ErrorCode::Internal => -32603,
            ErrorCode::CapabilityDenied => -32001,
            ErrorCode::UserDenied => -32002,
            ErrorCode::ShuttingDown => -32003,
            ErrorCode::NoWorkspace => -32004,
            ErrorCode::Other(c) => c,
        }
    }
    /// Inverse of [`ErrorCode::code`].
    pub fn from_code(c: i64) -> ErrorCode {
        match c {
            -32700 => ErrorCode::ParseError,
            -32600 => ErrorCode::InvalidRequest,
            -32601 => ErrorCode::MethodNotFound,
            -32602 => ErrorCode::InvalidParams,
            -32603 => ErrorCode::Internal,
            -32001 => ErrorCode::CapabilityDenied,
            -32002 => ErrorCode::UserDenied,
            -32003 => ErrorCode::ShuttingDown,
            -32004 => ErrorCode::NoWorkspace,
            other => ErrorCode::Other(other),
        }
    }
}

/// The `error` member of a response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    /// See [`ErrorCode`].
    pub code: i64,
    /// Human-readable.
    pub message: String,
    /// Anything structured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    /// Build from a code and message.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        RpcError {
            code: code.code(),
            message: message.into(),
            data: None,
        }
    }
    /// The typed code.
    pub fn kind(&self) -> ErrorCode {
        ErrorCode::from_code(self.code)
    }
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}
impl std::error::Error for RpcError {}

/// Any line after the hellos.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Message {
    /// Has `id` and `method`.
    Request(Request),
    /// Has `id` and `result` or `error`.
    Response(Response),
    /// Has `method`, no `id`.
    Notification(Notification),
}

impl Message {
    /// Parse one line. Distinguishes the three shapes by which members are present
    /// (serde's untagged order alone would misread a response with a `method` field).
    pub fn parse(line: &str) -> Result<Message, RpcError> {
        let v: Value = serde_json::from_str(line)
            .map_err(|e| RpcError::new(ErrorCode::ParseError, e.to_string()))?;
        let obj = v
            .as_object()
            .ok_or_else(|| RpcError::new(ErrorCode::InvalidRequest, "not an object"))?;
        if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Err(RpcError::new(
                ErrorCode::InvalidRequest,
                "jsonrpc must be \"2.0\"",
            ));
        }
        let has_id = obj.contains_key("id");
        let has_method = obj.contains_key("method");
        let parsed = match (has_id, has_method) {
            (true, true) => serde_json::from_value(v).map(Message::Request),
            (true, false) => serde_json::from_value(v).map(Message::Response),
            (false, true) => serde_json::from_value(v).map(Message::Notification),
            (false, false) => {
                return Err(RpcError::new(
                    ErrorCode::InvalidRequest,
                    "neither id nor method",
                ))
            }
        };
        parsed.map_err(|e| RpcError::new(ErrorCode::InvalidRequest, e.to_string()))
    }

    /// One line, no trailing newline.
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).expect("messages are always serializable")
    }
}

impl Request {
    /// Build a request.
    pub fn new(id: u64, method: impl Into<String>, params: Value) -> Self {
        Request {
            jsonrpc: Jsonrpc::V2,
            id: Id::Num(id),
            method: method.into(),
            params,
        }
    }
    /// A successful answer to this request.
    pub fn ok(&self, result: Value) -> Response {
        Response {
            jsonrpc: Jsonrpc::V2,
            id: self.id.clone(),
            result: Some(result),
            error: None,
        }
    }
    /// A failed answer to this request.
    pub fn err(&self, error: RpcError) -> Response {
        Response {
            jsonrpc: Jsonrpc::V2,
            id: self.id.clone(),
            result: None,
            error: Some(error),
        }
    }
}

impl Notification {
    /// Build a notification.
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Notification {
            jsonrpc: Jsonrpc::V2,
            method: method.into(),
            params,
        }
    }
}

/// Method names. A `const` per method keeps both sides from drifting on spelling.
pub mod methods {
    /// Host: register or replace the module's rail entries. Params: `{ entries: [RailEntry] }`.
    pub const HOST_RAIL_REGISTER: &str = "host.rail.register";
    /// Host: replace the rows shown under a rail entry. Params: `{ entry: id, rows: [Row] }`.
    pub const HOST_ROWS_SET: &str = "host.rows.set";
    /// Host: register commands. Params: `{ commands: [{ id, label, chord? }] }`.
    pub const HOST_COMMAND_REGISTER: &str = "host.command.register";
    /// Host: declare a typed preferences page. Params: `{ page: PrefsPage }`.
    pub const HOST_PREFS_DECLARE: &str = "host.prefs.declare";
    /// Host: read the module's prefs. Result: `{ values: {key: value} }`.
    pub const HOST_PREFS_GET: &str = "host.prefs.get";
    /// Host: open a pane. Params: `{ kind, path?, surface? }`. Result: `{ pane_id }`.
    pub const HOST_PANES_SPAWN: &str = "host.panes.spawn";
    /// Host: write to a pane's input. Params: `{ pane_id, text }`.
    pub const HOST_PANES_INPUT: &str = "host.panes.input";
    /// Host: read a file. Params: `{ path }`. Result: `{ text }` or `{ bytes_b64 }`.
    pub const HOST_FS_READ: &str = "host.fs.read";
    /// Host: write a file. Params: `{ path, text }`.
    pub const HOST_FS_WRITE: &str = "host.fs.write";
    /// Host: list a directory. Params: `{ path }`. Result: `{ entries: [{name, kind}] }`.
    pub const HOST_FS_LIST: &str = "host.fs.list";
    /// Host: subscribe to event kinds. Params: `{ kinds: [string] }`.
    pub const HOST_EVENTS_SUBSCRIBE: &str = "host.events.subscribe";
    /// Host: show a toast. Params: `{ text, level? }`.
    pub const HOST_TOAST: &str = "host.toast";
    /// Host: register control-plane routes. Params: `{ routes: [RouteDescriptor] }`.
    pub const HOST_ROUTES_REGISTER: &str = "host.routes.register";
    /// Host: read a keychain entry the module owns. Params: `{ key }`. Result: `{ value? }`.
    pub const HOST_KEYCHAIN_GET: &str = "host.keychain.get";
    /// Host: write a keychain entry the module owns. Params: `{ key, value }`.
    pub const HOST_KEYCHAIN_SET: &str = "host.keychain.set";

    /// Host: list the saved-workspace library and the sets drawer.
    /// Params: `{ what?: "workspaces" | "sets" }` (absent means both).
    /// Result: `{ workspaces: [LibraryItem], sets: [SetItem] }`.
    ///
    /// These live under the host's data directory, outside any workspace root, so
    /// `fs.read` cannot reach them; this is the only way a module can see them.
    pub const HOST_WORKSPACE_LIST: &str = "host.workspace.list";
    /// Host: open a saved workspace or set. Params: `{ path }` (as listed).
    pub const HOST_WORKSPACE_OPEN: &str = "host.workspace.open";
    /// Host: save the current workspace. Params: `{ name?, as_set?: bool }`.
    pub const HOST_WORKSPACE_SAVE: &str = "host.workspace.save";

    /// Module: a workspace became active. Params: `{ workspace: WorkspaceInfo }`.
    pub const MODULE_ACTIVATE: &str = "module.activate";
    /// Module: the workspace is going away. Params: `{ workspace_id }`.
    pub const MODULE_DEACTIVATE: &str = "module.deactivate";
    /// Module: the user invoked a command. Params: `{ id, args? }`.
    pub const MODULE_COMMAND_INVOKE: &str = "module.command.invoke";
    /// Module: a rail row was chosen. Params: `{ entry, row }`.
    pub const MODULE_ROW_ACTIVATE: &str = "module.row.activate";
    /// Module: a control-plane route the module registered was hit.
    /// Params: `{ route, params, body? }`. Result: the JSON body to return.
    pub const MODULE_ROUTE_INVOKE: &str = "module.route.invoke";
    /// Module: an event the module subscribed to (notification).
    pub const MODULE_EVENT: &str = "module.event";
    /// The `kind`s a host may put on a `module.event` notification.
    ///
    /// The payload of `module.event` is `{ kind, payload }`; `kind` comes from here and
    /// `payload` is the shape documented beside each constant. A module receives only the
    /// kinds it named in `host.events.subscribe`, and unknown kinds are ignored rather
    /// than an error — a newer host must be able to announce something an older module
    /// never heard of.
    pub mod events {
        /// The filter box under a tier-1 rail entry changed.
        /// Payload: `{ entry: String, query: String }`.
        pub const RAIL_QUERY: &str = "rail.query";
        /// Something asked for a path to be revealed in a file tree.
        /// Payload: `{ path: String, line?: u32, col?: u32 }`.
        pub const FILES_REVEAL: &str = "files.reveal";
        /// A file was opened onto a module's document surface, because the module's pane
        /// contribution claimed the file's extension in its `opens` list. The module reads
        /// `path`, parses it, and ships blocks back over `host.doc.set` for `surface`.
        /// Payload: `{ surface: String, path: String }`.
        pub const DOC_OPEN: &str = "doc.open";
    }

    /// Host: replace a grid surface's frame (UI tier 5). Params: [`crate::grid::GridFrame`].
    pub const HOST_GRID_SET: &str = "host.grid.set";
    /// Host: declare a grid surface's actions and keymap presets.
    /// Params: [`crate::grid::DeclareKeymap`].
    pub const HOST_KEYMAP_DECLARE: &str = "host.keymap.declare";

    /// Host: replace a document surface's blocks (UI tier 5). Params: [`crate::doc::Doc`].
    ///
    /// The one-directional twin of [`HOST_GRID_SET`]: the module ships parsed blocks —
    /// source text plus a language tag for code, never pixels — and the host renders them
    /// (mermaid, syntax highlighting, proportional wrapping all stay host-side, where the
    /// glyph metrics live). There is no `module.doc.*` reply because a rendered document
    /// takes no keystrokes; a surface that needs input is a grid, not a doc.
    pub const HOST_DOC_SET: &str = "host.doc.set";

    /// Module: a keystroke landed in a focused grid surface (notification).
    /// Params: [`crate::grid::GridKey`].
    ///
    /// A notification and not a request: a round trip per keystroke would put the module's
    /// scheduling latency between the user and their own typing. The host therefore does
    /// not learn whether the key was used, and instead keeps only what its own keymap
    /// claims before forwarding — the same bargain a focused terminal already makes.
    pub const MODULE_GRID_KEY: &str = "module.grid.key";
    /// Module: a grid surface's pane changed size (notification).
    /// Params: [`crate::grid::GridResize`].
    pub const MODULE_GRID_RESIZE: &str = "module.grid.resize";

    /// Module: prefs changed (notification). Params: `{ values }`.
    pub const MODULE_PREFS_CHANGED: &str = "module.prefs.changed";
    /// Module: shut down (notification). The module must exit within 5 s.
    pub const MODULE_SHUTDOWN: &str = "module.shutdown";

    /// Every method a host must serve at contract version 1.
    pub const HOST_REQUIRED_V1: &[&str] = &[
        HOST_RAIL_REGISTER,
        HOST_ROWS_SET,
        HOST_COMMAND_REGISTER,
        HOST_PREFS_DECLARE,
        HOST_PREFS_GET,
        HOST_PANES_SPAWN,
        HOST_PANES_INPUT,
        HOST_FS_READ,
        HOST_FS_WRITE,
        HOST_FS_LIST,
        HOST_EVENTS_SUBSCRIBE,
        HOST_TOAST,
        HOST_ROUTES_REGISTER,
        HOST_KEYCHAIN_GET,
        HOST_KEYCHAIN_SET,
    ];
    /// Every method a module must serve at contract version 1.
    pub const MODULE_REQUIRED_V1: &[&str] = &[
        MODULE_ACTIVATE,
        MODULE_DEACTIVATE,
        MODULE_COMMAND_INVOKE,
        MODULE_ROW_ACTIVATE,
        MODULE_ROUTE_INVOKE,
        MODULE_EVENT,
        MODULE_PREFS_CHANGED,
        MODULE_SHUTDOWN,
    ];

    /// Host methods that are **additive**: served by hosts new enough to have them, and
    /// absent from older ones.
    ///
    /// They are deliberately not in [`HOST_REQUIRED_V1`] — that list is the definition of
    /// what conformance at contract version 1 means, and appending to it would retroactively
    /// make every already-shipped host non-conformant. A module discovers these the way the
    /// contract says to (`HostHello::methods`) and does without them when they are missing.
    /// That is also why adding them does not bump `CONTRACT_VERSION`: the host negotiates
    /// with that constant as both its floor and its ceiling, so a bump would refuse every
    /// module already built against v1.
    pub const HOST_OPTIONAL: &[&str] = &[
        HOST_WORKSPACE_LIST,
        HOST_WORKSPACE_OPEN,
        HOST_WORKSPACE_SAVE,
        HOST_GRID_SET,
        HOST_KEYMAP_DECLARE,
        HOST_DOC_SET,
    ];

    /// Module methods that are **additive**, for the same reason [`HOST_OPTIONAL`] is.
    ///
    /// A host discovers whether a module speaks these from the tier its contributions
    /// declare, not by asking: a tier-5 contribution is the promise that these are served,
    /// and a module with none of them will simply never be sent one.
    pub const MODULE_OPTIONAL: &[&str] = &[MODULE_GRID_KEY, MODULE_GRID_RESIZE];

    /// The capability a `host.*` method needs, or `None` for the always-allowed ones.
    ///
    /// This is name-only, so it names one right per method. [`HOST_ROWS_SET`] is the
    /// exception: it is dual-target — `ui.rail` fills the left rail, `ui.pane` fills a pane
    /// the module owns — and the caller's `Connection::call` accepts *either* right for it,
    /// leaving the host to reject the wrong target. The `ui.rail` named here is only its
    /// rail face, kept so every host method still maps to a right.
    pub fn required_capability(method: &str) -> Option<crate::caps::Capability> {
        use crate::caps::Capability as C;
        Some(match method {
            HOST_RAIL_REGISTER | HOST_ROWS_SET => C::UiRail,
            HOST_COMMAND_REGISTER => C::UiCommands,
            HOST_PREFS_DECLARE | HOST_PREFS_GET => C::UiPrefs,
            HOST_PANES_SPAWN => C::PanesSpawn,
            HOST_PANES_INPUT => C::PanesInput,
            HOST_FS_READ | HOST_FS_LIST => C::FsRead,
            HOST_FS_WRITE => C::FsWrite,
            HOST_EVENTS_SUBSCRIBE => C::EventsSubscribe,
            HOST_TOAST => C::UiToast,
            HOST_ROUTES_REGISTER => C::ControlRoute,
            HOST_KEYCHAIN_GET | HOST_KEYCHAIN_SET => C::Keychain,
            // A grid surface is a pane the module owns, so it is gated by the same right
            // as its rows are. Its keymap rides along rather than needing `ui.commands`:
            // the bindings are live only while that pane has focus and never reach the
            // palette, and an editor should not have to ask for the palette to be typed
            // into.
            // A doc surface is likewise a pane the module owns; it takes no keystrokes,
            // so it needs only `ui.pane`, never the keymap's palette right.
            HOST_GRID_SET | HOST_KEYMAP_DECLARE | HOST_DOC_SET => C::UiPane,
            HOST_WORKSPACE_LIST => C::WorkspaceRead,
            HOST_WORKSPACE_OPEN | HOST_WORKSPACE_SAVE => C::WorkspaceWrite,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn negotiation_picks_the_highest_shared_version() {
        assert_eq!(negotiate(1, 3, 2, 5), Some(3));
        assert_eq!(negotiate(1, 1, 1, 1), Some(1));
        assert_eq!(negotiate(1, 1, 2, 3), None);
        assert_eq!(negotiate(4, 6, 1, 3), None);
    }

    #[test]
    fn request_response_notification_are_told_apart() {
        let req = Request::new(7, methods::HOST_TOAST, json!({"text": "hi"}));
        let line = Message::Request(req.clone()).to_line();
        assert!(line.contains("\"jsonrpc\":\"2.0\""));
        assert_eq!(
            Message::parse(&line).unwrap(),
            Message::Request(req.clone())
        );

        let ok = req.ok(json!({"pane_id": "p1"}));
        let back = Message::parse(&Message::Response(ok.clone()).to_line()).unwrap();
        assert_eq!(back, Message::Response(ok));

        let err = req.err(RpcError::new(
            ErrorCode::CapabilityDenied,
            "ui.toast not granted",
        ));
        let back = Message::parse(&Message::Response(err.clone()).to_line()).unwrap();
        assert_eq!(back, Message::Response(err.clone()));
        assert_eq!(err.error.unwrap().kind(), ErrorCode::CapabilityDenied);

        let note = Notification::new(methods::MODULE_SHUTDOWN, Value::Null);
        let line = Message::Notification(note.clone()).to_line();
        assert!(!line.contains("params"), "null params are omitted: {line}");
        assert_eq!(Message::parse(&line).unwrap(), Message::Notification(note));
    }

    #[test]
    fn malformed_lines_are_typed_errors() {
        assert_eq!(
            Message::parse("{").unwrap_err().kind(),
            ErrorCode::ParseError
        );
        assert_eq!(
            Message::parse("[]").unwrap_err().kind(),
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            Message::parse(r#"{"jsonrpc":"1.0","id":1,"method":"x"}"#)
                .unwrap_err()
                .kind(),
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            Message::parse(r#"{"jsonrpc":"2.0"}"#).unwrap_err().kind(),
            ErrorCode::InvalidRequest
        );
    }

    #[test]
    fn string_ids_are_accepted() {
        let m =
            Message::parse(r#"{"jsonrpc":"2.0","id":"abc","method":"module.activate"}"#).unwrap();
        match m {
            Message::Request(r) => assert_eq!(r.id, Id::Str("abc".into())),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn error_codes_round_trip() {
        for c in [
            ErrorCode::ParseError,
            ErrorCode::InvalidRequest,
            ErrorCode::MethodNotFound,
            ErrorCode::InvalidParams,
            ErrorCode::Internal,
            ErrorCode::CapabilityDenied,
            ErrorCode::UserDenied,
            ErrorCode::ShuttingDown,
            ErrorCode::NoWorkspace,
            ErrorCode::Other(-32050),
        ] {
            assert_eq!(ErrorCode::from_code(c.code()), c);
        }
    }

    #[test]
    fn every_host_method_maps_to_a_capability() {
        for m in methods::HOST_REQUIRED_V1
            .iter()
            .chain(methods::HOST_OPTIONAL)
        {
            assert!(
                methods::required_capability(m).is_some(),
                "{m} has no capability"
            );
            assert!(m.starts_with("host."));
        }
        for m in methods::MODULE_REQUIRED_V1 {
            assert!(m.starts_with("module."));
            assert!(methods::required_capability(m).is_none());
        }
    }

    #[test]
    fn hellos_serialize_with_a_type_tag() {
        let h = HostHello {
            kind: HelloKind::Host,
            contract_version: 1,
            host_version: "0.0.37".into(),
            product: crate::PRODUCT_NAME.into(),
            granted: vec![crate::caps::Capability::UiRail],
            methods: methods::HOST_REQUIRED_V1
                .iter()
                .map(|s| s.to_string())
                .collect(),
            data_dir: "/tmp/x".into(),
            workspace: None,
            token: None,
            control_url: None,
        };
        let v = serde_json::to_value(&h).unwrap();
        assert_eq!(v["type"], "host.hello");
        assert_eq!(v["granted"][0], "ui.rail");
        assert_eq!(serde_json::from_value::<HostHello>(v).unwrap(), h);
    }
}
