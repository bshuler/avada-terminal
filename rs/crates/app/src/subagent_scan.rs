//! Off-thread subagent discovery (B8 part 2, `docs/incidents/2026-10-14-subagent-scan-main-thread`).
//!
//! [`State::reconcile_subagents`](crate::state::State::reconcile_subagents) used to call the
//! filesystem scan (`claude_subagents::subagents_for_pane`) straight from the UI tick, once a
//! second per pty pane, reading every hook record ever written. That is what stalled the GUI.
//! Now the UI thread only decides *which* panes to scan and hands their marker ids to a short
//! lived worker thread; the worker does the reads (and the archiving of finished records) and
//! parks a snapshot per pane uid in [`SubagentScanner::results`]. The tick's reconcile looks
//! up that snapshot instead of touching the disk.
//!
//! Cadence: a scan starts every [`SCAN_EVERY`] when none is in flight, or at once after
//! [`SubagentScanner::nudge`] (part 3: the Claude hook posts a `subagentEvent` to the control
//! host when a subagent starts or stops, so the pane appears within one tick rather than
//! after the next poll). The snapshot persists between scans, so the reconcile's own 1 s
//! throttle never misses a record — a finished record stays visible until the next scan
//! replaces it, and `reconcile_subagents` is idempotent over repeats.
//!
//! Only one worker runs at a time; the in-flight flag is cleared by a drop guard so a
//! panicking scan cannot wedge the scanner. The worker never touches `State` (it is `!Send`):
//! the inputs are plain strings and the outputs are `Send` record lists.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::state::SubagentScan;

/// How often the fallback poll runs when no hook event nudges the scanner. Long enough that
/// an idle workspace is quiet; short enough that a hook whose event post failed (no control
/// token, scoped module token) still surfaces its subagents promptly.
pub const SCAN_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

/// One pane to scan: its session uid and the marker ids the Claude hook may have written
/// its session under (the control-plane alias first, then the uid — see
/// `App::pane_marker_ids`).
pub type Target = (String, Vec<String>);

/// The snapshot the worker produces: pane uid → what `reconcile_subagents` wants for it.
/// `None` means the pane has no live Claude marker, which the reconcile treats as "leave its
/// subagent panes alone".
type Results = HashMap<String, Option<SubagentScan>>;

pub struct SubagentScanner {
    results: Arc<Mutex<Results>>,
    in_flight: Arc<AtomicBool>,
    /// When the last scan started (not finished), for the poll cadence.
    last_started: Cell<Option<Instant>>,
    /// Set by [`Self::nudge`]; makes the next [`Self::due`] true regardless of cadence.
    nudged: Cell<bool>,
}

/// Clears the in-flight flag when the worker ends, however it ends.
struct Done(Arc<AtomicBool>);

impl Drop for Done {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl SubagentScanner {
    pub fn new() -> Self {
        Self {
            results: Arc::new(Mutex::new(HashMap::new())),
            in_flight: Arc::new(AtomicBool::new(false)),
            last_started: Cell::new(None),
            nudged: Cell::new(false),
        }
    }

    /// Ask for a scan as soon as possible (a hook reported a subagent start/stop). Cheap and
    /// idempotent; the scan itself starts on the next tick that finds no worker in flight.
    pub fn nudge(&self) {
        self.nudged.set(true);
    }

    /// Whether the tick should start a scan now: none in flight, and either a nudge is
    /// pending or [`SCAN_EVERY`] has passed since the last start.
    pub fn due(&self, now: Instant) -> bool {
        if self.in_flight.load(Ordering::Acquire) {
            return false;
        }
        self.nudged.get()
            || self
                .last_started
                .get()
                .is_none_or(|at| now.saturating_duration_since(at) >= SCAN_EVERY)
    }

    /// Start a worker over `targets` using the real filesystem scan.
    pub fn start(&self, targets: Vec<Target>, now: Instant) {
        self.start_with(targets, now, scan_pane);
    }

    /// [`Self::start`] with the per-pane scan injected (tests swap the filesystem out).
    /// Does nothing while a worker is in flight. An empty target list is folded in on the
    /// calling thread — there is nothing to read, and a stale snapshot for a pane that no
    /// longer exists must still be dropped.
    pub fn start_with<F>(&self, targets: Vec<Target>, now: Instant, scan: F)
    where
        F: Fn(&[String]) -> Option<SubagentScan> + Send + 'static,
    {
        if self
            .in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        self.nudged.set(false);
        self.last_started.set(Some(now));
        let done = Done(Arc::clone(&self.in_flight));
        if targets.is_empty() {
            self.results.lock().unwrap_or_else(|e| e.into_inner()).clear();
            drop(done);
            return;
        }
        let results = Arc::clone(&self.results);
        let spawned = std::thread::Builder::new()
            .name("subagent-scan".into())
            .spawn(move || {
                let _done = done;
                let fresh: Results = targets
                    .iter()
                    .map(|(uid, ids)| (uid.clone(), scan(ids)))
                    .collect();
                *results.lock().unwrap_or_else(|e| e.into_inner()) = fresh;
            });
        if let Err(e) = spawned {
            // `done` moved into the closure, which was dropped with the error: the flag is
            // already clear. Keep the old snapshot; the next tick retries.
            tracing::warn!(error = %e, "subagent scan thread failed to spawn");
        }
    }

    /// The latest snapshot for pane `uid` — what `reconcile_subagents`'s lookup returns.
    pub fn lookup(&self, uid: &str) -> Option<SubagentScan> {
        self.results
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(uid)
            .cloned()
            .flatten()
    }

    /// Block until no worker is in flight (tests only; the UI never waits on the worker).
    #[cfg(test)]
    pub fn wait_idle(&self) {
        let start = Instant::now();
        while self.in_flight.load(Ordering::Acquire) {
            assert!(start.elapsed() < std::time::Duration::from_secs(10), "scan never finished");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}

/// What `reconcile_subagents` needs for the pane whose Claude session may be marked under
/// any of `ids`: the subagents of its live conversation, read under the id the newest marker
/// was written under (a stale alias marker cannot shadow the one the running process
/// wrote). `None` without a marker. Runs on the worker thread.
#[tracing::instrument(level = "debug", skip_all)]
pub fn scan_pane(ids: &[String]) -> Option<SubagentScan> {
    use avada_core::{claude_panes, claude_subagents};
    let newest = claude_panes::read_newest_pane_session(ids.iter().map(String::as_str))?;
    let id = ids.iter().find(|id| {
        claude_panes::read_pane_session(id).is_some_and(|s| s.session_id == newest.session_id)
    })?;
    Some(SubagentScan {
        records: claude_subagents::subagents_for_pane(id),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use avada_core::claude_subagents::SubagentRecord;
    use std::time::Duration;

    fn rec(id: &str) -> SubagentRecord {
        SubagentRecord {
            agent_id: id.into(),
            agent_type: "general-purpose".into(),
            description: String::new(),
            spawn_depth: 1,
            transcript: std::path::PathBuf::new(),
            started_at: Some(std::time::SystemTime::now()),
            stopped_at: None,
            last_growth: None,
        }
    }

    fn one(id: &'static str) -> impl Fn(&[String]) -> Option<SubagentScan> + Send + 'static {
        move |ids| {
            (ids.first().map(String::as_str) == Some("marked")).then(|| SubagentScan {
                records: vec![rec(id)],
            })
        }
    }

    #[test]
    fn first_scan_is_due_at_once_then_every_scan_every() {
        let s = SubagentScanner::new();
        let t0 = Instant::now();
        assert!(s.due(t0), "nothing scanned yet");
        s.start_with(vec![], t0, one("a"));
        assert!(!s.due(t0 + Duration::from_secs(4)));
        assert!(s.due(t0 + SCAN_EVERY));
    }

    #[test]
    fn nudge_makes_the_next_tick_due_and_is_consumed_by_the_start() {
        let s = SubagentScanner::new();
        let t0 = Instant::now();
        s.start_with(vec![], t0, one("a"));
        assert!(!s.due(t0 + Duration::from_secs(1)));
        s.nudge();
        assert!(s.due(t0 + Duration::from_secs(1)));
        s.start_with(vec![], t0 + Duration::from_secs(1), one("a"));
        assert!(!s.due(t0 + Duration::from_secs(2)), "the start consumed the nudge");
    }

    #[test]
    fn results_persist_between_scans_and_follow_the_target_list() {
        let s = SubagentScanner::new();
        let t0 = Instant::now();
        let targets = vec![
            ("orch".to_string(), vec!["marked".to_string(), "orch".to_string()]),
            ("plain".to_string(), vec!["plain".to_string()]),
        ];
        s.start_with(targets.clone(), t0, one("a"));
        s.wait_idle();
        // Repeated lookups between scans see the same snapshot (the reconcile's 1 s throttle
        // may read it several times before the next scan replaces it).
        for _ in 0..3 {
            let got = s.lookup("orch").expect("marked pane has a scan");
            assert_eq!(got.records.len(), 1);
            assert_eq!(got.records[0].agent_id, "a");
            assert!(s.lookup("plain").is_none(), "no marker → no scan → pane left alone");
            assert!(s.lookup("gone").is_none());
        }
        // A later scan replaces the snapshot wholesale: a pane that left the workspace drops
        // out, a changed record set shows up.
        s.start_with(targets[..1].to_vec(), t0 + SCAN_EVERY, one("b"));
        s.wait_idle();
        assert_eq!(s.lookup("orch").unwrap().records[0].agent_id, "b");
        assert!(s.lookup("plain").is_none());
        // And an empty target list clears it on the calling thread.
        s.start_with(vec![], t0 + 2 * SCAN_EVERY, one("c"));
        assert!(s.lookup("orch").is_none());
    }

    #[test]
    fn only_one_worker_runs_at_a_time_and_a_panic_does_not_wedge_it() {
        let s = SubagentScanner::new();
        let t0 = Instant::now();
        let gate = Arc::new(Mutex::new(()));
        let held = gate.lock().unwrap();
        let g = Arc::clone(&gate);
        s.start_with(
            vec![("orch".into(), vec!["marked".into()])],
            t0,
            move |_ids| {
                drop(g.lock().unwrap());
                None
            },
        );
        assert!(!s.due(t0 + 2 * SCAN_EVERY), "in flight: never due");
        // A second start while the first is blocked is a no-op (the first still holds the
        // flag; this one's scan closure never runs).
        s.start_with(vec![], t0, |_| panic!("second worker must not run"));
        drop(held);
        s.wait_idle();
        assert!(s.due(t0 + SCAN_EVERY));
        // A panicking scan clears the flag on unwind.
        s.start_with(
            vec![("orch".into(), vec!["marked".into()])],
            t0 + SCAN_EVERY,
            |_| panic!("boom"),
        );
        s.wait_idle();
        assert!(s.due(t0 + 2 * SCAN_EVERY), "panic released the in-flight flag");
    }
}
