//! Linux: a systemd **user** `.service` + `.timer` pair under `~/.config/systemd/user`, driven by
//! `systemctl --user`. Not exercised against a live systemd (the build host here is macOS); the
//! generated unit text is unit-tested.
//!
//! TODO(linux): when `systemctl --user` is unavailable (init/SysV hosts) fall back to a crontab
//! entry (`# avada-task:<id>` marker line) or `at`.

use std::fs;
use std::path::{Path, PathBuf};

use super::{run_cli, NativeBackend, Schedule, Task, TaskError, LABEL_PREFIX};

pub struct SystemdUser {
    unit_dir: PathBuf,
}

impl SystemdUser {
    pub fn new() -> Self {
        let home = std::env::var("HOME").unwrap_or_default();
        let base = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(home).join(".config"));
        SystemdUser {
            unit_dir: base.join("systemd/user"),
        }
    }
    fn stem(id: &str) -> String {
        format!("{LABEL_PREFIX}{id}")
    }
    fn timer(id: &str) -> String {
        format!("{}.timer", Self::stem(id))
    }
}

impl Default for SystemdUser {
    fn default() -> Self {
        Self::new()
    }
}

pub fn render_service(task: &Task, wrapper: &Path) -> String {
    format!(
        "[Unit]\nDescription=Avada task: {}\n\n[Service]\nType=oneshot\nExecStart={}\n",
        task.name.replace('\n', " "),
        wrapper.display()
    )
}

/// Translate to `OnCalendar=` / `OnUnitActiveSec=`.
pub fn render_timer(task: &Task) -> String {
    let when = match &task.schedule {
        Schedule::Interval { secs } => format!("OnBootSec={secs}s\nOnUnitActiveSec={secs}s\n"),
        Schedule::Calendar {
            minute,
            hour,
            day,
            month,
            weekday,
        } => {
            let f = |v: &Option<u8>| v.map_or("*".to_string(), |x| format!("{x:02}"));
            const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
            let wd = weekday.map_or(String::new(), |d| format!("{} ", DAYS[d as usize % 7]));
            format!(
                "OnCalendar={wd}*-{}-{} {}:{}:00\n",
                f(month),
                f(day),
                f(hour),
                f(minute)
            )
        }
    };
    format!("[Unit]\nDescription=Avada task timer: {}\n\n[Timer]\n{when}\n[Install]\nWantedBy=timers.target\n", task.name.replace('\n', " "))
}

impl NativeBackend for SystemdUser {
    fn name(&self) -> &'static str {
        "systemd"
    }

    fn install(&self, task: &Task, wrapper: &Path) -> Result<(), TaskError> {
        fs::create_dir_all(&self.unit_dir)?;
        let stem = Self::stem(&task.id);
        fs::write(
            self.unit_dir.join(format!("{stem}.service")),
            render_service(task, wrapper),
        )?;
        fs::write(
            self.unit_dir.join(format!("{stem}.timer")),
            render_timer(task),
        )?;
        run_cli("systemctl", &["--user", "daemon-reload"])?;
        let verb = if task.enabled { "enable" } else { "disable" };
        run_cli(
            "systemctl",
            &["--user", verb, "--now", &Self::timer(&task.id)],
        )
        .map(|_| ())
    }

    fn uninstall(&self, id: &str) -> Result<(), TaskError> {
        let _ = run_cli(
            "systemctl",
            &["--user", "disable", "--now", &Self::timer(id)],
        );
        let stem = Self::stem(id);
        for ext in ["service", "timer"] {
            let _ = fs::remove_file(self.unit_dir.join(format!("{stem}.{ext}")));
        }
        let _ = run_cli("systemctl", &["--user", "daemon-reload"]);
        Ok(())
    }

    fn set_enabled(&self, task: &Task, wrapper: &Path, _enabled: bool) -> Result<(), TaskError> {
        self.install(task, wrapper)
    }

    fn start(&self, id: &str) -> Result<(), TaskError> {
        run_cli(
            "systemctl",
            &["--user", "start", &format!("{}.service", Self::stem(id))],
        )
        .map(|_| ())
    }

    fn is_active(&self, id: &str) -> bool {
        run_cli("systemctl", &["--user", "is-active", &Self::timer(id)]).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::tests::sample;

    #[test]
    fn timer_text() {
        let t = render_timer(&sample());
        assert!(t.contains("OnUnitActiveSec=900s"));
        let mut c = sample();
        c.schedule = Schedule::Calendar {
            minute: Some(30),
            hour: Some(9),
            day: None,
            month: None,
            weekday: Some(1),
        };
        assert!(render_timer(&c).contains("OnCalendar=Mon *-*-* 09:30:00"));
        assert!(render_service(&c, Path::new("/w/x.sh")).contains("ExecStart=/w/x.sh"));
    }
}
