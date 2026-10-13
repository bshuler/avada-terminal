//! The scheduled-tasks panel's controller half: turns the OS scheduler's task list
//! (`avada_core::tasks`) into the rows `TasksAdapter` shows, and runs the panel's actions.
//!
//! The panel is a pure projection (`ui/scheduledtasks.slint`); everything it can do is a
//! method here so the UI test and the control verbs exercise the same code. Scheduler calls
//! shell out to `launchctl`/`systemctl`/`schtasks` and finish in tens of milliseconds, so they
//! run inline on the UI thread like the other overlay actions.

use std::sync::Arc;

use avada_core::tasks::{Schedule, Task, TaskScheduler};

use crate::ctl_cli::relative;

/// What the panel shows: a snapshot taken on open and after every action.
#[derive(Default)]
pub struct TasksView {
    pub backend: String,
    pub tasks: Vec<Task>,
    /// Inline error from the last action; `""` = none.
    pub error: String,
    sched: Option<Arc<dyn TaskScheduler>>,
}

impl TasksView {
    fn sched(&mut self) -> Arc<dyn TaskScheduler> {
        self.sched
            .get_or_insert_with(|| Arc::from(avada_core::tasks::default_scheduler()))
            .clone()
    }

    /// Re-read the task list from the scheduler.
    pub fn refresh(&mut self) {
        let s = self.sched();
        self.backend = s.backend().to_string();
        match s.list() {
            Ok(t) => self.tasks = t,
            Err(e) => self.error = e.to_string(),
        }
    }

    /// Run one scheduler action, show its error inline, and re-read the list.
    fn act(
        &mut self,
        f: impl FnOnce(&dyn TaskScheduler) -> Result<(), avada_core::tasks::TaskError>,
    ) {
        let s = self.sched();
        self.error = f(&*s).err().map(|e| e.to_string()).unwrap_or_default();
        let err = std::mem::take(&mut self.error);
        self.refresh();
        if !err.is_empty() {
            self.error = err;
        }
    }

    pub fn set_enabled(&mut self, id: &str, enabled: bool) {
        self.act(|s| s.set_enabled(id, enabled).map(drop));
    }

    pub fn run_now(&mut self, id: &str) {
        self.act(|s| s.run_now(id));
    }

    pub fn remove(&mut self, id: &str) {
        self.act(|s| s.remove(id));
    }

    /// Create from the form's three fields; `command` is a command line split like a shell
    /// would, `schedule` the human string (`every 15m`, `daily 09:30`, 5 cron fields).
    pub fn add(&mut self, name: &str, command: &str, schedule: &str) {
        let mut argv = Task::split_command_line(command.trim());
        if name.trim().is_empty() || argv.is_empty() {
            self.error = "A task needs a name and a command".into();
            return;
        }
        let schedule = match Schedule::parse(schedule) {
            Ok(s) => s,
            Err(e) => {
                self.error = e.to_string();
                return;
            }
        };
        let task = Task {
            id: String::new(),
            name: name.trim().to_string(),
            command: argv.remove(0),
            args: argv,
            cwd: None,
            env: Vec::new(),
            schedule,
            enabled: true,
            created_by: "human".into(),
            created_at: 0,
            last_run: None,
            next_run: None,
            last_status: None,
        };
        self.act(|s| s.create(task).map(drop));
    }
}

/// `last`/`next`/status text for one row.
pub fn row_texts(t: &Task, now: u64) -> (String, String, i32, String) {
    let (status, text) = match t.last_status {
        None => (0, "—".to_string()),
        Some(0) => (1, "ok".to_string()),
        Some(c) => (2, format!("exit {c}")),
    };
    (
        relative(t.last_run, now),
        relative(t.next_run, now),
        status,
        text,
    )
}

/// Push the view into the Slint global.
pub fn fill(app: &crate::AppWindow, v: &TasksView) {
    use slint::ComponentHandle as _;
    let now = crate::loops::unix_now();
    let rows: Vec<crate::TaskRow> = v
        .tasks
        .iter()
        .map(|t| {
            let (last, next, status, status_text) = row_texts(t, now);
            let mut cmd = t.command.clone();
            for a in &t.args {
                cmd.push(' ');
                cmd.push_str(a);
            }
            crate::TaskRow {
                id: t.id.as_str().into(),
                name: t.name.as_str().into(),
                command: cmd.into(),
                schedule: t.schedule.describe().into(),
                enabled: t.enabled,
                last: last.into(),
                next: next.into(),
                status,
                status_text: status_text.into(),
            }
        })
        .collect();
    let g = app.global::<crate::TasksAdapter>();
    g.set_rows(slint::ModelRc::new(slint::VecModel::from(rows)));
    g.set_backend(v.backend.as_str().into());
    g.set_error(v.error.as_str().into());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_validates_before_touching_the_scheduler() {
        let mut v = TasksView::default();
        v.add("", "python3 x.py", "every 15m");
        assert!(v.error.contains("name"), "{}", v.error);
        v.add("job", "python3 x.py", "whenever");
        assert!(!v.error.is_empty());
        assert!(v.tasks.is_empty());
    }

    #[test]
    fn row_texts_classify_the_last_exit() {
        let mut t = Task {
            id: "j".into(),
            name: "j".into(),
            command: "true".into(),
            args: vec![],
            cwd: None,
            env: vec![],
            schedule: Schedule::Interval { secs: 60 },
            enabled: true,
            created_by: "t".into(),
            created_at: 0,
            last_run: None,
            next_run: None,
            last_status: None,
        };
        assert_eq!(row_texts(&t, 100).2, 0);
        t.last_status = Some(0);
        assert_eq!(row_texts(&t, 100).2, 1);
        t.last_status = Some(3);
        assert_eq!(row_texts(&t, 100).3, "exit 3");
    }
}
