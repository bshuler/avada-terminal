//! The rail seam with the left panel (track H4): what a module registered, the rows it
//! projected, and the event stream the panel consumes.
//!
//! Events go out on `std::sync::mpsc` channels. [`Host::rail_events`](super::Host::
//! rail_events) creates a fresh channel per call and fans every event out to all of them,
//! so the panel, a test and a logger can each hold their own receiver and block on it
//! (`recv_timeout`) without a runtime. A receiver that has been dropped is pruned at the
//! next send.

pub use avada_module_sdk::rail::{Gesture, RailEntry, Row, RowActivate};
use avada_module_sdk::ModuleId;
use std::collections::BTreeMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Mutex;

/// One change to the rail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RailEvent {
    /// The module registered (or re-registered) its entries; this replaces any earlier set.
    Registered {
        /// Which module.
        module: ModuleId,
        /// Every entry, validated, `module` field filled in.
        entries: Vec<RailEntry>,
    },
    /// The module replaced the rows beneath one entry.
    Rows {
        /// Which module.
        module: ModuleId,
        /// The entry id.
        entry: String,
        /// The whole new list.
        rows: Vec<Row>,
    },
    /// The module is gone (crashed, disabled, shut down); the panel should show the
    /// placeholder in its place and drop its rows.
    Gone {
        /// Which module.
        module: ModuleId,
    },
}

/// Fan-out of one event type to any number of `mpsc` receivers.
pub(crate) struct FanOut<T: Clone + Send> {
    senders: Mutex<Vec<Sender<T>>>,
}

impl<T: Clone + Send> Default for FanOut<T> {
    fn default() -> Self {
        FanOut {
            senders: Mutex::new(Vec::new()),
        }
    }
}

impl<T: Clone + Send> FanOut<T> {
    /// A new receiver that sees every event sent from now on.
    pub(crate) fn subscribe(&self) -> Receiver<T> {
        let (tx, rx) = mpsc::channel();
        self.senders
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(tx);
        rx
    }

    /// Deliver to every live receiver, forgetting the dead ones.
    pub(crate) fn send(&self, event: T) {
        let mut senders = self.senders.lock().unwrap_or_else(|e| e.into_inner());
        senders.retain(|tx| tx.send(event.clone()).is_ok());
    }

    /// How many receivers are still attached (after the last prune).
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.senders.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

/// What each running module has put on the rail. Rebuilt from scratch on every
/// `host.rail.register`; cleared when the module goes away.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RailState {
    /// Entries in registration order.
    pub entries: Vec<RailEntry>,
    /// Rows per entry id.
    pub rows: BTreeMap<String, Vec<Row>>,
}

impl RailState {
    /// Replace the entry set; rows for entries that no longer exist are dropped.
    pub fn register(&mut self, entries: Vec<RailEntry>) {
        self.rows
            .retain(|id, _| entries.iter().any(|e| &e.id == id));
        self.entries = entries;
    }

    /// Replace the rows of `entry`. `Err` if the entry was never registered.
    pub fn set_rows(&mut self, entry: &str, rows: Vec<Row>) -> Result<(), String> {
        if !self.entries.iter().any(|e| e.id == entry) {
            return Err(format!("rail entry `{entry}` is not registered"));
        }
        self.rows.insert(entry.to_string(), rows);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use avada_module_sdk::manifest::UiTier;

    fn entry(id: &str) -> RailEntry {
        RailEntry {
            id: id.into(),
            label: id.to_uppercase(),
            icon: None,
            tier: UiTier::Data,
            module: None,
            order: 0,
            component: None,
        }
    }

    fn row(id: &str) -> Row {
        Row {
            id: id.into(),
            label: id.into(),
            detail: String::new(),
            depth: 0,
            expandable: false,
            expanded: false,
            icon: None,
            marks: vec![],
            data: serde_json::Value::Null,
        }
    }

    #[test]
    fn fan_out_reaches_every_live_receiver_and_prunes_dead_ones() {
        let fan: FanOut<u32> = FanOut::default();
        let a = fan.subscribe();
        let b = fan.subscribe();
        fan.send(1);
        assert_eq!(a.recv().unwrap(), 1);
        assert_eq!(b.recv().unwrap(), 1);
        drop(b);
        fan.send(2);
        assert_eq!(a.recv().unwrap(), 2);
        assert_eq!(fan.len(), 1);
    }

    #[test]
    fn rows_need_a_registered_entry_and_survive_reregistration_of_it() {
        let mut state = RailState::default();
        assert!(state.set_rows("files", vec![row("a")]).is_err());
        state.register(vec![entry("files"), entry("git")]);
        state.set_rows("files", vec![row("a")]).unwrap();
        state.set_rows("git", vec![row("b")]).unwrap();
        state.register(vec![entry("files")]);
        assert_eq!(state.rows.get("files").map(Vec::len), Some(1));
        assert!(!state.rows.contains_key("git"));
    }
}
