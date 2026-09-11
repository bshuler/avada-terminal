//! The Hyperpane state machine, with no I/O in it.
//!
//! Every verb the module serves — a command, a row click, an event — is a pure function
//! from ([`State`], the params) to an [`Outcome`]: the JSON to answer with, an optional
//! toast, and at most one [`Action`] for `main` to carry out on the socket afterwards.
//! Keeping the socket out of here is what lets the interesting decisions (when is the
//! pane stale? what does `send` do to a closed pane?) be tested without a host at all;
//! `tests/e2e.rs` then proves the wiring once, against the real binary.

use std::path::PathBuf;

use avada_module_sdk::contract::{ErrorCode, RpcError};
use serde_json::{json, Value};

use crate::rows;

/// The command palette entries, in the order they are registered. Must match the
/// `[[contributions]] kind = "command"` list in `avada.toml`; `the_manifest_and_the_code_
/// agree_on_the_commands` asserts it.
pub const COMMANDS: &[(&str, &str)] = &[
    ("open", "Hyperpane: Open the tab"),
    ("restart", "Hyperpane: Restart the agent"),
    ("send", "Hyperpane: Send text to the agent"),
    ("reveal", "Hyperpane: Reveal the working directory"),
];

/// Everything the rail draws from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    /// The module's private data directory, from the hello. This is the agent's working
    /// directory: durable across restarts, owned by nothing else, and the one place a
    /// module is promised it may keep files.
    pub dir: PathBuf,
    /// The pane the host minted for us, once it has.
    pub pane: Option<String>,
    /// The rail's shared filter box (`rail.query`).
    pub filter: String,
}

/// The one side effect an outcome may ask for, carried out by `main` after the response
/// has gone out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Spawn the shell pane in [`State::dir`] and remember its id.
    Open,
    /// Forget the pane we have, then [`Action::Open`].
    Restart,
    /// Open [`State::dir`] in a file pane. Does not touch [`State::pane`].
    Reveal,
    /// Type into the pane we already have.
    Input(String),
}

/// What a verb decided.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    /// The JSON-RPC result.
    pub result: Value,
    /// A message for the human, if any.
    pub toast: Option<String>,
    /// The socket work `main` still owes.
    pub action: Option<Action>,
}

/// The module.
#[derive(Debug)]
pub struct App {
    /// The rail's view of the world.
    pub state: State,
}

impl App {
    /// A module that has said hello but has not been told its data directory yet.
    pub fn new() -> Self {
        App {
            state: State {
                dir: PathBuf::new(),
                pane: None,
                filter: String::new(),
            },
        }
    }

    /// The working directory, from `host.hello`'s `data_dir`.
    pub fn set_dir(&mut self, dir: PathBuf) {
        self.state.dir = dir;
    }

    /// The host minted a pane for our last [`Action::Open`].
    pub fn pane_opened(&mut self, pane_id: String) {
        self.state.pane = Some(pane_id);
    }

    /// A command from the palette.
    pub fn command(&mut self, id: &str, args: &Value) -> Result<Outcome, RpcError> {
        match id {
            "open" => Ok(self.open(false)),
            "restart" => Ok(self.open(true)),
            "reveal" => Ok(Outcome {
                result: json!({ "path": self.state.dir.display().to_string() }),
                action: Some(Action::Reveal),
                ..Outcome::default()
            }),
            "send" => self.send(args["text"].as_str().unwrap_or_default()),
            other => Err(RpcError::new(
                ErrorCode::MethodNotFound,
                format!("hyperpane has no command `{other}`"),
            )),
        }
    }

    /// A click on one of our rows.
    pub fn row_activate(&mut self, data: &Value, gesture: &str) -> Result<Outcome, RpcError> {
        match rows::action_for(data, gesture) {
            Some(Action::Open) => Ok(self.open(false)),
            Some(Action::Restart) => Ok(self.open(true)),
            Some(Action::Reveal) => Ok(Outcome {
                result: json!({ "path": self.state.dir.display().to_string() }),
                action: Some(Action::Reveal),
                ..Outcome::default()
            }),
            // `Input` is never a row gesture, and an unrecognised row is a host bug
            // rather than a human one — say which row, and change nothing.
            Some(Action::Input(_)) | None => Err(RpcError::new(
                ErrorCode::InvalidParams,
                format!("hyperpane does not own the row `{}`", data["action"]),
            )),
        }
    }

    /// A subscribed event. Unknown kinds are ignored on purpose: a newer host may send
    /// kinds this build has never heard of, and that must not be an error.
    pub fn event(&mut self, kind: &str, payload: &Value) {
        if kind == avada_module_sdk::contract::methods::events::RAIL_QUERY {
            self.state.filter = payload["query"].as_str().unwrap_or_default().to_string();
        }
    }

    /// The host is switching us off: drop the pane id. The pane itself belongs to the
    /// app and may well outlive us, but *our* claim on it does not survive a deactivate,
    /// and typing into a stale id is exactly the bug this prevents.
    pub fn deactivate(&mut self) {
        self.state.pane = None;
    }

    fn open(&mut self, restart: bool) -> Outcome {
        if restart {
            self.state.pane = None;
        }
        match &self.state.pane {
            // Already open, and nothing in the contract lets a module focus a pane it did
            // not just create — so say so rather than opening a second one.
            Some(id) => Outcome {
                result: json!({ "pane_id": id, "spawned": false }),
                toast: Some("Hyperpane is already open".into()),
                action: None,
            },
            None => Outcome {
                result: json!({ "spawned": true }),
                toast: None,
                action: Some(Action::Open),
            },
        }
    }

    fn send(&mut self, text: &str) -> Result<Outcome, RpcError> {
        if text.is_empty() {
            return Err(RpcError::new(
                ErrorCode::InvalidParams,
                "`send` needs a `text` argument",
            ));
        }
        let Some(pane) = self.state.pane.clone() else {
            // Not an internal error and not a denial: the human asked to type into a
            // window that isn't there, and the fix is to open it.
            return Err(RpcError::new(
                ErrorCode::InvalidRequest,
                "the Hyperpane pane is not open; run `Hyperpane: Open the tab` first",
            ));
        };
        Ok(Outcome {
            result: json!({ "pane_id": pane, "bytes": text.len() }),
            toast: None,
            action: Some(Action::Input(text.to_string())),
        })
    }
}

impl Default for App {
    fn default() -> Self {
        App::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use avada_module_sdk::manifest::{ContributionKind, Manifest};

    fn app() -> App {
        let mut a = App::new();
        a.set_dir(PathBuf::from("/data/hp"));
        a
    }

    #[test]
    fn open_spawns_once_and_then_reports_the_pane_it_already_has() {
        let mut a = app();
        let first = a.command("open", &json!({})).unwrap();
        assert_eq!(first.action, Some(Action::Open));
        assert_eq!(first.result["spawned"], true);

        a.pane_opened("p-7".into());
        let second = a.command("open", &json!({})).unwrap();
        assert_eq!(
            second.action, None,
            "a second pane is never what was wanted"
        );
        assert_eq!(second.result["pane_id"], "p-7");
        assert_eq!(second.result["spawned"], false);
        assert!(second.toast.is_some());
    }

    #[test]
    fn restart_forgets_the_pane_so_open_runs_again() {
        let mut a = app();
        a.pane_opened("p-7".into());
        let out = a.command("restart", &json!({})).unwrap();
        assert_eq!(out.action, Some(Action::Open));
        assert_eq!(a.state.pane, None);
    }

    #[test]
    fn send_needs_text_and_needs_a_pane() {
        let mut a = app();
        assert_eq!(
            a.command("send", &json!({ "text": "ls\r" }))
                .unwrap_err()
                .kind(),
            ErrorCode::InvalidRequest,
            "with no pane there is nothing to type into"
        );
        a.pane_opened("p-7".into());
        assert_eq!(
            a.command("send", &json!({})).unwrap_err().kind(),
            ErrorCode::InvalidParams
        );
        let out = a.command("send", &json!({ "text": "ls\r" })).unwrap();
        assert_eq!(out.action, Some(Action::Input("ls\r".into())));
        assert_eq!(out.result["pane_id"], "p-7");
    }

    #[test]
    fn deactivate_drops_the_pane_claim() {
        let mut a = app();
        a.pane_opened("p-7".into());
        a.deactivate();
        assert_eq!(
            a.command("send", &json!({ "text": "x" }))
                .unwrap_err()
                .kind(),
            ErrorCode::InvalidRequest
        );
    }

    #[test]
    fn an_unknown_command_is_method_not_found_and_the_rail_query_sets_the_filter() {
        let mut a = app();
        assert_eq!(
            a.command("eject", &json!({})).unwrap_err().kind(),
            ErrorCode::MethodNotFound
        );
        a.event("rail.query", &json!({ "query": "hyp" }));
        assert_eq!(a.state.filter, "hyp");
        a.event("something.new", &json!({}));
        assert_eq!(a.state.filter, "hyp", "an unknown kind changes nothing");
    }

    #[test]
    fn a_row_click_opens_and_alt_click_restarts() {
        let mut a = app();
        let data = json!({ "action": "open" });
        assert_eq!(
            a.row_activate(&data, "open").unwrap().action,
            Some(Action::Open)
        );
        a.pane_opened("p-7".into());
        assert_eq!(
            a.row_activate(&data, "alt").unwrap().action,
            Some(Action::Open)
        );
        assert_eq!(
            a.row_activate(&json!({ "action": "eject" }), "open")
                .unwrap_err()
                .kind(),
            ErrorCode::InvalidParams
        );
    }

    #[test]
    fn the_manifest_is_valid_and_agrees_with_the_code() {
        let m = Manifest::parse(crate::MANIFEST).expect("avada.toml parses");
        m.validate().expect("avada.toml is valid");
        assert_eq!(m.module.id.to_string(), "bshuler/avada-hyperpane");

        let declared: Vec<&str> = m
            .contributions
            .iter()
            .filter(|c| c.kind == ContributionKind::Command)
            .map(|c| c.id.as_str())
            .collect();
        let served: Vec<&str> = COMMANDS.iter().map(|(id, _)| *id).collect();
        assert_eq!(declared, served, "every command is declared, in order");

        for (id, label) in COMMANDS {
            let c = m
                .contributions
                .iter()
                .find(|c| c.kind == ContributionKind::Command && c.id == *id)
                .unwrap();
            assert_eq!(&c.label, label, "the palette label is the declared one");
        }
        assert_eq!(
            m.skills.paths,
            ["skills"],
            "the always-on rule is what makes this module worth installing"
        );
        assert!(m
            .contributions
            .iter()
            .any(|c| c.kind == ContributionKind::Rail && c.id == rows::ENTRY));
    }
}
