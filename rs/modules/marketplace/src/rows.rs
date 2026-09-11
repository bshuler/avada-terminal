//! Rows from state: the whole tier-1 rendering is this one pure function, so a
//! unit test can pin every line the user sees without a socket or a host.
//!
//! Layout (depth 0 headers, depth 1 items):
//!
//! ```text
//! Toolchain      ready | missing: cargo, git (open: show the guide)
//! GitHub         signed in | not signed in (open: sign in) | code ABCD-1234 (open: poll)
//! Installed      3 modules
//!   acme/avada-files 1.2.0   enabled (open: disable) | disabled (open: enable)
//! Search         "files" · 2 results
//!   acme/avada-files        A file browser · 12★ (open: install)
//! Jobs           1 running
//!   acme/avada-files        build 40% (open: poll)
//! ```

use avada_module_sdk::rail::Row;
use serde_json::{json, Value};

use crate::model::State;

/// The rail entry id from `avada.toml`.
pub const ENTRY: &str = "marketplace";

fn row(id: impl Into<String>, label: impl Into<String>, detail: impl Into<String>) -> Row {
    Row {
        id: id.into(),
        label: label.into(),
        detail: detail.into(),
        depth: 0,
        expandable: false,
        expanded: false,
        icon: None,
        marks: vec![],
        data: Value::Null,
    }
}

fn item(id: impl Into<String>, label: impl Into<String>, detail: impl Into<String>) -> Row {
    Row {
        depth: 1,
        ..row(id, label, detail)
    }
}

fn with(mut r: Row, data: Value) -> Row {
    r.data = data;
    r
}

fn marked(mut r: Row, mark: &str) -> Row {
    r.marks.push(mark.to_string());
    r
}

/// Percent-encode everything but RFC 3986 unreserved characters.
pub fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn sub(id: impl Into<String>, label: impl Into<String>, detail: impl Into<String>) -> Row {
    Row {
        depth: 2,
        ..row(id, label, detail)
    }
}

/// The four values a right can hold, plus the cleared state, in the order the pane
/// cycles them.
///
/// `None` is "no override": the profile decides. Cycling back to it is the only way
/// a user undoes a per-capability choice, so it has to be part of the ring rather
/// than a separate control.
const RIGHT_CYCLE: &[Option<&str>] = &[
    None,
    Some("never"),
    Some("always"),
    Some("workspace"),
    Some("ask"),
];

/// The value after `current` in [`RIGHT_CYCLE`].
fn next_right(current: Option<&str>) -> Option<&'static str> {
    let at = RIGHT_CYCLE.iter().position(|v| *v == current).unwrap_or(0);
    RIGHT_CYCLE[(at + 1) % RIGHT_CYCLE.len()]
}

/// The rows for `state`.
pub fn rows(state: &State) -> Vec<Row> {
    let mut out = Vec::new();

    if !state.manage_granted {
        out.push(row(
            "no-capability",
            "Marketplace is off",
            "Grant `marketplace.manage` to this module to search, install and manage modules",
        ));
        return out;
    }
    if !state.control_available {
        out.push(row(
            "no-control",
            "Marketplace is unreachable",
            "The host started this module without a control URL; restart Avada Terminal",
        ));
        return out;
    }

    if let Some(n) = &state.notice {
        out.push(marked(row("notice", "Last error", n.clone()), "error"));
    }

    // Toolchain.
    match &state.toolchain {
        None => out.push(row(
            "toolchain",
            "Toolchain",
            "not checked yet · run Refresh",
        )),
        Some(tc) if tc.ready => out.push(marked(
            row("toolchain", "Toolchain", "ready for free builds"),
            "ok",
        )),
        Some(tc) => out.push(marked(
            with(
                row(
                    "toolchain",
                    "Toolchain",
                    format!(
                        "missing: {} · open for install steps",
                        tc.missing.join(", ")
                    ),
                ),
                json!({ "action": "toolchain" }),
            ),
            "error",
        )),
    }

    // GitHub sign-in.
    let signed_in = state.toolchain.as_ref().is_some_and(|t| t.signed_in);
    match &state.signin {
        Some(s) if s.pending() => out.push(with(
            row(
                "github",
                "GitHub",
                format!(
                    "enter code {} at {} · open to check",
                    s.user_code, s.verification_uri
                ),
            ),
            json!({ "action": "signin.poll", "id": s.id }),
        )),
        Some(s) if s.status == "done" => {
            out.push(marked(row("github", "GitHub", "signed in"), "ok"))
        }
        Some(s) => out.push(with(
            row(
                "github",
                "GitHub",
                format!("sign-in {} · open to try again", s.status),
            ),
            json!({ "action": "signin" }),
        )),
        None if signed_in => out.push(marked(row("github", "GitHub", "signed in"), "ok")),
        None => out.push(with(
            row(
                "github",
                "GitHub",
                "not signed in (60 requests/hour) · open to sign in",
            ),
            json!({ "action": "signin" }),
        )),
    }

    // Installed.
    let ws = state.workspace.as_deref();
    out.push(row(
        "installed",
        "Installed",
        match state.installed.len() {
            0 => "nothing installed".to_string(),
            1 => "1 version".to_string(),
            n => format!("{n} versions"),
        },
    ));
    for (i, inst) in state.installed.iter().enumerate() {
        let module = inst.module.clone().unwrap_or_else(|| "?".into());
        let version = inst.version.clone().unwrap_or_else(|| "?".into());
        let enabled = inst.enabled_in(ws);
        let mut detail = String::new();
        if let Some(b) = &inst.broken {
            detail.push_str(&format!("broken: {b}"));
        } else {
            detail.push_str(&version);
            if inst.active {
                detail.push_str(" · active");
            }
            if inst.kind.as_deref() == Some("dependency") {
                detail.push_str(" · dependency");
            }
            match (ws, enabled) {
                (None, _) => detail.push_str(" · no workspace"),
                (Some(_), true) => detail.push_str(" · enabled · open to disable"),
                (Some(_), false) => detail.push_str(" · disabled · open to enable"),
            }
        }
        let mut r = with(
            item(format!("installed-{i}"), module.clone(), detail),
            json!({
                "action": if enabled { "disable" } else { "enable" },
                "module": module,
                "version": version,
            }),
        );
        if inst.broken.is_some() {
            r = marked(r, "error");
        } else if enabled {
            r = marked(r, "ok");
        }
        out.push(r);
    }

    // Search.
    out.push(row(
        "search",
        "Search",
        match &state.query {
            None => "run “Marketplace: Search modules” with q=<text>".to_string(),
            Some(q) => format!("“{q}” · {} results", state.results.len()),
        },
    ));
    for (i, repo) in state.results.iter().enumerate() {
        let mut detail = repo.description.clone().unwrap_or_default();
        if !detail.is_empty() {
            detail.push_str(" · ");
        }
        detail.push_str(&format!("{}★ · open to install", repo.stars));
        out.push(with(
            item(format!("result-{i}"), repo.full_name.clone(), detail),
            json!({ "action": "install", "module": repo.full_name }),
        ));
    }

    // Jobs.
    if !state.jobs.is_empty() {
        let running = state.jobs.iter().filter(|j| !j.finished()).count();
        out.push(row(
            "jobs",
            "Install jobs",
            match running {
                0 => "none running".to_string(),
                1 => "1 running".to_string(),
                n => format!("{n} running"),
            },
        ));
        for job in &state.jobs {
            let mut detail = job.phase.clone();
            if let Some(p) = job.progress {
                detail.push_str(&format!(" {p}%"));
            }
            if let Some(v) = &job.version {
                detail.push_str(&format!(" · {v}"));
            }
            if let Some(e) = &job.error {
                detail.push_str(&format!(" · {e}"));
            } else if !job.finished() {
                detail.push_str(" · open to check");
            }
            let mut r = with(
                item(format!("job-{}", job.id), job.module.clone(), detail),
                json!({ "action": "job", "id": job.id }),
            );
            r = match job.phase.as_str() {
                "done" => marked(r, "ok"),
                "failed" => marked(r, "error"),
                _ => marked(r, "modified"),
            };
            out.push(r);
        }
    }

    out
}

/// The rows for the module pane: the same state, projected wider.
///
/// The rail has room for one line per module; the pane has room for the whole
/// story, so this is where versions, pins and per-capability rights live. It is a
/// second pure function over the same [`State`] rather than a mode flag inside
/// [`rows`], because the two surfaces are pushed independently and a shared
/// function would have to be told which one it was rendering on every call.
pub fn pane_rows(state: &State) -> Vec<Row> {
    let mut out = Vec::new();

    if !state.manage_granted {
        out.push(row(
            "no-capability",
            "Marketplace is off",
            "Grant `marketplace.manage` to this module to search, install and manage modules",
        ));
        return out;
    }
    if let Some(n) = &state.notice {
        out.push(marked(row("notice", "Last error", n.clone()), "error"));
    }

    match state.focus.as_deref() {
        None => pane_index(state, &mut out),
        Some(module) => pane_module(state, module, &mut out),
    }
    out
}

/// The pane's index: everything installed, then everything the last search found.
fn pane_index(state: &State, out: &mut Vec<Row>) {
    let ws = state.workspace.as_deref();
    out.push(row(
        "installed",
        "Installed",
        match state.installed.len() {
            0 => "nothing installed".to_string(),
            1 => "1 version".to_string(),
            n => format!("{n} versions"),
        },
    ));
    for (i, inst) in state.installed.iter().enumerate() {
        let module = inst.module.clone().unwrap_or_else(|| "?".into());
        let version = inst.version.clone().unwrap_or_else(|| "?".into());
        let mut detail = version.clone();
        if inst.active {
            detail.push_str(" · active");
        }
        if let Some(p) = state.pin(&module) {
            detail.push_str(&format!(" · pinned {p}"));
        }
        if inst.enabled_in(ws) {
            detail.push_str(" · enabled");
        }
        let mut r = with(
            item(format!("installed-{i}"), module.clone(), detail),
            json!({ "action": "pane.focus", "module": module }),
        );
        if inst.broken.is_some() {
            r = marked(r, "error");
        } else if inst.enabled_in(ws) {
            r = marked(r, "ok");
        }
        if state.pin(&module).is_some() {
            r = marked(r, "modified");
        }
        out.push(r);
    }

    out.push(row(
        "search",
        "Search",
        match &state.query {
            None => "run “Marketplace: Search modules” with q=<text>".to_string(),
            Some(q) => format!("“{q}” · {} results", state.results.len()),
        },
    ));
    for (i, repo) in state.results.iter().enumerate() {
        let mut detail = repo.description.clone().unwrap_or_default();
        if !detail.is_empty() {
            detail.push_str(" · ");
        }
        detail.push_str(&format!("{}★ · open for versions", repo.stars));
        out.push(with(
            item(format!("result-{i}"), repo.full_name.clone(), detail),
            json!({ "action": "pane.focus", "module": repo.full_name }),
        ));
    }
}

/// One module in depth: heading, version picker, permissions.
fn pane_module(state: &State, module: &str, out: &mut Vec<Row>) {
    out.push(with(
        row("back", "← All modules", "open to leave this module"),
        json!({ "action": "pane.index" }),
    ));

    let view = state.view.as_ref();
    let heading = view
        .and_then(|v| v.repo.as_ref())
        .map(|r| {
            let mut d = r.description.clone().unwrap_or_default();
            if !d.is_empty() {
                d.push_str(" · ");
            }
            d.push_str(&format!("{}★", r.stars));
            d
        })
        .unwrap_or_else(|| "no repository details".to_string());
    out.push(row("module", module.to_string(), heading));
    if let Some(e) = view.and_then(|v| v.manifest_error.as_ref()) {
        out.push(marked(
            item("manifest-error", "Manifest", e.clone()),
            "error",
        ));
    }

    pane_versions(state, module, out);
    pane_rights(state, module, out);
}

/// The version picker: every tag the repo has, marked with what is installed,
/// active and pinned, and each one openable to install or pin it.
fn pane_versions(state: &State, module: &str, out: &mut Vec<Row>) {
    let view = state.view.as_ref();
    let tags: Vec<String> = view
        .map(|v| v.tags.iter().map(|t| t.name.clone()).collect())
        .unwrap_or_default();
    let installed: Vec<String> = view.map(|v| v.installed.clone()).unwrap_or_default();
    let active = view.and_then(|v| v.active.clone());
    let pin = state.pin(module).map(str::to_string);

    out.push(row(
        "versions",
        "Versions",
        match (tags.len(), &pin) {
            (0, _) => "no tags · sign in to GitHub to list them".to_string(),
            (n, Some(p)) => format!("{n} tags · pinned to {p} here"),
            (n, None) => format!("{n} tags · not pinned here"),
        },
    ));
    // Anything installed but no longer tagged still has to be reachable, or a
    // yanked release would strand the version the user is actually running.
    let mut names = tags.clone();
    for v in &installed {
        if !names.contains(v) {
            names.push(v.clone());
        }
    }
    for (i, name) in names.iter().enumerate() {
        let is_installed = installed.contains(name);
        let is_active = active.as_deref() == Some(name.as_str());
        let is_pinned = pin.as_deref() == Some(name.as_str());
        let mut detail = Vec::new();
        if is_active {
            detail.push("active".to_string());
        } else if is_installed {
            detail.push("installed".to_string());
        }
        if is_pinned {
            detail.push("pinned".to_string());
        }
        detail.push(
            match (is_installed, is_pinned) {
                (false, _) => "open to install",
                (true, true) => "open to unpin",
                (true, false) => "open to pin here",
            }
            .to_string(),
        );
        let data = match (is_installed, is_pinned) {
            (false, _) => json!({ "action": "pane.install", "module": module, "tag": name }),
            (true, true) => json!({ "action": "pane.unpin", "module": module }),
            (true, false) => {
                json!({ "action": "pane.pin", "module": module, "version": name })
            }
        };
        let mut r = with(
            item(format!("version-{i}"), name.clone(), detail.join(" · ")),
            data,
        );
        if is_active {
            r = marked(r, "ok");
        }
        if is_pinned {
            r = marked(r, "modified");
        }
        out.push(r);
    }
}

/// The profiles picker and the per-capability rights table.
fn pane_rights(state: &State, module: &str, out: &mut Vec<Row>) {
    let Some(rights) = state.rights.as_ref() else {
        out.push(row(
            "rights",
            "Permissions",
            "install this module to set its permissions",
        ));
        return;
    };

    out.push(row(
        "rights",
        "Permissions",
        match &rights.profile {
            Some(p) => format!("profile “{p}” · {} capabilities", rights.rows.len()),
            None => format!("no profile · {} capabilities", rights.rows.len()),
        },
    ));

    for (i, profile) in rights.profiles.iter().enumerate() {
        let chosen = rights.profile.as_deref() == Some(profile.name.as_str());
        let mut detail = profile.description.clone();
        if !detail.is_empty() {
            detail.push_str(" · ");
        }
        detail.push_str(if chosen {
            "in use · open to clear"
        } else {
            "open to use"
        });
        // Opening the profile that is already in use clears it, so the same row is
        // both the indicator and the undo.
        let mut data = json!({ "action": "pane.profile", "module": module });
        if !chosen {
            data["profile"] = Value::String(profile.name.clone());
        }
        let mut r = with(
            item(format!("profile-{i}"), profile.name.clone(), detail),
            data,
        );
        if chosen {
            r = marked(r, "ok");
        }
        out.push(r);
    }

    for (i, cap) in rights.rows.iter().enumerate() {
        let mut detail = cap.effective.clone();
        if !cap.accepted {
            detail.push_str(" · not accepted at install");
        }
        if !cap.description.is_empty() {
            detail.push_str(&format!(" · {}", cap.description));
        }
        let mut r = with(
            item(format!("cap-{i}"), cap.cap.clone(), detail),
            right_data(module, &cap.cap, cap.user.as_deref(), false),
        );
        r = match (cap.effective.as_str(), cap.user.is_some()) {
            ("never", _) => marked(r, "error"),
            (_, true) => marked(r, "modified"),
            ("always", _) => marked(r, "ok"),
            _ => r,
        };
        out.push(r);

        out.push(with(
            sub(
                format!("cap-{i}-user"),
                "everywhere",
                match &cap.user {
                    Some(v) => format!("{v} · open for {}", show(next_right(Some(v)))),
                    None => format!("follows the profile · open for {}", show(next_right(None))),
                },
            ),
            right_data(module, &cap.cap, cap.user.as_deref(), false),
        ));

        if state.workspace.is_some() {
            out.push(with(
                sub(
                    format!("cap-{i}-workspace"),
                    "in this workspace",
                    match &cap.workspace {
                        Some(v) => format!("{v} · open for {}", show(next_right(Some(v)))),
                        None => {
                            format!("no override · open for {}", show(next_right(None)))
                        }
                    },
                ),
                right_data(module, &cap.cap, cap.workspace.as_deref(), true),
            ));
        }
    }
}

/// How a cycle position reads in a row detail.
fn show(v: Option<&str>) -> &str {
    v.unwrap_or("no override")
}

/// The activation payload that moves one right to its next value; a cleared value
/// carries no `value` key at all, which is what the route reads as "clear it".
fn right_data(module: &str, cap: &str, current: Option<&str>, workspace: bool) -> Value {
    let mut data = json!({ "action": "pane.right", "module": module, "cap": cap });
    if workspace {
        data["scope"] = Value::String("workspace".into());
    }
    if let Some(next) = next_right(current) {
        data["value"] = Value::String(next.to_string());
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        Installed, Job, ModuleView, Profile, RepoSummary, RightsRow, RightsView, SignIn, TagInfo,
        Toolchain,
    };

    fn ready_state() -> State {
        State {
            manage_granted: true,
            control_available: true,
            workspace: Some("ws1".into()),
            toolchain: Some(Toolchain {
                ready: true,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn find(all: &[Row], id: &str) -> Row {
        all.iter()
            .find(|r| r.id == id)
            .cloned()
            .unwrap_or_else(|| panic!("no row {id}"))
    }

    fn row(st: &State, id: &str) -> Row {
        find(&rows(st), id)
    }

    #[test]
    fn without_the_capability_only_the_explanation_shows() {
        let rows = rows(&State::default());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "no-capability");
        assert!(rows[0].detail.contains("marketplace.manage"));
    }

    #[test]
    fn without_a_control_url_only_the_explanation_shows() {
        let rows = rows(&State {
            manage_granted: true,
            ..Default::default()
        });
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "no-control");
    }

    #[test]
    fn toolchain_rows_from_route_json() {
        let tc: Toolchain = serde_json::from_value(json!({
            "ready": false, "missing": ["cargo", "git"], "guide": "brew install rustup",
            "toolchain": { "rustup": null }, "signed_in": false
        }))
        .unwrap();
        let mut st = ready_state();
        st.toolchain = Some(tc);
        let rows = rows(&st);
        let r = find(&rows, "toolchain");
        assert!(r.detail.contains("missing: cargo, git"));
        assert_eq!(r.data["action"], "toolchain");
        assert!(r.marks.contains(&"error".to_string()));

        let r = row(&ready_state(), "toolchain");
        assert!(r.detail.contains("ready"));
        assert!(r.data.is_null());
    }

    #[test]
    fn signin_rows_follow_state() {
        let r = row(&ready_state(), "github");
        assert_eq!(r.data["action"], "signin");

        let mut st = ready_state();
        st.signin = Some(
            serde_json::from_value::<SignIn>(json!({
                "id": "s1", "user_code": "ABCD-1234", "verification_uri": "https://github.com/login/device",
                "expires_at": 0, "interval": 5, "status": "pending"
            }))
            .unwrap(),
        );
        let r = row(&st, "github");
        assert!(r.detail.contains("ABCD-1234"));
        assert_eq!(r.data["action"], "signin.poll");
        assert_eq!(r.data["id"], "s1");

        st.signin.as_mut().unwrap().status = "done".into();
        let r = row(&st, "github");
        assert_eq!(r.detail, "signed in");
        assert!(r.data.is_null());

        st.signin = None;
        st.toolchain.as_mut().unwrap().signed_in = true;
        assert_eq!(row(&st, "github").detail, "signed in");
    }

    #[test]
    fn installed_rows_carry_toggle_actions_for_the_workspace() {
        let list: Vec<Installed> = serde_json::from_value(json!([
            { "module": "acme/avada-files", "version": "1.2.0", "active": true, "kind": "manual",
              "tag": "v1.2.0", "accepted": ["ui.rail"], "enabled": { "ws1": true } },
            { "module": "acme/avada-git", "version": "0.3.0", "active": true, "kind": "dependency",
              "accepted": [], "enabled": {} },
            { "module": null, "version": null, "active": false, "accepted": [], "enabled": {},
              "broken": "unreadable record" }
        ]))
        .unwrap();
        let mut st = ready_state();
        st.installed = list;
        let rows = rows(&st);
        assert_eq!(find(&rows, "installed").detail, "3 versions");
        let a = find(&rows, "installed-0");
        assert_eq!(a.label, "acme/avada-files");
        assert!(a.detail.contains("enabled · open to disable"));
        assert_eq!(a.data["action"], "disable");
        assert_eq!(a.data["module"], "acme/avada-files");
        assert_eq!(a.data["version"], "1.2.0");
        let b = find(&rows, "installed-1");
        assert!(b.detail.contains("dependency"));
        assert!(b.detail.contains("disabled · open to enable"));
        assert_eq!(b.data["action"], "enable");
        let c = find(&rows, "installed-2");
        assert!(c.detail.starts_with("broken:"));
        assert!(c.marks.contains(&"error".to_string()));
    }

    #[test]
    fn search_rows_carry_install_actions() {
        let results: Vec<RepoSummary> = serde_json::from_value(json!([
            { "full_name": "acme/avada-files", "description": "A file browser", "html_url": "https://github.com/acme/avada-files", "stars": 12 },
            { "full_name": "acme/avada-git", "description": null, "html_url": "", "stars": 0 }
        ]))
        .unwrap();
        let mut st = ready_state();
        st.query = Some("files".into());
        st.results = results;
        let rows = rows(&st);
        assert!(find(&rows, "search").detail.contains("2 results"));
        let r = find(&rows, "result-0");
        assert_eq!(r.label, "acme/avada-files");
        assert!(r.detail.starts_with("A file browser · 12★"));
        assert_eq!(
            r.data,
            json!({ "action": "install", "module": "acme/avada-files" })
        );
        assert_eq!(find(&rows, "result-1").detail, "0★ · open to install");
    }

    #[test]
    fn job_rows_show_phase_and_poll_action() {
        let mut st = ready_state();
        st.upsert_job(
            serde_json::from_value::<Job>(json!({
                "id": "j1", "module": "acme/avada-files", "kind": "manual", "phase": "build",
                "progress": 40, "log_tail": [], "started_at": 1
            }))
            .unwrap(),
        );
        let all = rows(&st);
        assert_eq!(find(&all, "jobs").detail, "1 running");
        let r = find(&all, "job-j1");
        assert!(r.detail.starts_with("build 40%"));
        assert_eq!(r.data, json!({ "action": "job", "id": "j1" }));

        st.upsert_job(
            serde_json::from_value::<Job>(json!({
                "id": "j1", "module": "acme/avada-files", "kind": "manual", "phase": "failed",
                "error": "refused: not free", "log_tail": [], "started_at": 1
            }))
            .unwrap(),
        );
        let all = rows(&st);
        assert_eq!(find(&all, "jobs").detail, "none running");
        let r = find(&all, "job-j1");
        assert!(r.detail.contains("refused: not free"));
        assert!(r.marks.contains(&"error".to_string()));
        assert_eq!(st.jobs.len(), 1, "upsert replaces by id");
    }

    #[test]
    fn a_notice_is_the_first_row() {
        let mut st = ready_state();
        st.notice = Some("502: GitHub is down".into());
        let rows = rows(&st);
        assert_eq!(rows[0].id, "notice");
        assert_eq!(rows[0].detail, "502: GitHub is down");
    }

    #[test]
    fn url_encode_keeps_unreserved_and_escapes_the_rest() {
        assert_eq!(url_encode("files"), "files");
        assert_eq!(url_encode("a b&c=d/é"), "a%20b%26c%3Dd%2F%C3%A9");
    }

    #[test]
    fn row_ids_are_unique() {
        let mut st = ready_state();
        st.installed = vec![Installed::default(), Installed::default()];
        st.results = vec![RepoSummary::default(), RepoSummary::default()];
        st.jobs = vec![
            Job {
                id: "a".into(),
                ..Default::default()
            },
            Job {
                id: "b".into(),
                ..Default::default()
            },
        ];
        st.notice = Some("x".into());
        let rows = rows(&st);
        let mut ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), rows.len());
    }

    /// A state focused on one module that has three tags, two installed, one pinned,
    /// and a rights table with a chosen profile.
    fn focused_state() -> State {
        let mut st = ready_state();
        st.pane_open = true;
        st.focus = Some("acme/avada-files".into());
        st.pins
            .insert("acme/avada-files".into(), "1.1.0".to_string());
        st.view = Some(ModuleView {
            module: "acme/avada-files".into(),
            repo: Some(RepoSummary {
                full_name: "acme/avada-files".into(),
                description: Some("A file browser".into()),
                stars: 7,
                ..Default::default()
            }),
            tags: ["1.2.0", "1.1.0", "1.0.0"]
                .iter()
                .map(|n| TagInfo {
                    name: (*n).to_string(),
                    commit: "c".into(),
                })
                .collect(),
            newest_tag: Some("1.2.0".into()),
            installed: vec!["1.1.0".into(), "1.2.0".into()],
            active: Some("1.1.0".into()),
            ..Default::default()
        });
        st.rights = Some(RightsView {
            module: "acme/avada-files".into(),
            version: "1.1.0".into(),
            workspace: Some("ws1".into()),
            profile: Some("reader".into()),
            profiles: vec![
                Profile {
                    name: "reader".into(),
                    description: "read only".into(),
                    ..Default::default()
                },
                Profile {
                    name: "writer".into(),
                    description: "read and write".into(),
                    ..Default::default()
                },
            ],
            rows: vec![
                RightsRow {
                    cap: "fs.read".into(),
                    description: "read files".into(),
                    accepted: true,
                    effective: "always".into(),
                    ..Default::default()
                },
                RightsRow {
                    cap: "net.fetch".into(),
                    description: "reach the network".into(),
                    accepted: false,
                    user: Some("never".into()),
                    workspace: Some("ask".into()),
                    effective: "never".into(),
                },
            ],
        });
        st
    }

    fn pane(st: &State, id: &str) -> Row {
        find(&pane_rows(st), id)
    }

    #[test]
    fn the_pane_index_lists_installed_then_search_hits_and_both_open_a_module() {
        let mut st = ready_state();
        st.pane_open = true;
        st.installed = vec![Installed {
            module: Some("acme/avada-files".into()),
            version: Some("1.1.0".into()),
            active: true,
            enabled: [("ws1".to_string(), true)].into_iter().collect(),
            ..Default::default()
        }];
        st.results = vec![RepoSummary {
            full_name: "acme/avada-git".into(),
            stars: 3,
            ..Default::default()
        }];
        st.pins.insert("acme/avada-files".into(), "1.1.0".into());

        let r = pane(&st, "installed-0");
        assert_eq!(r.data["action"], "pane.focus");
        assert_eq!(r.data["module"], "acme/avada-files");
        assert!(r.detail.contains("active"), "{}", r.detail);
        assert!(r.detail.contains("pinned 1.1.0"), "{}", r.detail);
        assert!(r.marks.contains(&"ok".to_string()));
        assert!(r.marks.contains(&"modified".to_string()));

        let r = pane(&st, "result-0");
        assert_eq!(r.data["action"], "pane.focus");
        assert_eq!(r.data["module"], "acme/avada-git");
    }

    #[test]
    fn the_version_picker_offers_install_pin_and_unpin_in_the_right_places() {
        let st = focused_state();
        let all = pane_rows(&st);
        let by_label = |label: &str| {
            all.iter()
                .find(|r| r.label == label && r.id.starts_with("version-"))
                .cloned()
                .unwrap_or_else(|| panic!("no version row {label}"))
        };

        // Installed and pinned: the only way back out is the same row.
        let cur = by_label("1.1.0");
        assert_eq!(cur.data["action"], "pane.unpin");
        assert!(cur.marks.contains(&"ok".to_string()), "active");
        assert!(cur.marks.contains(&"modified".to_string()), "pinned");

        // Installed, not pinned: pinning is the offer.
        let newer = by_label("1.2.0");
        assert_eq!(newer.data["action"], "pane.pin");
        assert_eq!(newer.data["version"], "1.2.0");
        assert!(newer.detail.contains("installed"), "{}", newer.detail);

        // Not installed: installing is.
        let old = by_label("1.0.0");
        assert_eq!(old.data["action"], "pane.install");
        assert_eq!(old.data["tag"], "1.0.0");

        assert!(pane(&st, "versions")
            .detail
            .contains("pinned to 1.1.0 here"));
    }

    #[test]
    fn an_installed_version_that_is_no_longer_tagged_is_still_listed() {
        // A yanked release must not strand the version the user is running.
        let mut st = focused_state();
        let view = st.view.as_mut().unwrap();
        view.tags.clear();
        view.installed = vec!["9.9.9".into()];
        view.active = Some("9.9.9".into());
        let all = pane_rows(&st);
        assert!(all.iter().any(|r| r.label == "9.9.9"));
    }

    #[test]
    fn the_profile_rows_mark_the_chosen_one_and_clearing_it_sends_no_name() {
        let st = focused_state();
        let chosen = pane(&st, "profile-0");
        assert_eq!(chosen.label, "reader");
        assert!(chosen.marks.contains(&"ok".to_string()));
        assert_eq!(chosen.data["action"], "pane.profile");
        assert!(
            chosen.data.get("profile").is_none(),
            "opening the profile in use clears it"
        );

        let other = pane(&st, "profile-1");
        assert_eq!(other.data["profile"], "writer");
        assert!(other.marks.is_empty());
    }

    #[test]
    fn each_capability_shows_its_effective_value_and_carries_the_next_one() {
        let st = focused_state();
        // No override: the ring starts at `never`.
        let read = pane(&st, "cap-0");
        assert_eq!(read.label, "fs.read");
        assert!(read.detail.starts_with("always"), "{}", read.detail);
        assert!(read.marks.contains(&"ok".to_string()));
        assert_eq!(read.data["value"], "never");
        let read_user = pane(&st, "cap-0-user");
        assert_eq!(read_user.data["value"], "never");
        assert!(read_user.data.get("scope").is_none());
        assert_eq!(pane(&st, "cap-0-workspace").data["scope"], "workspace");

        // `never` for the user, `ask` for the workspace: two independent rings.
        let net = pane(&st, "cap-1");
        assert!(net.marks.contains(&"error".to_string()));
        assert!(net.detail.contains("not accepted at install"));
        assert_eq!(net.data["value"], "always");
        assert_eq!(pane(&st, "cap-1-workspace").data["value"], Value::Null);
        assert!(
            pane(&st, "cap-1-workspace").data.get("value").is_none(),
            "`ask` wraps to no override, which is the absence of the key"
        );
    }

    #[test]
    fn the_right_cycle_is_a_ring_over_every_value_plus_the_cleared_state() {
        let mut seen = vec![];
        let mut at = None;
        for _ in 0..RIGHT_CYCLE.len() {
            at = next_right(at);
            seen.push(at);
        }
        assert_eq!(
            seen,
            vec![
                Some("never"),
                Some("always"),
                Some("workspace"),
                Some("ask"),
                None
            ]
        );
        // An unknown value (a newer host) restarts the ring rather than sticking.
        assert_eq!(next_right(Some("bogus")), Some("never"));
    }

    #[test]
    fn without_the_capability_the_pane_says_so_and_stops() {
        let rows = pane_rows(&State::default());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "no-capability");
    }

    #[test]
    fn a_module_that_is_not_installed_has_no_permissions_table() {
        let mut st = focused_state();
        st.rights = None;
        assert!(pane(&st, "rights").detail.contains("install this module"));
        assert!(!pane_rows(&st).iter().any(|r| r.id.starts_with("cap-")));
    }

    #[test]
    fn pane_row_ids_are_unique() {
        let mut st = focused_state();
        st.notice = Some("x".into());
        let rows = pane_rows(&st);
        let mut ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), rows.len());

        let mut st = ready_state();
        st.installed = vec![Installed::default(), Installed::default()];
        st.results = vec![RepoSummary::default(), RepoSummary::default()];
        let rows = pane_rows(&st);
        let mut ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), rows.len());
    }

    #[test]
    fn the_back_row_is_first_under_the_notice_and_leaves_the_module() {
        let st = focused_state();
        let all = pane_rows(&st);
        assert_eq!(all[0].id, "back");
        assert_eq!(all[0].data["action"], "pane.index");
        assert_eq!(all[1].label, "acme/avada-files");
        assert!(all[1].detail.contains("A file browser"));
        assert!(all[1].detail.contains("7★"));
    }
}
