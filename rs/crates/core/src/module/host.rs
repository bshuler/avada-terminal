//! The host: one [`Host`] per process, one slot per installed module. Everything the
//! rest of the crate (and `core/tests/module_host_e2e.rs`) touches is on `Host`.
//!
//! Threads per running module: a **reader** (drains the socket, answers `host.*`
//! requests through the [`Dispatcher`], routes responses to whoever is waiting) and a
//! **waiter** (notices the process exit and drives the [`Supervisor`]). Neither holds a
//! lock across I/O. Host→module calls are synchronous: write the request, block on a
//! one-shot channel with a timeout.

use super::gate::CapabilityGate;
use super::rail::{RailEvent, RailState};
use super::rpc::{CommandSpec, Dispatcher, Shared};
use super::spawn::{self, HandshakeError, SpawnError};
use super::supervisor::{ModuleStatus, RestartPolicy, Supervisor, Verdict};
use super::token::Token;
use super::transport::{Closer, LineReader, LineWriter};
use crate::license::Gate;
use avada_module_sdk::contract::methods;
use avada_module_sdk::contract::{
    HelloKind, HostHello, Message, Notification, Request, Response, RpcError, WorkspaceInfo,
    CONTRACT_VERSION,
};
use avada_module_sdk::descriptor::RouteDescriptor;
use avada_module_sdk::doc::Doc;
use avada_module_sdk::grid::{DeclareKeymap, GridFrame, GridKey, GridResize};
use avada_module_sdk::manifest::ContributionKind;
use avada_module_sdk::rail::RowActivate;
use avada_module_sdk::rights::InstallRecord;
use avada_module_sdk::ModuleId;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

/// Static facts about this host, fixed at construction.
#[derive(Debug, Clone)]
pub struct HostConfig {
    /// Every module gets `data_root/<owner__repo>/` as its private data dir.
    pub data_root: PathBuf,
    /// Reported in the host hello.
    pub host_version: String,
    /// Reported in the host hello (`Avada Terminal`).
    pub product: String,
    /// The workspace the modules are activated in, if any.
    pub workspace: Option<WorkspaceInfo>,
    /// Base URL of the control server, handed to modules in the hello so they can call
    /// HTTP routes with their token. `None` until the app wires it.
    pub control_url: Option<String>,
    /// Restart budget after crashes.
    pub policy: RestartPolicy,
    /// How long a freshly spawned module has to say hello.
    pub handshake_timeout: Duration,
    /// How long `module.shutdown` gets before the process is killed.
    pub shutdown_grace: Duration,
    /// How long a host→module request may take before it fails with `Timeout`.
    pub call_timeout: Duration,
}

impl HostConfig {
    /// Defaults from the contract: 10 s hello, 5 s shutdown grace, 5 s calls.
    pub fn new(data_root: impl Into<PathBuf>) -> Self {
        HostConfig {
            data_root: data_root.into(),
            host_version: env!("CARGO_PKG_VERSION").to_string(),
            product: "Avada Terminal".to_string(),
            workspace: None,
            control_url: None,
            policy: RestartPolicy::default(),
            handshake_timeout: Duration::from_secs(10),
            shutdown_grace: Duration::from_secs(5),
            call_timeout: Duration::from_secs(5),
        }
    }
}

/// What the host tells the application. Delivered through [`Host::events`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostEvent {
    /// A module asked for a toast (`host.toast`), or the host has something to say about
    /// a module (restart, disable). `level` is `info`, `warn` or `error`.
    Toast {
        /// Which module.
        module: ModuleId,
        /// The text.
        text: String,
        /// The level.
        level: String,
    },
    /// The module's status changed.
    Status {
        /// Which module.
        module: ModuleId,
        /// The new status.
        status: ModuleStatus,
    },
    /// The module (re)registered its command palette entries.
    Commands {
        /// Which module.
        module: ModuleId,
        /// The whole set.
        commands: Vec<CommandSpec>,
    },
    /// The module declared its preferences page.
    PrefsDeclared {
        /// Which module.
        module: ModuleId,
        /// The page description, as sent (a `PrefsPage`).
        page: Value,
    },
    /// The module (re)registered its control-plane routes (`host.routes.register`).
    /// The whole set, each stamped with the module id; `control::modules::attach_host`
    /// mounts them under `/m/<owner>/<repo>/...` and takes them down when the module
    /// stops.
    Routes {
        /// Which module.
        module: ModuleId,
        /// The whole set.
        routes: Vec<RouteDescriptor>,
    },
    /// The module asked for a pane (`host.panes.spawn`). The host has already minted
    /// `pane_id` and answered the module; opening the pane is the app's job. `kind` is
    /// `file` (open `path` in the viewer) or `module` (a pane owned by the module,
    /// showing `surface`); anything else the app does not know is a toast, not a pane.
    PaneSpawn {
        /// Which module.
        module: ModuleId,
        /// The id already handed back to the module.
        pane_id: String,
        /// What to open.
        kind: String,
        /// The path, for `kind: "file"`.
        path: Option<String>,
        /// The module surface, for `kind: "module"`.
        surface: Option<String>,
    },
    /// The module wrote to a pane's input (`host.panes.input`). The host has already
    /// answered the module; delivering the bytes is the app's job, and a `pane_id` the
    /// app no longer has is dropped rather than reported — the module may well have been
    /// typing into a pane the human closed a frame earlier.
    PaneInput {
        /// Which module.
        module: ModuleId,
        /// The pane, as minted by an earlier `host.panes.spawn`.
        pane_id: String,
        /// The bytes to feed the pane, exactly as the module sent them.
        text: String,
    },
    /// The module replaced a grid surface's frame (`host.grid.set`). A tier-5 pane owns
    /// its text and the host owns its pixels, so this carries the whole rectangle: the
    /// app paints it and keeps nothing of what came before.
    Grid {
        /// Which module.
        module: ModuleId,
        /// The whole frame, surface included.
        frame: GridFrame,
    },
    /// The module replaced a doc surface's document (`host.doc.set`). The reader half of
    /// tier 5: the module ships parsed blocks and the app typesets them, keeping nothing of
    /// what came before — a document replaces a document, the one-directional twin of
    /// [`HostEvent::Grid`].
    Doc {
        /// Which module.
        module: ModuleId,
        /// The whole document, surface included.
        doc: Doc,
    },
    /// The module replaced an image surface's picture (`host.image.set`). The deliberate
    /// pixel-carrying exception to [`HostEvent::Doc`]: a picture has no source to re-render
    /// from, so the module ships already-decoded RGBA and the app parks it as a texture,
    /// keeping the fit/zoom/caption geometry. A decode the module could not complete rides
    /// [`ImageData::error`] instead of the pixels.
    Image {
        /// Which module.
        module: ModuleId,
        /// The decoded picture (or the decode error), surface included.
        image: ImageData,
    },
    /// The module declared a grid surface's actions and keymap presets
    /// (`host.keymap.declare`). The host resolves keystrokes to action ids before it
    /// forwards them, so this is what that resolution — and the rebinding UI — reads.
    Keymap {
        /// Which module.
        module: ModuleId,
        /// The whole declaration, surface included; it replaces any earlier one.
        keymap: DeclareKeymap,
    },
    /// The module asked for a saved workspace or set to be opened
    /// (`host.workspace.open`). The path is one the module got back from
    /// `host.workspace.list`; opening it is the app's job, and a path that no longer
    /// exists is a toast, not an error the module ever sees.
    WorkspaceOpen {
        /// Which module.
        module: ModuleId,
        /// The file to open, as listed.
        path: String,
    },
    /// The module asked for the current workspace to be saved (`host.workspace.save`).
    /// What "current" means is the app's business — the host has no window.
    WorkspaceSave {
        /// Which module.
        module: ModuleId,
        /// The name to save under; `None` means "ask the human", exactly as the
        /// built-in Save button does.
        name: Option<String>,
        /// Save the open windows as a *set* rather than as one workspace.
        as_set: bool,
    },
}

/// A decoded picture on its way from a module to the app, carried by [`HostEvent::Image`].
///
/// The host-side twin of the SDK's `avada_module_sdk::image::SetImage`, with the base64
/// already decoded back to raw bytes: [`rgba`](Self::rgba) is `width * height * 4` bytes of
/// row-major RGBA8, or empty when [`error`](Self::error) names why the file could not be
/// shown. Exactly one of the two carries the payload. The `Debug` impl is hand-written to
/// name the buffer's length rather than dump megabytes of pixels into a trace.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct ImageData {
    /// The surface id — the pane the picture fills.
    pub surface: String,
    /// The file name for the caption ("cat.png"), not the full path.
    pub name: String,
    /// Decoded width in pixels. Zero on an error.
    pub width: u32,
    /// Decoded height in pixels. Zero on an error.
    pub height: u32,
    /// The sniffed container ("PNG", ...) for the caption. Empty on an error.
    pub format: String,
    /// The size of the encoded file in bytes, for the caption. Zero on an error.
    pub bytes: u64,
    /// The decoded pixels: `width * height * 4` bytes of row-major RGBA8. Empty on an error.
    pub rgba: Vec<u8>,
    /// The reason the file could not be shown, or empty on success.
    pub error: String,
}

impl std::fmt::Debug for ImageData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageData")
            .field("surface", &self.surface)
            .field("name", &self.name)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("format", &self.format)
            .field("bytes", &self.bytes)
            .field("rgba_len", &self.rgba.len())
            .field("error", &self.error)
            .finish()
    }
}

/// One file type an installed module's pane surface claims to open — the host's projection
/// of a [`ContributionKind::Pane`] contribution's `opens` list. The app routes a file whose
/// extension matches `ext` to a module pane on `surface` instead of a built-in viewer.
///
/// This exists so the app can consult that routing table without naming a `Contribution`:
/// the manifest vocabulary stays in the SDK and core, and the app is handed only the three
/// strings it needs. One [`Opener`] is a single (extension, module, surface) claim, so a
/// pane that opens `md` and `markdown` yields two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opener {
    /// The extension claimed, lowercase and without the dot (`"md"`).
    pub ext: String,
    /// The module whose pane opens it.
    pub module: ModuleId,
    /// The pane surface to open — the contribution's id.
    pub surface: String,
}

/// Project a set of install records into the file-open claims their pane contributions make.
///
/// One [`Opener`] per (extension, module, surface): a pane that opens `md` and `markdown`
/// yields two, and extensions are lowercased so the table is matched case-insensitively. The
/// result is sorted by extension, then module, then surface, so it is deterministic — and
/// when two modules claim one extension, the lower module id sorts first, which is the tie
/// the app breaks on. Records with no pane contributions, or panes with an empty `opens`,
/// contribute nothing.
pub fn openers_from<'a>(records: impl Iterator<Item = &'a InstallRecord>) -> Vec<Opener> {
    let mut out: Vec<Opener> = records
        .flat_map(|record| {
            let module = record.module_id.clone();
            record
                .manifest
                .contributions
                .iter()
                .filter(|c| c.kind == ContributionKind::Pane)
                .flat_map(move |c| {
                    let module = module.clone();
                    let surface = c.id.clone();
                    c.opens.iter().map(move |ext| Opener {
                        ext: ext.to_ascii_lowercase(),
                        module: module.clone(),
                        surface: surface.clone(),
                    })
                })
        })
        .collect();
    out.sort_by(|a, b| {
        a.ext
            .cmp(&b.ext)
            .then_with(|| a.module.cmp(&b.module))
            .then_with(|| a.surface.cmp(&b.surface))
    });
    out
}

/// Why a host operation failed.
#[derive(Debug)]
pub enum HostError {
    /// No record was ever given to the host for this id.
    NotInstalled(ModuleId),
    /// The binary hash differs from the record; the module is now `Broken`.
    HashMismatch {
        /// From the record.
        expected: String,
        /// Now on disk.
        actual: String,
    },
    /// The process could not be started.
    Spawn(SpawnError),
    /// The hello exchange failed; the module is `Broken` (manifest) or `Disabled`.
    Handshake(HandshakeError),
    /// The module said hello but not within `handshake_timeout`.
    HandshakeTimeout,
    /// The module is not running, so it cannot be called.
    NotRunning(ModuleId),
    /// The module answered a host→module request with an error.
    Rpc(RpcError),
    /// The module did not answer within `call_timeout`.
    Timeout,
    /// The pipe went away mid-call.
    Closed,
    /// A commercial module has no licence this host will accept; it is now `Broken`.
    Unlicensed(String),
    /// Persisting or reading module data failed.
    Io(io::Error),
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostError::NotInstalled(id) => write!(f, "module `{id}` is not installed"),
            HostError::HashMismatch { .. } => f.write_str("binary hash mismatch"),
            HostError::Spawn(e) => write!(f, "{e}"),
            HostError::Handshake(e) => write!(f, "handshake: {e}"),
            HostError::HandshakeTimeout => f.write_str("module did not say hello in time"),
            HostError::NotRunning(id) => write!(f, "module `{id}` is not running"),
            HostError::Rpc(e) => write!(f, "module error: {e}"),
            HostError::Timeout => f.write_str("module did not answer in time"),
            HostError::Closed => f.write_str("module connection closed"),
            HostError::Unlicensed(reason) => write!(f, "not licensed: {reason}"),
            HostError::Io(e) => write!(f, "{e}"),
        }
    }
}
impl std::error::Error for HostError {}

impl From<io::Error> for HostError {
    fn from(e: io::Error) -> Self {
        HostError::Io(e)
    }
}

/// One live process: everything a thread needs to talk to it.
struct Live {
    writer: Arc<Mutex<LineWriter>>,
    closer: Closer,
    child: Arc<Mutex<Child>>,
    token: Token,
    pending: Arc<Mutex<HashMap<u64, Sender<Response>>>>,
    next_id: Arc<AtomicU64>,
}

struct SlotState {
    status: ModuleStatus,
    live: Option<Live>,
    /// Bumped at every start; threads of an older generation stand down.
    generation: u64,
    /// Set by `shutdown`: the next exit is not a crash.
    expected_exit: bool,
}

/// Consulted before a module whose manifest says `distribution.commercial = true` is
/// spawned. `crate::license::LicenseService` decides what a licence means; this is only
/// the seam through which the host asks.
///
/// Deliberately **synchronous and non-blocking**. `Slot::start` runs on whichever thread
/// asked for the module, and `LicenseService::gate_for_major` is async because it may
/// fetch an issuer's JWKS over the network --- a spawn that waited on that would stall
/// the caller every time the network is slow, and would fail closed on a laptop that is
/// merely offline. An implementation answers from what it already knows and refreshes
/// somewhere else; [`crate::license::CachedGate`] is the one core provides.
pub trait Licensing: Send + Sync {
    /// May `product` run at major version `major`?
    fn gate(&self, product: &ModuleId, major: u64) -> Gate;
}

/// Shared by the host and every slot so [`Host::set_licensing`] reaches modules that
/// were already installed when it is called.
type LicensingCell = Arc<Mutex<Option<Arc<dyn Licensing>>>>;

/// One installed module, whether or not it is running.
struct Slot {
    id: ModuleId,
    record: InstallRecord,
    binary: PathBuf,
    data_dir: PathBuf,
    config: Arc<HostConfig>,
    shared: Arc<Shared>,
    dispatcher: Dispatcher,
    supervisor: Mutex<Supervisor>,
    state: Mutex<SlotState>,
    extra_env: Vec<(String, String)>,
    extra_args: Vec<String>,
    licensing: LicensingCell,
}

struct Inner {
    config: Arc<HostConfig>,
    shared: Arc<Shared>,
    slots: Mutex<HashMap<ModuleId, Arc<Slot>>>,
    licensing: LicensingCell,
}

/// The module host. Cheap to clone; all clones share one set of modules. Dropping the
/// last clone shuts every module down.
#[derive(Clone)]
pub struct Host {
    inner: Arc<Inner>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Host {
    /// A host with `gate` deciding every capability check.
    pub fn new(config: HostConfig, gate: Arc<dyn CapabilityGate>) -> Host {
        let config_root = config.workspace.as_ref().and_then(|w| w.root.clone());
        Host {
            inner: Arc::new(Inner {
                config: Arc::new(config),
                shared: Arc::new(Shared {
                    gate,
                    rail_events: Default::default(),
                    events: Default::default(),
                    workspace_root: Mutex::new(config_root.map(std::path::PathBuf::from)),
                }),
                slots: Mutex::new(HashMap::new()),
                licensing: Arc::new(Mutex::new(None)),
            }),
        }
    }

    /// The configuration this host was built with.
    pub fn config(&self) -> &HostConfig {
        &self.inner.config
    }

    /// Install the licence gate consulted before a commercial module spawns.
    ///
    /// Until this is called the host runs every module it is given: the free edition
    /// has no licence service at all, and it already refuses a `kind = "binary"`
    /// distribution at install time, so failing open here costs nothing it was
    /// protecting. A build that ships commercial modules calls this at startup.
    /// Modules already installed see the change --- the gate is consulted at each
    /// spawn, not captured when the slot is made.
    pub fn set_licensing(&self, licensing: Arc<dyn Licensing>) {
        *lock(&self.inner.licensing) = Some(licensing);
    }

    /// The installed licence gate, if any.
    pub fn licensing(&self) -> Option<Arc<dyn Licensing>> {
        lock(&self.inner.licensing).clone()
    }

    /// A receiver that sees every [`RailEvent`] from now on (`std::sync::mpsc`; each call
    /// gets its own channel, all fed the same events).
    pub fn rail_events(&self) -> Receiver<RailEvent> {
        self.inner.shared.rail_events.subscribe()
    }

    /// A receiver that sees every [`HostEvent`] from now on. Same fan-out as
    /// [`rail_events`](Self::rail_events).
    pub fn events(&self) -> Receiver<HostEvent> {
        self.inner.shared.events.subscribe()
    }

    /// Hash-check, start and handshake the module described by `record`, whose binary
    /// is at `binary`. Returns once the module is `Running`, or with the reason it is
    /// not. Calling it again for a module that is already live restarts nothing and
    /// returns `Ok`. Re-registering a record for a stopped module replaces it.
    pub fn spawn(&self, record: &InstallRecord, binary: &Path) -> Result<(), HostError> {
        self.spawn_with(record, binary, &[], &[])
    }

    /// [`spawn`](Self::spawn) with extra environment and arguments for the child; tests
    /// use this to steer a fake module.
    pub fn spawn_with(
        &self,
        record: &InstallRecord,
        binary: &Path,
        extra_env: &[(String, String)],
        extra_args: &[String],
    ) -> Result<(), HostError> {
        let slot = {
            let mut slots = lock(&self.inner.slots);
            if let Some(existing) = slots.get(&record.module_id) {
                if lock(&existing.state).status.is_live() {
                    return Ok(());
                }
            }
            let data_dir = self
                .inner
                .config
                .data_root
                .join(record.module_id.dir_name());
            let slot = Arc::new(Slot {
                id: record.module_id.clone(),
                record: record.clone(),
                binary: binary.to_path_buf(),
                dispatcher: Dispatcher::new(
                    record.module_id.clone(),
                    self.inner.shared.clone(),
                    &data_dir,
                ),
                data_dir,
                config: self.inner.config.clone(),
                shared: self.inner.shared.clone(),
                supervisor: Mutex::new(Supervisor::new(self.inner.config.policy.clone())),
                state: Mutex::new(SlotState {
                    status: ModuleStatus::NotInstalled,
                    live: None,
                    generation: 0,
                    expected_exit: false,
                }),
                extra_env: extra_env.to_vec(),
                extra_args: extra_args.to_vec(),
                licensing: self.inner.licensing.clone(),
            });
            slots.insert(record.module_id.clone(), slot.clone());
            slot
        };
        slot.start()
    }

    /// Where a module is in its life; `NotInstalled` for an id the host never saw.
    pub fn status(&self, id: &ModuleId) -> ModuleStatus {
        match lock(&self.inner.slots).get(id) {
            Some(slot) => lock(&slot.state).status.clone(),
            None => ModuleStatus::NotInstalled,
        }
    }

    /// Every module the host knows and its status.
    pub fn statuses(&self) -> Vec<(ModuleId, ModuleStatus)> {
        let slots = lock(&self.inner.slots);
        let mut out: Vec<_> = slots
            .values()
            .map(|s| (s.id.clone(), lock(&s.state).status.clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// The record the module was spawned from.
    pub fn record(&self, id: &ModuleId) -> Option<InstallRecord> {
        lock(&self.inner.slots).get(id).map(|s| s.record.clone())
    }

    /// The module's private data directory.
    pub fn data_dir(&self, id: &ModuleId) -> Option<PathBuf> {
        lock(&self.inner.slots).get(id).map(|s| s.data_dir.clone())
    }

    /// What the module currently has on the rail (empty when it is not running).
    pub fn rail_state(&self, id: &ModuleId) -> RailState {
        match lock(&self.inner.slots).get(id) {
            Some(slot) => lock(&slot.dispatcher.rail).clone(),
            None => RailState::default(),
        }
    }

    /// The commands the module registered.
    pub fn commands(&self, id: &ModuleId) -> Vec<CommandSpec> {
        match lock(&self.inner.slots).get(id) {
            Some(slot) => slot.dispatcher.commands(),
            None => Vec::new(),
        }
    }

    /// The control-plane routes the module registered (each stamped with its id).
    pub fn routes(&self, id: &ModuleId) -> Vec<RouteDescriptor> {
        match lock(&self.inner.slots).get(id) {
            Some(slot) => slot.dispatcher.routes(),
            None => Vec::new(),
        }
    }

    /// Every file type any installed module's pane surface claims to open.
    ///
    /// Read from the install records' manifests, not from live dispatch: a claim is a
    /// static fact about what is installed, so this holds whether or not the module is
    /// running — the app can open the pane, and the module renders into it once it is up.
    /// See [`openers_from`] for the projection and its ordering guarantees.
    pub fn openers(&self) -> Vec<Opener> {
        let slots = lock(&self.inner.slots);
        openers_from(slots.values().map(|slot| &slot.record))
    }

    /// The module's stored preference values.
    pub fn prefs(&self, id: &ModuleId) -> Map<String, Value> {
        match lock(&self.inner.slots).get(id) {
            Some(slot) => lock(&slot.dispatcher.prefs).values.clone(),
            None => Map::new(),
        }
    }

    /// Replace the module's preference values, persist them under its data dir and,
    /// if it is running, push `module.prefs.changed`.
    pub fn set_prefs(&self, id: &ModuleId, values: Map<String, Value>) -> Result<(), HostError> {
        let slot = self.slot(id)?;
        let params = slot.dispatcher.set_prefs(values)?;
        if let Some(writer) = slot.writer() {
            let n = Notification::new(methods::MODULE_PREFS_CHANGED, params);
            lock(&writer).write_message(&Message::Notification(n))?;
        }
        Ok(())
    }

    /// Whether `presented` is the token minted for this module's current run. Constant
    /// time; false when the module is not running.
    pub fn token_matches(&self, id: &ModuleId, presented: &str) -> bool {
        match lock(&self.inner.slots).get(id) {
            Some(slot) => lock(&slot.state)
                .live
                .as_ref()
                .is_some_and(|l| l.token.matches(presented)),
            None => false,
        }
    }

    /// Which running module `presented` is the token of, if any. Every live token is
    /// compared in constant time, so the answer's timing does not say which slot matched.
    /// The control server uses this to put a name to a bearer on `/m/...` routes.
    pub fn module_for_token(&self, presented: &str) -> Option<ModuleId> {
        if presented.is_empty() {
            return None;
        }
        let mut found = None;
        for (id, slot) in lock(&self.inner.slots).iter() {
            let hit = lock(&slot.state)
                .live
                .as_ref()
                .is_some_and(|l| l.token.matches(presented));
            if hit && found.is_none() {
                found = Some(id.clone());
            }
        }
        found
    }

    /// The live token itself, for tests that play the module's side of the wire. Never
    /// compiled into the shipped binary.
    #[cfg(test)]
    #[cfg_attr(not(unix), allow(dead_code))] // its callers are unix-gated tests
    pub(crate) fn test_token(&self, id: &ModuleId) -> Option<String> {
        lock(&self.inner.slots).get(id).and_then(|slot| {
            lock(&slot.state)
                .live
                .as_ref()
                .map(|l| l.token.expose().to_string())
        })
    }

    /// `module.activate` with the host's workspace (or `workspace` if given).
    pub fn activate(
        &self,
        id: &ModuleId,
        workspace: Option<WorkspaceInfo>,
    ) -> Result<Value, HostError> {
        let ws = workspace.or_else(|| self.inner.config.workspace.clone());
        // The filesystem scope follows the workspace. A module activated on a new
        // workspace must not keep reading the old one's tree through `host.fs.*`.
        if let Some(w) = &ws {
            self.set_workspace_root(w.root.clone());
        }
        self.slot(id)?
            .request(methods::MODULE_ACTIVATE, json!({ "workspace": ws }))
    }

    /// Point every module's `host.fs.*` scope at `root` (or at nothing). Called by
    /// [`Host::activate`]; also available to an app that switches workspace without
    /// re-activating.
    pub fn set_workspace_root(&self, root: Option<String>) {
        *lock(&self.inner.shared.workspace_root) = root.map(PathBuf::from);
    }

    /// The root `host.fs.*` is currently scoped to.
    pub fn workspace_root(&self) -> Option<PathBuf> {
        lock(&self.inner.shared.workspace_root).clone()
    }

    /// Send `module.event { kind, payload }` to every running module that subscribed to
    /// `kind`. Returns how many modules were told.
    ///
    /// A notification, not a request: the host is announcing something that already
    /// happened, and a module that is slow to react must not stall the UI thread that
    /// emitted it. The count is the answer to "did anyone hear me" — the app uses it to
    /// decide whether to fall back to a toast.
    pub fn emit(&self, kind: &str, payload: Value) -> usize {
        let slots: Vec<Arc<Slot>> = lock(&self.inner.slots).values().cloned().collect();
        let mut told = 0;
        for slot in slots {
            if !slot.dispatcher.subscribed(kind) {
                continue;
            }
            let Some(writer) = slot.writer() else {
                continue;
            };
            let n = Notification::new(
                methods::MODULE_EVENT,
                json!({ "kind": kind, "payload": payload }),
            );
            let sent = lock(&writer).write_message(&Message::Notification(n));
            match sent {
                Ok(()) => told += 1,
                Err(e) => tracing::warn!(module = %slot.id, %kind, "module.event: {e}"),
            }
        }
        told
    }

    /// The keymaps a module has declared, by surface.
    ///
    /// Empty for a module that never declared one, which is every module that paints no
    /// grid — the absence is the normal case, not a failure.
    pub fn keymaps(&self, id: &ModuleId) -> Vec<DeclareKeymap> {
        match lock(&self.inner.slots).get(id) {
            Some(slot) => lock(&slot.dispatcher.keymaps).values().cloned().collect(),
            None => Vec::new(),
        }
    }

    /// The app opened a module pane for `surface` without going through `host.panes.spawn`
    /// — an opener-routed file, where the app resolves a `ContributionKind::Pane` and creates
    /// the pane itself before the module has said anything. Register the surface so its
    /// `doc`/`rows`/`grid` guards pass, exactly as a module-initiated spawn would, but emit no
    /// `PaneSpawn`: the pane already exists. A no-op for an unknown module or a re-registered
    /// surface, since the guard's set is presence-only ([`Dispatcher::panes`]).
    pub fn note_opener_pane(&self, id: &ModuleId, surface: &str) {
        if let Some(slot) = lock(&self.inner.slots).get(id) {
            lock(&slot.dispatcher.panes).insert(surface.to_string());
        }
    }

    /// `module.grid.key` — one keystroke that landed in a focused grid surface.
    ///
    /// A notification, deliberately. A request would put the module's scheduling latency
    /// between a human and their own typing, and there is no answer worth waiting for:
    /// what the keystroke did shows up as the next frame.
    pub fn grid_key(&self, id: &ModuleId, key: &GridKey) -> Result<(), HostError> {
        self.notify(id, methods::MODULE_GRID_KEY, key)
    }

    /// `module.grid.resize` — the pane showing a grid surface changed size. Also a
    /// notification: the module answers by painting, not by replying.
    pub fn grid_resize(&self, id: &ModuleId, resize: &GridResize) -> Result<(), HostError> {
        self.notify(id, methods::MODULE_GRID_RESIZE, resize)
    }

    /// Write one notification to a running module. `NotRunning` when it is not — a
    /// keystroke for a dead module is worth reporting, because the pane it was typed into
    /// is still on screen.
    fn notify<T: serde::Serialize>(
        &self,
        id: &ModuleId,
        method: &str,
        params: &T,
    ) -> Result<(), HostError> {
        let slot = self.slot(id)?;
        let writer = slot
            .writer()
            .ok_or_else(|| HostError::NotRunning(id.clone()))?;
        let params = serde_json::to_value(params).map_err(io::Error::other)?;
        let n = Notification::new(method, params);
        lock(&writer).write_message(&Message::Notification(n))?;
        Ok(())
    }

    /// `module.deactivate`.
    pub fn deactivate(&self, id: &ModuleId) -> Result<Value, HostError> {
        self.slot(id)?
            .request(methods::MODULE_DEACTIVATE, Value::Null)
    }

    /// `module.command.invoke {id, args}`.
    pub fn invoke_command(
        &self,
        id: &ModuleId,
        command: &str,
        args: Value,
    ) -> Result<Value, HostError> {
        let params = if args.is_null() {
            json!({ "id": command })
        } else {
            json!({ "id": command, "args": args })
        };
        self.slot(id)?
            .request(methods::MODULE_COMMAND_INVOKE, params)
    }

    /// `module.row.activate`.
    pub fn activate_row(&self, id: &ModuleId, row: &RowActivate) -> Result<Value, HostError> {
        let params = serde_json::to_value(row).map_err(io::Error::other)?;
        self.slot(id)?.request(methods::MODULE_ROW_ACTIVATE, params)
    }

    /// Any host→module request by name; the typed wrappers above are preferred.
    pub fn call(&self, id: &ModuleId, method: &str, params: Value) -> Result<Value, HostError> {
        self.slot(id)?.request(method, params)
    }

    /// Ask the module to stop (`module.shutdown`), wait up to `shutdown_grace`, then
    /// kill it. Its status becomes `Disabled { reason: "shut down" }`. No-op for a
    /// module that is not running.
    pub fn shutdown(&self, id: &ModuleId) -> Result<(), HostError> {
        self.slot(id)?.shutdown("shut down");
        Ok(())
    }

    /// [`shutdown`](Self::shutdown) with the reason the user should be told.
    ///
    /// `shutdown` says only "shut down", which is honest for a user who asked for it and
    /// useless for a module the host stopped on its own: the licence sweep pulls a module
    /// out from under someone who did not touch it, and `Disabled { reason }` is the only
    /// place that explanation can live where the placeholder and the toast will find it.
    pub fn disable(&self, id: &ModuleId, reason: &str) -> Result<(), HostError> {
        self.slot(id)?.shutdown(reason);
        Ok(())
    }

    /// Raise a toast against a module from outside the host.
    ///
    /// Everything else the user sees about a module is a consequence of the module doing
    /// something. A licence entering its grace window is the exception --- nothing happened,
    /// which is exactly why it needs saying before the day it stops being nothing.
    pub fn toast(&self, id: &ModuleId, text: &str, level: &str) -> Result<(), HostError> {
        self.slot(id)?.toast(text.to_string(), level);
        Ok(())
    }

    /// Start a stopped, crashed, disabled or broken module again from a clean restart
    /// budget. The binary is re-hashed, so a `Broken` module stays broken unless it was
    /// reinstalled in place.
    pub fn reopen(&self, id: &ModuleId) -> Result<(), HostError> {
        let slot = self.slot(id)?;
        if lock(&slot.state).status.is_live() {
            return Ok(());
        }
        lock(&slot.supervisor).reset();
        slot.start()
    }

    /// Shut every running module down, in parallel, each with its own grace period.
    pub fn shutdown_all(&self) {
        let slots: Vec<Arc<Slot>> = lock(&self.inner.slots).values().cloned().collect();
        let handles: Vec<_> = slots
            .into_iter()
            .filter(|s| lock(&s.state).status.is_live())
            .map(|s| thread::spawn(move || s.shutdown("host shut down")))
            .collect();
        for h in handles {
            let _ = h.join();
        }
    }

    /// Forget a module entirely (after shutting it down). Its data dir is kept.
    pub fn remove(&self, id: &ModuleId) {
        let slot = lock(&self.inner.slots).remove(id);
        if let Some(slot) = slot {
            slot.shutdown("removed");
            self.inner
                .shared
                .rail_events
                .send(RailEvent::Gone { module: id.clone() });
        }
    }

    fn slot(&self, id: &ModuleId) -> Result<Arc<Slot>, HostError> {
        lock(&self.inner.slots)
            .get(id)
            .cloned()
            .ok_or_else(|| HostError::NotInstalled(id.clone()))
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        let slots: Vec<Arc<Slot>> = lock(&self.slots).drain().map(|(_, s)| s).collect();
        for s in slots {
            if lock(&s.state).status.is_live() {
                s.shutdown("host dropped");
            }
        }
    }
}

impl Slot {
    fn set_status(&self, status: ModuleStatus) {
        lock(&self.state).status = status.clone();
        self.shared.events.send(HostEvent::Status {
            module: self.id.clone(),
            status,
        });
    }

    fn toast(&self, text: String, level: &str) {
        self.shared.events.send(HostEvent::Toast {
            module: self.id.clone(),
            text,
            level: level.to_string(),
        });
    }

    fn writer(&self) -> Option<Arc<Mutex<LineWriter>>> {
        lock(&self.state).live.as_ref().map(|l| l.writer.clone())
    }

    fn host_hello(&self) -> HostHello {
        HostHello {
            kind: HelloKind::Host,
            contract_version: CONTRACT_VERSION,
            host_version: self.config.host_version.clone(),
            product: self.config.product.clone(),
            granted: self.record.accepted.iter().cloned().collect(),
            methods: super::rpc::SERVED.iter().map(|s| s.to_string()).collect(),
            data_dir: self.data_dir.to_string_lossy().into_owned(),
            workspace: self.config.workspace.clone(),
            token: None,
            control_url: self.config.control_url.clone(),
        }
    }

    /// `Some(reason)` when a commercial module may not run.
    ///
    /// A module whose manifest does not say `commercial` is never asked about, and a
    /// host with no [`Licensing`] installed runs everything (see
    /// [`Host::set_licensing`]). A licence inside its grace window runs and says why on
    /// a toast, so the first the user hears of an expiring licence is not the module
    /// vanishing.
    fn license_refusal(&self) -> Option<String> {
        if !self.record.manifest.distribution.commercial {
            return None;
        }
        let licensing = lock(&self.licensing).clone()?;
        match licensing.gate(&self.id, self.record.version.major) {
            Gate::Run => None,
            Gate::RunWithBanner(reason) => {
                self.toast(reason, "warn");
                None
            }
            Gate::Refuse(reason) => Some(reason),
        }
    }

    /// Hash → spawn → handshake → threads. Sets the status at every exit.
    fn start(self: &Arc<Self>) -> Result<(), HostError> {
        if let Err(e) = spawn::verify_hash(&self.binary, &self.record) {
            let err = match e {
                SpawnError::HashMismatch { expected, actual } => {
                    self.set_status(ModuleStatus::Broken {
                        reason: "binary changed since it was installed".into(),
                    });
                    HostError::HashMismatch { expected, actual }
                }
                other => {
                    self.set_status(ModuleStatus::Broken {
                        reason: other.to_string(),
                    });
                    HostError::Spawn(other)
                }
            };
            tracing::warn!(module = %self.id, "refusing to spawn: {err}");
            return Err(err);
        }
        if let Some(reason) = self.license_refusal() {
            self.set_status(ModuleStatus::Broken {
                reason: reason.clone(),
            });
            tracing::warn!(module = %self.id, "refusing to spawn: not licensed: {reason}");
            return Err(HostError::Unlicensed(reason));
        }
        let generation = {
            let mut st = lock(&self.state);
            st.generation += 1;
            st.expected_exit = false;
            st.live = None;
            st.generation
        };
        self.set_status(ModuleStatus::Starting);
        if let Err(e) = std::fs::create_dir_all(&self.data_dir) {
            self.set_status(ModuleStatus::Disabled {
                reason: format!("cannot create data dir: {e}"),
            });
            return Err(HostError::Io(e));
        }
        let spawned = match spawn::spawn(
            &self.binary,
            &self.data_dir,
            &self.extra_env,
            &self.extra_args,
        ) {
            Ok(s) => s,
            Err(e) => {
                self.set_status(ModuleStatus::Disabled {
                    reason: e.to_string(),
                });
                return Err(HostError::Spawn(e));
            }
        };
        let spawn::Spawned {
            mut child,
            mut reader,
            mut writer,
            closer,
        } = spawned;
        let token = Token::mint();

        // The handshake runs on its own thread so a module that never speaks cannot
        // wedge the caller: on timeout the closer turns its blocking read into EOF.
        let (tx, rx) = mpsc::channel();
        let record = self.record.clone();
        let reply = self.host_hello();
        let hs_token = token.clone();
        thread::spawn(move || {
            let r = spawn::handshake(&mut reader, &mut writer, &record, &reply, &hs_token);
            let _ = tx.send((r, reader, writer));
        });
        let (result, reader, writer) = match rx.recv_timeout(self.config.handshake_timeout) {
            Ok(x) => x,
            Err(_) => {
                closer.close();
                let _ = child.kill();
                let _ = child.wait();
                self.set_status(ModuleStatus::Disabled {
                    reason: "did not say hello in time".into(),
                });
                return Err(HostError::HandshakeTimeout);
            }
        };
        if let Err(e) = result {
            closer.close();
            let _ = child.kill();
            let _ = child.wait();
            let status = match &e {
                HandshakeError::ManifestMismatch => ModuleStatus::Broken {
                    reason: "manifest differs from the install record".into(),
                },
                HandshakeError::Contract { .. } => ModuleStatus::Broken {
                    reason: e.to_string(),
                },
                other => ModuleStatus::Disabled {
                    reason: other.to_string(),
                },
            };
            tracing::warn!(module = %self.id, "handshake refused: {e}");
            self.set_status(status);
            return Err(HostError::Handshake(e));
        }

        let live = Live {
            writer: Arc::new(Mutex::new(writer)),
            closer,
            child: Arc::new(Mutex::new(child)),
            token,
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
        };
        let child = live.child.clone();
        let writer = live.writer.clone();
        let pending = live.pending.clone();
        lock(&self.state).live = Some(live);
        self.set_status(ModuleStatus::Running);

        let me = self.clone();
        thread::spawn(move || me.read_loop(generation, reader, writer, pending));
        let me = self.clone();
        thread::spawn(move || me.wait_loop(generation, child));
        Ok(())
    }

    fn read_loop(
        &self,
        generation: u64,
        mut reader: LineReader,
        writer: Arc<Mutex<LineWriter>>,
        pending: Arc<Mutex<HashMap<u64, Sender<Response>>>>,
    ) {
        loop {
            let msg = match reader.read_message() {
                Ok(Some(m)) => m,
                Ok(None) => break,
                Err(e) => {
                    tracing::debug!(module = %self.id, "module stream: {e}");
                    break;
                }
            };
            match msg {
                Message::Request(req) => {
                    let resp = self.dispatcher.handle(&req);
                    if lock(&writer)
                        .write_message(&Message::Response(resp))
                        .is_err()
                    {
                        break;
                    }
                }
                Message::Notification(n) => self.dispatcher.notification(&n),
                Message::Response(resp) => {
                    let id = match &resp.id {
                        avada_module_sdk::contract::Id::Num(n) => Some(*n),
                        _ => None,
                    };
                    if let Some(tx) = id.and_then(|n| lock(&pending).remove(&n)) {
                        let _ = tx.send(resp);
                    }
                }
            }
        }
        // Whoever is still waiting on a response will never get one.
        lock(&pending).clear();
        let _ = generation;
    }

    fn wait_loop(self: Arc<Self>, generation: u64, child: Arc<Mutex<Child>>) {
        let exit = loop {
            // Bind the poll result first: a guard living in the scrutinee would stay
            // held across the arms, and `shutdown` needs the same lock to kill.
            let polled = lock(&child).try_wait();
            match polled {
                Ok(Some(status)) => break Some(status),
                Ok(None) => thread::sleep(Duration::from_millis(15)),
                Err(_) => break None,
            }
        };
        let expected = {
            let mut st = lock(&self.state);
            if st.generation != generation {
                return;
            }
            if let Some(live) = st.live.take() {
                live.closer.close();
            }
            st.expected_exit
        };
        self.shared.rail_events.send(RailEvent::Gone {
            module: self.id.clone(),
        });
        *lock(&self.dispatcher.rail) = RailState::default();
        if expected {
            return;
        }
        let code = exit
            .and_then(|s| s.code())
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".into());
        tracing::warn!(module = %self.id, code, "module exited unexpectedly");
        let verdict = lock(&self.supervisor).on_crash(Instant::now());
        match verdict {
            Verdict::Restart { delay, attempt } => {
                self.set_status(ModuleStatus::Crashed { restarts: attempt });
                self.toast(
                    format!(
                        "{} crashed (exit {code}); restarting in {} ms",
                        self.id,
                        delay.as_millis()
                    ),
                    "warn",
                );
                thread::sleep(delay);
                {
                    let st = lock(&self.state);
                    if st.generation != generation || st.expected_exit {
                        return;
                    }
                }
                if let Err(e) = self.start() {
                    tracing::warn!(module = %self.id, "restart failed: {e}");
                }
            }
            Verdict::Disable { reason } => {
                self.toast(format!("{} disabled: {reason}", self.id), "error");
                self.set_status(ModuleStatus::Disabled { reason });
            }
        }
    }

    fn request(&self, method: &str, params: Value) -> Result<Value, HostError> {
        let (writer, pending, id) = {
            let st = lock(&self.state);
            let live = st
                .live
                .as_ref()
                .ok_or_else(|| HostError::NotRunning(self.id.clone()))?;
            (
                live.writer.clone(),
                live.pending.clone(),
                live.next_id.fetch_add(1, Ordering::Relaxed),
            )
        };
        let (tx, rx) = mpsc::channel();
        lock(&pending).insert(id, tx);
        let req = Request::new(id, method, params);
        if let Err(e) = lock(&writer).write_message(&Message::Request(req)) {
            lock(&pending).remove(&id);
            return Err(HostError::Io(e));
        }
        match rx.recv_timeout(self.config.call_timeout) {
            Ok(resp) => match (resp.result, resp.error) {
                (_, Some(e)) => Err(HostError::Rpc(e)),
                (Some(v), None) => Ok(v),
                (None, None) => Ok(Value::Null),
            },
            Err(mpsc::RecvTimeoutError::Timeout) => {
                lock(&pending).remove(&id);
                Err(HostError::Timeout)
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(HostError::Closed),
        }
    }

    /// `module.shutdown`, grace, kill. Idempotent.
    fn shutdown(&self, reason: &str) {
        let live = {
            let mut st = lock(&self.state);
            st.expected_exit = true;
            match st.live.as_ref() {
                Some(l) => (l.writer.clone(), l.child.clone()),
                None => {
                    if st.status.is_live() {
                        drop(st);
                        self.set_status(ModuleStatus::Disabled {
                            reason: reason.into(),
                        });
                    }
                    return;
                }
            }
        };
        self.set_status(ModuleStatus::Disabled {
            reason: reason.into(),
        });
        let (writer, child) = live;
        let n = Notification::new(methods::MODULE_SHUTDOWN, Value::Null);
        let _ = lock(&writer).write_message(&Message::Notification(n));
        let deadline = Instant::now() + self.config.shutdown_grace;
        loop {
            // Same as `wait_loop`: take the poll result, then release the lock, or the
            // kill below would wait on a guard this very thread still holds.
            let polled = lock(&child).try_wait();
            match polled {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                _ => {
                    tracing::warn!(module = %self.id, "did not exit in time; killing");
                    let mut c = lock(&child);
                    let _ = c.kill();
                    let _ = c.wait();
                    break;
                }
            }
        }
        // Let the waiter thread notice and emit `Gone` before we return, so a caller
        // that polls status/rail right after sees a consistent picture.
        let deadline = Instant::now() + Duration::from_millis(500);
        while lock(&self.state).live.is_some() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
    }
}
