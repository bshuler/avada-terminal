//! macOS: a per-user LaunchAgent plist in `~/Library/LaunchAgents`, driven by `launchctl`.
//!
//! Enabled = plist present and bootstrapped into `gui/<uid>`. Disabled = plist present with
//! `Disabled` true and booted out, so a login does not resurrect it.

use std::fs;
use std::path::{Path, PathBuf};

use super::{run_cli, NativeBackend, Schedule, Task, TaskError, LABEL_PREFIX};

pub struct Launchd {
    agents_dir: PathBuf,
    /// `gui/<uid>`
    domain: String,
}

impl Launchd {
    pub fn new() -> Self {
        let home = std::env::var("HOME").unwrap_or_default();
        let uid = run_cli("id", &["-u"])
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "501".into());
        Launchd {
            agents_dir: PathBuf::from(home).join("Library/LaunchAgents"),
            domain: format!("gui/{uid}"),
        }
    }

    fn label(id: &str) -> String {
        format!("{LABEL_PREFIX}{id}")
    }
    fn plist_path(&self, id: &str) -> PathBuf {
        self.agents_dir.join(format!("{}.plist", Self::label(id)))
    }
    fn target(&self, id: &str) -> String {
        format!("{}/{}", self.domain, Self::label(id))
    }
    fn bootout(&self, id: &str) {
        // Not loaded is the normal case for a first install or a disabled task; ignore.
        let _ = run_cli("launchctl", &["bootout", &self.target(id)]);
    }
    fn bootstrap(&self, id: &str) -> Result<(), TaskError> {
        let p = self.plist_path(id);
        run_cli(
            "launchctl",
            &["bootstrap", &self.domain, &p.to_string_lossy()],
        )
        .map(|_| ())
    }
}

impl Default for Launchd {
    fn default() -> Self {
        Self::new()
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The LaunchAgent plist for `task`, running `wrapper`.
pub fn render_plist(task: &Task, wrapper: &Path) -> String {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n",
    );
    s.push_str(&format!(
        "  <key>Label</key><string>{}</string>\n",
        xml_escape(&Launchd::label(&task.id))
    ));
    s.push_str(&format!(
        "  <key>ProgramArguments</key><array><string>{}</string></array>\n",
        xml_escape(&wrapper.to_string_lossy())
    ));
    if !task.enabled {
        s.push_str("  <key>Disabled</key><true/>\n");
    }
    match &task.schedule {
        Schedule::Interval { secs } => {
            s.push_str(&format!(
                "  <key>StartInterval</key><integer>{secs}</integer>\n"
            ));
        }
        Schedule::Calendar {
            minute,
            hour,
            day,
            month,
            weekday,
        } => {
            s.push_str("  <key>StartCalendarInterval</key>\n  <dict>\n");
            for (k, v) in [
                ("Minute", minute),
                ("Hour", hour),
                ("Day", day),
                ("Month", month),
                ("Weekday", weekday),
            ] {
                if let Some(v) = v {
                    s.push_str(&format!("    <key>{k}</key><integer>{v}</integer>\n"));
                }
            }
            s.push_str("  </dict>\n");
        }
    }
    s.push_str("  <key>ProcessType</key><string>Background</string>\n</dict>\n</plist>\n");
    s
}

impl NativeBackend for Launchd {
    fn name(&self) -> &'static str {
        "launchd"
    }

    fn install(&self, task: &Task, wrapper: &Path) -> Result<(), TaskError> {
        fs::create_dir_all(&self.agents_dir)?;
        // Bootout first so a changed plist is re-read rather than ignored by the loaded copy.
        self.bootout(&task.id);
        fs::write(self.plist_path(&task.id), render_plist(task, wrapper))?;
        if task.enabled {
            self.bootstrap(&task.id)?;
        }
        Ok(())
    }

    fn uninstall(&self, id: &str) -> Result<(), TaskError> {
        self.bootout(id);
        match fs::remove_file(self.plist_path(id)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    fn set_enabled(&self, task: &Task, wrapper: &Path, _enabled: bool) -> Result<(), TaskError> {
        // `task.enabled` is already the new value; rewriting + (re)bootstrapping covers both ways.
        self.install(task, wrapper)
    }

    fn start(&self, id: &str) -> Result<(), TaskError> {
        // No -k: a run-now must not preempt a run already in flight.
        run_cli("launchctl", &["kickstart", &self.target(id)]).map(|_| ())
    }

    fn is_active(&self, id: &str) -> bool {
        run_cli("launchctl", &["print", &self.target(id)]).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::tests::sample;

    #[test]
    fn interval_plist() {
        let p = render_plist(&sample(), Path::new("/w/janitor.sh"));
        assert!(p.contains("<key>Label</key><string>to.avada.task.janitor</string>"));
        assert!(p.contains("<key>StartInterval</key><integer>900</integer>"));
        assert!(p.contains("<string>/w/janitor.sh</string>"));
        assert!(!p.contains("Disabled"));
    }

    #[test]
    fn calendar_plist_and_disabled() {
        let mut t = sample();
        t.enabled = false;
        t.schedule = Schedule::Calendar {
            minute: Some(30),
            hour: Some(9),
            day: None,
            month: None,
            weekday: Some(1),
        };
        let p = render_plist(&t, Path::new("/w/x.sh"));
        assert!(p.contains("<key>Disabled</key><true/>"));
        assert!(p.contains("<key>Hour</key><integer>9</integer>"));
        assert!(p.contains("<key>Weekday</key><integer>1</integer>"));
        assert!(!p.contains("<key>Day</key>"));
    }
}
