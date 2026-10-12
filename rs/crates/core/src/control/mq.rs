//! Built-in message queue (MQ) — the event-driven spine for Manager / Orchestrator / Agent.
//!
//! Avada's messaging today is the per-pane [`crate::control::inbox`] (durable, pull-only) plus
//! the scope-filtered [`crate::control::events::EventHub`] WS push. This module adds a small
//! **in-process topic broker** so tabs, orchestrators, and the manager can address each other by
//! topic and get an instant push — replacing interval polling — WITHOUT cross-talk between
//! unrelated agents (see `docs/manager-architecture-plan.md` §3, `manager-architecture-spec.md`
//! req 6).
//!
//! **Placement.** The broker lives on the control plane (`control::server::Shared`), in the
//! GUI-host process, NOT in `session::daemon` (which is a pty sink with no message surface). The
//! spec's §4 default — "in-process / local-IPC broker owned by the session daemon unless the
//! daemon design already dictates otherwise" — points here: `Shared` already owns the inbox,
//! scope, and read-model the MQ must cooperate with. See the plan's §0 for the full rationale.
//!
//! **Topics / addressing.** [`TopicAddr`] is one of `manager`, `orchestrator:<id>`, or
//! `agent:<pane-id>`. An `agent:` id is a canonical pane id (the caller runs it through
//! `readmodel::resolve_pane_id` first, so `pane-<uuid>` and bare `<uuid>` map to one topic).
//!
//! **No-cross-talk has two independent halves:**
//!   1. *Topic isolation* — the broker keys subscribers by the EXACT [`TopicAddr`]
//!      (`Eq`+`Hash`), so a publish to `agent:A` can only ever reach `agent:A` subscribers,
//!      never `agent:B`, another orchestrator, or the manager.
//!   2. *Authority* — [`may_subscribe`] decides whether a caller holding a given
//!      [`crate::control::scope::Scope`] is even allowed to subscribe to a topic, reusing the
//!      same `scope::pane_in_scope` check that backs the existing proven "ZERO sibling frames"
//!      guarantee. A scoped caller can subscribe only to `agent:` topics for panes inside its
//!      scope; `manager` is master-only.
//!
//! The broker is the EPHEMERAL push layer: the durable `inbox` stays the source of truth, exactly
//! as the WS push is "only a nudge, the read is the source of truth". Dropped broadcast frames
//! (slow subscriber) are therefore recoverable from the inbox, not lost work.
//!
//! The transport is `tokio::sync::broadcast` (already in the dependency graph — no new crate).
//! The `TopicAddr` parsing + `may_subscribe` authority are pure and unit-testable without a
//! runtime; the pub/sub is exercised with `#[tokio::test]`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::control::scope::{pane_in_scope, PaneCoords, Scope};

/// Per-topic broadcast capacity. A slow subscriber that lags more than this many messages behind
/// sees a `Lagged` error on its next `recv` and resyncs — acceptable because the durable inbox is
/// the source of truth for `agent:` traffic. Pinned by a test (this crate is mutation-tested).
pub const MQ_CAPACITY: usize = 1024;

/// A message queue address. The three tiers of the control hierarchy, each its own isolated
/// topic namespace.
///
/// Wire form (round-trips through [`TopicAddr::parse`] / [`TopicAddr::as_string`]):
///   * [`TopicAddr::Manager`] ⇄ `"manager"`
///   * [`TopicAddr::Orchestrator`] ⇄ `"orchestrator:<id>"`
///   * [`TopicAddr::Agent`] ⇄ `"agent:<pane-id>"`
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TopicAddr {
    /// The Avada singleton manager. Exactly one; master-authority only.
    Manager,
    /// A per-project / per-workstream orchestrator, addressed by its id.
    Orchestrator(String),
    /// A worker agent = a pane, addressed by its CANONICAL pane id.
    Agent(String),
}

impl TopicAddr {
    /// Parse the wire form. Returns `None` for an unknown prefix or an empty id (an empty
    /// `orchestrator:`/`agent:` id would collapse distinct agents onto one topic — reject it).
    #[tracing::instrument(level = "debug", ret)]
    pub fn parse(s: &str) -> Option<Self> {
        if s == "manager" {
            return Some(TopicAddr::Manager);
        }
        if let Some(id) = s.strip_prefix("orchestrator:") {
            if id.is_empty() {
                return None;
            }
            return Some(TopicAddr::Orchestrator(id.to_string()));
        }
        if let Some(id) = s.strip_prefix("agent:") {
            if id.is_empty() {
                return None;
            }
            return Some(TopicAddr::Agent(id.to_string()));
        }
        None
    }

    /// The wire form (inverse of [`TopicAddr::parse`]).
    #[tracing::instrument(level = "debug", ret)]
    pub fn as_string(&self) -> String {
        match self {
            TopicAddr::Manager => "manager".to_string(),
            TopicAddr::Orchestrator(id) => format!("orchestrator:{id}"),
            TopicAddr::Agent(id) => format!("agent:{id}"),
        }
    }
}

/// A message on the MQ. CamelCase on the wire for parity with [`crate::control::inbox::PaneMessage`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MqMessage {
    /// The destination topic, in wire form (`agent:<pane-id>` / `orchestrator:<id>` / `manager`).
    pub topic: String,
    /// The sender. Free-form in phase 1 for inbox compatibility; phase 3 Q/A reply-routing will
    /// tighten this to a parseable [`TopicAddr`] (see the plan's data-model section).
    pub from: String,
    /// The message body.
    pub body: String,
    /// Broker-assigned, strictly increasing across the whole broker (observability + ordering).
    pub seq: u64,
    /// Wall-clock milliseconds at publish.
    pub ts: i64,
}

/// In-process topic broker. One `tokio::sync::broadcast` channel per live topic, created lazily on
/// first subscribe. Cheap to construct; held in `Shared` behind an `Arc`.
#[derive(Debug)]
pub struct Broker {
    topics: Mutex<HashMap<TopicAddr, broadcast::Sender<MqMessage>>>,
    seq: AtomicU64,
}

impl Default for Broker {
    fn default() -> Self {
        Self::new()
    }
}

impl Broker {
    /// A broker with no live topics.
    #[tracing::instrument(level = "debug")]
    pub fn new() -> Self {
        Broker {
            topics: Mutex::new(HashMap::new()),
            seq: AtomicU64::new(0),
        }
    }

    /// Subscribe to a topic, creating its channel if this is the first subscriber. The returned
    /// receiver sees only messages published to THIS exact topic — never another topic's traffic.
    #[tracing::instrument(level = "debug")]
    pub fn subscribe(&self, topic: TopicAddr) -> broadcast::Receiver<MqMessage> {
        let mut topics = self.topics.lock().unwrap();
        topics
            .entry(topic)
            .or_insert_with(|| broadcast::channel(MQ_CAPACITY).0)
            .subscribe()
    }

    /// Publish to a topic. Mints the next broker seq, stamps the message, and delivers it to that
    /// topic's current subscribers only. If no one is subscribed the send is a no-op (the durable
    /// inbox remains the record), but the seq is still consumed and the stamped message returned.
    #[tracing::instrument(level = "debug", ret)]
    pub fn publish(&self, topic: TopicAddr, from: &str, body: &str, ts: i64) -> MqMessage {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let msg = MqMessage {
            topic: topic.as_string(),
            from: from.to_string(),
            body: body.to_string(),
            seq,
            ts,
        };
        if let Some(tx) = self.topics.lock().unwrap().get(&topic) {
            // `send` errors only when there are zero receivers (all dropped since the last
            // subscribe); that is the no-op case above, not a failure.
            let _ = tx.send(msg.clone());
        }
        msg
    }

    /// How many live subscribers a topic currently has (0 if the topic was never subscribed or
    /// every receiver has been dropped). Diagnostics / tests.
    #[tracing::instrument(level = "debug", ret)]
    pub fn subscriber_count(&self, topic: &TopicAddr) -> usize {
        self.topics
            .lock()
            .unwrap()
            .get(topic)
            .map(broadcast::Sender::receiver_count)
            .unwrap_or(0)
    }
}

/// Whether a caller holding `scope` (`None` = unscoped/master) may subscribe to `topic`. This is
/// the AUTHORITY half of the no-cross-talk guarantee (topic isolation is the other half, enforced
/// by the broker's exact-topic keying).
///
///   * [`TopicAddr::Manager`] — master only. A scoped caller is never the manager.
///   * [`TopicAddr::Agent`] — allowed iff the pane is inside the caller's scope. `coords` is the
///     pane's read-model coordinates (`None` ⇒ the pane can't be resolved ⇒ deny for a scoped
///     caller; master is allowed since master reaches every pane).
///   * [`TopicAddr::Orchestrator`] — master only for now. Orchestrator identity is not yet a
///     first-class scope dimension (phase 2 introduces `orchestrator:<id> = Project.id`); until
///     then a scoped caller cannot subscribe to an orchestrator topic.
#[tracing::instrument(level = "debug", ret)]
pub fn may_subscribe(topic: &TopicAddr, scope: Option<&Scope>, coords: Option<&PaneCoords>) -> bool {
    match topic {
        TopicAddr::Manager => scope.is_none(),
        TopicAddr::Orchestrator(_) => scope.is_none(),
        TopicAddr::Agent(_) => match coords {
            Some(c) => pane_in_scope(scope, c),
            // Master (None scope) reaches every pane even without resolved coords; a scoped
            // caller cannot be granted a pane it can't even resolve.
            None => scope.is_none(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coords(pane: &str, tab: &str, window: i64) -> PaneCoords {
        PaneCoords {
            pane_id: pane.to_string(),
            tab_id: tab.to_string(),
            window_id: window,
        }
    }

    fn scope_panes(ids: &[&str]) -> Scope {
        Scope {
            window_ids: None,
            tab_ids: None,
            pane_ids: Some(ids.iter().map(|s| s.to_string()).collect()),
            queue_ids: None,
        }
    }

    #[test]
    fn topic_roundtrips_through_wire_form() {
        for s in ["manager", "orchestrator:proj-1", "agent:pane-abc"] {
            let t = TopicAddr::parse(s).expect("parse");
            assert_eq!(t.as_string(), s);
        }
    }

    #[test]
    fn topic_parse_rejects_empty_id_and_unknown_prefix() {
        assert_eq!(TopicAddr::parse("orchestrator:"), None);
        assert_eq!(TopicAddr::parse("agent:"), None);
        assert_eq!(TopicAddr::parse("worker:x"), None);
        assert_eq!(TopicAddr::parse(""), None);
        assert_eq!(TopicAddr::parse("Manager"), None); // case-sensitive
    }

    #[test]
    fn capacity_is_pinned() {
        assert_eq!(MQ_CAPACITY, 1024);
    }

    #[tokio::test]
    async fn publish_reaches_only_the_exact_topic_subscriber() {
        let b = Broker::new();
        let mut a = b.subscribe(TopicAddr::Agent("A".into()));
        let mut other = b.subscribe(TopicAddr::Agent("B".into()));
        b.publish(TopicAddr::Agent("A".into()), "mgr", "hi-A", 10);
        // The addressed subscriber gets exactly the message...
        let got = a.try_recv().expect("A should receive");
        assert_eq!(got.body, "hi-A");
        assert_eq!(got.topic, "agent:A");
        assert_eq!(got.seq, 1);
        // ...and the sibling agent receives NOTHING (no cross-talk).
        assert!(matches!(
            other.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn orchestrator_and_manager_topics_are_isolated_from_agents() {
        let b = Broker::new();
        let mut orch = b.subscribe(TopicAddr::Orchestrator("proj-1".into()));
        let mut mgr = b.subscribe(TopicAddr::Manager);
        let mut agent = b.subscribe(TopicAddr::Agent("A".into()));
        b.publish(TopicAddr::Agent("A".into()), "mgr", "for-agent", 1);
        // Neither the orchestrator nor the manager topic sees an agent-addressed message.
        assert!(matches!(
            orch.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            mgr.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        assert_eq!(agent.try_recv().unwrap().body, "for-agent");
    }

    #[tokio::test]
    async fn two_subscribers_to_same_topic_both_receive() {
        let b = Broker::new();
        let mut one = b.subscribe(TopicAddr::Manager);
        let mut two = b.subscribe(TopicAddr::Manager);
        b.publish(TopicAddr::Manager, "root", "all-hands", 5);
        assert_eq!(one.try_recv().unwrap().body, "all-hands");
        assert_eq!(two.try_recv().unwrap().body, "all-hands");
    }

    #[tokio::test]
    async fn seq_is_strictly_increasing_across_topics() {
        let b = Broker::new();
        let m1 = b.publish(TopicAddr::Agent("A".into()), "x", "a", 0);
        let m2 = b.publish(TopicAddr::Orchestrator("p".into()), "x", "b", 0);
        let m3 = b.publish(TopicAddr::Manager, "x", "c", 0);
        assert_eq!((m1.seq, m2.seq, m3.seq), (1, 2, 3));
    }

    #[test]
    fn publish_to_nobody_is_a_noop_but_still_stamps() {
        let b = Broker::new();
        let msg = b.publish(TopicAddr::Agent("ghost".into()), "mgr", "body", 42);
        assert_eq!(msg.seq, 1);
        assert_eq!(msg.ts, 42);
        assert_eq!(b.subscriber_count(&TopicAddr::Agent("ghost".into())), 0);
    }

    // --- authority (the no-cross-talk second half) ---

    #[test]
    fn master_may_subscribe_to_anything() {
        assert!(may_subscribe(&TopicAddr::Manager, None, None));
        assert!(may_subscribe(&TopicAddr::Orchestrator("p".into()), None, None));
        assert!(may_subscribe(
            &TopicAddr::Agent("A".into()),
            None,
            Some(&coords("A", "t1", 1))
        ));
    }

    #[test]
    fn scoped_caller_cannot_subscribe_to_manager_or_orchestrator() {
        let s = scope_panes(&["A"]);
        assert!(!may_subscribe(&TopicAddr::Manager, Some(&s), None));
        assert!(!may_subscribe(
            &TopicAddr::Orchestrator("p".into()),
            Some(&s),
            None
        ));
    }

    #[test]
    fn scoped_caller_subscribes_to_in_scope_agent_only() {
        let s = scope_panes(&["A"]);
        assert!(may_subscribe(
            &TopicAddr::Agent("A".into()),
            Some(&s),
            Some(&coords("A", "t1", 1))
        ));
        // A sibling pane out of scope is denied.
        assert!(!may_subscribe(
            &TopicAddr::Agent("B".into()),
            Some(&s),
            Some(&coords("B", "t1", 1))
        ));
        // An unresolvable pane (no coords) is denied for a scoped caller.
        assert!(!may_subscribe(&TopicAddr::Agent("A".into()), Some(&s), None));
    }
}
