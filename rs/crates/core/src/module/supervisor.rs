//! Supervision policy: the module state machine and the restart budget. Pure — no
//! threads, no clock of its own — so every transition is unit-tested with a fake `now`.
//! `host.rs` drives it from the process-waiter thread.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Where a module is in its life. `Host::status` reports this and the placeholder pane
/// renders from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleStatus {
    /// No install record was ever given to the host for this id.
    NotInstalled,
    /// Spawned (or about to be) and not yet through the handshake.
    Starting,
    /// Handshake done; the module serves and is served.
    Running,
    /// Exited without being asked; a restart is scheduled.
    Crashed {
        /// Restarts so far in the current rolling window (this crash included).
        restarts: u32,
    },
    /// Gave up restarting, or the user turned it off. Reopening starts it again.
    Disabled {
        /// Human-readable, shown on the placeholder and in the toast.
        reason: String,
    },
    /// The binary or its manifest no longer matches the install record; only a
    /// reinstall clears this.
    Broken {
        /// Human-readable.
        reason: String,
    },
}

impl ModuleStatus {
    /// Short machine name (`not-installed`, `starting`, `running`, `crashed`, `disabled`,
    /// `broken`) for logs and the UI adapter.
    pub fn kind(&self) -> &'static str {
        match self {
            ModuleStatus::NotInstalled => "not-installed",
            ModuleStatus::Starting => "starting",
            ModuleStatus::Running => "running",
            ModuleStatus::Crashed { .. } => "crashed",
            ModuleStatus::Disabled { .. } => "disabled",
            ModuleStatus::Broken { .. } => "broken",
        }
    }

    /// True while the process is expected to be alive.
    pub fn is_live(&self) -> bool {
        matches!(self, ModuleStatus::Starting | ModuleStatus::Running)
    }
}

/// How many crashes a module may have before it is disabled, and how long to wait
/// before each restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestartPolicy {
    /// Restarts allowed inside one `window`; the next crash disables the module.
    pub max_restarts: u32,
    /// The rolling window.
    pub window: Duration,
    /// Delay before the 1st, 2nd, 3rd… restart; the last value repeats.
    pub backoff: Vec<Duration>,
}

impl Default for RestartPolicy {
    /// At most three restarts per rolling minute, backing off 250 ms, 1 s, 4 s.
    fn default() -> Self {
        RestartPolicy {
            max_restarts: 3,
            window: Duration::from_secs(60),
            backoff: vec![
                Duration::from_millis(250),
                Duration::from_secs(1),
                Duration::from_secs(4),
            ],
        }
    }
}

/// What to do after a crash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Start it again after `delay`; this is restart number `attempt` in the window.
    Restart {
        /// How long to wait first.
        delay: Duration,
        /// 1-based count inside the rolling window.
        attempt: u32,
    },
    /// Stop trying.
    Disable {
        /// Why, for the toast and the placeholder.
        reason: String,
    },
}

/// The restart budget for one module.
#[derive(Debug, Clone)]
pub struct Supervisor {
    policy: RestartPolicy,
    crashes: VecDeque<Instant>,
}

impl Supervisor {
    /// A fresh budget under `policy`.
    pub fn new(policy: RestartPolicy) -> Self {
        Supervisor {
            policy,
            crashes: VecDeque::new(),
        }
    }

    /// Record a crash at `now` and decide.
    pub fn on_crash(&mut self, now: Instant) -> Verdict {
        let window = self.policy.window;
        while let Some(first) = self.crashes.front() {
            if now.saturating_duration_since(*first) >= window {
                self.crashes.pop_front();
            } else {
                break;
            }
        }
        if self.crashes.len() as u32 >= self.policy.max_restarts {
            return Verdict::Disable {
                reason: format!(
                    "crashed {} times in {} s",
                    self.crashes.len() + 1,
                    window.as_secs()
                ),
            };
        }
        self.crashes.push_back(now);
        let attempt = self.crashes.len() as u32;
        let delay = self
            .policy
            .backoff
            .get(attempt as usize - 1)
            .or_else(|| self.policy.backoff.last())
            .copied()
            .unwrap_or(Duration::ZERO);
        Verdict::Restart { delay, attempt }
    }

    /// Crashes still inside the window.
    pub fn recent_crashes(&self) -> u32 {
        self.crashes.len() as u32
    }

    /// Forget the history (the user reopened the module on purpose).
    pub fn reset(&mut self) {
        self.crashes.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn three_restarts_with_backoff_then_disabled() {
        let mut s = Supervisor::new(RestartPolicy::default());
        let t0 = Instant::now();
        assert_eq!(
            s.on_crash(t0),
            Verdict::Restart {
                delay: ms(250),
                attempt: 1
            }
        );
        assert_eq!(
            s.on_crash(t0 + ms(10)),
            Verdict::Restart {
                delay: ms(1000),
                attempt: 2
            }
        );
        assert_eq!(
            s.on_crash(t0 + ms(20)),
            Verdict::Restart {
                delay: ms(4000),
                attempt: 3
            }
        );
        match s.on_crash(t0 + ms(30)) {
            Verdict::Disable { reason } => assert!(reason.contains("4 times")),
            other => panic!("expected disable, got {other:?}"),
        }
    }

    #[test]
    fn the_window_rolls() {
        let mut s = Supervisor::new(RestartPolicy::default());
        let t0 = Instant::now();
        for i in 0..3 {
            assert!(matches!(s.on_crash(t0 + ms(i)), Verdict::Restart { .. }));
        }
        // 61 s later the first three have aged out: back to attempt 1 and 250 ms.
        assert_eq!(
            s.on_crash(t0 + Duration::from_secs(61)),
            Verdict::Restart {
                delay: ms(250),
                attempt: 1
            }
        );
        assert_eq!(s.recent_crashes(), 1);
    }

    #[test]
    fn reset_forgets_history() {
        let mut s = Supervisor::new(RestartPolicy::default());
        let t0 = Instant::now();
        for i in 0..3 {
            s.on_crash(t0 + ms(i));
        }
        s.reset();
        assert!(matches!(
            s.on_crash(t0 + ms(5)),
            Verdict::Restart { attempt: 1, .. }
        ));
    }

    #[test]
    fn status_kind_names_are_stable() {
        assert_eq!(ModuleStatus::NotInstalled.kind(), "not-installed");
        assert_eq!(ModuleStatus::Crashed { restarts: 1 }.kind(), "crashed");
        assert_eq!(
            ModuleStatus::Disabled { reason: "x".into() }.kind(),
            "disabled"
        );
        assert_eq!(ModuleStatus::Broken { reason: "x".into() }.kind(), "broken");
        assert!(ModuleStatus::Running.is_live());
        assert!(!ModuleStatus::Crashed { restarts: 0 }.is_live());
    }
}
