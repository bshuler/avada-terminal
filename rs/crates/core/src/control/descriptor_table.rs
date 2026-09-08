//! The self-describing route table (plan track W0 / H4). One `RouteDescriptor` per verb
//! the control plane serves, so `avada schema` can print the API and the CLI can build
//! its subcommand tree from it instead of a hand-written parser per route.
//!
//! Wave 1 (track H3) mounts it: `routes::router` is built *from* this table, so a route
//! that exists in one place and not the other is a startup panic naming the route, and
//! `GET /schema` (`control::schema`) serves the same table back. The capability each
//! route needs lives in [`core_capability`] with the rationale per group.

use std::collections::BTreeMap;

use avada_module_sdk::caps::Capability;
use avada_module_sdk::descriptor::{
    validate_table, DescriptorError, Param, ParamLocation, RouteDescriptor, Scope, Verb,
};
use avada_module_sdk::manifest::ModuleId;

fn p(name: &str, location: ParamLocation, kind: &str, required: bool, summary: &str) -> Param {
    Param {
        name: name.into(),
        location,
        kind: kind.into(),
        required,
        summary: summary.into(),
    }
}

fn path_id(name: &str, summary: &str) -> Param {
    p(name, ParamLocation::Path, "string", true, summary)
}

fn route(method: &str, path: &str, verb: Verb, summary: &str) -> RouteDescriptor {
    RouteDescriptor {
        method: method.into(),
        path: path.into(),
        verb,
        capability: None,
        summary: summary.into(),
        params: Vec::new(),
        scope: Scope::Token,
        module: None,
        response: None,
    }
}

fn with(mut r: RouteDescriptor, params: Vec<Param>) -> RouteDescriptor {
    r.params = params;
    r
}

fn master(mut r: RouteDescriptor) -> RouteDescriptor {
    r.scope = Scope::Master;
    r
}

fn public(mut r: RouteDescriptor) -> RouteDescriptor {
    r.scope = Scope::Public;
    r
}

/// Every core route, in the order `routes.rs` mounts them.
pub fn core_routes() -> Vec<RouteDescriptor> {
    use ParamLocation::{Body, Query};
    use Verb::{Delete, Get, Patch, Post};
    let mut routes = vec![
        public(route(
            "health",
            "/health",
            Get,
            "Liveness; the only unauthenticated route",
        )),
        route(
            "state",
            "/state",
            Get,
            "Scope-filtered windows/tabs/panes tree",
        ),
        route("loops", "/loops", Get, "Agent loops the app is running"),
        with(
            route(
                "tokens.mint",
                "/tokens",
                Post,
                "Mint a token narrower than the caller's",
            ),
            vec![
                p(
                    "scope",
                    Body,
                    "object",
                    true,
                    "windowIds | tabIds | paneIds",
                ),
                p("ttlMs", Body, "integer", false, "Lifetime in milliseconds"),
            ],
        ),
        master(route(
            "devices.list",
            "/devices",
            Get,
            "Paired remote devices",
        )),
        master(with(
            route("devices.mint", "/devices", Post, "Pair a remote device"),
            vec![p("name", Body, "string", false, "Display name")],
        )),
        master(with(
            route(
                "devices.revoke",
                "/devices",
                Delete,
                "Unpair a remote device",
            ),
            vec![p("id", Body, "string", true, "Device id")],
        )),
        with(
            route("command", "/command", Post, "Dispatch a UI command"),
            vec![
                p("command", Body, "string", true, "Command name"),
                p("args", Body, "object", false, "Command arguments"),
            ],
        ),
        route("projects.list", "/projects", Get, "Projects"),
        with(
            route("projects.add", "/projects", Post, "Add a project"),
            vec![p("path", Body, "string", true, "Directory")],
        ),
        with(
            route(
                "projects.patch",
                "/projects/{id}",
                Patch,
                "Rename or re-point a project",
            ),
            vec![path_id("id", "Project id")],
        ),
        with(
            route(
                "projects.delete",
                "/projects/{id}",
                Delete,
                "Forget a project",
            ),
            vec![path_id("id", "Project id")],
        ),
        with(
            route(
                "panes.output",
                "/panes/{id}/output",
                Get,
                "Read a pane's screen or raw stream",
            ),
            vec![
                path_id("id", "Pane id"),
                p("mode", Query, "string", false, "screen | raw"),
                p("tail", Query, "integer", false, "Trailing lines"),
                p("strip", Query, "boolean", false, "Strip ANSI"),
                p("since", Query, "string", false, "Cursor from the last read"),
                p(
                    "waitForIdle",
                    Query,
                    "boolean",
                    false,
                    "Block until output settles",
                ),
                p(
                    "settleMs",
                    Query,
                    "integer",
                    false,
                    "Quiet period that counts as idle",
                ),
                p(
                    "timeoutMs",
                    Query,
                    "integer",
                    false,
                    "Give up waiting after this",
                ),
            ],
        ),
        with(
            route("panes.input", "/panes/{id}/input", Post, "Type into a pane"),
            vec![
                path_id("id", "Pane id"),
                p("data", Body, "string", false, "Literal bytes"),
                p("keys", Body, "array", false, "Named keys"),
                p("submit", Body, "boolean", false, "Press Enter after"),
            ],
        ),
        with(
            route(
                "panes.messages.list",
                "/panes/{id}/messages",
                Get,
                "Read a pane's inbox",
            ),
            vec![path_id("id", "Pane id")],
        ),
        with(
            route(
                "panes.messages.post",
                "/panes/{id}/messages",
                Post,
                "Leave a message for a pane",
            ),
            vec![
                path_id("id", "Pane id"),
                p("text", Body, "string", true, "Message"),
            ],
        ),
        with(
            route(
                "panes.lock",
                "/panes/{id}/lock",
                Post,
                "Take the advisory lock",
            ),
            vec![path_id("id", "Pane id")],
        ),
        with(
            route(
                "panes.unlock",
                "/panes/{id}/lock",
                Delete,
                "Release the advisory lock",
            ),
            vec![path_id("id", "Pane id")],
        ),
        route("queues.list", "/queues", Get, "Work queues"),
        with(
            route(
                "queues.tasks.list",
                "/queues/{queue}/tasks",
                Get,
                "Tasks in a queue",
            ),
            vec![path_id("queue", "Queue name")],
        ),
        with(
            route(
                "queues.tasks.enqueue",
                "/queues/{queue}/tasks",
                Post,
                "Enqueue a task",
            ),
            vec![
                path_id("queue", "Queue name"),
                p("payload", Body, "object", true, "Task body"),
            ],
        ),
        with(
            route(
                "queues.claim",
                "/queues/{queue}/claim",
                Post,
                "Claim the next task",
            ),
            vec![path_id("queue", "Queue name")],
        ),
        with(
            route(
                "queues.purge",
                "/queues/{queue}/purge",
                Post,
                "Drop every task",
            ),
            vec![path_id("queue", "Queue name")],
        ),
        with(
            route("tasks.get", "/tasks/{id}", Get, "One task"),
            vec![path_id("id", "Task id")],
        ),
        with(
            route("tasks.ack", "/tasks/{id}/ack", Post, "Finish a task"),
            vec![path_id("id", "Task id")],
        ),
        with(
            route("tasks.nack", "/tasks/{id}/nack", Post, "Return a task"),
            vec![path_id("id", "Task id")],
        ),
        with(
            route(
                "tasks.extend",
                "/tasks/{id}/extend",
                Post,
                "Extend a task's visibility timeout",
            ),
            vec![path_id("id", "Task id")],
        ),
        route("settings.get", "/settings", Get, "Read settings"),
        route("settings.patch", "/settings", Patch, "Change settings"),
        master(with(
            route("fs.read", "/fs/read", Get, "Read a file"),
            vec![p("path", Query, "string", true, "Absolute path")],
        )),
        route("events", "/events", Get, "WebSocket event stream"),
        route(
            "schema",
            "/schema",
            Get,
            "This table: every route, RPC and module the host serves",
        ),
    ];
    for r in &mut routes {
        r.capability = core_capability(&r.method);
    }
    routes
}

/// The capability a core route needs, by dotted method name. This is the single place
/// the assignment lives (the table applies it), so the schema, the router's gate and the
/// tests cannot disagree.
///
/// The rule (plan §5): a route gets the contract capability that covers what it does; a
/// route only the local UI/CLI ever calls gets the *closest* one; `None` is reserved for
/// routes that are unrestricted for every authenticated caller. Legacy tokens (master,
/// device, scoped) hold every capability, so nothing here changes what the desktop app,
/// the CLI or the mobile client can do — the assignment only bites for a caller whose
/// token carries an explicit capability set (a module).
pub fn core_capability(method: &str) -> Option<Capability> {
    use Capability::*;
    Some(match method {
        // Unrestricted: liveness is unauthenticated by design, and the schema is the
        // read-only self-description every token holder needs to drive the CLI at all —
        // it names routes, never user data.
        "health" | "schema" => return None,
        // The device registry is master-only (`Scope::Master`); the scope check is the
        // restriction, and a master token is never a capability-limited caller.
        "devices.list" | "devices.mint" | "devices.revoke" => return None,
        // Reading the windows/tabs/panes tree and the loop ledger is workspace metadata.
        "state" | "loops" => WorkspaceRead,
        // Minting a token is control-plane administration. No contract capability names
        // it, and it must not be open to a capability-limited caller (a minted token
        // would otherwise be an unrestricted legacy token — an escalation), so it takes
        // the one capability that touches the control plane at all.
        "tokens.mint" => ControlRoute,
        // `/command` multiplexes the workspace verbs (open, close, move, rename, focus);
        // the route-level floor is workspace.write and `dispatch::verb_capability` adds
        // the verb's own requirement (panes.spawn, panes.output, ...) on top.
        "command" => WorkspaceWrite,
        // The project rail is workspace metadata.
        "projects.list" => WorkspaceRead,
        "projects.add" | "projects.patch" | "projects.delete" => WorkspaceWrite,
        // Reading what a pane shows — its screen, its raw stream, or its inbox.
        "panes.output" | "panes.messages.list" => PanesOutput,
        // Putting bytes or messages into a pane, and the advisory lock that serializes
        // who may do so.
        "panes.input" | "panes.messages.post" | "panes.lock" | "panes.unlock" => PanesInput,
        // The work queue is agent coordination inside the workspace: reads are metadata,
        // claiming/finishing/purging changes it. Closest capability; there is no `work.*`.
        "queues.list" | "queues.tasks.list" | "tasks.get" => WorkspaceRead,
        "queues.tasks.enqueue"
        | "queues.claim"
        | "queues.purge"
        | "tasks.ack"
        | "tasks.nack"
        | "tasks.extend" => WorkspaceWrite,
        "settings.get" => SettingsRead,
        "settings.patch" => SettingsWrite,
        // Master-only *and* reads any path the user can.
        "fs.read" => FsReadAny,
        "events" => EventsSubscribe,
        other => panic!("core route {other:?} has no capability assignment"),
    })
}

/// The live table: core routes plus whatever modules have registered.
#[derive(Debug, Default)]
pub struct Registry {
    routes: Vec<RouteDescriptor>,
    modules: BTreeMap<ModuleId, Vec<usize>>,
}

impl Registry {
    /// A registry holding the core table.
    pub fn core() -> Registry {
        Registry {
            routes: core_routes(),
            modules: BTreeMap::new(),
        }
    }

    /// Everything currently described.
    pub fn routes(&self) -> &[RouteDescriptor] {
        &self.routes
    }

    /// Add a module's routes. Every descriptor is stamped with the module id (a module
    /// cannot describe a core route) and the whole table is re-validated; on failure
    /// nothing is added.
    pub fn register_module(
        &mut self,
        id: &ModuleId,
        routes: Vec<RouteDescriptor>,
    ) -> Result<(), DescriptorError> {
        let start = self.routes.len();
        let mut next = self.routes.clone();
        for mut r in routes {
            r.module = Some(id.clone());
            r.validate()?;
            next.push(r);
        }
        validate_table(&next)?;
        self.modules
            .entry(id.clone())
            .or_default()
            .extend(start..next.len());
        self.routes = next;
        Ok(())
    }

    /// Drop a module's routes.
    pub fn unregister_module(&mut self, id: &ModuleId) {
        if self.modules.remove(id).is_none() {
            return;
        }
        self.routes.retain(|r| r.module.as_ref() != Some(id));
        // Re-index the survivors.
        self.modules.clear();
        for (i, r) in self.routes.iter().enumerate() {
            if let Some(m) = &r.module {
                self.modules.entry(m.clone()).or_default().push(i);
            }
        }
    }

    /// Look a route up by dotted method name.
    pub fn by_method(&self, method: &str) -> Option<&RouteDescriptor> {
        self.routes.iter().find(|r| r.method == method)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn core_table_validates() {
        validate_table(&core_routes()).unwrap();
        for r in core_routes() {
            r.validate().unwrap_or_else(|e| panic!("{}: {e}", r.method));
        }
    }

    /// The router is built from this table (`routes::router`), so the two cannot drift by
    /// construction; what can drift is a handler with no descriptor or a descriptor with
    /// no handler. `routes::handlers()` is the method → handler list the router consumes,
    /// and both directions are checked here by name (the live round trip over a real
    /// socket is `routes::table::every_described_route_is_mounted`).
    #[test]
    fn every_route_has_a_handler_and_every_handler_a_route() {
        let described: BTreeSet<String> = core_routes().into_iter().map(|r| r.method).collect();
        let handled: BTreeSet<String> = crate::control::routes::handlers()
            .into_iter()
            .map(|(method, _)| method.to_string())
            .collect();
        let unhandled: Vec<_> = described.difference(&handled).collect();
        let undescribed: Vec<_> = handled.difference(&described).collect();
        assert!(
            unhandled.is_empty(),
            "described but no handler: {unhandled:?}"
        );
        assert!(
            undescribed.is_empty(),
            "handler but undescribed: {undescribed:?}"
        );
        assert!(
            described.len() >= 31,
            "sanity: {} routes described",
            described.len()
        );
    }

    #[test]
    fn capabilities_follow_the_documented_rule() {
        let table = core_routes();
        let unrestricted: BTreeSet<_> = table
            .iter()
            .filter(|r| r.capability.is_none())
            .map(|r| r.method.as_str())
            .collect();
        // `None` only where the route is unrestricted for any authenticated caller or
        // gated by scope instead (the comments on `core_capability` say why for each).
        assert_eq!(
            unrestricted,
            [
                "health",
                "schema",
                "devices.list",
                "devices.mint",
                "devices.revoke"
            ]
            .into_iter()
            .collect()
        );
        let by = |m: &str| core_capability(m);
        assert_eq!(by("panes.output"), Some(Capability::PanesOutput));
        assert_eq!(by("panes.input"), Some(Capability::PanesInput));
        assert_eq!(by("settings.get"), Some(Capability::SettingsRead));
        assert_eq!(by("settings.patch"), Some(Capability::SettingsWrite));
        assert_eq!(by("fs.read"), Some(Capability::FsReadAny));
        assert_eq!(by("events"), Some(Capability::EventsSubscribe));
        assert_eq!(by("tokens.mint"), Some(Capability::ControlRoute));
        // Every route the table carries has an entry (an unlisted method panics).
        for r in &table {
            let _ = core_capability(&r.method);
        }
    }

    #[test]
    fn only_health_is_public_and_the_master_routes_are_the_documented_ones() {
        let public: Vec<_> = core_routes()
            .into_iter()
            .filter(|r| r.scope == Scope::Public)
            .map(|r| r.path)
            .collect();
        assert_eq!(public, ["/health"]);
        let master: BTreeSet<_> = core_routes()
            .into_iter()
            .filter(|r| r.scope == Scope::Master)
            .map(|r| r.method)
            .collect();
        assert_eq!(
            master,
            ["devices.list", "devices.mint", "devices.revoke", "fs.read"]
                .into_iter()
                .map(String::from)
                .collect()
        );
    }

    fn module_route(method: &str, path: &str) -> RouteDescriptor {
        let mut r = route(method, path, Verb::Get, "x");
        r.capability = Some(Capability::FsRead);
        r
    }

    #[test]
    fn registry_mounts_module_routes_under_the_module_and_unmounts_cleanly() {
        let id: ModuleId = "acme/files".parse().unwrap();
        let mut reg = Registry::core();
        let n = reg.routes().len();
        reg.register_module(&id, vec![module_route("files.tree", "/tree")])
            .unwrap();
        assert_eq!(reg.routes().len(), n + 1);
        let r = reg.by_method("files.tree").unwrap();
        assert_eq!(r.module.as_ref(), Some(&id));
        assert_eq!(r.mounted_path(), "/m/acme/files/tree");
        // A second module may reuse the path; the mount point differs.
        let other: ModuleId = "acme/git".parse().unwrap();
        reg.register_module(&other, vec![module_route("git.tree", "/tree")])
            .unwrap();
        // The same method name twice is refused and nothing is added.
        let before = reg.routes().len();
        assert!(reg
            .register_module(&other, vec![module_route("files.tree", "/dup")])
            .is_err());
        assert_eq!(reg.routes().len(), before);
        reg.unregister_module(&id);
        assert!(reg.by_method("files.tree").is_none());
        assert!(reg.by_method("git.tree").is_some());
        assert_eq!(reg.routes().len(), n + 1);
        reg.unregister_module(&other);
        assert_eq!(reg.routes().len(), n);
    }
}
