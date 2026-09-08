//! Install jobs: one background pipeline run, observable while it runs.
//!
//! A job is a value the routes hand out ([`Job`]) plus the book that keeps it current
//! ([`JobBook`]). The pipeline thread mutates through the book; readers clone a snapshot.
//! The log tail is bounded so a chatty `cargo build` cannot grow the process without
//! limit — the last [`LOG_TAIL`] lines are what a UI shows anyway.

use avada_module_sdk::rights::InstallKind;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// How many log lines a job keeps.
pub const LOG_TAIL: usize = 200;

/// Where the pipeline is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    /// Resolving the tag and cloning.
    Fetch,
    /// Checking the commit, reading and validating the manifest, resolving requires.
    Verify,
    /// `cargo build --release --locked`.
    Build,
    /// Hashing, signing the record, writing the store, activating.
    Install,
    /// Finished; `version` says what was installed.
    Done,
    /// Stopped; `error` says why.
    Failed,
}

impl Phase {
    /// Whether the job has stopped moving.
    pub fn is_terminal(self) -> bool {
        matches!(self, Phase::Done | Phase::Failed)
    }
}

/// One install job as the routes show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    /// Job id (UUID).
    pub id: String,
    /// `owner/repo`.
    pub module: String,
    /// The tag being installed, once known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// Manual or dependency.
    pub kind: InstallKind,
    /// Where the pipeline is.
    pub phase: Phase,
    /// Rough percent, when the phase can estimate one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<u8>,
    /// The last [`LOG_TAIL`] lines of git/cargo output.
    pub log_tail: Vec<String>,
    /// Why the job failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The installed version, once done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Unix seconds when the job started.
    pub started_at: u64,
    /// Unix seconds when it reached a terminal phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
}

struct Slot {
    job: Job,
    log: VecDeque<String>,
}

/// The shared book of jobs, newest last.
#[derive(Clone, Default)]
pub struct JobBook(Arc<Mutex<Vec<Slot>>>);

impl JobBook {
    /// An empty book.
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a job in [`Phase::Fetch`] and return its snapshot.
    pub fn start(&self, module: &str, tag: Option<&str>, kind: InstallKind) -> Job {
        let job = Job {
            id: uuid::Uuid::new_v4().to_string(),
            module: module.to_string(),
            tag: tag.map(str::to_string),
            kind,
            phase: Phase::Fetch,
            progress: Some(0),
            log_tail: Vec::new(),
            error: None,
            version: None,
            started_at: super::cache::now_secs(),
            finished_at: None,
        };
        self.0.lock().unwrap().push(Slot {
            job: job.clone(),
            log: VecDeque::new(),
        });
        job
    }

    /// Snapshot of one job.
    pub fn get(&self, id: &str) -> Option<Job> {
        let book = self.0.lock().unwrap();
        book.iter().find(|s| s.job.id == id).map(snapshot)
    }

    /// Snapshots of every job, oldest first.
    pub fn list(&self) -> Vec<Job> {
        self.0.lock().unwrap().iter().map(snapshot).collect()
    }

    fn update(&self, id: &str, f: impl FnOnce(&mut Slot)) {
        let mut book = self.0.lock().unwrap();
        if let Some(slot) = book.iter_mut().find(|s| s.job.id == id) {
            f(slot);
        }
    }

    /// Append a log line (trimmed; the tail is bounded).
    pub fn log(&self, id: &str, line: &str) {
        let line = line.trim_end().to_string();
        self.update(id, |slot| {
            if slot.log.len() >= LOG_TAIL {
                slot.log.pop_front();
            }
            slot.log.push_back(line);
        });
    }

    /// Move to `phase` with a progress estimate.
    pub fn phase(&self, id: &str, phase: Phase, progress: Option<u8>) {
        self.update(id, |slot| {
            slot.job.phase = phase;
            slot.job.progress = progress;
        });
    }

    /// Adjust progress only (the build phase ticks it per crate compiled).
    pub fn progress(&self, id: &str, progress: u8) {
        self.update(id, |slot| slot.job.progress = Some(progress.min(100)));
    }

    /// Record the tag once resolved.
    pub fn tag(&self, id: &str, tag: &str) {
        self.update(id, |slot| slot.job.tag = Some(tag.to_string()));
    }

    /// Finish in [`Phase::Failed`].
    pub fn fail(&self, id: &str, error: &str) {
        self.update(id, |slot| {
            slot.job.phase = Phase::Failed;
            slot.job.error = Some(error.to_string());
            slot.job.finished_at = Some(super::cache::now_secs());
        });
    }

    /// Finish in [`Phase::Done`].
    pub fn done(&self, id: &str, version: &str) {
        self.update(id, |slot| {
            slot.job.phase = Phase::Done;
            slot.job.progress = Some(100);
            slot.job.version = Some(version.to_string());
            slot.job.finished_at = Some(super::cache::now_secs());
        });
    }
}

fn snapshot(slot: &Slot) -> Job {
    let mut job = slot.job.clone();
    job.log_tail = slot.log.iter().cloned().collect();
    job
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_job_moves_through_phases_and_keeps_a_bounded_tail() {
        let book = JobBook::new();
        let job = book.start("acme/avada-files", None, InstallKind::Manual);
        assert_eq!(job.phase, Phase::Fetch);
        assert!(!job.phase.is_terminal());
        assert_eq!(book.list().len(), 1);
        book.tag(&job.id, "v1.0.0");
        book.phase(&job.id, Phase::Build, Some(50));
        for i in 0..(LOG_TAIL + 10) {
            book.log(&job.id, &format!("line {i}\n"));
        }
        book.progress(&job.id, 200);
        let now = book.get(&job.id).unwrap();
        assert_eq!(now.tag.as_deref(), Some("v1.0.0"));
        assert_eq!(now.phase, Phase::Build);
        assert_eq!(now.progress, Some(100), "progress is clamped");
        assert_eq!(now.log_tail.len(), LOG_TAIL);
        assert_eq!(now.log_tail[0], "line 10", "oldest lines fall off");
        assert_eq!(
            now.log_tail.last().unwrap(),
            &format!("line {}", LOG_TAIL + 9)
        );
        book.done(&job.id, "1.0.0");
        let done = book.get(&job.id).unwrap();
        assert_eq!(done.phase, Phase::Done);
        assert!(done.phase.is_terminal());
        assert_eq!(done.version.as_deref(), Some("1.0.0"));
        assert!(done.finished_at.is_some());
        assert!(done.error.is_none());
    }

    #[test]
    fn failure_records_the_reason_and_unknown_ids_are_ignored() {
        let book = JobBook::new();
        let job = book.start("acme/avada-files", Some("v2.0.0"), InstallKind::Dependency);
        book.fail(&job.id, "cargo exited with status 101");
        let failed = book.get(&job.id).unwrap();
        assert_eq!(failed.phase, Phase::Failed);
        assert_eq!(failed.kind, InstallKind::Dependency);
        assert_eq!(
            failed.error.as_deref(),
            Some("cargo exited with status 101")
        );
        assert!(book.get("nope").is_none());
        book.log("nope", "ignored");
        book.done("nope", "1.0.0");
        assert_eq!(book.list().len(), 1);
        let json = serde_json::to_value(&failed).unwrap();
        assert_eq!(json["phase"], "failed");
        assert_eq!(json["kind"], "dependency");
        assert!(json.get("version").is_none(), "absent fields are omitted");
    }
}
