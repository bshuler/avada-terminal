//! `GET /schema` (docs/modules-fanout-plan.md, track H3): the `SchemaDocument` built from
//! the route descriptor table, so `avada schema` prints the API and the CLI builds its
//! subcommand tree from it.
//!
//! The document is deterministic: routes come out core-first then per module, each run
//! sorted by method; RPCs and module summaries are sorted too, and every map inside is a
//! `BTreeMap`. Two consecutive calls on an unchanged registry are byte-identical, which is
//! what lets a CLI cache it by hash.
//!
//! [`SchemaRegistry::register_module_routes`] is the hook the module host (tracks F2/F3)
//! calls once a module answers `host.routes.register`: routes land under
//! `/m/<owner>/<repo>/...` ([`RouteDescriptor::mounted_path`]) and a descriptor without a
//! capability is refused — a module route is never unrestricted, the contract says which
//! right it spends (docs/module-contract.md §7).

use std::collections::BTreeMap;
use std::sync::Mutex;

use avada_module_sdk::contract::{methods, CONTRACT_VERSION};
use avada_module_sdk::descriptor::{
    DescriptorError, ModuleSummary, RouteDescriptor, RpcDescriptor, SchemaDocument,
};
use avada_module_sdk::manifest::ModuleId;
use avada_module_sdk::PRODUCT_NAME;

use crate::control::descriptor_table::Registry;

/// Why a module's routes were not registered. Nothing is added on any error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaError {
    /// A module route declared no capability; the method name is carried.
    NoCapability(String),
    /// The table would be invalid (duplicate method, bad path, ...).
    Descriptor(DescriptorError),
}

impl std::fmt::Display for SchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SchemaError::NoCapability(m) => {
                write!(
                    f,
                    "module route `{m}` names no capability; module routes must"
                )
            }
            SchemaError::Descriptor(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SchemaError {}

impl From<DescriptorError> for SchemaError {
    fn from(e: DescriptorError) -> Self {
        SchemaError::Descriptor(e)
    }
}

/// The live schema: the core table plus every module's routes and summary.
#[derive(Debug)]
pub struct SchemaRegistry {
    registry: Registry,
    modules: BTreeMap<ModuleId, ModuleSummary>,
}

impl Default for SchemaRegistry {
    fn default() -> Self {
        Self::core()
    }
}

impl SchemaRegistry {
    /// The core table alone — what a host serves before any module registers.
    pub fn core() -> Self {
        SchemaRegistry {
            registry: Registry::core(),
            modules: BTreeMap::new(),
        }
    }

    /// Every route currently described, core and module, in registration order.
    pub fn routes(&self) -> &[RouteDescriptor] {
        self.registry.routes()
    }

    /// Mount a module's routes under `/m/<owner>/<repo>/...`. Each descriptor must name a
    /// capability; the whole batch is validated against the live table and either all of
    /// it lands or none of it does. Registering the same module again replaces its routes.
    pub fn register_module_routes(
        &mut self,
        module: &ModuleId,
        routes: Vec<RouteDescriptor>,
    ) -> Result<(), SchemaError> {
        if let Some(r) = routes.iter().find(|r| r.capability.is_none()) {
            return Err(SchemaError::NoCapability(r.method.clone()));
        }
        // Replace, not append: a module that re-registers after a restart must not be
        // refused for duplicating itself. Keep the old set until the new one validates.
        let previous: Vec<RouteDescriptor> = self
            .registry
            .routes()
            .iter()
            .filter(|r| r.module.as_ref() == Some(module))
            .cloned()
            .collect();
        self.registry.unregister_module(module);
        if let Err(e) = self.registry.register_module(module, routes) {
            // Validation failed; put the previous routes back (they validated before).
            let _ = self.registry.register_module(module, previous);
            return Err(e.into());
        }
        Ok(())
    }

    /// Drop a module's routes (its summary, if any, stays until [`Self::remove_module`]).
    pub fn unregister_module_routes(&mut self, module: &ModuleId) {
        self.registry.unregister_module(module);
    }

    /// Record (or refresh) a module in the `modules` list of the document.
    pub fn set_module(&mut self, summary: ModuleSummary) {
        self.modules.insert(summary.id.clone(), summary);
    }

    /// Forget a module entirely: its summary and its routes.
    pub fn remove_module(&mut self, module: &ModuleId) {
        self.modules.remove(module);
        self.registry.unregister_module(module);
    }

    /// The `GET /schema` body. Deterministic for an unchanged registry.
    pub fn document(&self, host_version: &str) -> SchemaDocument {
        let mut core: Vec<RouteDescriptor> = self
            .routes()
            .iter()
            .filter(|r| r.module.is_none())
            .cloned()
            .collect();
        core.sort_by(|a, b| a.method.cmp(&b.method));
        let mut per_module: BTreeMap<&ModuleId, Vec<RouteDescriptor>> = BTreeMap::new();
        for r in self.routes().iter().filter(|r| r.module.is_some()) {
            per_module
                .entry(r.module.as_ref().expect("filtered"))
                .or_default()
                .push(r.clone());
        }
        let mut routes = core;
        for (_, mut rs) in per_module {
            rs.sort_by(|a, b| a.method.cmp(&b.method));
            routes.extend(rs);
        }
        SchemaDocument {
            contract_version: CONTRACT_VERSION,
            product: PRODUCT_NAME.to_string(),
            host_version: host_version.to_string(),
            routes,
            rpcs: rpc_descriptors(),
            modules: self.modules.values().cloned().collect(),
        }
    }
}

/// Every JSON-RPC method of contract v1, host- and module-side, sorted by name, with the
/// capability `contract::methods::required_capability` demands for the host ones.
///
/// The additive host methods are listed too: this document is what a module author reads
/// to find out what *this* host can do, and an optional method that never appears here is
/// one nobody will discover.
pub fn rpc_descriptors() -> Vec<RpcDescriptor> {
    let mut out: Vec<RpcDescriptor> = methods::HOST_REQUIRED_V1
        .iter()
        .chain(methods::HOST_OPTIONAL.iter())
        .chain(methods::MODULE_REQUIRED_V1.iter())
        .chain(methods::MODULE_OPTIONAL.iter())
        .map(|m| RpcDescriptor {
            method: m.to_string(),
            summary: rpc_summary(m).to_string(),
            capability: if m.starts_with("host.") {
                methods::required_capability(m)
            } else {
                None
            },
            params: None,
            result: None,
        })
        .collect();
    out.sort_by(|a, b| a.method.cmp(&b.method));
    out
}

/// One line per RPC, mirroring the doc comments on `contract::methods`.
fn rpc_summary(method: &str) -> &'static str {
    match method {
        methods::HOST_RAIL_REGISTER => "Register or replace the module's rail entries",
        methods::HOST_ROWS_SET => "Replace the rows shown under a rail entry",
        methods::HOST_COMMAND_REGISTER => "Register palette commands",
        methods::HOST_PREFS_DECLARE => "Declare a typed preferences page",
        methods::HOST_PREFS_GET => "Read the module's preferences",
        methods::HOST_PANES_SPAWN => "Open a pane",
        methods::HOST_PANES_INPUT => "Write to a pane's input",
        methods::HOST_FS_READ => "Read a file",
        methods::HOST_FS_WRITE => "Write a file",
        methods::HOST_FS_LIST => "List a directory",
        methods::HOST_EVENTS_SUBSCRIBE => "Subscribe to event kinds",
        methods::HOST_TOAST => "Show a toast",
        methods::HOST_ROUTES_REGISTER => "Register control-plane routes",
        methods::HOST_KEYCHAIN_GET => "Read a keychain entry the module owns",
        methods::HOST_KEYCHAIN_SET => "Write a keychain entry the module owns",
        methods::HOST_WORKSPACE_LIST => "List the saved-workspace library and the sets drawer",
        methods::HOST_WORKSPACE_OPEN => "Open a saved workspace or set",
        methods::HOST_WORKSPACE_SAVE => "Save the current workspace",
        methods::MODULE_ACTIVATE => "A workspace became active",
        methods::MODULE_DEACTIVATE => "The workspace is going away",
        methods::MODULE_COMMAND_INVOKE => "The user invoked a command",
        methods::MODULE_ROW_ACTIVATE => "A rail row was chosen",
        methods::MODULE_ROUTE_INVOKE => "A registered control-plane route was hit",
        methods::MODULE_EVENT => "An event the module subscribed to",
        methods::MODULE_PREFS_CHANGED => "Preferences changed",
        methods::HOST_GRID_SET => "Replace a grid surface's frame",
        methods::HOST_KEYMAP_DECLARE => "Declare a grid surface's actions and keymap presets",
        methods::MODULE_GRID_KEY => "A keystroke landed in a focused grid surface",
        methods::MODULE_GRID_RESIZE => "A grid surface's pane changed size",
        methods::MODULE_SHUTDOWN => "Shut down within 5 s",
        _ => "",
    }
}

/// The registry behind a lock, as `Shared` holds it: handlers read the document, the
/// module host registers and removes modules.
#[derive(Debug, Default)]
pub struct SchemaState {
    inner: Mutex<SchemaRegistry>,
}

impl SchemaState {
    /// The core table alone.
    pub fn new() -> Self {
        SchemaState {
            inner: Mutex::new(SchemaRegistry::core()),
        }
    }

    /// See [`SchemaRegistry::register_module_routes`].
    pub fn register_module_routes(
        &self,
        module: &ModuleId,
        routes: Vec<RouteDescriptor>,
    ) -> Result<(), SchemaError> {
        self.inner
            .lock()
            .unwrap()
            .register_module_routes(module, routes)
    }

    /// See [`SchemaRegistry::set_module`].
    pub fn set_module(&self, summary: ModuleSummary) {
        self.inner.lock().unwrap().set_module(summary);
    }

    /// See [`SchemaRegistry::remove_module`].
    pub fn remove_module(&self, module: &ModuleId) {
        self.inner.lock().unwrap().remove_module(module);
    }

    /// See [`SchemaRegistry::document`].
    pub fn document(&self, host_version: &str) -> SchemaDocument {
        self.inner.lock().unwrap().document(host_version)
    }

    /// Run `f` against the registry (for anything the shortcuts above do not cover).
    pub fn with<R>(&self, f: impl FnOnce(&mut SchemaRegistry) -> R) -> R {
        f(&mut self.inner.lock().unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::descriptor_table::core_routes;
    use avada_module_sdk::caps::Capability;
    use avada_module_sdk::descriptor::{Scope, Verb};
    use std::collections::BTreeSet;

    fn module_route(method: &str, path: &str, cap: Option<Capability>) -> RouteDescriptor {
        RouteDescriptor {
            method: method.into(),
            path: path.into(),
            verb: Verb::Get,
            capability: cap,
            summary: "x".into(),
            params: vec![],
            scope: Scope::Token,
            module: None,
            response: None,
        }
    }

    #[test]
    fn document_lists_every_core_route_sorted_and_is_byte_identical_twice() {
        let reg = SchemaRegistry::core();
        let a = serde_json::to_string(&reg.document("0.1.8")).unwrap();
        let b = serde_json::to_string(&reg.document("0.1.8")).unwrap();
        assert_eq!(a, b);
        let doc = reg.document("0.1.8");
        assert_eq!(doc.contract_version, CONTRACT_VERSION);
        assert_eq!(doc.product, PRODUCT_NAME);
        assert_eq!(doc.host_version, "0.1.8");
        let listed: BTreeSet<_> = doc.routes.iter().map(|r| r.method.clone()).collect();
        let table: BTreeSet<_> = core_routes().into_iter().map(|r| r.method).collect();
        assert_eq!(listed, table);
        let methods: Vec<_> = doc.routes.iter().map(|r| r.method.as_str()).collect();
        let mut sorted = methods.clone();
        sorted.sort_unstable();
        assert_eq!(methods, sorted, "core routes are sorted by method");
        assert!(doc.modules.is_empty());
        let rpcs: Vec<_> = doc.rpcs.iter().map(|r| r.method.as_str()).collect();
        let mut sorted = rpcs.clone();
        sorted.sort_unstable();
        assert_eq!(rpcs, sorted, "rpcs are sorted by method");
        assert_eq!(
            rpcs.len(),
            methods::HOST_REQUIRED_V1.len()
                + methods::HOST_OPTIONAL.len()
                + methods::MODULE_REQUIRED_V1.len()
                + methods::MODULE_OPTIONAL.len()
        );
        let spawn = doc
            .rpcs
            .iter()
            .find(|r| r.method == methods::HOST_PANES_SPAWN)
            .unwrap();
        assert_eq!(spawn.capability, Some(Capability::PanesSpawn));
        // The document round-trips through the SDK type unchanged.
        let back: SchemaDocument = serde_json::from_str(&a).unwrap();
        assert_eq!(back, doc);
    }

    #[test]
    fn module_routes_need_a_capability_and_mount_under_the_module() {
        let id: ModuleId = "acme/files".parse().unwrap();
        let mut reg = SchemaRegistry::core();
        let n = reg.routes().len();
        let err = reg
            .register_module_routes(&id, vec![module_route("files.tree", "/tree", None)])
            .unwrap_err();
        assert_eq!(err, SchemaError::NoCapability("files.tree".into()));
        assert_eq!(reg.routes().len(), n, "nothing lands on refusal");
        reg.register_module_routes(
            &id,
            vec![
                module_route("files.tree", "/tree", Some(Capability::FsRead)),
                module_route("files.blame", "/blame", Some(Capability::GitRead)),
            ],
        )
        .unwrap();
        let doc = reg.document("0");
        let tree = doc
            .routes
            .iter()
            .find(|r| r.method == "files.tree")
            .unwrap();
        assert_eq!(tree.mounted_path(), "/m/acme/files/tree");
        assert_eq!(tree.module.as_ref(), Some(&id));
        // Core first, then the module's routes sorted by method.
        let tail: Vec<_> = doc.routes[n..].iter().map(|r| r.method.as_str()).collect();
        assert_eq!(tail, ["files.blame", "files.tree"]);
        // Re-registering replaces rather than duplicates; a bad batch keeps the old set.
        reg.register_module_routes(
            &id,
            vec![module_route(
                "files.tree",
                "/tree",
                Some(Capability::FsRead),
            )],
        )
        .unwrap();
        assert_eq!(reg.routes().len(), n + 1);
        assert!(reg
            .register_module_routes(
                &id,
                vec![module_route("health", "/dup", Some(Capability::FsRead))]
            )
            .is_err());
        assert_eq!(reg.routes().len(), n + 1);
        assert!(reg.routes().iter().any(|r| r.method == "files.tree"));
        reg.set_module(ModuleSummary {
            id: id.clone(),
            name: "Files".into(),
            version: "1.0.0".into(),
            running: true,
        });
        assert_eq!(reg.document("0").modules.len(), 1);
        reg.remove_module(&id);
        assert_eq!(reg.routes().len(), n);
        assert!(reg.document("0").modules.is_empty());
    }
}
