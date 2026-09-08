//! The ask queue: `Decision::Ask` is a toast, not a modal. A module's request that
//! resolves to `Ask` is parked here with an id; the toast shows the front of the queue
//! and the user's answer is routed back by id ([`super::RightsService::answer`]).
//!
//! Nothing here persists — a pending ask that outlives the process is simply asked
//! again next time the module tries.

use std::collections::VecDeque;

use super::{Capability, ModuleId};

/// One parked capability request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAsk {
    /// Unique within the process; the toast's buttons carry it back.
    pub id: u64,
    /// The module that asked.
    pub module: ModuleId,
    /// What it asked for.
    pub cap: Capability,
    /// The workspace the request was made in, when there is one — the `workspace`
    /// answer writes there.
    pub workspace: Option<String>,
}

/// The four buttons an ask toast can offer, mapped onto the right value they write.
/// `AllowOnce` writes nothing; the other three are the persisted `RightValue`s
/// (`Workspace` writes `always` into the workspace column).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AskAnswer {
    /// Allow this one request; ask again next time.
    AllowOnce,
    /// Allow and set the user-level value to `always`.
    Always,
    /// Allow and set this workspace's override to `always`.
    Workspace,
    /// Deny and set the user-level value to `never`.
    Never,
}

/// FIFO of pending asks. The front is what the toast shows.
#[derive(Debug, Default)]
pub struct AskQueue {
    next_id: u64,
    pending: VecDeque<PendingAsk>,
}

impl AskQueue {
    /// Park a request; returns its id. A request identical (module, cap, workspace) to
    /// one already pending is coalesced onto that ask's id rather than stacking a
    /// second toast for the same question.
    pub fn push(&mut self, module: ModuleId, cap: Capability, workspace: Option<String>) -> u64 {
        if let Some(existing) = self
            .pending
            .iter()
            .find(|a| a.module == module && a.cap == cap && a.workspace == workspace)
        {
            return existing.id;
        }
        self.next_id += 1;
        let id = self.next_id;
        self.pending.push_back(PendingAsk {
            id,
            module,
            cap,
            workspace,
        });
        id
    }

    /// The ask the toast should show right now.
    pub fn front(&self) -> Option<&PendingAsk> {
        self.pending.front()
    }

    /// Look up a pending ask by id.
    pub fn get(&self, id: u64) -> Option<&PendingAsk> {
        self.pending.iter().find(|a| a.id == id)
    }

    /// Remove and return the ask with this id (`None` if it was already answered).
    pub fn take(&mut self, id: u64) -> Option<PendingAsk> {
        let idx = self.pending.iter().position(|a| a.id == id)?;
        self.pending.remove(idx)
    }

    /// Drop every ask from one module (it was uninstalled or stopped).
    pub fn drop_module(&mut self, module: &ModuleId) {
        self.pending.retain(|a| &a.module != module);
    }

    /// Number of parked asks.
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// True when nothing is parked.
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Every parked ask, front first.
    pub fn iter(&self) -> impl Iterator<Item = &PendingAsk> {
        self.pending.iter()
    }
}
