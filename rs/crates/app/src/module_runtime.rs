//! Running the installed modules beside the GUI.
//!
//! Everything either side of this file has existed for a while: `core::module::Host` knows
//! how to spawn, hash-check, supervise and talk to a module process; `app::leftpanel` knows
//! how to project a module's rail entries and rows into the left panel; `State` queues the
//! gestures the human made on them. Nothing joined the two, so every one of those pieces
//! carried an `#[allow(dead_code)]` and a note saying "dead until the app constructs a
//! module `Host`". This is that construction.
//!
//! ## Shape
//!
//! [`ModuleRuntime`] is UI-thread-owned, exactly like [`crate::control_host::ControlHost`]:
//! the GUI calls [`ModuleRuntime::poll`] once per tick, then [`ModuleRuntime::apply`] and
//! [`ModuleRuntime::submit`] once per open window, and nothing else touches it. But
//! unlike the control host it cannot do its work inline, because every host→module call
//! ([`Host::activate`], [`Host::call`], [`Host::activate_row`]) blocks on a child process
//! until it answers or the 5 s call timeout fires. Blocking the UI thread on a module is
//! not an option, so the runtime owns **one worker thread** and posts [`Job`]s to it. The
//! answers never come back through that channel — they arrive as `RailEvent`s and
//! `HostEvent`s on the host's own fan-out receivers, which `sync` drains. That is the whole
//! concurrency story: requests out on a queue, facts in on two queues.
//!
//! ## What gets spawned
//!
//! Every install the store reports as `Ok` and `active`, minus anything explicitly disabled
//! in the open workspace. Absence from a workspace's `modules.json` means "not mentioned",
//! which is enabled — the state file records decisions, not defaults. A `Broken` record is
//! logged and skipped; it is already visible in the marketplace UI as broken, and refusing
//! to start the *other* modules because one directory was tampered with would be a denial of
//! service by way of a bad neighbour.
//!
//! The capability gate is [`DeclaredOnly`] seeded from each record's signed `accepted` set,
//! so a module can only reach the host methods the human agreed to at install time.
//!
//! A module whose manifest says `commercial` also needs a licence decision, and the host
//! reads that decision synchronously from a cache. [`decide_license`] fills the cache on
//! the worker thread just before each start; see its note for why the issuer binding
//! matters as much as the refresh does.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;

use serde_json::Value;

use avada_core::control::modules::attach_host;
use avada_core::control::server::Shared;
use avada_core::install::dirs::InstallPaths;
use avada_core::install::store::{InstallStore, Installed, RecordStatus};
use avada_core::install::{FileKeyStore, KeyStore};
use avada_core::license::{CachedGate, Gate, LicenseService};
use avada_core::marketplace::state_dir_beside;
use avada_core::marketplace::workspace::WorkspaceStates;
use avada_core::module::grid::{GridKey, GridResize};
use avada_core::module::{
    DeclaredOnly, Gesture, Host, HostConfig, HostEvent, RailEvent, RowActivate,
};
use avada_core::rights::{InstallRecord, ModuleId};

use crate::leftpanel::{RailGesture, RailRequest};
use crate::prefs::rights::Applied;
use crate::state::State;

/// The licence service and the synchronous gate the module host reads.
///
/// Kept together because neither is useful alone: `Slot::start` runs on whichever thread
/// asked for the module and cannot await, so it reads [`CachedGate`], which answers only
/// from decisions the service made earlier. Something has to make those decisions, and
/// [`decide_license`] is it.
struct Licenses {
    service: Arc<LicenseService>,
    gate: Arc<CachedGate>,
}

/// Give the host a licence decision for `record` before it is asked to spawn it, and bind
/// the product to the issuer its manifest names.
///
/// The binding is the security half. Without it `LicenseService` falls back to whatever
/// issuer the *stored token* claims, so a token minted by anybody's issuer would unlock a
/// commercial module; with it, a token from any issuer but the one the manifest declares
/// is refused. The refresh is the liveness half: a gate nothing refreshed refuses
/// everything, and says so.
///
/// Done per start rather than once at boot so a module installed, licensed or restarted
/// mid-session gets a decision made now rather than one made before its licence existed.
/// `None` for a module whose manifest does not say `commercial` — the host never asks
/// about those, so deciding would only cache an answer nobody reads.
async fn decide_license(licenses: &Licenses, record: &InstallRecord) -> Option<Gate> {
    let dist = &record.manifest.distribution;
    if !dist.commercial {
        return None;
    }
    let product = record.module_id.to_string();
    if let Some(issuer) = &dist.issuer {
        licenses.service.register_issuer(&product, issuer);
    }
    let gate = licenses.gate.refresh(&product, record.version.major).await;
    if let Gate::Refuse(reason) = &gate {
        tracing::info!(module = %product, "not licensed: {reason}");
    }
    Some(gate)
}

/// One blocking piece of host→module work, posted to the worker thread.
///
/// Deliberately owned data rather than borrows: the worker outlives any one tick, and a
/// job that borrowed from `State` would pin the `RefCell` borrow across a call that can
/// take five seconds.
enum Job {
    /// Hash-check and start a module, then activate it so it projects its rail.
    Start {
        record: Box<InstallRecord>,
        binary: PathBuf,
    },
    /// `module.activate` — the human selected the module's rail entry.
    Activate(ModuleId),
    /// `module.row.activate` — a row under the active entry was clicked.
    Row {
        module: ModuleId,
        row: Box<RowActivate>,
    },
    /// `module.event` fan-out to every subscriber of `kind`.
    Emit { kind: String, payload: Value },
    /// Stop every module and end the worker.
    Stop,
}

/// A pane operation a module asked for, waiting for the app to carry it out.
///
/// The host has already answered the module, so these are announcements, not requests:
/// dropping one is a missed pane, never a stuck module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneOp {
    /// Open a pane (`host.panes.spawn`).
    Spawn {
        /// Which module asked.
        module: ModuleId,
        /// The id the host already handed the module.
        pane_id: String,
        /// `file` or `module`.
        kind: String,
        /// The file to open, for `kind: "file"`.
        path: Option<String>,
        /// The module surface to show, for `kind: "module"`.
        surface: Option<String>,
    },
    /// Feed bytes to a pane's input (`host.panes.input`).
    Input {
        /// The pane, as minted by an earlier spawn.
        pane_id: String,
        /// The bytes, exactly as the module sent them.
        text: String,
    },
}

/// A saved-workspace operation a module asked for, waiting for the app to carry it out.
///
/// Same shape and same promise as [`PaneOp`]: the host answered `{}` the moment the
/// request arrived, because "open a workspace" happens on the UI thread and a module that
/// waited for it would block its own request loop. A path that no longer resolves is the
/// app's problem to report, never an error the module sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceOp {
    /// Open a saved workspace or set (`host.workspace.open`).
    ///
    /// One variant for both drawers on purpose: the path is the only thing the module was
    /// given, and which drawer it came from is decided by reading the file, not by
    /// trusting a flag a module could get wrong.
    Open {
        /// A path the module got back from `host.workspace.list`.
        path: String,
    },
    /// Save the current workspace (`host.workspace.save`).
    Save {
        /// The name to save under; `None` means "ask the human", exactly as the panel's
        /// own Save button does.
        name: Option<String>,
        /// Save into the sets drawer rather than the library.
        as_set: bool,
    },
}

/// One tick's worth of what the modules said, drained once and folded into every window.
#[derive(Debug, Default, Clone)]
pub struct ModuleTick {
    /// Rail registrations, row projections and departures, in the order they arrived.
    pub rail: Vec<RailEvent>,
    /// Text a module asked to be shown to the human.
    pub toasts: Vec<String>,
    /// Whether a tier-5 surface repainted or redeclared its keymap this tick. The frame
    /// itself went straight into [`crate::module_ui::grid`] — this is only the "something
    /// moved" flag, because a repaint is a redraw and not a change to [`State`].
    pub grid: bool,
}

/// The module host, its worker thread, and the two event streams the GUI folds into
/// [`State`] each tick.
pub struct ModuleRuntime {
    /// `None` when the store could not be opened at all — the app runs without modules
    /// rather than refusing to start.
    host: Option<Host>,
    store: Option<Arc<InstallStore>>,
    gate: Arc<DeclaredOnly>,
    rail_rx: Option<Receiver<RailEvent>>,
    events_rx: Option<Receiver<HostEvent>>,
    jobs: Option<Sender<Job>>,
    worker: RefCell<Option<JoinHandle<()>>>,
    /// Whether the control server has already been handed this host. Idempotent because
    /// the server can be stopped and restarted from Preferences, and each restart makes a
    /// fresh `Shared` that needs the invoker again.
    attached: Cell<bool>,
    panes: RefCell<Vec<PaneOp>>,
    workspaces: RefCell<Vec<WorkspaceOp>>,
    /// Host pane id → the session uid the app opened for it.
    ///
    /// The host mints a uuid inside `host.panes.spawn` and answers the module with it
    /// before the pane exists, because the pane lives on the UI thread and a module that
    /// waited for it would block its own request loop. So the id a module types into is
    /// never the app's own session uid, and this is the only place the two meet.
    pane_uids: RefCell<std::collections::HashMap<String, String>>,
    /// `<owner/repo>#<surface>` → the cell size that surface was last told about.
    ///
    /// The layout pass offers a size every tick once a pane has settled, because it has no
    /// memory of its own; this is the memory. Without it a module would be woken by a
    /// `module.grid.resize` at the pump's cadence and spend its life repainting the same
    /// picture.
    grid_sizes: RefCell<std::collections::HashMap<String, (u16, u16)>>,
}

impl ModuleRuntime {
    /// The production runtime: the install store under the app-support modules root, the
    /// licence gate beside it, and every enabled module started.
    pub fn new() -> ModuleRuntime {
        let root = InstallPaths::host().root().to_path_buf();
        ModuleRuntime::under(&root, None)
    }

    /// The same runtime rooted at `modules_root`, with `workspace` deciding which modules
    /// are disabled. Tests root it in a temp dir so nothing under the real app-support
    /// directory is touched.
    ///
    /// A store that will not open leaves a runtime with no host: [`ModuleRuntime::sync`]
    /// then does nothing, which is exactly the behaviour of a build with no modules
    /// installed. That is a better failure than taking the GUI down over a directory the
    /// user can delete.
    pub fn under(modules_root: &Path, workspace: Option<&str>) -> ModuleRuntime {
        let gate = Arc::new(DeclaredOnly::new());
        let paths = InstallPaths::under(modules_root);
        let keys: Arc<dyn KeyStore> = Arc::new(FileKeyStore::new(paths.keys_dir()));
        let store = match InstallStore::open(paths, keys) {
            Ok(s) => Arc::new(s),
            Err(e) => {
                tracing::warn!(error = %e, "modules unavailable; none will start");
                return ModuleRuntime {
                    host: None,
                    store: None,
                    gate,
                    rail_rx: None,
                    events_rx: None,
                    jobs: None,
                    worker: RefCell::new(None),
                    attached: Cell::new(false),
                    panes: RefCell::new(Vec::new()),
                    workspaces: RefCell::new(Vec::new()),
                    pane_uids: RefCell::new(std::collections::HashMap::new()),
                    grid_sizes: RefCell::new(std::collections::HashMap::new()),
                };
            }
        };

        // A sibling of the modules root, never inside it: `InstallStore::records` walks
        // every child of the root expecting `<owner>__<repo>`, and a `data` directory in
        // there would be reported as a broken install on every scan.
        let data_root = modules_root
            .parent()
            .unwrap_or(modules_root)
            .join("module-data");
        let host = Host::new(HostConfig::new(data_root), gate.clone());
        let service = Arc::new(LicenseService::under(modules_root));
        let licenses = Arc::new(Licenses {
            gate: CachedGate::new(service.clone()),
            service,
        });
        host.set_licensing(licenses.gate.clone());

        // Subscribe BEFORE anything spawns, or the first rail registration — which arrives
        // during the handshake — is sent to nobody and the panel starts empty.
        let rail_rx = host.rail_events();
        let events_rx = host.events();

        let (tx, rx) = channel::<Job>();
        let worker_host = host.clone();
        let worker = std::thread::Builder::new()
            .name("module-runtime".into())
            .spawn(move || run_worker(worker_host, licenses, rx))
            .ok();
        if worker.is_none() {
            tracing::warn!("could not start the module worker thread; modules will not run");
        }

        let rt = ModuleRuntime {
            host: Some(host),
            store: Some(store),
            gate,
            rail_rx: Some(rail_rx),
            events_rx: Some(events_rx),
            jobs: worker.is_some().then(|| tx.clone()),
            worker: RefCell::new(worker),
            attached: Cell::new(false),
            panes: RefCell::new(Vec::new()),
            workspaces: RefCell::new(Vec::new()),
            pane_uids: RefCell::new(std::collections::HashMap::new()),
            grid_sizes: RefCell::new(std::collections::HashMap::new()),
        };
        rt.start_installed(modules_root, workspace);
        rt
    }

    /// Queue a start for every install that should be running.
    fn start_installed(&self, modules_root: &Path, workspace: Option<&str>) {
        let Some(store) = self.store.as_ref() else {
            return;
        };
        for installed in installs_to_start(store, modules_root, workspace) {
            let record = installed.rights().clone();
            self.gate.insert(&record);
            self.post(Job::Start {
                record: Box::new(record),
                binary: installed.binary.clone(),
            });
        }
    }

    /// Hand the control server this host, so `/m/<owner>/<repo>/...` routes reach the
    /// modules. Cheap and idempotent per server instance; call it whenever the server
    /// starts.
    pub fn attach_control(&self, shared: &Arc<Shared>) {
        let Some(host) = self.host.as_ref() else {
            return;
        };
        if self.attached.replace(true) {
            return;
        }
        attach_host(shared.clone(), host);
    }

    /// The control server stopped; the next start needs the invoker installed again.
    pub fn detach_control(&self) {
        self.attached.set(false);
    }

    /// Drain everything the modules have said since the last tick.
    ///
    /// Split from [`ModuleRuntime::apply`] because a window is not the unit a module talks
    /// to: the host says a thing once, and every open window's left panel has to learn it.
    /// Draining per window would give the first window all the events and the rest none.
    pub fn poll(&self) -> ModuleTick {
        let mut tick = ModuleTick::default();
        if let Some(rx) = self.rail_rx.as_ref() {
            while let Ok(event) = rx.try_recv() {
                tick.rail.push(event);
            }
        }
        if let Some(rx) = self.events_rx.as_ref() {
            while let Ok(event) = rx.try_recv() {
                self.fold_event(event, &mut tick);
            }
        }
        tick
    }

    /// Fold one drained tick into one window's panel state. Returns whether it changed.
    pub fn apply(&self, tick: &ModuleTick, st: &mut State) -> bool {
        let mut dirty = false;
        for event in &tick.rail {
            st.apply_rail_event(event.clone());
            dirty = true;
        }
        for text in &tick.toasts {
            st.toast_active(text);
            dirty = true;
        }
        // A repaint changes nothing in `State` — the frame lives in the grid store — but
        // the window still has to be told to draw again, or an editor would only move when
        // something else happened to move too.
        dirty |= tick.grid;
        dirty
    }

    /// Post everything this window queued — rail gestures, module events, rights
    /// decisions — to the worker thread.
    pub fn submit(&self, st: &mut State) {
        self.drain_requests(st);
    }

    /// Pane operations the modules asked for since the last drain. The app applies them;
    /// the runtime deliberately does not know how a pane is made.
    pub fn take_pane_ops(&self) -> Vec<PaneOp> {
        std::mem::take(&mut self.panes.borrow_mut())
    }

    /// Workspace operations the modules asked for since the last drain. Drained beside
    /// [`Self::take_pane_ops`] and for the same reason: the runtime knows what was asked
    /// for, the app knows how to do it.
    pub fn take_workspace_ops(&self) -> Vec<WorkspaceOp> {
        std::mem::take(&mut self.workspaces.borrow_mut())
    }

    /// Remember which session a module's pane became, so later `host.panes.input` for that
    /// id reaches the right PTY.
    pub fn record_pane(&self, pane_id: &str, uid: &str) {
        self.pane_uids
            .borrow_mut()
            .insert(pane_id.to_string(), uid.to_string());
    }

    /// The session uid for a module's pane id, if the app ever opened it.
    pub fn pane_uid(&self, pane_id: &str) -> Option<String> {
        self.pane_uids.borrow().get(pane_id).cloned()
    }

    /// Forget a pane the app has closed. Input for it is dropped from then on, which is
    /// what the contract promises a module: an unknown id is a race, not an error.
    pub fn forget_pane(&self, pane_id: &str) {
        self.pane_uids.borrow_mut().remove(pane_id);
    }

    /// Hand a resolved keystroke to a tier-5 module (`module.grid.key`).
    ///
    /// A notification, not a call: it is one framed write to the child's stdin and never
    /// waits for an answer, which is the only reason this may run on the UI thread at all.
    /// A module that has stopped simply does not receive it — a keystroke into a dead
    /// editor is a missed keystroke, not an error the human should see.
    pub fn grid_key(&self, module: &ModuleId, key: &GridKey) {
        if let Some(host) = self.host.as_ref() {
            if let Err(e) = host.grid_key(module, key) {
                tracing::debug!(module = %module.as_str(), "grid key dropped: {e}");
            }
        }
    }

    /// Tell a tier-5 module its surface is a different size now (`module.grid.resize`).
    ///
    /// Same one-way shape as [`Self::grid_key`], and sent only when the cell count actually
    /// changed — the app relayouts on every window resize, and a module that got a
    /// notification per frame would spend its life repainting the same picture.
    pub fn grid_resize(&self, module: &ModuleId, resize: &GridResize) {
        let key = crate::leftpanel::entry_key(module, &resize.surface);
        if self.grid_sizes.borrow().get(&key) == Some(&(resize.cols, resize.rows)) {
            return;
        }
        if let Some(host) = self.host.as_ref() {
            if let Err(e) = host.grid_resize(module, resize) {
                tracing::debug!(module = %module.as_str(), "grid resize dropped: {e}");
                return;
            }
        }
        self.grid_sizes
            .borrow_mut()
            .insert(key, (resize.cols, resize.rows));
    }

    /// The cell size a surface was last told about, for the layout pass's own tests.
    #[cfg(test)]
    pub(crate) fn grid_size(&self, module: &ModuleId, surface: &str) -> Option<(u16, u16)> {
        let key = crate::leftpanel::entry_key(module, surface);
        self.grid_sizes.borrow().get(&key).copied()
    }

    fn fold_event(&self, event: HostEvent, tick: &mut ModuleTick) {
        match event {
            HostEvent::Toast {
                module,
                text,
                level,
            } => {
                tracing::debug!(module = %module.as_str(), %level, "module toast: {text}");
                tick.toasts.push(text);
            }
            HostEvent::PaneSpawn {
                module,
                pane_id,
                kind,
                path,
                surface,
            } => self.panes.borrow_mut().push(PaneOp::Spawn {
                module,
                pane_id,
                kind,
                path,
                surface,
            }),
            HostEvent::PaneInput { pane_id, text, .. } => self
                .panes
                .borrow_mut()
                .push(PaneOp::Input { pane_id, text }),
            HostEvent::WorkspaceOpen { path, .. } => self
                .workspaces
                .borrow_mut()
                .push(WorkspaceOp::Open { path }),
            HostEvent::WorkspaceSave { name, as_set, .. } => self
                .workspaces
                .borrow_mut()
                .push(WorkspaceOp::Save { name, as_set }),
            // The rail already learns a module is gone through `RailEvent::Gone`, and the
            // control plane unmounts its routes in `attach_host`'s own thread. Nothing
            // left for the panel to do.
            HostEvent::Status { module, status } => {
                tracing::debug!(module = %module.as_str(), ?status, "module status")
            }
            // A frame belongs to the pane that shows it, not to the panel, so it goes
            // straight into the grid store — the same shape as the row store, and safe
            // here because `poll` runs on the window thread.
            HostEvent::Grid { module, frame } => {
                crate::module_ui::grid::set_frame(&module, frame);
                tick.grid = true;
            }
            HostEvent::Keymap { module, keymap } => {
                crate::module_ui::grid::set_keymap(&module, keymap);
                tick.grid = true;
            }
            // Commands, prefs pages and routes have their own consumers (the palette, the
            // Preferences page, `attach_host`) and none of them is the left panel.
            HostEvent::Commands { .. }
            | HostEvent::PrefsDeclared { .. }
            | HostEvent::Routes { .. } => {}
        }
    }

    fn drain_requests(&self, st: &mut State) {
        for req in st.take_rail_requests() {
            match req {
                // The contract has no "entry selected" method: a module projects its rail
                // in response to `module.activate`, which is exactly what a fresh
                // selection wants. A module with several entries therefore re-projects all
                // of them, which is cheap and idempotent by design (`host.rail.register`
                // replaces the whole set).
                RailRequest::Activate { module, .. } => self.post(Job::Activate(module)),
                RailRequest::Row {
                    module,
                    entry,
                    target,
                    row,
                    data,
                    gesture,
                } => self.post(Job::Row {
                    module,
                    row: Box::new(RowActivate {
                        entry,
                        target,
                        row,
                        data,
                        gesture: wire_gesture(gesture),
                    }),
                }),
            }
        }
        for (kind, payload) in st.take_module_events() {
            self.post(Job::Emit { kind, payload });
        }
        for effect in st.take_rights_effects() {
            self.apply_rights(effect);
        }
    }

    /// Carry out one decision the rights page made that only the install store can finish.
    fn apply_rights(&self, effect: Applied) {
        let Some(store) = self.store.as_ref() else {
            return;
        };
        match effect {
            Applied::Accepted { module, accepted } => {
                // The held update was agreed to: the record on disk has to carry the new
                // set before anything runs with it, and the live gate has to agree with
                // the record or the module would be denied a capability it was just
                // granted.
                let found = store.record(&module);
                match found {
                    Ok(Some(installed)) => {
                        match store.re_sign(&module, &installed.version, accepted) {
                            Ok(signed) => self.gate.insert(signed.rights()),
                            Err(e) => {
                                tracing::warn!(error = %e, module = %module.as_str(), "could not re-sign the record")
                            }
                        }
                    }
                    Ok(None) => {
                        tracing::warn!(module = %module.as_str(), "accepted an update for a module that is not installed")
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, module = %module.as_str(), "install store unreadable")
                    }
                }
            }
            // Nothing on disk changes: the held update stays held and the module keeps
            // running the version it already has.
            Applied::Rejected(module) => {
                tracing::info!(module = %module.as_str(), "held update rejected")
            }
            // Unreachable with the gate this runtime installs: `DeclaredOnly` answers
            // every check from the signed accepted set and never returns `Ask`, so no
            // request is ever left waiting for an answer. Logged rather than dropped so
            // that a future ask-capable gate shows up here instead of silently doing
            // nothing.
            Applied::Answered { ask, .. } => {
                tracing::debug!(module = %ask.module.as_str(), "ask answered with no request waiting")
            }
            Applied::Nothing | Applied::Written | Applied::Selected(_) => {}
        }
    }

    fn post(&self, job: Job) {
        let Some(tx) = self.jobs.as_ref() else {
            return;
        };
        if tx.send(job).is_err() {
            tracing::warn!("the module worker is gone; dropping the request");
        }
    }
}

impl Default for ModuleRuntime {
    fn default() -> Self {
        ModuleRuntime::new()
    }
}

impl Drop for ModuleRuntime {
    fn drop(&mut self) {
        // Ask the worker to shut the modules down and wait for it. Dropping the `Host`
        // would kill them anyway, but only after the child processes are already orphaned
        // mid-request; `module.shutdown` with its grace period is the polite door.
        if let Some(tx) = self.jobs.take() {
            let _ = tx.send(Job::Stop);
        }
        if let Some(w) = self.worker.borrow_mut().take() {
            let _ = w.join();
        }
    }
}

/// Which installs should be running in `workspace`.
///
/// Split out from [`ModuleRuntime::start_installed`] so the answer can be tested without
/// spawning anything: this is the whole of the "which modules run" policy, and every one
/// of its rules — a broken directory is skipped rather than fatal, an inactive version is
/// not started, a workspace opt-out beats the lockfile default — is a decision somebody
/// will want to check.
fn installs_to_start(
    store: &InstallStore,
    modules_root: &Path,
    workspace: Option<&str>,
) -> Vec<Installed> {
    // Beside the modules root, not inside it: `Marketplace::new` puts the workspace state
    // in `state_dir_beside(root)`, and reading it anywhere else would silently ignore
    // every per-workspace opt-out the human made through the marketplace.
    let states = WorkspaceStates::under(&state_dir_beside(modules_root));
    // Read once: the lockfile carries the per-module default that applies to any workspace
    // which has not said otherwise, and re-reading it per module would let a concurrent
    // install change the answer half way down the list.
    let lock = match store.lockfile() {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(error = %e, "lockfile unreadable; no modules will start");
            return Vec::new();
        }
    };
    let workspace_state = workspace.and_then(|key| match states.get(key) {
        Ok(state) => Some(state),
        Err(e) => {
            tracing::warn!(error = %e, workspace = key, "workspace module state unreadable; using the defaults");
            None
        }
    });
    let records = match store.records() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "could not scan the install store");
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for status in records {
        let installed = match status {
            RecordStatus::Ok(i) => i,
            RecordStatus::Broken {
                id, dir, reason, ..
            } => {
                tracing::warn!(module = ?id, dir = %dir.display(), %reason, "broken install; not starting it");
                continue;
            }
        };
        if !installed.active {
            continue;
        }
        let enabled = match workspace_state.as_ref() {
            Some(state) => state.is_enabled(&installed.id, &lock),
            None => lock.defaults.get(&installed.id).copied().unwrap_or(true),
        };
        if !enabled {
            continue;
        }
        out.push(*installed);
    }
    out
}

/// The app's panel-side gesture as the contract spells it.
fn wire_gesture(g: RailGesture) -> Gesture {
    match g {
        RailGesture::Open => Gesture::Open,
        RailGesture::Toggle => Gesture::Toggle,
        RailGesture::Context => Gesture::Context,
    }
}

/// The worker loop: every blocking host→module call in the app happens here.
fn run_worker(host: Host, licenses: Arc<Licenses>, rx: Receiver<Job>) {
    // One runtime for the worker's whole life. The only async work here is
    // `decide_license`, which for an installed licence is a signature check against keys
    // already on disk, so a current-thread runtime is the right size; it reaches the
    // network only the first time it meets an unknown signing key. A runtime that will
    // not build costs the commercial modules and nothing else, which is why this is a
    // warning and not a return.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .inspect_err(|e| tracing::warn!(error = %e, "no runtime for license checks; commercial modules will not start"))
        .ok();
    while let Ok(job) = rx.recv() {
        match job {
            Job::Start { record, binary } => {
                let id = record.module_id.clone();
                if let Some(rt) = rt.as_ref() {
                    rt.block_on(decide_license(&licenses, &record));
                }
                match host.spawn(&record, &binary) {
                    // Activating right after the handshake is what makes the entry appear:
                    // a module registers its rail from inside `module.activate`.
                    Ok(()) => {
                        if let Err(e) = host.activate(&id, None) {
                            tracing::warn!(error = %e, module = %id.as_str(), "module.activate failed");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, module = %id.as_str(), "module would not start")
                    }
                }
            }
            Job::Activate(id) => {
                if let Err(e) = host.activate(&id, None) {
                    tracing::warn!(error = %e, module = %id.as_str(), "module.activate failed");
                }
            }
            Job::Row { module, row } => {
                if let Err(e) = host.activate_row(&module, &row) {
                    tracing::warn!(error = %e, module = %module.as_str(), "module.row.activate failed");
                }
            }
            Job::Emit { kind, payload } => {
                host.emit(&kind, payload);
            }
            Job::Stop => {
                host.shutdown_all();
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use avada_core::install::keyring::MemoryKeyStore;
    use avada_core::license::stub_issuer::{Grant, StubIssuer};
    use avada_core::license::{LicenseHttp, LicenseToken, MemoryLicenseStore};
    use avada_core::module::RailEvent;
    use avada_core::rights::{Capability, DistributionKind, InstallKind, Manifest};
    use std::collections::{BTreeSet, HashMap};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A manifest with no `[skills]` section, so an install needs no checkout to stage
    /// from and every test here is a pure store round trip.
    fn manifest(id: &str) -> Manifest {
        Manifest::parse(&format!(
            r#"
capabilities = ["ui.rail"]

[module]
id = "{id}"
name = "Test"
version = "1.0.0"
description = "A module that exists only in this test"
publisher = "Test"
contract = "^1"

[distribution]
kind = "source"

[[contributions]]
kind = "rail"
id = "tree"
tier = 1
label = "Tree"
"#
        ))
        .unwrap()
    }

    fn record(id: &str) -> InstallRecord {
        let manifest = manifest(id);
        InstallRecord {
            module_id: manifest.id().clone(),
            repo: format!("https://github.com/{id}"),
            tag: manifest.tag(),
            commit: "0123456789abcdef0123456789abcdef01234567".into(),
            version: manifest.module.version.clone(),
            artifact_sha256: String::new(),
            skills_sha256: String::new(),
            source: DistributionKind::Source,
            accepted: BTreeSet::from([Capability::UiRail]),
            manifest,
            installed_at: 1_700_000_000,
            kind: InstallKind::Manual,
        }
    }

    /// A scratch modules root under the OS temp dir, named so two tests running at once
    /// never share one. `<base>/modules` and never `<base>` itself: the workspace state
    /// lives *beside* the root, and a root at the top of the temp dir would put it in
    /// `$TMPDIR` where the whole suite would share it.
    fn scratch(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir().join(format!(
            "avada-modrt-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let root = base.join("modules");
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// A store over `root`, plus an installed copy of every id given.
    fn store_with(root: &Path, ids: &[&str]) -> InstallStore {
        let keys: Arc<dyn KeyStore> = Arc::new(MemoryKeyStore::new());
        let store = InstallStore::open(InstallPaths::under(root), keys).unwrap();
        let artifact = root.parent().unwrap().join("artifact");
        std::fs::write(&artifact, b"#!/bin/sh\nexit 0\n").unwrap();
        for id in ids {
            store.install(record(id), &artifact, None).unwrap();
        }
        store
    }

    /// A runtime with no host: every field the pure-folding tests touch, and nothing that
    /// spawns a process. Construction is the same shape `under` falls back to when the
    /// store will not open.
    fn hostless() -> ModuleRuntime {
        ModuleRuntime {
            host: None,
            store: None,
            gate: Arc::new(DeclaredOnly::new()),
            rail_rx: None,
            events_rx: None,
            jobs: None,
            worker: RefCell::new(None),
            attached: Cell::new(false),
            panes: RefCell::new(Vec::new()),
            workspaces: RefCell::new(Vec::new()),
            pane_uids: RefCell::new(HashMap::new()),
            grid_sizes: RefCell::new(HashMap::new()),
        }
    }

    fn id(s: &str) -> ModuleId {
        ModuleId::new(s).unwrap()
    }

    #[test]
    fn every_active_install_starts_when_no_workspace_says_otherwise() {
        let root = scratch("plain");
        let store = store_with(&root, &["acme/avada-one", "acme/avada-two"]);
        let starting: Vec<String> = installs_to_start(&store, &root, None)
            .iter()
            .map(|i| i.id.as_str().to_string())
            .collect();
        assert_eq!(starting, ["acme/avada-one", "acme/avada-two"]);
    }

    /// The opt-out the human made in the marketplace has to be the one the app reads at
    /// startup. It is written beside the modules root, not inside it, and this test fails
    /// if the two ever disagree about where that is.
    #[test]
    fn a_workspace_opt_out_keeps_a_module_from_starting() {
        let root = scratch("optout");
        let store = store_with(&root, &["acme/avada-one", "acme/avada-two"]);
        let states = WorkspaceStates::under(&state_dir_beside(&root));
        states
            .set_enabled("proj", &id("acme/avada-one"), false)
            .unwrap();

        let starting: Vec<String> = installs_to_start(&store, &root, Some("proj"))
            .iter()
            .map(|i| i.id.as_str().to_string())
            .collect();
        assert_eq!(starting, ["acme/avada-two"]);

        // Another workspace never said so, and is unaffected.
        assert_eq!(installs_to_start(&store, &root, Some("other")).len(), 2);
    }

    /// One tampered-with directory is not a reason to leave the other modules dead. The
    /// broken one is skipped and logged; refusing to start its neighbours would turn a
    /// bad directory into a denial of service.
    #[test]
    fn a_broken_install_is_skipped_and_the_rest_still_start() {
        let root = scratch("broken");
        let store = store_with(&root, &["acme/avada-one"]);
        std::fs::create_dir_all(root.join("acme__ghost").join("1.0.0")).unwrap();

        let starting: Vec<String> = installs_to_start(&store, &root, None)
            .iter()
            .map(|i| i.id.as_str().to_string())
            .collect();
        assert_eq!(starting, ["acme/avada-one"]);
    }

    /// A store that will not open leaves a runtime that does nothing rather than a GUI
    /// that will not start.
    #[test]
    fn a_hostless_runtime_is_quiet() {
        let rt = hostless();
        let tick = rt.poll();
        assert!(tick.rail.is_empty() && tick.toasts.is_empty());
        assert!(rt.take_pane_ops().is_empty());
        assert!(rt.take_workspace_ops().is_empty());
        // Neither control-plane call panics without a host.
        rt.detach_control();
    }

    #[test]
    fn a_pane_spawn_becomes_a_pane_op_and_a_toast_becomes_a_toast() {
        let rt = hostless();
        let mut tick = ModuleTick::default();
        rt.fold_event(
            HostEvent::Toast {
                module: id("acme/avada-one"),
                text: "hello".into(),
                level: "info".into(),
            },
            &mut tick,
        );
        rt.fold_event(
            HostEvent::PaneSpawn {
                module: id("acme/avada-one"),
                pane_id: "p1".into(),
                kind: "file".into(),
                path: Some("/tmp/README.md".into()),
                surface: None,
            },
            &mut tick,
        );
        rt.fold_event(
            HostEvent::PaneInput {
                module: id("acme/avada-one"),
                pane_id: "p1".into(),
                text: "ls\n".into(),
            },
            &mut tick,
        );
        // Routes, commands and prefs have other consumers; none of them is the panel.
        rt.fold_event(
            HostEvent::Routes {
                module: id("acme/avada-one"),
                routes: vec![],
            },
            &mut tick,
        );

        assert_eq!(tick.toasts, ["hello"]);
        assert!(tick.rail.is_empty());
        assert_eq!(
            rt.take_pane_ops(),
            vec![
                PaneOp::Spawn {
                    module: id("acme/avada-one"),
                    pane_id: "p1".into(),
                    kind: "file".into(),
                    path: Some("/tmp/README.md".into()),
                    surface: None,
                },
                PaneOp::Input {
                    pane_id: "p1".into(),
                    text: "ls\n".into(),
                },
            ]
        );
        // Drained, not copied.
        assert!(rt.take_pane_ops().is_empty());
    }

    /// Opening and saving are announcements too, and they drain on their own queue: a
    /// module that asks for a workspace must not have its request folded in with panes,
    /// because the app applies the two in different orders and to different state.
    #[test]
    fn workspace_open_and_save_become_workspace_ops_on_their_own_queue() {
        let rt = hostless();
        let mut tick = ModuleTick::default();
        rt.fold_event(
            HostEvent::WorkspaceOpen {
                module: id("acme/avada-one"),
                path: "/w/api.avada".into(),
            },
            &mut tick,
        );
        rt.fold_event(
            HostEvent::WorkspaceSave {
                module: id("acme/avada-one"),
                name: Some("API work".into()),
                as_set: true,
            },
            &mut tick,
        );
        rt.fold_event(
            HostEvent::WorkspaceSave {
                module: id("acme/avada-one"),
                name: None,
                as_set: false,
            },
            &mut tick,
        );

        assert!(tick.toasts.is_empty() && tick.rail.is_empty());
        assert!(
            rt.take_pane_ops().is_empty(),
            "workspace ops must not land on the pane queue"
        );
        assert_eq!(
            rt.take_workspace_ops(),
            vec![
                WorkspaceOp::Open {
                    path: "/w/api.avada".into()
                },
                WorkspaceOp::Save {
                    name: Some("API work".into()),
                    as_set: true,
                },
                WorkspaceOp::Save {
                    name: None,
                    as_set: false,
                },
            ]
        );
        // Drained, not copied.
        assert!(rt.take_workspace_ops().is_empty());
    }

    /// The host mints a pane id before the pane exists, so the app has to remember which
    /// session that id became. An id it never opened — or one the human has closed — is a
    /// race the contract tells the app to drop, not an error.
    #[test]
    fn a_pane_id_maps_to_the_session_the_app_opened() {
        let rt = hostless();
        assert_eq!(rt.pane_uid("p1"), None);
        rt.record_pane("p1", "uid-7");
        assert_eq!(rt.pane_uid("p1").as_deref(), Some("uid-7"));
        rt.forget_pane("p1");
        assert_eq!(rt.pane_uid("p1"), None);
    }

    /// One drained tick, folded into two windows: both panels learn the entry. This is
    /// why `poll` and `apply` are separate — a per-window drain would give the first
    /// window everything and the second nothing.
    #[test]
    fn one_tick_reaches_every_window() {
        let rt = hostless();
        let entry: avada_core::module::RailEntry = serde_json::from_value(serde_json::json!({
            "id": "tree", "label": "Tree", "tier": 1, "order": 0
        }))
        .unwrap();
        let tick = ModuleTick {
            rail: vec![RailEvent::Registered {
                module: id("acme/avada-one"),
                entries: vec![entry],
            }],
            toasts: vec![],
            grid: false,
        };

        let mut first = State::new(crate::theme::load_font(1.0));
        let mut second = State::new(crate::theme::load_font(1.0));
        assert!(rt.apply(&tick, &mut first));
        assert!(rt.apply(&tick, &mut second));
        for st in [&first, &second] {
            let keys: Vec<String> = st.rail.entries().iter().map(|e| e.key.clone()).collect();
            assert_eq!(keys, ["acme/avada-one#tree"]);
        }
    }

    /// What the human did on the rail leaves the window on `submit`, once.
    #[test]
    fn submit_drains_the_window_queue() {
        let rt = hostless();
        let mut st = State::new(crate::theme::load_font(1.0));
        st.rail_requests.push(RailRequest::Activate {
            module: id("acme/avada-one"),
            entry: "tree".into(),
        });
        rt.submit(&mut st);
        assert!(st.rail_requests.is_empty());
    }

    #[test]
    fn the_three_gestures_map_onto_the_contract() {
        assert_eq!(wire_gesture(RailGesture::Open), Gesture::Open);
        assert_eq!(wire_gesture(RailGesture::Toggle), Gesture::Toggle);
        assert_eq!(wire_gesture(RailGesture::Context), Gesture::Context);
    }

    /// The layout pass re-offers a settled size on every tick, so the memory that turns that
    /// stream back into one notification per real change has to live here.
    ///
    /// It is keyed per surface, not per module: an editor and a diff in two panes of
    /// different widths must each be told their own, and neither may silence the other.
    #[test]
    fn a_surface_remembers_its_size_per_surface_and_only_a_change_is_worth_sending() {
        let rt = hostless();
        let m = id("bshuler/avada-editor");
        let resize = |surface: &str, cols: u16, rows: u16| GridResize {
            surface: surface.into(),
            cols,
            rows,
        };

        assert_eq!(
            rt.grid_size(&m, "editor"),
            None,
            "nothing told, nothing known"
        );
        rt.grid_resize(&m, &resize("editor", 80, 24));
        assert_eq!(rt.grid_size(&m, "editor"), Some((80, 24)));

        // The tick that repeats it changes nothing; the tick that moves it does.
        rt.grid_resize(&m, &resize("editor", 80, 24));
        assert_eq!(rt.grid_size(&m, "editor"), Some((80, 24)));
        rt.grid_resize(&m, &resize("editor", 100, 24));
        assert_eq!(rt.grid_size(&m, "editor"), Some((100, 24)));

        // A second surface of the same module keeps its own.
        assert_eq!(rt.grid_size(&m, "diff"), None);
        rt.grid_resize(&m, &resize("diff", 40, 24));
        assert_eq!(rt.grid_size(&m, "diff"), Some((40, 24)));
        assert_eq!(rt.grid_size(&m, "editor"), Some((100, 24)));

        // And so does the same surface name under a different module.
        let other = id("bshuler/avada-files");
        assert_eq!(rt.grid_size(&other, "editor"), None);
    }

    // --- licensing -------------------------------------------------------------------
    //
    // `Slot::start` reads a decision the gate already holds, so everything that can go
    // wrong with licensing in the shipped binary goes wrong *here*, before a process is
    // ever spawned: a gate nobody refreshed refuses every commercial module, and a
    // product bound to no issuer accepts a token from any of them.

    /// Epoch instant every licence test runs at, so a perpetual grant is unambiguously
    /// live and a check-in interval has not yet elapsed.
    const NOW: u64 = 1_700_000_000;

    /// A commercial manifest naming `issuer` as the only issuer whose tokens count.
    fn commercial_manifest(id: &str, issuer: &str) -> Manifest {
        Manifest::parse(&format!(
            r#"
capabilities = ["ui.rail"]

[module]
id = "{id}"
name = "Test"
version = "1.0.0"
description = "A commercial module that exists only in this test"
publisher = "Test"
contract = "^1"

[distribution]
kind = "source"
commercial = true
issuer = "{issuer}"

[[contributions]]
kind = "rail"
id = "tree"
tier = 1
label = "Tree"
"#
        ))
        .unwrap()
    }

    fn commercial_record(id: &str, issuer: &str) -> InstallRecord {
        InstallRecord {
            manifest: commercial_manifest(id, issuer),
            ..record(id)
        }
    }

    /// A service and gate over an in-process issuer and a store that never touches disk —
    /// the same pair `under` builds, with the network and the filesystem taken out.
    fn licenses(http: Arc<dyn LicenseHttp>) -> Licenses {
        let service = Arc::new(LicenseService::new(
            Arc::new(MemoryLicenseStore::new()),
            http,
            Arc::new(|| NOW),
        ));
        Licenses {
            gate: CachedGate::new(service.clone()),
            service,
        }
    }

    async fn install(issuer: &StubIssuer, service: &LicenseService, product: &str) {
        let token = LicenseToken::new(issuer.issue(&Grant::for_product(product)).unwrap());
        service
            .install_token(token, None)
            .await
            .expect("a freshly minted license installs");
    }

    /// A module that is not commercial is never asked about, so deciding for one would
    /// cache an answer the host will not read — and, worse, make the cache look refreshed.
    #[tokio::test]
    async fn a_free_module_is_never_asked_about() {
        let issuer = Arc::new(StubIssuer::with_clock(
            "https://issuer.test",
            Arc::new(|| NOW),
        ));
        let lic = licenses(issuer);
        assert_eq!(decide_license(&lic, &record("acme/avada-free")).await, None);
        assert_eq!(lic.gate.decided("acme/avada-free", 1), None);
    }

    /// The liveness half. `CachedGate::new` starts empty and refuses everything with
    /// "nothing has refreshed it", which is what the app shipped before this: a correct
    /// licence, correctly installed, and the module still would not start.
    #[tokio::test]
    async fn a_licensed_module_has_a_decision_waiting_before_it_is_spawned() {
        let issuer = Arc::new(StubIssuer::with_clock(
            "https://issuer.test",
            Arc::new(|| NOW),
        ));
        let lic = licenses(issuer.clone());
        install(&issuer, &lic.service, "acme/avada-pro").await;

        let record = commercial_record("acme/avada-pro", "https://issuer.test");
        assert_eq!(decide_license(&lic, &record).await, Some(Gate::Run));
        // Not just returned: left where the synchronous gate will find it, keyed by the
        // major the record carries.
        assert_eq!(lic.gate.decided("acme/avada-pro", 1), Some(Gate::Run));
    }

    /// The three tests above prove [`decide_license`] is right; this proves the worker
    /// actually calls it, which is the half that regresses silently. The binary here does
    /// not exist, so the start fails at the hash check immediately afterwards — the point
    /// is that by then the decision is already in the gate, because `Slot::start` reads it
    /// synchronously and there is no later moment at which to make it.
    #[test]
    fn the_worker_decides_before_it_tries_to_spawn() {
        let issuer = Arc::new(StubIssuer::with_clock(
            "https://issuer.test",
            Arc::new(|| NOW),
        ));
        let lic = Arc::new(licenses(issuer.clone()));
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(install(&issuer, &lic.service, "acme/avada-pro"));

        let root = scratch("worker-license");
        let host = Host::new(
            HostConfig::new(root.join("data")),
            Arc::new(DeclaredOnly::new()),
        );
        let (tx, rx) = channel::<Job>();
        let worker = std::thread::spawn({
            let lic = lic.clone();
            move || run_worker(host, lic, rx)
        });
        tx.send(Job::Start {
            record: Box::new(commercial_record("acme/avada-pro", "https://issuer.test")),
            binary: root.join("nothing-is-here"),
        })
        .unwrap();
        tx.send(Job::Stop).unwrap();
        worker.join().unwrap();

        assert_eq!(
            lic.gate.decided("acme/avada-pro", 1),
            Some(Gate::Run),
            "the worker started a commercial module without asking about its license"
        );
    }

    /// The security half, and the reason the issuer binding is not merely tidy.
    ///
    /// The token here verifies perfectly — it is properly signed, unexpired and names the
    /// right product. It is simply not from the issuer the manifest declares. With no
    /// binding, `LicenseService` believes whatever issuer the stored token claims, so
    /// anybody who can run a signing key can unlock any commercial module. The binding
    /// makes the manifest, which is covered by the signed install record, the authority.
    #[tokio::test]
    async fn a_perfectly_valid_token_from_the_wrong_issuer_unlocks_nothing() {
        let forger = Arc::new(StubIssuer::with_clock(
            "https://forger.test",
            Arc::new(|| NOW),
        ));
        let lic = licenses(forger.clone());
        install(&forger, &lic.service, "acme/avada-pro").await;
        // Believable on its own: this is what the gate says with nothing bound.
        assert_eq!(lic.service.gate("acme/avada-pro").await, Gate::Run);

        let record = commercial_record("acme/avada-pro", "https://issuer.test");
        match decide_license(&lic, &record).await {
            Some(Gate::Refuse(why)) => {
                assert!(
                    why.contains("forger.test"),
                    "the refusal should name the issuer that signed it: {why}"
                );
                assert!(
                    why.contains("issuer.test"),
                    "and the one the manifest requires: {why}"
                );
            }
            other => panic!("a token from an unbound issuer must not run: {other:?}"),
        }
    }
}
