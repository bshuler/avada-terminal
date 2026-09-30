//! Port of `src/main/control-lock.ts` — advisory per-pane write locks: TTL,
//! holder/owner tracking, acquire/refresh/release, non-owner rejection.
//! Mirror every case in `control-lock.test.ts`.
//!
//! Advisory per-pane write locks (agent-orchestration H). When several managers
//! might drive the same pane, a holder takes a short-lived lock so `send_input`
//! from anyone else is refused until it expires or is released. ADVISORY: an
//! unlocked pane is writable by anyone (preserves the single-orchestrator case);
//! the lock only bites once someone has explicitly claimed the pane.
//!
//! Pure + clock-injected (`now` passed in) so it's deterministic in tests.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockState {
    pub owner: String,
    /// ms epoch
    pub expires_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockResult {
    pub ok: bool,
    /// current holder (the requester on success, else the blocker)
    pub owner: String,
    pub expires_at: i64,
}

#[derive(Debug, Default)]
pub struct PaneLocks {
    locks: HashMap<String, LockState>,
}

impl PaneLocks {
    #[tracing::instrument(level = "debug", ret)]
    pub fn new() -> Self {
        Self::default()
    }

    /// Acquire/renew. Succeeds if the pane is free, the prior lock has expired, or
    /// the requester already holds it (renew). Fails (`ok:false`) if a *different*
    /// owner holds an unexpired lock — the result names the blocking holder.
    #[tracing::instrument(level = "debug", ret)]
    pub fn acquire(&mut self, pane_id: &str, owner: &str, now: i64, ttl_ms: i64) -> LockResult {
        if let Some(cur) = self.locks.get(pane_id) {
            if cur.expires_at > now && cur.owner != owner {
                return LockResult {
                    ok: false,
                    owner: cur.owner.clone(),
                    expires_at: cur.expires_at,
                };
            }
        }
        let expires_at = now + ttl_ms.max(0);
        self.locks.insert(
            pane_id.to_string(),
            LockState {
                owner: owner.to_string(),
                expires_at,
            },
        );
        LockResult {
            ok: true,
            owner: owner.to_string(),
            expires_at,
        }
    }

    /// Release. Only the holder may release; an expired/absent lock counts as freed.
    #[tracing::instrument(level = "debug", ret)]
    pub fn release(&mut self, pane_id: &str, owner: &str, now: i64) -> bool {
        match self.locks.get(pane_id) {
            None => {
                self.locks.remove(pane_id);
                true
            }
            Some(cur) if cur.expires_at <= now => {
                self.locks.remove(pane_id);
                true
            }
            Some(cur) if cur.owner != owner => false,
            Some(_) => {
                self.locks.remove(pane_id);
                true
            }
        }
    }

    /// The current unexpired holder, or `None` if the pane is free. `send_input`
    /// uses this: free → anyone writes; held → only that owner writes.
    #[tracing::instrument(level = "debug", ret)]
    pub fn holder(&self, pane_id: &str, now: i64) -> Option<String> {
        match self.locks.get(pane_id) {
            Some(cur) if cur.expires_at > now => Some(cur.owner.clone()),
            _ => None,
        }
    }

    /// Forget a pane's lock (on close).
    #[tracing::instrument(level = "debug", ret)]
    pub fn drop(&mut self, pane_id: &str) {
        self.locks.remove(pane_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unlocked_pane_has_no_holder() {
        let locks = PaneLocks::new();
        assert_eq!(locks.holder("p", 1000), None);
    }

    #[test]
    fn acquire_blocks_a_different_owner_until_expiry() {
        let mut locks = PaneLocks::new();
        let a = locks.acquire("p", "mgrA", 1000, 5000); // holds until 6000
        assert!(a.ok);
        assert_eq!(a.owner, "mgrA");
        assert_eq!(a.expires_at, 6000);
        assert_eq!(locks.holder("p", 2000), Some("mgrA".to_string()));

        // A different owner is refused while the lock is live, told who blocks.
        let b = locks.acquire("p", "mgrB", 2000, 5000);
        assert!(!b.ok);
        assert_eq!(b.owner, "mgrA");

        // After expiry the pane is free; mgrB can take it.
        assert_eq!(locks.holder("p", 7000), None);
        assert!(locks.acquire("p", "mgrB", 7000, 1000).ok);
    }

    #[test]
    fn the_holder_may_renew_its_own_lock() {
        let mut locks = PaneLocks::new();
        locks.acquire("p", "mgr", 1000, 1000); // expires 2000
        let renew = locks.acquire("p", "mgr", 1500, 1000); // extend to 2500
        assert!(renew.ok);
        assert_eq!(renew.owner, "mgr");
        assert_eq!(renew.expires_at, 2500);
    }

    #[test]
    fn only_the_holder_may_release_expired_or_absent_counts_as_freed() {
        let mut locks = PaneLocks::new();
        locks.acquire("p", "mgr", 1000, 5000);
        assert!(!locks.release("p", "intruder", 2000));
        assert!(locks.release("p", "mgr", 2000));
        assert_eq!(locks.holder("p", 2000), None);
        // Releasing a free pane is a no-op success.
        assert!(locks.release("free", "anyone", 0));
    }

    /// An *expired* lock counts as freed for ANY caller — once the lock is dead, release
    /// need not come from the holder (the `release` contract: "an expired/absent lock counts
    /// as freed"). mgr holds "p" until 6000. A non-holder is refused while it is live, frees
    /// it AT the exact expiry instant, and frees it again strictly after. This kills every
    /// mutation of the `cur.expires_at <= now` guard at line 80: `-> false` and `-> <` both
    /// wrongly refuse at the boundary (falling through to the owner-mismatch arm), and
    /// `-> ==` wrongly refuses strictly past expiry — each leaving a stale lock a non-holder
    /// cannot clear.
    #[test]
    fn a_different_owner_may_release_an_expired_lock() {
        let mut locks = PaneLocks::new();
        // mgr holds "p" until expires_at == 6000.
        locks.acquire("p", "mgr", 1000, 5000);
        // Live at 5999: a non-holder is refused.
        assert!(!locks.release("p", "intruder", 5999));
        // AT the expiry instant (6000): the lock is dead, so the non-holder frees it.
        assert!(locks.release("p", "intruder", 6000));
        assert_eq!(locks.holder("p", 6000), None);
        // Re-hold to expires_at == 7000, then release strictly past it (8000), which
        // distinguishes `<=` from `==`.
        locks.acquire("p", "mgr", 6000, 1000);
        assert!(locks.release("p", "intruder", 8000));
    }

    /// The expiry comparison in `acquire` is a strict `>`: a lock is live only while
    /// `now` is *before* `expires_at`. At the exact expiry instant the pane is FREE, so a
    /// different owner may take it. (Kills `expires_at > now` → `>=` at line 49: a `>=`
    /// would keep the incumbent holding at the boundary and refuse the newcomer.)
    #[test]
    fn acquire_at_the_exact_expiry_instant_treats_the_pane_as_free() {
        let mut locks = PaneLocks::new();
        // mgrA holds until exactly 2000.
        assert_eq!(locks.acquire("p", "mgrA", 1000, 1000).expires_at, 2000);
        // A different owner acquiring AT 2000 must succeed — the old lock is expired.
        let b = locks.acquire("p", "mgrB", 2000, 1000);
        assert!(
            b.ok,
            "at now == expires_at the incumbent lock is dead; mgrB wins"
        );
        assert_eq!(b.owner, "mgrB");
        assert_eq!(b.expires_at, 3000);
    }

    /// `holder` mirrors that strict boundary: at `now == expires_at` the pane reports FREE,
    /// not held. (Kills `expires_at > now` → `>=` at line 97: `>=` would still name the
    /// owner one tick too long.)
    #[test]
    fn holder_reports_free_at_the_exact_expiry_instant() {
        let mut locks = PaneLocks::new();
        locks.acquire("p", "mgr", 1000, 1000); // expires 2000
        assert_eq!(locks.holder("p", 1999), Some("mgr".to_string()));
        assert_eq!(
            locks.holder("p", 2000),
            None,
            "at the expiry instant the pane is free"
        );
    }

    /// `drop` forgets a pane's lock outright, regardless of TTL. (Kills the `drop` body →
    /// `()` mutant at line 105: a no-op would leave the still-live lock in place, so the
    /// pane would keep reporting a holder.)
    #[test]
    fn drop_forgets_a_live_lock() {
        let mut locks = PaneLocks::new();
        // A long-lived lock: expiry alone can't explain a later `None`.
        locks.acquire("p", "mgr", 1000, 10_000); // live until 11000
        assert_eq!(locks.holder("p", 2000), Some("mgr".to_string()));
        locks.drop("p");
        // Well before expiry, yet the lock is gone — only `drop` removing it explains this.
        assert_eq!(locks.holder("p", 2000), None);
        // And the pane is immediately re-acquirable by anyone.
        assert!(locks.acquire("p", "someone-else", 2000, 1000).ok);
    }
}
