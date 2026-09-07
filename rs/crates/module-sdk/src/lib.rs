//! # Avada Terminal module SDK
//!
//! Everything a module and the host agree on, and nothing either of them does alone.
//!
//! A **module** is a separate process (or, for a short first-party shell tier, a linked
//! crate) that the host spawns, hands one end of a socket, and talks JSON-RPC 2.0 with
//! over that socket in both directions. This crate is the whole of that agreement:
//!
//! | module | what it fixes |
//! |---|---|
//! | [`manifest`] | the `avada.toml` a module repo carries: identity, version, dependencies, what it provides and requires, its UI contributions, the capabilities it asks for, permission profiles, skill paths |
//! | [`contract`] | the wire: [`CONTRACT_VERSION`](contract::CONTRACT_VERSION), the handshake, JSON-RPC envelopes, every method name and its parameter shape, newline-delimited framing |
//! | [`caps`] | the closed, namespaced capability vocabulary and the four-valued right (`never` / `always` / `workspace` / `ask`) with its user + workspace resolution |
//! | [`rights`] | the install record — the only place a right comes from — its HMAC, permission profiles, and the held-update diff |
//! | [`install`] | the lockfile, side-by-side versions, workspace pins and the enable/disable state |
//! | [`descriptor`] | the route descriptor table the control plane is built from and `GET /schema` serves; the CLI is generated from it |
//! | [`rail`] | the left-panel rail entry a module registers at handshake |
//! | [`skills`] | `SKILL.md` frontmatter, the ownership fence, and the adapter table for every supported AI tool |
//! | [`license`] | license claims and the introspection response, the one licensing mechanism for core and modules |
//! | [`client`] | what a module process links: read the socket from the environment, frame messages, run the handshake |
//!
//! Two products share it. The free edition compiles modules from source against this
//! crate; the commercial edition additionally loads precompiled, notarized modules. The
//! types are the same in both, which is why nothing here may reach for a dependency a
//! module author could not also build.
//!
//! # Versioning
//!
//! The contract is a single integer, [`contract::CONTRACT_VERSION`]. A module declares
//! the range it speaks in its manifest; the host picks the highest version both sides
//! support or refuses the handshake. Shape matching is layered on top: a requirement is
//! also met when any installed module implements the required extension-point shape at
//! the required version, whatever its name.

#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod caps;
pub mod client;
pub mod contract;
pub mod descriptor;
pub mod install;
pub mod license;
pub mod manifest;
pub mod rail;
pub mod rights;
pub mod skills;

pub use caps::{Capability, Decision, RightValue};
pub use contract::CONTRACT_VERSION;
pub use manifest::{Manifest, ModuleId};

/// Product name, spelled once. The rename wave owns every other spelling.
pub const PRODUCT_NAME: &str = "Avada Terminal";
/// The GitHub topic a repository carries to be discovered as a module.
pub const GITHUB_TOPIC: &str = "avada-module";
/// The manifest file name at a module repository's root.
pub const MANIFEST_FILE: &str = "avada.toml";
/// The lockfile name inside the modules directory.
pub const LOCKFILE_NAME: &str = "modules.lock";
