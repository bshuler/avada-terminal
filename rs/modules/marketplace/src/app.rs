//! Commands and row activations → marketplace routes → state.
//!
//! [`App`] owns the [`State`] and a [`Control`]; it never touches the pipe, so the
//! command table is unit-tested against a recording fake and the process-level
//! test only has to prove the wiring.

use avada_module_sdk::contract::{ErrorCode, RpcError};
use serde_json::{json, Value};

use crate::control::{Control, ControlError};
use crate::model::{Installed, Job, ModuleView, RepoSummary, RightsView, SignIn, State, Toolchain};
use crate::rows::url_encode;

/// Every command id in `avada.toml`, in the order the host lists them.
pub const COMMANDS: &[(&str, &str)] = &[
    ("search", "Marketplace: Search modules"),
    ("install", "Marketplace: Install module"),
    ("enable", "Marketplace: Enable module in this workspace"),
    ("disable", "Marketplace: Disable module in this workspace"),
    ("uninstall", "Marketplace: Uninstall module"),
    ("job", "Marketplace: Show install job"),
    ("signin", "Marketplace: Sign in to GitHub"),
    ("refresh", "Marketplace: Refresh"),
    ("pane", "Marketplace: Open pane"),
];

/// The pane surface this module fills.
///
/// The same string as the rail entry id: rail and pane contributions are separate
/// namespaces in the manifest, and `RowTarget` is what tells the two apart on the
/// wire, so there is nothing to gain from a second name.
pub const PANE: &str = "marketplace";

/// What a command asks the host to show after it ran.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    /// The JSON-RPC result.
    pub result: Value,
    /// A toast, when the user should see something without looking at the rail.
    pub toast: Option<String>,
    /// Ask the host for a module pane on [`PANE`] before the next row push. The
    /// host refuses rows for a surface it never spawned, so this is the only way
    /// the pane rows ever reach it.
    pub spawn_pane: bool,
}

/// The module's brain.
pub struct App<C: Control> {
    /// What the rows are drawn from.
    pub state: State,
    control: C,
}

fn invalid(msg: impl Into<String>) -> RpcError {
    RpcError::new(ErrorCode::InvalidParams, msg)
}

/// `owner/repo` with a single slash and repository-safe characters on each side.
pub fn valid_module_id(id: &str) -> bool {
    let Some((owner, repo)) = id.split_once('/') else {
        return false;
    };
    let ok = |s: &str| {
        !s.is_empty()
            && s.len() <= 100
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            && s != "."
            && s != ".."
    };
    ok(owner) && ok(repo)
}

fn arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn module_arg(args: &Value) -> Result<String, RpcError> {
    let id = arg(args, "module").ok_or_else(|| invalid("`module` (owner/repo) is required"))?;
    if !valid_module_id(id) {
        return Err(invalid(format!("`{id}` is not an owner/repo module id")));
    }
    Ok(id.to_string())
}

impl<C: Control> App<C> {
    /// A fresh app; `manage_granted` and `control_available` describe the handshake.
    pub fn new(control: C, manage_granted: bool, control_available: bool) -> Self {
        App {
            state: State {
                manage_granted,
                control_available,
                ..Default::default()
            },
            control,
        }
    }

    /// `module.activate` / hello workspace.
    pub fn set_workspace(&mut self, id: Option<String>) {
        self.state.workspace = id;
    }

    /// One exchange; non-2xx answers become `RpcError`s and the notice row.
    fn call(&mut self, method: &str, path: &str, body: Option<Value>) -> Result<Value, RpcError> {
        if !self.state.manage_granted {
            return Err(RpcError::new(
                ErrorCode::CapabilityDenied,
                "`marketplace.manage` was not granted at install",
            ));
        }
        if !self.state.control_available {
            return Err(RpcError::new(
                ErrorCode::Internal,
                ControlError::Unavailable.to_string(),
            ));
        }
        let (status, value) = self
            .control
            .request(method, path, body.as_ref())
            .map_err(|e| {
                self.state.notice = Some(e.to_string());
                RpcError::new(ErrorCode::Internal, e.to_string())
            })?;
        if (200..300).contains(&status) {
            self.state.notice = None;
            return Ok(value);
        }
        let reason = value
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| value.as_str().map(str::to_string))
            .unwrap_or_else(|| value.to_string());
        let mut text = format!("{status}: {reason}");
        if let Some(guide) = value.get("guide").and_then(Value::as_str) {
            text.push_str(" — ");
            text.push_str(guide);
        }
        self.state.notice = Some(text.clone());
        let code = match status {
            401 | 403 => ErrorCode::CapabilityDenied,
            400 | 404 => ErrorCode::InvalidParams,
            // The HTTP status rides in the code: a 412 is -32412.
            s => ErrorCode::Other(-32000 - i64::from(s)),
        };
        Err(RpcError::new(code, text))
    }

    fn parse<T: serde::de::DeserializeOwned>(v: Value, what: &str) -> Result<T, RpcError> {
        serde_json::from_value(v)
            .map_err(|e| RpcError::new(ErrorCode::Internal, format!("{what}: {e}")))
    }

    /// `GET /marketplace/toolchain` + `GET /marketplace/installed` (+ jobs when any
    /// are known). The first failure stops the refresh; what was fetched stays.
    pub fn refresh(&mut self) -> Result<Outcome, RpcError> {
        let tc = self.call("GET", "/marketplace/toolchain", None)?;
        self.state.toolchain = Some(Self::parse::<Toolchain>(tc, "toolchain")?);
        let inst = self.call("GET", "/marketplace/installed", None)?;
        let list = inst.get("modules").cloned().unwrap_or(Value::Array(vec![]));
        self.state.installed = Self::parse::<Vec<Installed>>(list, "installed")?;
        if !self.state.jobs.is_empty() {
            let jobs = self.call("GET", "/marketplace/jobs", None)?;
            let list = jobs.get("jobs").cloned().unwrap_or(Value::Array(vec![]));
            for job in Self::parse::<Vec<Job>>(list, "jobs")? {
                if self.state.jobs.iter().any(|j| j.id == job.id) {
                    self.state.upsert_job(job);
                }
            }
        }
        Ok(Outcome {
            result: json!({
                "ready": self.state.toolchain.as_ref().is_some_and(|t| t.ready),
                "installed": self.state.installed.len(),
            }),
            toast: None,
            spawn_pane: false,
        })
    }

    /// `GET /marketplace/search?q=`.
    pub fn search(&mut self, q: &str) -> Result<Outcome, RpcError> {
        let v = self.call(
            "GET",
            &format!("/marketplace/search?q={}", url_encode(q)),
            None,
        )?;
        let list = v.get("modules").cloned().unwrap_or(Value::Array(vec![]));
        self.state.results = Self::parse::<Vec<RepoSummary>>(list, "search")?;
        self.state.query = Some(q.to_string());
        let n = self.state.results.len();
        Ok(Outcome {
            result: json!({ "query": q, "results": n }),
            toast: Some(format!("Marketplace: {n} result(s) for “{q}”")),
            spawn_pane: false,
        })
    }

    /// `POST /marketplace/install` → 202 job.
    pub fn install(&mut self, module: &str, tag: Option<&str>) -> Result<Outcome, RpcError> {
        let mut body = json!({ "module": module });
        if let Some(t) = tag {
            body["tag"] = Value::String(t.to_string());
        }
        if let Some(ws) = &self.state.workspace {
            body["workspace"] = Value::String(ws.clone());
        }
        let v = self.call("POST", "/marketplace/install", Some(body))?;
        let job: Job = Self::parse(v.get("job").cloned().unwrap_or(v), "install job")?;
        let id = job.id.clone();
        self.state.upsert_job(job);
        Ok(Outcome {
            result: json!({ "job": id, "module": module }),
            toast: Some(format!("Marketplace: installing {module}")),
            spawn_pane: false,
        })
    }

    /// `GET /marketplace/jobs/{id}`; the newest known job when `id` is `None`.
    pub fn job(&mut self, id: Option<&str>) -> Result<Outcome, RpcError> {
        let id = match id {
            Some(id) => id.to_string(),
            None => self
                .state
                .jobs
                .first()
                .map(|j| j.id.clone())
                .ok_or_else(|| invalid("`id` is required: no install job is known"))?,
        };
        let v = self.call(
            "GET",
            &format!("/marketplace/jobs/{}", url_encode(&id)),
            None,
        )?;
        let job: Job = Self::parse(v, "job")?;
        let summary = json!({
            "job": job.id, "module": job.module, "phase": job.phase,
            "progress": job.progress, "error": job.error, "version": job.version,
        });
        let toast = match job.phase.as_str() {
            "done" => Some(format!(
                "Marketplace: installed {} {}",
                job.module,
                job.version.clone().unwrap_or_default()
            )),
            "failed" => Some(format!(
                "Marketplace: {} failed: {}",
                job.module,
                job.error.clone().unwrap_or_default()
            )),
            _ => None,
        };
        let finished = job.finished();
        self.state.upsert_job(job);
        if finished {
            // The install store changed; a failed refresh must not hide the job result.
            let _ = self.refresh();
        }
        Ok(Outcome {
            result: summary,
            toast,
            spawn_pane: false,
        })
    }

    /// `POST /marketplace/modules/{owner}/{repo}/(enable|disable)` `{workspace}`.
    pub fn set_enabled(&mut self, module: &str, enabled: bool) -> Result<Outcome, RpcError> {
        let ws = self
            .state
            .workspace
            .clone()
            .ok_or_else(|| RpcError::new(ErrorCode::NoWorkspace, "no workspace is active"))?;
        let verb = if enabled { "enable" } else { "disable" };
        let v = self.call(
            "POST",
            &format!("/marketplace/modules/{module}/{verb}"),
            Some(json!({ "workspace": ws })),
        )?;
        let map = v.get("enabled").cloned().unwrap_or(json!({}));
        for inst in self
            .state
            .installed
            .iter_mut()
            .filter(|i| i.module.as_deref() == Some(module))
        {
            if let Ok(m) = serde_json::from_value(map.clone()) {
                inst.enabled = m;
            }
        }
        Ok(Outcome {
            result: json!({ "module": module, "workspace": ws, "enabled": enabled }),
            toast: Some(format!("Marketplace: {module} {verb}d in this workspace")),
            spawn_pane: false,
        })
    }

    /// `DELETE /marketplace/modules/{owner}/{repo}/{version}`; the active version
    /// when `version` is `None`.
    pub fn uninstall(&mut self, module: &str, version: Option<&str>) -> Result<Outcome, RpcError> {
        let version = match version {
            Some(v) => v.to_string(),
            None => self.state.installed_version(module).ok_or_else(|| {
                invalid(format!("`version` is required: {module} is not installed"))
            })?,
        };
        self.call(
            "DELETE",
            &format!("/marketplace/modules/{module}/{}", url_encode(&version)),
            None,
        )?;
        self.state.installed.retain(|i| {
            !(i.module.as_deref() == Some(module) && i.version.as_deref() == Some(&version))
        });
        Ok(Outcome {
            result: json!({ "module": module, "version": version, "ok": true }),
            toast: Some(format!("Marketplace: removed {module} {version}")),
            spawn_pane: false,
        })
    }

    /// `POST /marketplace/signin`, or `GET /marketplace/signin/{id}` while one is
    /// pending.
    pub fn signin(&mut self) -> Result<Outcome, RpcError> {
        if let Some(id) = self
            .state
            .signin
            .as_ref()
            .filter(|s| s.pending())
            .map(|s| s.id.clone())
        {
            return self.signin_poll(&id);
        }
        let v = self.call("POST", "/marketplace/signin", None)?;
        let s: SignIn = Self::parse(v, "sign-in")?;
        let toast = format!(
            "Marketplace: enter code {} at {}",
            s.user_code, s.verification_uri
        );
        let result = json!({ "id": s.id, "user_code": s.user_code, "verification_uri": s.verification_uri, "status": s.status });
        self.state.signin = Some(s);
        Ok(Outcome {
            result,
            toast: Some(toast),
            spawn_pane: false,
        })
    }

    /// `GET /marketplace/signin/{id}`.
    pub fn signin_poll(&mut self, id: &str) -> Result<Outcome, RpcError> {
        let v = self.call(
            "GET",
            &format!("/marketplace/signin/{}", url_encode(id)),
            None,
        )?;
        let s: SignIn = Self::parse(v, "sign-in")?;
        let toast = match s.status.as_str() {
            "pending" => format!("Marketplace: still waiting for code {}", s.user_code),
            "done" => "Marketplace: signed in to GitHub".to_string(),
            other => format!("Marketplace: sign-in {other}"),
        };
        let result = json!({ "id": s.id, "status": s.status });
        let done = s.status == "done";
        self.state.signin = Some(s);
        if done {
            if let Some(tc) = self.state.toolchain.as_mut() {
                tc.signed_in = true;
            }
        }
        Ok(Outcome {
            result,
            toast: Some(toast),
            spawn_pane: false,
        })
    }

    /// Ask the host for the pane, and focus `module` in it when one was named.
    ///
    /// A failure to describe the module does not stop the pane opening: the pane is
    /// where the notice row is read, so refusing to show it would hide the reason.
    pub fn open_pane(&mut self, module: Option<&str>) -> Result<Outcome, RpcError> {
        self.state.pane_open = true;
        match module {
            Some(m) => {
                let _ = self.focus(m);
            }
            None => self.state.clear_focus(),
        }
        Ok(Outcome {
            result: json!({ "surface": PANE, "module": module }),
            toast: None,
            spawn_pane: true,
        })
    }

    /// `GET /marketplace/modules/{owner}/{repo}`, then its rights and this
    /// workspace's pins: everything the pane draws about one module.
    pub fn focus(&mut self, module: &str) -> Result<Outcome, RpcError> {
        if !valid_module_id(module) {
            return Err(invalid(format!(
                "`{module}` is not an owner/repo module id"
            )));
        }
        let v = self.call("GET", &format!("/marketplace/modules/{module}"), None)?;
        let mut view: ModuleView = Self::parse(v, "module")?;
        if view.module.is_empty() {
            view.module = module.to_string();
        }
        let versions = view.installed.len();
        self.state.view = Some(view);
        self.state.focus = Some(module.to_string());
        // Rights exist only for an installed module, so a search result answers 404
        // here. That is the ordinary case, not a failure worth a notice — and the
        // version list just fetched must survive it.
        let before = self.state.notice.clone();
        if self.refresh_rights(module).is_err() {
            self.state.rights = None;
            self.state.notice = before;
        }
        let before = self.state.notice.clone();
        if self.refresh_pins().is_err() {
            self.state.notice = before;
        }
        Ok(Outcome {
            result: json!({ "module": module, "installed": versions }),
            ..Default::default()
        })
    }

    /// Leave the module view for the pane's index.
    pub fn unfocus(&mut self) -> Outcome {
        self.state.clear_focus();
        Outcome {
            result: json!({ "module": Value::Null }),
            ..Default::default()
        }
    }

    /// `GET /marketplace/modules/{owner}/{repo}/rights[?workspace=]` into the state.
    fn refresh_rights(&mut self, module: &str) -> Result<(), RpcError> {
        let path = match &self.state.workspace {
            Some(ws) => format!(
                "/marketplace/modules/{module}/rights?workspace={}",
                url_encode(ws)
            ),
            None => format!("/marketplace/modules/{module}/rights"),
        };
        let v = self.call("GET", &path, None)?;
        self.state.rights = Some(Self::parse::<RightsView>(v, "rights")?);
        Ok(())
    }

    /// `GET /marketplace/pins?workspace=` into the state; a no-op without a workspace.
    fn refresh_pins(&mut self) -> Result<(), RpcError> {
        let Some(ws) = self.state.workspace.clone() else {
            self.state.pins.clear();
            return Ok(());
        };
        let v = self.call(
            "GET",
            &format!("/marketplace/pins?workspace={}", url_encode(&ws)),
            None,
        )?;
        let map = v.get("pins").cloned().unwrap_or(json!({}));
        self.state.pins = Self::parse(map, "pins")?;
        Ok(())
    }

    /// `POST /marketplace/modules/{owner}/{repo}/(pin|unpin)`; `None` unpins.
    pub fn set_pin(&mut self, module: &str, version: Option<&str>) -> Result<Outcome, RpcError> {
        let ws = self
            .state
            .workspace
            .clone()
            .ok_or_else(|| RpcError::new(ErrorCode::NoWorkspace, "no workspace is active"))?;
        let mut body = json!({ "workspace": ws });
        let verb = match version {
            Some(v) => {
                body["version"] = Value::String(v.to_string());
                "pin"
            }
            None => "unpin",
        };
        self.call(
            "POST",
            &format!("/marketplace/modules/{module}/{verb}"),
            Some(body),
        )?;
        let _ = self.refresh_pins();
        Ok(Outcome {
            result: json!({ "module": module, "workspace": ws, "version": version }),
            toast: Some(match version {
                Some(v) => format!("Marketplace: {module} pinned to {v} here"),
                None => format!("Marketplace: {module} unpinned here"),
            }),
            spawn_pane: false,
        })
    }

    /// `POST /marketplace/modules/{owner}/{repo}/rights`; `value` `None` clears the
    /// override rather than setting one, which is how a user goes back to the profile.
    pub fn set_right(
        &mut self,
        module: &str,
        cap: &str,
        value: Option<&str>,
        workspace: bool,
    ) -> Result<Outcome, RpcError> {
        let mut body = json!({ "cap": cap });
        if let Some(v) = value {
            body["value"] = Value::String(v.to_string());
        }
        if workspace {
            let ws =
                self.state.workspace.clone().ok_or_else(|| {
                    RpcError::new(ErrorCode::NoWorkspace, "no workspace is active")
                })?;
            body["workspace"] = Value::String(ws);
        }
        let v = self.call(
            "POST",
            &format!("/marketplace/modules/{module}/rights"),
            Some(body),
        )?;
        self.state.rights = Some(Self::parse::<RightsView>(v, "rights")?);
        let where_ = if workspace {
            "in this workspace"
        } else {
            "everywhere"
        };
        Ok(Outcome {
            result: json!({ "module": module, "cap": cap, "value": value }),
            toast: Some(match value {
                Some(v) => format!("Marketplace: {cap} is {v} {where_}"),
                None => format!("Marketplace: {cap} follows the profile again"),
            }),
            spawn_pane: false,
        })
    }

    /// `POST /marketplace/modules/{owner}/{repo}/profile`; `None` selects no profile.
    pub fn set_profile(
        &mut self,
        module: &str,
        profile: Option<&str>,
    ) -> Result<Outcome, RpcError> {
        let mut body = json!({});
        if let Some(p) = profile {
            body["profile"] = Value::String(p.to_string());
        }
        let v = self.call(
            "POST",
            &format!("/marketplace/modules/{module}/profile"),
            Some(body),
        )?;
        self.state.rights = Some(Self::parse::<RightsView>(v, "rights")?);
        Ok(Outcome {
            result: json!({ "module": module, "profile": profile }),
            toast: Some(match profile {
                Some(p) => format!("Marketplace: {module} uses the “{p}” profile"),
                None => format!("Marketplace: {module} uses no profile"),
            }),
            spawn_pane: false,
        })
    }

    /// `module.command.invoke`: `id` from `avada.toml`, `args` an object.
    pub fn command(&mut self, id: &str, args: &Value) -> Result<Outcome, RpcError> {
        match id {
            "search" => {
                let q = arg(args, "q")
                    .or_else(|| arg(args, "query"))
                    .ok_or_else(|| invalid("`q` is required"))?
                    .to_string();
                self.search(&q)
            }
            "install" => {
                let module = module_arg(args)?;
                let tag = arg(args, "tag").map(str::to_string);
                self.install(&module, tag.as_deref())
            }
            "enable" => {
                let module = module_arg(args)?;
                self.set_enabled(&module, true)
            }
            "disable" => {
                let module = module_arg(args)?;
                self.set_enabled(&module, false)
            }
            "uninstall" => {
                let module = module_arg(args)?;
                let version = arg(args, "version").map(str::to_string);
                self.uninstall(&module, version.as_deref())
            }
            "job" => {
                let id = arg(args, "id").map(str::to_string);
                self.job(id.as_deref())
            }
            "signin" => self.signin(),
            "refresh" => self.refresh(),
            "pane" => {
                let module = arg(args, "module").map(str::to_string);
                if let Some(m) = &module {
                    if !valid_module_id(m) {
                        return Err(invalid(format!("`{m}` is not an owner/repo module id")));
                    }
                }
                self.open_pane(module.as_deref())
            }
            other => Err(invalid(format!("unknown command {other:?}"))),
        }
    }

    /// `module.row.activate`: the row's `data` decides; `open` acts, `toggle` and
    /// `context` only refresh the row's section.
    pub fn row_activate(&mut self, data: &Value, gesture: &str) -> Result<Outcome, RpcError> {
        if gesture != "open" {
            return Ok(Outcome::default());
        }
        match data.get("action").and_then(Value::as_str) {
            Some("toolchain") => {
                let guide = self
                    .state
                    .toolchain
                    .as_ref()
                    .and_then(|t| t.guide.clone())
                    .unwrap_or_else(|| "The toolchain is ready".to_string());
                Ok(Outcome {
                    result: json!({ "guide": guide }),
                    toast: Some(guide),
                    spawn_pane: false,
                })
            }
            Some("signin") => self.signin(),
            Some("signin.poll") => {
                let id = arg(data, "id")
                    .ok_or_else(|| invalid("row has no sign-in id"))?
                    .to_string();
                self.signin_poll(&id)
            }
            Some("install") => {
                let module = module_arg(data)?;
                self.install(&module, None)
            }
            Some("enable") => {
                let module = module_arg(data)?;
                self.set_enabled(&module, true)
            }
            Some("disable") => {
                let module = module_arg(data)?;
                self.set_enabled(&module, false)
            }
            Some("job") => {
                let id = arg(data, "id").map(str::to_string);
                self.job(id.as_deref())
            }
            Some("pane.focus") => {
                let module = module_arg(data)?;
                self.focus(&module)
            }
            Some("pane.index") => Ok(self.unfocus()),
            Some("pane.install") => {
                let module = module_arg(data)?;
                let tag = arg(data, "tag").map(str::to_string);
                self.install(&module, tag.as_deref())
            }
            Some("pane.pin") => {
                let module = module_arg(data)?;
                let version = arg(data, "version")
                    .ok_or_else(|| invalid("row has no version to pin"))?
                    .to_string();
                self.set_pin(&module, Some(&version))
            }
            Some("pane.unpin") => {
                let module = module_arg(data)?;
                self.set_pin(&module, None)
            }
            Some("pane.right") => {
                let module = module_arg(data)?;
                let cap = arg(data, "cap")
                    .ok_or_else(|| invalid("row has no capability"))?
                    .to_string();
                let value = arg(data, "value").map(str::to_string);
                let workspace = arg(data, "scope") == Some("workspace");
                self.set_right(&module, &cap, value.as_deref(), workspace)
            }
            Some("pane.profile") => {
                let module = module_arg(data)?;
                let profile = arg(data, "profile").map(str::to_string);
                self.set_profile(&module, profile.as_deref())
            }
            _ => Ok(Outcome::default()),
        }
    }
}

#[cfg(test)]
pub mod fake {
    //! A [`Control`] that records calls and answers from a table.
    use std::cell::RefCell;

    use serde_json::Value;

    use crate::control::{Answer, Control, ControlError};

    /// `(method, path, body)` as seen.
    pub type Seen = (String, String, Option<Value>);

    /// Canned answers keyed by `"METHOD path"` (exact, query included).
    #[derive(Default)]
    pub struct FakeControl {
        pub answers: RefCell<Vec<(String, u16, Value)>>,
        pub seen: RefCell<Vec<Seen>>,
    }

    impl FakeControl {
        pub fn answer(self, key: &str, status: u16, body: Value) -> Self {
            self.answers
                .borrow_mut()
                .push((key.to_string(), status, body));
            self
        }

        pub fn calls(&self) -> Vec<String> {
            self.seen
                .borrow()
                .iter()
                .map(|(m, p, _)| format!("{m} {p}"))
                .collect()
        }

        pub fn body_of(&self, key: &str) -> Option<Value> {
            self.seen
                .borrow()
                .iter()
                .find(|(m, p, _)| format!("{m} {p}") == key)
                .and_then(|(_, _, b)| b.clone())
        }
    }

    impl Control for FakeControl {
        fn request(
            &self,
            method: &str,
            path: &str,
            body: Option<&Value>,
        ) -> Result<Answer, ControlError> {
            self.seen
                .borrow_mut()
                .push((method.to_string(), path.to_string(), body.cloned()));
            let key = format!("{method} {path}");
            self.answers
                .borrow()
                .iter()
                .find(|(k, _, _)| *k == key)
                .map(|(_, s, v)| (*s, v.clone()))
                .ok_or_else(|| ControlError::Malformed(format!("no canned answer for {key}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeControl;
    use super::*;

    fn ready(fake: FakeControl) -> App<FakeControl> {
        let mut app = App::new(fake, true, true);
        app.set_workspace(Some("ws1".into()));
        app
    }

    /// `GET /marketplace/modules/{m}` + rights + pins, as `focus` asks for them.
    fn focus_fake() -> FakeControl {
        FakeControl::default()
            .answer(
                "GET /marketplace/modules/acme/avada-files",
                200,
                json!({
                    "module": "acme/avada-files",
                    "repo": { "full_name": "acme/avada-files", "stars": 7 },
                    "tags": [
                        { "name": "1.2.0", "commit": "abc" },
                        { "name": "1.1.0", "commit": "d" },
                        { "name": "1.0.0", "commit": "e" },
                    ],
                    "newest_tag": "1.2.0",
                    "installed": ["1.1.0", "1.2.0"],
                    "active": "1.1.0",
                    "enabled": { "ws1": true },
                }),
            )
            .answer(
                "GET /marketplace/modules/acme/avada-files/rights?workspace=ws1",
                200,
                json!({
                    "module": "acme/avada-files",
                    "version": "1.1.0",
                    "workspace": "ws1",
                    "profile": "reader",
                    "profiles": [
                        { "name": "reader", "description": "read only", "values": { "fs.read": "always" } },
                        { "name": "writer", "description": "read and write", "values": {} },
                    ],
                    "rows": [
                        { "cap": "fs.read", "description": "read files", "accepted": true,
                          "user": null, "workspace": null, "effective": "always" },
                        { "cap": "net.fetch", "description": "reach the network", "accepted": false,
                          "user": "never", "workspace": "ask", "effective": "never" },
                    ],
                }),
            )
            .answer(
                "GET /marketplace/pins?workspace=ws1",
                200,
                json!({ "workspace": "ws1", "pins": { "acme/avada-files": "1.1.0" } }),
            )
    }

    #[test]
    fn the_pane_command_opens_the_pane_and_may_focus_a_module() {
        let mut app = ready(focus_fake());
        let out = app.command("pane", &json!({})).unwrap();
        assert!(out.spawn_pane);
        assert!(app.state.pane_open);
        assert_eq!(app.state.focus, None);
        assert!(app.control.calls().is_empty(), "the index fetches nothing");

        let out = app
            .command("pane", &json!({ "module": "acme/avada-files" }))
            .unwrap();
        assert!(out.spawn_pane);
        assert_eq!(app.state.focus.as_deref(), Some("acme/avada-files"));
        assert_eq!(app.state.view.as_ref().unwrap().tags.len(), 3);
        assert_eq!(
            app.state.rights.as_ref().unwrap().profile.as_deref(),
            Some("reader")
        );
        assert_eq!(app.state.pin("acme/avada-files"), Some("1.1.0"));

        assert_eq!(
            app.command("pane", &json!({ "module": "nope" }))
                .unwrap_err()
                .kind(),
            ErrorCode::InvalidParams
        );
    }

    #[test]
    fn focus_keeps_the_version_list_when_the_module_has_no_rights_yet() {
        // A search result is not installed, so its rights route answers 404. That is
        // the ordinary case: it must not wipe the tags or leave a scary notice.
        let fake = FakeControl::default()
            .answer(
                "GET /marketplace/modules/acme/avada-files",
                200,
                json!({ "tags": [{ "name": "1.2.0" }], "installed": [] }),
            )
            .answer(
                "GET /marketplace/modules/acme/avada-files/rights?workspace=ws1",
                404,
                json!({ "error": "module acme/avada-files is not installed" }),
            )
            .answer(
                "GET /marketplace/pins?workspace=ws1",
                200,
                json!({ "pins": {} }),
            );
        let mut app = ready(fake);
        app.focus("acme/avada-files").unwrap();
        assert_eq!(app.state.view.as_ref().unwrap().tags.len(), 1);
        // The route answered with no `module`, so `focus` fills it in from the id.
        assert_eq!(app.state.view.as_ref().unwrap().module, "acme/avada-files");
        assert!(app.state.rights.is_none());
        assert_eq!(app.state.notice, None, "a 404 here is not an error to show");
    }

    #[test]
    fn unfocus_drops_everything_that_described_the_module() {
        let mut app = ready(focus_fake());
        app.focus("acme/avada-files").unwrap();
        app.unfocus();
        assert_eq!(app.state.focus, None);
        assert!(app.state.view.is_none());
        assert!(app.state.rights.is_none());
        // Pins are per-workspace, not per-module, so leaving the module keeps them.
        assert_eq!(app.state.pin("acme/avada-files"), Some("1.1.0"));
    }

    #[test]
    fn pin_and_unpin_post_the_workspace_and_reread_the_pins() {
        let fake = focus_fake()
            .answer(
                "POST /marketplace/modules/acme/avada-files/pin",
                200,
                json!({ "ok": true }),
            )
            .answer(
                "POST /marketplace/modules/acme/avada-files/unpin",
                200,
                json!({ "ok": true }),
            );
        let mut app = ready(fake);
        app.set_pin("acme/avada-files", Some("1.2.0")).unwrap();
        assert_eq!(
            app.control
                .body_of("POST /marketplace/modules/acme/avada-files/pin"),
            Some(json!({ "workspace": "ws1", "version": "1.2.0" }))
        );
        app.set_pin("acme/avada-files", None).unwrap();
        assert_eq!(
            app.control
                .body_of("POST /marketplace/modules/acme/avada-files/unpin"),
            Some(json!({ "workspace": "ws1" }))
        );

        let mut app = App::new(FakeControl::default(), true, true);
        assert_eq!(
            app.set_pin("acme/avada-files", Some("1.0.0"))
                .unwrap_err()
                .kind(),
            ErrorCode::NoWorkspace
        );
    }

    #[test]
    fn setting_a_right_posts_the_value_and_takes_the_answer_as_the_new_table() {
        let after = json!({
            "module": "acme/avada-files", "version": "1.1.0", "profile": "reader",
            "profiles": [], "rows": [
                { "cap": "net.fetch", "description": "", "accepted": true,
                  "user": "always", "workspace": null, "effective": "always" }
            ],
        });
        let fake = focus_fake().answer(
            "POST /marketplace/modules/acme/avada-files/rights",
            200,
            after,
        );
        let mut app = ready(fake);
        app.focus("acme/avada-files").unwrap();

        app.set_right("acme/avada-files", "net.fetch", Some("always"), false)
            .unwrap();
        assert_eq!(
            app.control
                .body_of("POST /marketplace/modules/acme/avada-files/rights"),
            Some(json!({ "cap": "net.fetch", "value": "always" }))
        );
        let rights = app.state.rights.as_ref().unwrap();
        assert_eq!(
            rights.row("net.fetch").unwrap().user.as_deref(),
            Some("always")
        );
        assert!(
            rights.row("fs.read").is_none(),
            "the answer replaced the table"
        );
    }

    #[test]
    fn clearing_a_right_sends_no_value_and_a_workspace_one_sends_the_workspace() {
        let after = json!({ "module": "acme/avada-files", "profiles": [], "rows": [] });
        let fake = FakeControl::default().answer(
            "POST /marketplace/modules/acme/avada-files/rights",
            200,
            after,
        );
        let mut app = ready(fake);
        app.set_right("acme/avada-files", "net.fetch", None, false)
            .unwrap();
        assert_eq!(
            app.control
                .body_of("POST /marketplace/modules/acme/avada-files/rights"),
            Some(json!({ "cap": "net.fetch" })),
            "no `value` key is how the route is told to clear the override"
        );

        let mut app = ready(FakeControl::default().answer(
            "POST /marketplace/modules/acme/avada-files/rights",
            200,
            json!({ "profiles": [], "rows": [] }),
        ));
        app.set_right("acme/avada-files", "net.fetch", Some("ask"), true)
            .unwrap();
        assert_eq!(
            app.control
                .body_of("POST /marketplace/modules/acme/avada-files/rights"),
            Some(json!({ "cap": "net.fetch", "value": "ask", "workspace": "ws1" }))
        );

        let mut app = App::new(FakeControl::default(), true, true);
        assert_eq!(
            app.set_right("a/b", "net.fetch", Some("ask"), true)
                .unwrap_err()
                .kind(),
            ErrorCode::NoWorkspace
        );
    }

    #[test]
    fn choosing_and_clearing_a_profile_both_go_to_the_profile_route() {
        let fake = FakeControl::default().answer(
            "POST /marketplace/modules/acme/avada-files/profile",
            200,
            json!({ "profile": "writer", "profiles": [], "rows": [] }),
        );
        let mut app = ready(fake);
        app.set_profile("acme/avada-files", Some("writer")).unwrap();
        assert_eq!(
            app.control
                .body_of("POST /marketplace/modules/acme/avada-files/profile"),
            Some(json!({ "profile": "writer" }))
        );
        assert_eq!(
            app.state.rights.as_ref().unwrap().profile.as_deref(),
            Some("writer")
        );

        let mut app = ready(FakeControl::default().answer(
            "POST /marketplace/modules/acme/avada-files/profile",
            200,
            json!({ "profiles": [], "rows": [] }),
        ));
        app.set_profile("acme/avada-files", None).unwrap();
        assert_eq!(
            app.control
                .body_of("POST /marketplace/modules/acme/avada-files/profile"),
            Some(json!({}))
        );
    }

    #[test]
    fn every_pane_row_action_dispatches() {
        let fake = focus_fake()
            .answer(
                "POST /marketplace/modules/acme/avada-files/pin",
                200,
                json!({}),
            )
            .answer(
                "POST /marketplace/modules/acme/avada-files/unpin",
                200,
                json!({}),
            )
            .answer(
                "POST /marketplace/modules/acme/avada-files/rights",
                200,
                json!({ "profiles": [], "rows": [] }),
            )
            .answer(
                "POST /marketplace/modules/acme/avada-files/profile",
                200,
                json!({ "profiles": [], "rows": [] }),
            )
            .answer("POST /marketplace/install", 200, json!({ "id": "j9" }));
        let mut app = ready(fake);
        app.state.pane_open = true;
        // Walk the real pane rows so a row whose payload no arm handles is caught.
        app.focus("acme/avada-files").unwrap();
        let mut seen = std::collections::BTreeSet::new();
        for row in crate::rows::pane_rows(&app.state) {
            let Some(action) = row.data.get("action").and_then(Value::as_str) else {
                continue;
            };
            if !action.starts_with("pane.") {
                continue;
            }
            seen.insert(action.to_string());
            app.row_activate(&row.data, "open")
                .unwrap_or_else(|e| panic!("{action}: {}", e.message));
            // Every arm but `pane.index` leaves the module focused.
            if action != "pane.index" {
                assert_eq!(app.state.focus.as_deref(), Some("acme/avada-files"));
            } else {
                app.focus("acme/avada-files").unwrap();
            }
        }
        assert_eq!(
            seen.iter().map(String::as_str).collect::<Vec<_>>(),
            [
                "pane.index",
                "pane.install",
                "pane.pin",
                "pane.profile",
                "pane.right",
                "pane.unpin",
            ]
        );
    }

    #[test]
    fn a_pane_row_without_its_payload_is_refused_rather_than_guessed() {
        let mut app = ready(focus_fake());
        for data in [
            json!({ "action": "pane.pin", "module": "acme/avada-files" }),
            json!({ "action": "pane.right", "module": "acme/avada-files" }),
            json!({ "action": "pane.focus" }),
        ] {
            assert_eq!(
                app.row_activate(&data, "open").unwrap_err().kind(),
                ErrorCode::InvalidParams,
                "{data}"
            );
        }
    }

    #[test]
    fn manifest_commands_match_the_table() {
        let manifest = avada_module_sdk::Manifest::parse(crate::MANIFEST).unwrap();
        let mut declared: Vec<String> = manifest
            .contributions
            .iter()
            .filter(|c| c.kind == avada_module_sdk::manifest::ContributionKind::Command)
            .map(|c| c.id.clone())
            .collect();
        declared.sort();
        let mut table: Vec<String> = COMMANDS.iter().map(|(id, _)| id.to_string()).collect();
        table.sort();
        assert_eq!(declared, table);
    }

    #[test]
    fn every_declared_command_is_dispatched() {
        // A command that reaches the control fake is handled; an unknown one is not.
        for (id, _) in COMMANDS {
            let fake = FakeControl::default();
            let mut app = ready(fake);
            app.state.jobs.push(Job {
                id: "j1".into(),
                ..Default::default()
            });
            let args =
                json!({ "q": "x", "module": "acme/avada-files", "version": "1.0.0", "id": "j1" });
            // `pane` swallows the focus failure on purpose — the pane has to open so
            // the notice row can explain itself — so the proof of dispatch is the
            // control call, not the error.
            let _ = app.command(id, &args);
            assert!(
                !app.control.calls().is_empty(),
                "{id}: never reached the control leg"
            );
        }
        let mut app = ready(FakeControl::default());
        let err = app.command("bogus", &json!({})).unwrap_err();
        assert_eq!(err.kind(), ErrorCode::InvalidParams);
        assert!(app.control.calls().is_empty());
    }

    #[test]
    fn search_hits_the_route_with_an_encoded_query() {
        let fake = FakeControl::default().answer(
            "GET /marketplace/search?q=file%20browser",
            200,
            json!({ "modules": [{ "full_name": "acme/avada-files", "stars": 3 }] }),
        );
        let mut app = ready(fake);
        let out = app
            .command("search", &json!({ "q": "file browser" }))
            .unwrap();
        assert_eq!(out.result["results"], 1);
        assert_eq!(app.state.query.as_deref(), Some("file browser"));
        assert_eq!(app.state.results[0].full_name, "acme/avada-files");
        assert!(app.command("search", &json!({})).is_err());
    }

    #[test]
    fn install_posts_module_tag_and_workspace_and_records_the_job() {
        let fake = FakeControl::default().answer(
            "POST /marketplace/install",
            202,
            json!({ "job": { "id": "j7", "module": "acme/avada-files", "kind": "manual", "phase": "fetch", "log_tail": [], "started_at": 1 } }),
        );
        let mut app = ready(fake);
        let out = app
            .command(
                "install",
                &json!({ "module": "acme/avada-files", "tag": "v1.2.0" }),
            )
            .unwrap();
        assert_eq!(out.result["job"], "j7");
        assert_eq!(
            app.control.body_of("POST /marketplace/install").unwrap(),
            json!({ "module": "acme/avada-files", "tag": "v1.2.0", "workspace": "ws1" })
        );
        assert_eq!(app.state.jobs[0].id, "j7");
        assert!(out.toast.unwrap().contains("installing acme/avada-files"));
    }

    #[test]
    fn install_refuses_a_bad_module_id_before_calling_out() {
        let mut app = ready(FakeControl::default());
        for bad in ["", "acme", "acme/", "/x", "a/b/c", "../x/y", "acme/re po"] {
            let err = app
                .command("install", &json!({ "module": bad }))
                .unwrap_err();
            assert_eq!(err.kind(), ErrorCode::InvalidParams, "{bad:?}");
        }
        assert!(app.control.calls().is_empty());
    }

    #[test]
    fn a_412_carries_the_guide_into_the_notice() {
        let fake = FakeControl::default().answer(
            "POST /marketplace/install",
            412,
            json!({ "error": "toolchain missing: cargo", "guide": "brew install rustup" }),
        );
        let mut app = ready(fake);
        let err = app
            .command("install", &json!({ "module": "acme/avada-files" }))
            .unwrap_err();
        assert_eq!(err.kind(), ErrorCode::Other(-32412));
        assert!(err.message.contains("brew install rustup"));
        assert_eq!(app.state.notice.as_deref(), Some(err.message.as_str()));
    }

    #[test]
    fn enable_and_disable_post_the_workspace() {
        let fake = FakeControl::default()
            .answer(
                "POST /marketplace/modules/acme/avada-files/enable",
                200,
                json!({ "module": "acme/avada-files", "enabled": { "ws1": true } }),
            )
            .answer(
                "POST /marketplace/modules/acme/avada-files/disable",
                200,
                json!({ "module": "acme/avada-files", "enabled": { "ws1": false } }),
            );
        let mut app = ready(fake);
        app.state.installed = vec![Installed {
            module: Some("acme/avada-files".into()),
            version: Some("1.2.0".into()),
            ..Default::default()
        }];
        app.command("enable", &json!({ "module": "acme/avada-files" }))
            .unwrap();
        assert_eq!(
            app.control
                .body_of("POST /marketplace/modules/acme/avada-files/enable")
                .unwrap(),
            json!({ "workspace": "ws1" })
        );
        assert!(app.state.installed[0].enabled_in(Some("ws1")));
        app.command("disable", &json!({ "module": "acme/avada-files" }))
            .unwrap();
        assert_eq!(
            app.control
                .body_of("POST /marketplace/modules/acme/avada-files/disable")
                .unwrap(),
            json!({ "workspace": "ws1" })
        );
        assert!(!app.state.installed[0].enabled_in(Some("ws1")));
    }

    #[test]
    fn enable_without_a_workspace_is_no_workspace() {
        let mut app = App::new(FakeControl::default(), true, true);
        let err = app
            .command("enable", &json!({ "module": "acme/avada-files" }))
            .unwrap_err();
        assert_eq!(err.kind(), ErrorCode::NoWorkspace);
        assert!(app.control.calls().is_empty());
    }

    #[test]
    fn uninstall_deletes_the_given_or_active_version() {
        let fake = FakeControl::default()
            .answer(
                "DELETE /marketplace/modules/acme/avada-files/1.2.0",
                200,
                json!({ "ok": true }),
            )
            .answer(
                "DELETE /marketplace/modules/acme/avada-files/1.1.0",
                200,
                json!({ "ok": true }),
            );
        let mut app = ready(fake);
        app.state.installed = vec![
            Installed {
                module: Some("acme/avada-files".into()),
                version: Some("1.1.0".into()),
                active: false,
                ..Default::default()
            },
            Installed {
                module: Some("acme/avada-files".into()),
                version: Some("1.2.0".into()),
                active: true,
                ..Default::default()
            },
        ];
        app.command("uninstall", &json!({ "module": "acme/avada-files" }))
            .unwrap();
        assert_eq!(
            app.control.calls(),
            ["DELETE /marketplace/modules/acme/avada-files/1.2.0"]
        );
        assert_eq!(app.state.installed.len(), 1);
        app.command(
            "uninstall",
            &json!({ "module": "acme/avada-files", "version": "1.1.0" }),
        )
        .unwrap();
        assert!(app.state.installed.is_empty());
        let err = app
            .command("uninstall", &json!({ "module": "acme/avada-git" }))
            .unwrap_err();
        assert_eq!(err.kind(), ErrorCode::InvalidParams);
    }

    #[test]
    fn job_polls_and_refreshes_when_finished() {
        let fake = FakeControl::default()
            .answer("GET /marketplace/jobs/j1", 200, json!({ "id": "j1", "module": "acme/avada-files", "phase": "done", "version": "1.2.0" }))
            .answer("GET /marketplace/toolchain", 200, json!({ "ready": true, "missing": [], "signed_in": false }))
            .answer("GET /marketplace/installed", 200, json!({ "modules": [{ "module": "acme/avada-files", "version": "1.2.0", "active": true, "accepted": [], "enabled": {} }] }))
            .answer("GET /marketplace/jobs", 200, json!({ "jobs": [] }));
        let mut app = ready(fake);
        app.state.upsert_job(Job {
            id: "j1".into(),
            phase: "build".into(),
            ..Default::default()
        });
        let out = app.command("job", &json!({})).unwrap();
        assert_eq!(out.result["phase"], "done");
        assert!(out
            .toast
            .unwrap()
            .contains("installed acme/avada-files 1.2.0"));
        assert_eq!(app.state.jobs[0].phase, "done");
        assert_eq!(
            app.state.installed.len(),
            1,
            "a finished job refreshes the store"
        );
        assert!(app
            .control
            .calls()
            .contains(&"GET /marketplace/installed".to_string()));
    }

    #[test]
    fn job_without_any_known_job_needs_an_id() {
        let mut app = ready(FakeControl::default());
        let err = app.command("job", &json!({})).unwrap_err();
        assert_eq!(err.kind(), ErrorCode::InvalidParams);
    }

    #[test]
    fn signin_starts_then_polls_until_done() {
        let fake = FakeControl::default()
            .answer("POST /marketplace/signin", 200, json!({ "id": "s1", "user_code": "ABCD-1234", "verification_uri": "https://github.com/login/device", "expires_at": 9, "interval": 5, "status": "pending" }))
            .answer("GET /marketplace/signin/s1", 200, json!({ "id": "s1", "user_code": "ABCD-1234", "verification_uri": "https://github.com/login/device", "expires_at": 9, "interval": 5, "status": "done" }));
        let mut app = ready(fake);
        app.state.toolchain = Some(Toolchain::default());
        let out = app.command("signin", &json!({})).unwrap();
        assert_eq!(out.result["user_code"], "ABCD-1234");
        assert!(out.toast.unwrap().contains("ABCD-1234"));
        let out = app.command("signin", &json!({})).unwrap();
        assert_eq!(out.result["status"], "done");
        assert_eq!(
            app.control.calls(),
            ["POST /marketplace/signin", "GET /marketplace/signin/s1"]
        );
        assert!(app.state.toolchain.as_ref().unwrap().signed_in);
    }

    #[test]
    fn signin_503_is_reported_not_swallowed() {
        let fake = FakeControl::default().answer(
            "POST /marketplace/signin",
            503,
            json!({ "error": "sign-in unavailable" }),
        );
        let mut app = ready(fake);
        let err = app.command("signin", &json!({})).unwrap_err();
        assert!(err.message.contains("sign-in unavailable"));
        assert!(app.state.signin.is_none());
    }

    #[test]
    fn refresh_fetches_toolchain_and_installed_then_known_jobs() {
        let fake = FakeControl::default()
            .answer("GET /marketplace/toolchain", 200, json!({ "ready": false, "missing": ["git"], "guide": "xcode-select --install", "signed_in": true }))
            .answer("GET /marketplace/installed", 200, json!({ "modules": [{ "module": "acme/avada-files", "version": "1.2.0", "active": true, "accepted": [], "enabled": { "ws1": true } }] }))
            .answer("GET /marketplace/jobs", 200, json!({ "jobs": [{ "id": "j1", "module": "acme/avada-files", "phase": "failed", "error": "refused" }, { "id": "other", "module": "x/y", "phase": "build" }] }));
        let mut app = ready(fake);
        let out = app.command("refresh", &json!({})).unwrap();
        assert_eq!(out.result["installed"], 1);
        assert_eq!(
            app.control.calls(),
            ["GET /marketplace/toolchain", "GET /marketplace/installed"]
        );
        assert_eq!(app.state.toolchain.as_ref().unwrap().missing, ["git"]);
        app.state.upsert_job(Job {
            id: "j1".into(),
            ..Default::default()
        });
        app.command("refresh", &json!({})).unwrap();
        assert_eq!(
            app.state.jobs.len(),
            1,
            "only jobs this UI knows are tracked"
        );
        assert_eq!(app.state.jobs[0].phase, "failed");
    }

    #[test]
    fn without_the_capability_nothing_leaves_the_process() {
        let mut app = App::new(FakeControl::default(), false, true);
        let err = app.command("refresh", &json!({})).unwrap_err();
        assert_eq!(err.kind(), ErrorCode::CapabilityDenied);
        assert!(app.control.calls().is_empty());
        let mut app = App::new(FakeControl::default(), true, false);
        let err = app.command("refresh", &json!({})).unwrap_err();
        assert_eq!(err.kind(), ErrorCode::Internal);
        assert!(app.control.calls().is_empty());
    }

    #[test]
    fn row_activation_maps_data_to_the_same_routes() {
        let fake = FakeControl::default()
            .answer(
                "POST /marketplace/install",
                202,
                json!({ "job": { "id": "j1", "module": "acme/avada-files", "phase": "fetch" } }),
            )
            .answer(
                "POST /marketplace/modules/acme/avada-files/disable",
                200,
                json!({ "enabled": {} }),
            )
            .answer(
                "POST /marketplace/modules/acme/avada-files/enable",
                200,
                json!({ "enabled": {} }),
            )
            .answer(
                "GET /marketplace/jobs/j1",
                200,
                json!({ "id": "j1", "module": "acme/avada-files", "phase": "build" }),
            )
            .answer(
                "POST /marketplace/signin",
                200,
                json!({ "id": "s1", "status": "pending" }),
            )
            .answer(
                "GET /marketplace/signin/s1",
                200,
                json!({ "id": "s1", "status": "pending" }),
            );
        let mut app = ready(fake);
        app.state.toolchain = Some(Toolchain {
            guide: Some("brew install rustup".into()),
            ..Default::default()
        });
        let m = json!({ "action": "install", "module": "acme/avada-files" });
        app.row_activate(&m, "open").unwrap();
        app.row_activate(
            &json!({ "action": "disable", "module": "acme/avada-files" }),
            "open",
        )
        .unwrap();
        app.row_activate(
            &json!({ "action": "enable", "module": "acme/avada-files" }),
            "open",
        )
        .unwrap();
        app.row_activate(&json!({ "action": "job", "id": "j1" }), "open")
            .unwrap();
        app.row_activate(&json!({ "action": "signin" }), "open")
            .unwrap();
        app.row_activate(&json!({ "action": "signin.poll", "id": "s1" }), "open")
            .unwrap();
        let out = app
            .row_activate(&json!({ "action": "toolchain" }), "open")
            .unwrap();
        assert_eq!(out.toast.as_deref(), Some("brew install rustup"));
        assert_eq!(
            app.control.calls(),
            [
                "POST /marketplace/install",
                "POST /marketplace/modules/acme/avada-files/disable",
                "POST /marketplace/modules/acme/avada-files/enable",
                "GET /marketplace/jobs/j1",
                "POST /marketplace/signin",
                "GET /marketplace/signin/s1",
            ]
        );
        // Other gestures and unknown payloads do nothing.
        assert_eq!(app.row_activate(&m, "context").unwrap(), Outcome::default());
        assert_eq!(
            app.row_activate(&json!({}), "open").unwrap(),
            Outcome::default()
        );
        assert_eq!(app.control.calls().len(), 6);
    }
}
