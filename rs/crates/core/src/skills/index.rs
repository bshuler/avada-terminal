//! The generated `avada-modules` index skill: which modules are installed, whether
//! each is enabled in this workspace, and the CLI verbs the host's `GET /schema`
//! exposes. Rendered from a caller-supplied [`SchemaDocument`], never fetched here.

use std::fmt::Write as _;

use avada_module_sdk::descriptor::{SchemaDocument, Verb};

use super::materialize::{render_skill, ModuleInput, HOST_ID, INDEX_NAME};

/// Description the model picks the index by.
pub const DESCRIPTION: &str = "Which Avada Terminal modules are installed and enabled in this workspace, and every `avada` CLI verb the host and its modules expose. Read this before driving Avada from a shell.";

/// The full `SKILL.md` text.
pub fn render(schema: &SchemaDocument, modules: &[ModuleInput], workspace: Option<&str>) -> String {
    let mut b = String::new();
    let _ = writeln!(b, "# Avada modules\n");
    let _ = writeln!(
        b,
        "Host: {} {} (contract v{}). Workspace: {}.\n",
        schema.product,
        schema.host_version,
        schema.contract_version,
        workspace.unwrap_or("(none)")
    );
    if schema.modules.is_empty() {
        let _ = writeln!(b, "No modules are installed.\n");
    } else {
        let _ = writeln!(b, "| Module | Id | Version | Enabled here | Running |");
        let _ = writeln!(b, "|---|---|---|---|---|");
        for m in &schema.modules {
            let enabled = modules.iter().find(|i| i.id == m.id).map(|i| i.enabled);
            let _ = writeln!(
                b,
                "| {} | `{}` | {} | {} | {} |",
                cell(&m.name),
                m.id.as_str(),
                cell(&m.version),
                match enabled {
                    Some(true) => "yes",
                    Some(false) => "no",
                    None => "unknown",
                },
                if m.running { "yes" } else { "no" }
            );
        }
        b.push('\n');
    }
    let _ = writeln!(b, "## CLI verbs\n");
    if schema.routes.is_empty() {
        let _ = writeln!(b, "None published.");
    } else {
        let _ = writeln!(
            b,
            "Each route is a verb chain under `avada`; `avada <verbs> --help` shows its parameters.\n"
        );
        let mut routes: Vec<_> = schema.routes.iter().collect();
        routes.sort_by(|a, b| a.method.cmp(&b.method));
        for r in routes {
            let verbs = r.method.split('.').collect::<Vec<_>>().join(" ");
            let owner = r
                .module
                .as_ref()
                .map(|m| format!(" [{}]", m.as_str()))
                .unwrap_or_default();
            let _ = writeln!(
                b,
                "- `avada {verbs}` — {} ({} `{}`){owner}",
                cell(&r.summary),
                verb_name(r.verb),
                r.path
            );
        }
    }
    render_skill(HOST_ID, INDEX_NAME, DESCRIPTION, &b)
}

fn verb_name(v: Verb) -> &'static str {
    match v {
        Verb::Get => "GET",
        Verb::Post => "POST",
        Verb::Put => "PUT",
        Verb::Patch => "PATCH",
        Verb::Delete => "DELETE",
    }
}

fn cell(s: &str) -> String {
    s.replace('|', "\\|").replace(['\r', '\n'], " ")
}
