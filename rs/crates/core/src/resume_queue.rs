//! Durable session→prompt queue — the "speak-first" half of claude-resume.
//!
//! `queue_prompt` stores a message for a *conversation* (a Claude session id, see
//! [`crate::claude_panes`]) in `<state dir>/resume-prompts.json`. The GUI's delivery
//! tick watches the claude-sessions marker dir: when a marker for a queued session
//! (re)appears — the resumed claude's SessionStart hook wrote it, so the agent is up —
//! the prompt is typed into the owning pane and removed from the queue.
//!
//! Deliver-once, file-backed (survives GUI relaunch, daemon death, reboot — the whole
//! point: "after the restart, continue X" outlives every process involved).
//!
//! Deliver-once also means *say-once*: queuing a sentence that is already waiting for the
//! same session is a no-op, so a caller on a timer cannot pile up copies of one message.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::persistence::paths;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueuedPrompt {
    /// Claude session id the prompt is addressed to (validated at enqueue).
    pub session_id: String,
    /// The message typed into the pane once the session is ready.
    pub text: String,
    /// ms epoch, stamped at enqueue — for observability, not ordering (FIFO by position).
    pub queued_at: u64,
}

#[tracing::instrument(level = "debug", ret)]
fn queue_file() -> PathBuf {
    // Test seam: an explicit path override, honored only when set. Production never sets it,
    // so the queue always lives at the real state-dir path. Tests use it to get a hermetic,
    // guaranteed-writable file independent of `state_dir()`'s platform behavior (on macOS
    // that path ignores XDG_STATE_HOME, so env-based isolation there is a no-op).
    if let Some(p) = std::env::var_os("HP_RESUME_PROMPTS_FILE") {
        return PathBuf::from(p);
    }
    paths::resume_prompts_json()
}

#[tracing::instrument(level = "debug", ret)]
fn load() -> Vec<QueuedPrompt> {
    let Ok(text) = fs::read_to_string(queue_file()) else {
        return Vec::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

#[tracing::instrument(level = "debug", ret)]
fn persist(all: &[QueuedPrompt]) {
    if let Ok(json) = serde_json::to_vec_pretty(all) {
        let _ = paths::write_atomic(&queue_file(), &json);
    }
}

/// Append a prompt for `session_id`. The id must be marker-shaped
/// ([`crate::claude_panes::valid_session_id`]) and the text non-empty.
///
/// A prompt identical to one already waiting for the same session is not added again: the
/// session has not read the first copy yet, so a second is the same request twice (the
/// status loop re-firing every interval while its last prompt is still undelivered stacked
/// them up). The waiting copy keeps its place and its `queued_at`.
#[tracing::instrument(level = "debug", ret)]
pub fn enqueue(session_id: &str, text: &str) -> Result<(), String> {
    if !crate::claude_panes::valid_session_id(session_id) {
        return Err(format!("not a valid session id: {session_id}"));
    }
    if text.trim().is_empty() {
        return Err("empty prompt".into());
    }
    let mut all = load();
    if all.iter().any(|p| p.session_id == session_id && p.text == text) {
        return Ok(());
    }
    all.push(QueuedPrompt {
        session_id: session_id.to_string(),
        text: text.to_string(),
        queued_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    });
    persist(&all);
    Ok(())
}

/// Every session id that has at least one prompt waiting.
///
/// A delivery pass walks every marker this machine has ever written, so asking the queue
/// about them one at a time costs a full read of the file per marker, several times a
/// second. One read answers the whole pass instead, and the answer is the only thing it
/// needs: which markers are worth looking at.
#[tracing::instrument(level = "debug", ret)]
pub fn pending_sessions() -> std::collections::HashSet<String> {
    load().into_iter().map(|p| p.session_id).collect()
}

/// How many prompts are waiting for `session_id`, without taking them.
///
/// A peek, so a delivery pass can tell "nothing to say" from "something to say but the
/// pane is not ready for it" and say so in the log, instead of draining the queue to
/// find out.
#[tracing::instrument(level = "debug", ret)]
pub fn count_for(session_id: &str) -> usize {
    load().iter().filter(|p| p.session_id == session_id).count()
}

/// Remove and return every queued prompt for `session_id`, oldest first.
#[tracing::instrument(level = "debug", ret)]
pub fn take_for(session_id: &str) -> Vec<QueuedPrompt> {
    let all = load();
    let (taken, kept): (Vec<_>, Vec<_>) = all.into_iter().partition(|p| p.session_id == session_id);
    if !taken.is_empty() {
        persist(&kept);
    }
    // Same collapse as [`enqueue`], applied on the way out so a backlog written before that
    // guard existed is healed rather than typed into the pane one identical copy at a time.
    let mut seen = std::collections::HashSet::new();
    taken.into_iter().filter(|p| seen.insert(p.text.clone())).collect()
}

/// Does anything wait for any session? Cheap gate for the delivery tick (one stat).
#[tracing::instrument(level = "debug", ret)]
pub fn is_empty() -> bool {
    match fs::metadata(queue_file()) {
        Err(_) => true,
        // A written-out empty array is 2 bytes ("[]") — anything bigger may hold work.
        Ok(m) => m.len() <= 2,
    }
}

/// Peek at all queued prompts (for a `/state`-style listing; never removes).
#[tracing::instrument(level = "debug", ret)]
pub fn list() -> Vec<QueuedPrompt> {
    load()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Mutex, OnceLock};

    /// The queue path is a process-global (an env var), so the tests that point it at their
    /// own file must not run concurrently — serialize them on one mutex.
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        static L: OnceLock<Mutex<()>> = OnceLock::new();
        L.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    /// Point the queue at a unique, empty temp file for this test — hermetic and
    /// platform-independent (no reliance on `state_dir()` honoring XDG_STATE_HOME).
    fn use_scratch_queue() {
        static N: AtomicU32 = AtomicU32::new(0);
        let f = std::env::temp_dir().join(format!(
            "hp-rq-{}-{}.json",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_file(&f);
        std::env::set_var("HP_RESUME_PROMPTS_FILE", &f);
    }

    #[test]
    fn enqueue_take_roundtrip_is_fifo_and_deliver_once() {
        let _g = lock();
        use_scratch_queue();
        assert!(is_empty());
        enqueue("deadbeef-0000", "first").unwrap();
        enqueue("deadbeef-0000", "second").unwrap();
        enqueue("cafecafe-1111", "other session").unwrap();
        assert!(!is_empty());

        let taken = take_for("deadbeef-0000");
        assert_eq!(
            taken.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(),
            vec!["first", "second"]
        );
        // Deliver-once: a second take finds nothing; the other session's prompt survives.
        assert!(take_for("deadbeef-0000").is_empty());
        assert_eq!(take_for("cafecafe-1111").len(), 1);
        assert!(is_empty());
    }

    #[test]
    fn an_identical_waiting_prompt_is_not_queued_twice() {
        let _g = lock();
        use_scratch_queue();
        enqueue("deadbeef-0000", "status?").unwrap();
        let first = list();
        enqueue("deadbeef-0000", "status?").unwrap();
        // Same session, same text: coalesced, and the waiting copy is untouched.
        assert_eq!(list(), first);
        // A different text, or the same text for another session, is a different request.
        enqueue("deadbeef-0000", "something else").unwrap();
        enqueue("cafecafe-1111", "status?").unwrap();
        assert_eq!(list().len(), 3);
        // Once delivered, the same prompt may be queued again.
        assert_eq!(take_for("deadbeef-0000").len(), 2);
        enqueue("deadbeef-0000", "status?").unwrap();
        assert_eq!(take_for("deadbeef-0000").len(), 1);
    }

    #[test]
    fn pending_sessions_names_each_waiting_session_once() {
        let _g = lock();
        use_scratch_queue();
        assert!(pending_sessions().is_empty());
        enqueue("deadbeef-0000", "one").unwrap();
        enqueue("deadbeef-0000", "two").unwrap();
        enqueue("cafecafe-1111", "three").unwrap();
        assert_eq!(
            pending_sessions(),
            ["deadbeef-0000".to_string(), "cafecafe-1111".to_string()]
                .into_iter()
                .collect()
        );
        // Taking a session's prompts takes it out of the set; the other one stays.
        take_for("deadbeef-0000");
        assert_eq!(
            pending_sessions(),
            ["cafecafe-1111".to_string()].into_iter().collect()
        );
    }

    #[test]
    fn a_backlog_of_identical_prompts_is_collapsed_on_the_way_out() {
        let _g = lock();
        use_scratch_queue();
        // A file written before the enqueue-side guard existed: the same sentence, over and
        // over. Delivery must type it once, not once per copy.
        let repeated = QueuedPrompt {
            session_id: "deadbeef-0000".into(),
            text: "Status check: review every pane.".into(),
            queued_at: 0,
        };
        let mut backlog = vec![repeated.clone(); 66];
        backlog.push(QueuedPrompt {
            text: "then stop".into(),
            ..repeated
        });
        persist(&backlog);

        let taken = take_for("deadbeef-0000");
        assert_eq!(
            taken.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(),
            vec!["Status check: review every pane.", "then stop"]
        );
        assert!(is_empty());
    }

    #[test]
    fn rejects_invalid_ids_and_empty_text() {
        let _g = lock();
        use_scratch_queue();
        assert!(enqueue("$(boom)", "hi").is_err());
        assert!(enqueue("deadbeef-0000", "   ").is_err());
        assert!(is_empty());
    }
}
