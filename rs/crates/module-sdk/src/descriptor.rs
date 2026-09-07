//! Control-plane route descriptors: what `GET /schema` returns and what the CLI
//! is generated from.
//!
//! Core's own routes are described with the same type, so the CLI treats a module
//! route and a built-in route identically.

use crate::caps::Capability;
use crate::manifest::ModuleId;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// HTTP verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Verb {
    /// Read.
    Get,
    /// Create or act.
    Post,
    /// Replace.
    Put,
    /// Update.
    Patch,
    /// Remove.
    Delete,
}

/// Who may call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// Any holder of a control token.
    #[default]
    Token,
    /// Master token only.
    Master,
    /// No auth (only `/health`).
    Public,
}

/// One parameter of a route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Param {
    /// Name as it appears in the path (`{id}`), query, or JSON body.
    pub name: String,
    /// Where it goes.
    pub location: ParamLocation,
    /// `string`, `integer`, `boolean`, `object`, `array`.
    pub kind: String,
    /// Required.
    #[serde(default)]
    pub required: bool,
    /// One line.
    #[serde(default)]
    pub summary: String,
}

/// Where a parameter lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ParamLocation {
    /// `/panes/{id}`.
    Path,
    /// `?since=`.
    Query,
    /// JSON body member.
    Body,
}

/// One HTTP route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteDescriptor {
    /// Dotted command name the CLI exposes: `panes.output` → `avada panes output`.
    pub method: String,
    /// Axum-style path: `/panes/{id}/output`. Module routes are mounted under
    /// `/m/<owner>/<repo>/…` by the host regardless of what the module wrote.
    pub path: String,
    /// Verb.
    pub verb: Verb,
    /// Capability the caller's token must carry (for module routes, also the
    /// capability the module must hold to serve it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<Capability>,
    /// One line for `--help`.
    #[serde(default)]
    pub summary: String,
    /// Parameters.
    #[serde(default)]
    pub params: Vec<Param>,
    /// Who may call.
    #[serde(default)]
    pub scope: Scope,
    /// The module that owns it, or `None` for core.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<ModuleId>,
    /// JSON schema of the response body, if described.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<Value>,
}

/// One JSON-RPC method (module-side or host-side) for documentation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcDescriptor {
    /// `host.rail.register`.
    pub method: String,
    /// One line.
    #[serde(default)]
    pub summary: String,
    /// Capability required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<Capability>,
    /// Params schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    /// Result schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
}

/// An installed module as listed in the schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleSummary {
    /// `owner/repo`.
    pub id: ModuleId,
    /// Display name.
    pub name: String,
    /// Installed version.
    pub version: String,
    /// Running right now.
    pub running: bool,
}

/// The whole `GET /schema` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaDocument {
    /// [`crate::contract::CONTRACT_VERSION`].
    pub contract_version: u32,
    /// Product name.
    pub product: String,
    /// Host version.
    pub host_version: String,
    /// All routes, core first, then modules, each sorted by method.
    pub routes: Vec<RouteDescriptor>,
    /// RPC methods, for documentation and SDK generation.
    #[serde(default)]
    pub rpcs: Vec<RpcDescriptor>,
    /// Installed modules.
    #[serde(default)]
    pub modules: Vec<ModuleSummary>,
}

/// Errors in a route table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DescriptorError {
    /// A method name is not dotted-lowercase.
    BadMethod(String),
    /// Two routes share a verb and path, or a method name.
    Duplicate(String),
    /// A path param is declared but not in the path, or vice versa.
    PathParamMismatch(String, String),
    /// A path does not start with `/`.
    BadPath(String),
}

impl std::fmt::Display for DescriptorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DescriptorError::BadMethod(m) => {
                write!(f, "route method `{m}` is not lowercase dotted segments")
            }
            DescriptorError::Duplicate(m) => write!(f, "route `{m}` is declared twice"),
            DescriptorError::PathParamMismatch(m, p) => {
                write!(
                    f,
                    "route `{m}`: path parameter `{p}` is not in both path and params"
                )
            }
            DescriptorError::BadPath(p) => write!(f, "route path `{p}` must start with `/`"),
        }
    }
}
impl std::error::Error for DescriptorError {}

impl RouteDescriptor {
    /// The `{name}` segments of the path.
    pub fn path_params(&self) -> Vec<&str> {
        self.path
            .split('/')
            .filter_map(|s| s.strip_prefix('{').and_then(|s| s.strip_suffix('}')))
            .collect()
    }

    /// Rules that make a route usable by the CLI generator.
    pub fn validate(&self) -> Result<(), DescriptorError> {
        if !is_method_name(&self.method) {
            return Err(DescriptorError::BadMethod(self.method.clone()));
        }
        if !self.path.starts_with('/') {
            return Err(DescriptorError::BadPath(self.path.clone()));
        }
        let in_path = self.path_params();
        let declared: Vec<&str> = self
            .params
            .iter()
            .filter(|p| p.location == ParamLocation::Path)
            .map(|p| p.name.as_str())
            .collect();
        for p in &in_path {
            if !declared.contains(p) {
                return Err(DescriptorError::PathParamMismatch(
                    self.method.clone(),
                    p.to_string(),
                ));
            }
        }
        for p in &declared {
            if !in_path.contains(p) {
                return Err(DescriptorError::PathParamMismatch(
                    self.method.clone(),
                    p.to_string(),
                ));
            }
        }
        Ok(())
    }

    /// The path the host actually mounts: module routes are prefixed.
    pub fn mounted_path(&self) -> String {
        match &self.module {
            Some(id) => format!("/m/{}/{}{}", id.owner(), id.repo(), self.path),
            None => self.path.clone(),
        }
    }
}

/// Validate a whole table: each route, plus uniqueness of method and verb+path.
/// A CLI method name: one or more `[a-z][a-z0-9_]*` segments joined by dots. One segment is
/// fine (`health` → `avada health`); a *shape* needs two or more, see [`crate::manifest::is_shape`].
pub fn is_method_name(s: &str) -> bool {
    !s.is_empty()
        && s.split('.').all(|seg| {
            seg.chars().next().is_some_and(|c| c.is_ascii_lowercase())
                && seg
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        })
}

/// Check a whole route table at once: every descriptor is individually valid and no two
/// share a method name. The host runs this over its built-in table in a test and over a
/// module's declared routes at install time, so a collision is refused before it can
/// shadow a built-in.
pub fn validate_table(routes: &[RouteDescriptor]) -> Result<(), DescriptorError> {
    let mut methods = std::collections::BTreeSet::new();
    let mut paths = std::collections::BTreeSet::new();
    for r in routes {
        r.validate()?;
        if !methods.insert(r.method.as_str()) {
            return Err(DescriptorError::Duplicate(r.method.clone()));
        }
        if !paths.insert((r.verb, r.mounted_path())) {
            return Err(DescriptorError::Duplicate(r.method.clone()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(method: &str, path: &str, verb: Verb) -> RouteDescriptor {
        RouteDescriptor {
            method: method.into(),
            path: path.into(),
            verb,
            capability: None,
            summary: String::new(),
            params: Vec::new(),
            scope: Scope::Token,
            module: None,
            response: None,
        }
    }
    fn path_param(name: &str) -> Param {
        Param {
            name: name.into(),
            location: ParamLocation::Path,
            kind: "string".into(),
            required: true,
            summary: String::new(),
        }
    }

    #[test]
    fn path_params_must_be_declared_both_ways() {
        let mut r = route("panes.output", "/panes/{id}/output", Verb::Get);
        assert_eq!(r.path_params(), vec!["id"]);
        assert!(matches!(
            r.validate(),
            Err(DescriptorError::PathParamMismatch(..))
        ));
        r.params.push(path_param("id"));
        assert_eq!(r.validate(), Ok(()));
        r.params.push(path_param("extra"));
        assert!(matches!(
            r.validate(),
            Err(DescriptorError::PathParamMismatch(..))
        ));
    }

    #[test]
    fn method_and_path_shape() {
        assert!(
            is_method_name("health") && is_method_name("panes.output") && is_method_name("a1.b_2")
        );
        assert!(
            !is_method_name("")
                && !is_method_name(".a")
                && !is_method_name("a..b")
                && !is_method_name("1a")
                && !is_method_name("a-b")
        );
        assert!(matches!(
            route("Panes", "/x", Verb::Get).validate(),
            Err(DescriptorError::BadMethod(_))
        ));
        assert!(matches!(
            route("a.b", "x", Verb::Get).validate(),
            Err(DescriptorError::BadPath(_))
        ));
    }

    #[test]
    fn module_routes_are_mounted_under_a_prefix() {
        let mut r = route("files.tree", "/tree", Verb::Get);
        assert_eq!(r.mounted_path(), "/tree");
        r.module = Some(ModuleId::new("acme/avada-files").unwrap());
        assert_eq!(r.mounted_path(), "/m/acme/avada-files/tree");
    }

    #[test]
    fn table_rejects_duplicates_but_allows_same_path_different_verb() {
        let a = route("panes.list", "/panes", Verb::Get);
        let b = route("panes.create", "/panes", Verb::Post);
        assert_eq!(validate_table(&[a.clone(), b.clone()]), Ok(()));
        assert!(matches!(
            validate_table(&[a.clone(), a.clone()]),
            Err(DescriptorError::Duplicate(_))
        ));
        let mut c = route("panes.other", "/panes", Verb::Get);
        assert!(matches!(
            validate_table(&[a.clone(), c.clone()]),
            Err(DescriptorError::Duplicate(_))
        ));
        c.module = Some(ModuleId::new("x/y").unwrap());
        assert_eq!(
            validate_table(&[a, b, c]),
            Ok(()),
            "module prefix disambiguates"
        );
    }

    #[test]
    fn schema_document_round_trips() {
        let doc = SchemaDocument {
            contract_version: 1,
            product: crate::PRODUCT_NAME.into(),
            host_version: "0.0.37".into(),
            routes: vec![route("health", "/health", Verb::Get)],
            rpcs: vec![RpcDescriptor {
                method: "host.toast".into(),
                summary: "show a toast".into(),
                capability: Some(Capability::UiToast),
                params: None,
                result: None,
            }],
            modules: vec![],
        };
        let json = serde_json::to_value(&doc).unwrap();
        assert_eq!(json["routes"][0]["verb"], "GET");
        assert_eq!(json["routes"][0]["scope"], "token");
        assert_eq!(json["rpcs"][0]["capability"], "ui.toast");
        assert_eq!(serde_json::from_value::<SchemaDocument>(json).unwrap(), doc);
    }
}
