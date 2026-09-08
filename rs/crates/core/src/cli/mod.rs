//! CLI: the launch grammar (`parse`, `routing`) and the schema-generated control verbs
//! (`schema_cli` → `invoke`, cached by `cache`, completed by `complete`).

pub mod cache;
pub mod complete;
pub mod invoke;
pub mod parse;
pub mod routing;
pub mod schema_cli;

/// The SDK types the generated CLI is built from, re-exported so a consumer that depends
/// only on `avada-core` (the app binary) can name them.
pub use avada_module_sdk::descriptor::{
    Param, ParamLocation, RouteDescriptor, SchemaDocument, Verb,
};
pub use avada_module_sdk::manifest::ModuleId;
/// The app crate has no `clap` dependency of its own; it reaches the parser through here.
pub use clap;
