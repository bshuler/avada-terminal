//! Scheduled-task management: a front end for the **operating system's own scheduler**.
//!
//! Avada does not schedule anything itself. A [`Task`] is translated into whatever the host
//! already runs unattended — a launchd LaunchAgent on macOS, a systemd *user* timer on Linux, a
//! Task Scheduler entry on Windows — so a task keeps firing when Avada is closed, and the OS's
//! own tools (`launchctl`, `systemctl --user`, `schtasks`) show exactly the same thing.
//!
//! This is NOT [`crate::ai::scheduler`], which paces the AI loop inside the app.
//!
//! Shape:
//!   * [`Task`] / [`Schedule`] — the OS-neutral model.
//!   * [`TaskScheduler`] — the operations (create, list, get, update, enable, run-now, remove).
//!   * [`NativeBackend`] — the four things a host scheduler must do (install, uninstall, enable,
//!     start). [`Scheduler`] layers the registry and status bookkeeping over any backend, so a
//!     new OS is one small file. [`launchd`] is complete; [`systemd`] and [`schtasks`] generate
//!     and drive the real units but are only unit-tested for their generated text.
//!
//! **Registry.** The OS knows a task's schedule but not its `created_by`, display name or
//! allow-listed env, so `tasks.json` in the scheduler directory is the record of what Avada
//! created. Native artifacts are named `to.avada.task.<id>`; only those are ever touched.
//!
//! **Status.** Each task runs through a small generated wrapper that records
//! `"<epoch-start> <epoch-end> <exit>"` in `<dir>/status/<id>`; `last_run` / `last_status` are
//! read back from it, which works identically under every backend.
//!
//! **Env.** [`Task::env`] is an allow-list of variable *names*. Values are never stored: the
//! wrapper resolves each name from the user's login environment at run time.

pub mod launchd;
pub mod schtasks;
pub mod systemd;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Prefix of every native artifact Avada creates (launchd label, systemd unit stem, schtasks name).
pub const LABEL_PREFIX: &str = "to.avada.task.";

#[derive(Debug)]
pub enum TaskError {
    /// The request is malformed (bad name, schedule, env name…). Maps to HTTP 400.
    Invalid(String),
    NotFound(String),
    /// The OS scheduler refused or failed. Maps to HTTP 500.
    Native(String),
    Io(io::Error),
}

impl std::fmt::Display for TaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskError::Invalid(m) => write!(f, "invalid task: {m}"),
            TaskError::NotFound(id) => write!(f, "no such task: {id}"),
            TaskError::Native(m) => write!(f, "scheduler: {m}"),
            TaskError::Io(e) => write!(f, "io: {e}"),
        }
    }
}
impl std::error::Error for TaskError {}
impl From<io::Error> for TaskError {
    fn from(e: io::Error) -> Self {
        TaskError::Io(e)
    }
}

/// When a task fires. Local time throughout, as every host scheduler interprets it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Schedule {
    /// Every `secs` seconds (≥ 60).
    Interval { secs: u64 },
    /// Cron-style calendar match; `None` is a wildcard. `weekday` 0 = Sunday … 6 = Saturday.
    #[serde(rename_all = "camelCase")]
    Calendar {
        minute: Option<u8>,
        hour: Option<u8>,
        day: Option<u8>,
        month: Option<u8>,
        weekday: Option<u8>,
    },
}

impl Schedule {
    pub fn validate(&self) -> Result<(), TaskError> {
        let bad = |m: &str| Err(TaskError::Invalid(m.into()));
        match self {
            Schedule::Interval { secs } if *secs < 60 => {
                bad("interval must be at least 60 seconds")
            }
            Schedule::Interval { .. } => Ok(()),
            Schedule::Calendar {
                minute,
                hour,
                day,
                month,
                weekday,
            } => {
                let ok = |v: &Option<u8>, lo: u8, hi: u8| v.is_none_or(|x| (lo..=hi).contains(&x));
                if ok(minute, 0, 59)
                    && ok(hour, 0, 23)
                    && ok(day, 1, 31)
                    && ok(month, 1, 12)
                    && ok(weekday, 0, 6)
                {
                    Ok(())
                } else {
                    bad("calendar field out of range")
                }
            }
        }
    }

    /// Parse the human forms the CLI and UI accept:
    /// `every 15m` / `every 2h` / `every 90s` / `every 1d`, `daily 09:30`, `hourly` (minute 0),
    /// or five cron fields `M H DoM Mon DoW` made of numbers and `*` (`*/N` in the minute field
    /// alone means "every N minutes").
    pub fn parse(s: &str) -> Result<Schedule, TaskError> {
        let s = s.trim();
        let bad = |m: String| TaskError::Invalid(m);
        let sched = if let Some(rest) = s.strip_prefix("every ") {
            let rest = rest.trim();
            let (num, unit) = rest.split_at(
                rest.find(|c: char| !c.is_ascii_digit())
                    .unwrap_or(rest.len()),
            );
            let n: u64 = num
                .parse()
                .map_err(|_| bad(format!("bad interval {rest:?}")))?;
            let mult = match unit.trim() {
                "s" | "sec" | "secs" => 1,
                "m" | "min" | "mins" => 60,
                "h" | "hr" | "hrs" => 3600,
                "d" | "day" | "days" => 86400,
                u => {
                    return Err(bad(format!(
                        "unknown interval unit {u:?} (use s, m, h or d)"
                    )))
                }
            };
            Schedule::Interval { secs: n * mult }
        } else if s == "hourly" {
            Schedule::Calendar {
                minute: Some(0),
                hour: None,
                day: None,
                month: None,
                weekday: None,
            }
        } else if let Some(t) = s.strip_prefix("daily ") {
            let (h, m) = t
                .trim()
                .split_once(':')
                .ok_or_else(|| bad("daily needs HH:MM".into()))?;
            Schedule::Calendar {
                minute: Some(m.parse().map_err(|_| bad("bad minute".into()))?),
                hour: Some(h.parse().map_err(|_| bad("bad hour".into()))?),
                day: None,
                month: None,
                weekday: None,
            }
        } else {
            let f: Vec<&str> = s.split_whitespace().collect();
            if f.len() != 5 {
                return Err(bad(format!(
                    "unrecognised schedule {s:?}: use 'every 15m', 'daily 09:30', 'hourly' or 5 cron fields"
                )));
            }
            if let Some(n) = f[0].strip_prefix("*/") {
                if f[1..].iter().all(|x| *x == "*") {
                    let n: u64 = n.parse().map_err(|_| bad("bad */N".into()))?;
                    return Schedule::Interval { secs: n * 60 }.validated();
                }
                return Err(bad("*/N is only supported alone in the minute field".into()));
            }
            let num = |x: &str| -> Result<Option<u8>, TaskError> {
                if x == "*" {
                    Ok(None)
                } else {
                    x.parse()
                        .map(Some)
                        .map_err(|_| bad(format!("bad cron field {x:?} (numbers and * only)")))
                }
            };
            // Cron's day-of-week allows 7 for Sunday; fold it to 0.
            let weekday = num(f[4])?.map(|d| if d == 7 { 0 } else { d });
            Schedule::Calendar {
                minute: num(f[0])?,
                hour: num(f[1])?,
                day: num(f[2])?,
                month: num(f[3])?,
                weekday,
            }
        };
        sched.validated()
    }

    fn validated(self) -> Result<Schedule, TaskError> {
        self.validate()?;
        Ok(self)
    }

    /// Short human form, for lists.
    pub fn describe(&self) -> String {
        match self {
            Schedule::Interval { secs } if secs % 86400 == 0 => format!("every {}d", secs / 86400),
            Schedule::Interval { secs } if secs % 3600 == 0 => format!("every {}h", secs / 3600),
            Schedule::Interval { secs } if secs % 60 == 0 => format!("every {}m", secs / 60),
            Schedule::Interval { secs } => format!("every {secs}s"),
            Schedule::Calendar {
                minute,
                hour,
                day,
                month,
                weekday,
            } => {
                let f = |v: &Option<u8>| v.map_or("*".to_string(), |x| x.to_string());
                format!(
                    "cron {} {} {} {} {}",
                    f(minute),
                    f(hour),
                    f(day),
                    f(month),
                    f(weekday)
                )
            }
        }
    }

    /// The next firing strictly after `after` (epoch seconds). `local_offset` is the host's UTC
    /// offset in seconds, needed only for calendar schedules. For an interval the OS counts from
    /// when it loaded the job, so `anchor` (last run, else creation) stands in for that.
    pub fn next_after(&self, after: u64, anchor: u64, local_offset: i64) -> Option<u64> {
        match self {
            Schedule::Interval { secs } => {
                if after < anchor {
                    return Some(anchor + secs);
                }
                Some(anchor + ((after - anchor) / secs + 1) * secs)
            }
            Schedule::Calendar {
                minute,
                hour,
                day,
                month,
                weekday,
            } => {
                // Walk minute by minute in local time; the longest gap (Feb 29 + a weekday) fits
                // comfortably in a few years, but a year covers every realistic schedule.
                let mut t = (after / 60 + 1) * 60;
                for _ in 0..(366 * 24 * 60) {
                    let local = (t as i64 + local_offset) as u64;
                    let (_, mo, d, h, mi, wd) = civil(local);
                    let hit = |v: &Option<u8>, x: u8| v.is_none_or(|w| w == x);
                    if hit(minute, mi)
                        && hit(hour, h)
                        && hit(day, d)
                        && hit(month, mo)
                        && hit(weekday, wd)
                    {
                        return Some(t);
                    }
                    t += 60;
                }
                None
            }
        }
    }
}

/// (year, month, day, hour, minute, weekday 0=Sunday) of an epoch-seconds instant, proleptic UTC
/// (callers pre-add the local offset). Howard Hinnant's days-from-civil inverse.
fn civil(secs: u64) -> (i64, u8, u8, u8, u8, u8) {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    let y = yoe + era * 400 + i64::from(m <= 2);
    let wd = ((days + 4).rem_euclid(7)) as u8; // 1970-01-01 was a Thursday
    (y, m, d, (rem / 3600) as u8, ((rem % 3600) / 60) as u8, wd)
}

/// The OS-neutral task. Fields after `enabled` that the OS or the status file own
/// (`last_run`, `next_run`, `last_status`) are filled in on read and ignored on write.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    /// Stable slug (`[a-z0-9-]`, ≤ 48). Becomes part of the native label.
    pub id: String,
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    /// Names only, never values — see the module docs.
    #[serde(default)]
    pub env: Vec<String>,
    pub schedule: Schedule,
    pub enabled: bool,
    /// Who registered it: `"human"`, `"orchestrator"`, a pane id…
    #[serde(default)]
    pub created_by: String,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub last_run: Option<u64>,
    #[serde(default)]
    pub next_run: Option<u64>,
    /// Exit code of the last run; `None` if it has not run yet.
    #[serde(default)]
    pub last_status: Option<i32>,
}

impl Task {
    pub fn validate(&self) -> Result<(), TaskError> {
        let bad = |m: &str| Err(TaskError::Invalid(m.into()));
        if self.id.is_empty()
            || self.id.len() > 48
            || !self
                .id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            || self.id.starts_with('-')
        {
            return bad("id must be 1-48 chars of a-z, 0-9, '-'");
        }
        if self.name.trim().is_empty() {
            return bad("name is empty");
        }
        if self.command.trim().is_empty() {
            return bad("command is empty");
        }
        if self.command.contains('\0') || self.args.iter().any(|a| a.contains('\0')) {
            return bad("NUL in command");
        }
        for e in &self.env {
            let ok = !e.is_empty()
                && !e.starts_with(|c: char| c.is_ascii_digit())
                && e.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !ok {
                return bad(
                    "env entries are variable NAMES (letters, digits, _), never NAME=value",
                );
            }
        }
        if let Some(cwd) = &self.cwd {
            if !Path::new(cwd).is_absolute() {
                return bad("cwd must be an absolute path");
            }
        }
        self.schedule.validate()
    }

    /// `python3 janitor.py` → argv. Honors single and double quotes; no expansion of any kind.
    pub fn split_command_line(line: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut quote: Option<char> = None;
        let mut any = false;
        for c in line.chars() {
            match (quote, c) {
                (Some(q), c) if c == q => quote = None,
                (Some(_), c) => cur.push(c),
                (None, '\'' | '"') => {
                    quote = Some(c);
                    any = true;
                }
                (None, c) if c.is_whitespace() => {
                    if any || !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                        any = false;
                    }
                }
                (None, c) => cur.push(c),
            }
        }
        if any || !cur.is_empty() {
            out.push(cur);
        }
        out
    }
}

/// A slug from a display name: lowercase, runs of other characters collapsed to one `-`.
pub fn slugify(name: &str) -> String {
    let mut s = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c.to_ascii_lowercase());
        } else if !s.ends_with('-') && !s.is_empty() {
            s.push('-');
        }
    }
    let s = s.trim_end_matches('-');
    s.chars()
        .take(48)
        .collect::<String>()
        .trim_end_matches('-')
        .to_string()
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The operations, as the control API and the UI see them.
pub trait TaskScheduler: Send + Sync {
    /// Which host scheduler this fronts: `"launchd"`, `"systemd"`, `"schtasks"`.
    fn backend(&self) -> &'static str;
    fn create(&self, task: Task) -> Result<Task, TaskError>;
    fn list(&self) -> Result<Vec<Task>, TaskError>;
    fn get(&self, id: &str) -> Result<Task, TaskError>;
    /// Replace a task's definition (same id) and re-register it natively.
    fn update(&self, task: Task) -> Result<Task, TaskError>;
    fn set_enabled(&self, id: &str, enabled: bool) -> Result<Task, TaskError>;
    /// Fire the task now, outside its schedule. Returns once the OS has accepted the start.
    fn run_now(&self, id: &str) -> Result<(), TaskError>;
    fn remove(&self, id: &str) -> Result<(), TaskError>;
}

/// The host-scheduler half: turn a [`Task`] into native artifacts and drive them.
pub trait NativeBackend: Send + Sync {
    fn name(&self) -> &'static str;
    /// Write the native definition (always) and, when `task.enabled`, activate it. Idempotent.
    fn install(&self, task: &Task, wrapper: &Path) -> Result<(), TaskError>;
    /// Deactivate and delete the native definition. Missing is not an error.
    fn uninstall(&self, id: &str) -> Result<(), TaskError>;
    fn set_enabled(&self, task: &Task, wrapper: &Path, enabled: bool) -> Result<(), TaskError>;
    /// Start the job now.
    fn start(&self, id: &str) -> Result<(), TaskError>;
    /// Is the job currently registered and active with the OS?
    fn is_active(&self, id: &str) -> bool;
}

/// Registry + wrapper + status bookkeeping over any [`NativeBackend`].
pub struct Scheduler<B: NativeBackend> {
    dir: PathBuf,
    backend: B,
    lock: Mutex<()>,
}

impl<B: NativeBackend> Scheduler<B> {
    pub fn new(dir: PathBuf, backend: B) -> Self {
        Scheduler {
            dir,
            backend,
            lock: Mutex::new(()),
        }
    }

    fn registry_path(&self) -> PathBuf {
        self.dir.join("tasks.json")
    }
    fn status_path(&self, id: &str) -> PathBuf {
        self.dir.join("status").join(id)
    }
    fn wrapper_path(&self, id: &str) -> PathBuf {
        self.dir.join("run").join(format!("{id}{}", wrapper_ext()))
    }

    fn load(&self) -> Result<Vec<Task>, TaskError> {
        match fs::read(self.registry_path()) {
            Ok(b) => serde_json::from_slice(&b)
                .map_err(|e| TaskError::Io(io::Error::new(io::ErrorKind::InvalidData, e))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    fn save(&self, tasks: &[Task]) -> Result<(), TaskError> {
        fs::create_dir_all(&self.dir)?;
        let json = serde_json::to_vec_pretty(tasks)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        crate::persistence::paths::write_atomic(&self.registry_path(), &json)?;
        Ok(())
    }

    /// Fill the OS/status-owned fields.
    fn decorate(&self, mut t: Task) -> Task {
        let status = fs::read_to_string(self.status_path(&t.id)).ok();
        if let Some(line) = status {
            let mut it = line.split_whitespace();
            let _start = it.next();
            let end = it.next().and_then(|x| x.parse::<u64>().ok());
            let code = it.next().and_then(|x| x.parse::<i32>().ok());
            if let (Some(end), Some(code)) = (end, code) {
                t.last_run = Some(end);
                t.last_status = Some(code);
            }
        }
        let now = unix_now();
        t.next_run = if t.enabled {
            let anchor = t.last_run.unwrap_or(t.created_at);
            t.schedule.next_after(now, anchor, local_utc_offset())
        } else {
            None
        };
        t
    }

    fn write_wrapper(&self, t: &Task) -> Result<PathBuf, TaskError> {
        let path = self.wrapper_path(&t.id);
        fs::create_dir_all(path.parent().unwrap())?;
        fs::create_dir_all(self.dir.join("status"))?;
        let body = render_wrapper(
            t,
            &self.status_path(&t.id),
            &self.dir.join("logs").join(format!("{}.log", t.id)),
        );
        fs::create_dir_all(self.dir.join("logs"))?;
        fs::write(&path, body)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
        }
        Ok(path)
    }
}

impl<B: NativeBackend> TaskScheduler for Scheduler<B> {
    fn backend(&self) -> &'static str {
        self.backend.name()
    }

    fn create(&self, mut task: Task) -> Result<Task, TaskError> {
        let _g = self.lock.lock().unwrap();
        if task.id.is_empty() {
            task.id = slugify(&task.name);
        }
        task.validate()?;
        let mut all = self.load()?;
        if all.iter().any(|t| t.id == task.id) {
            return Err(TaskError::Invalid(format!(
                "task {:?} already exists",
                task.id
            )));
        }
        task.created_at = unix_now();
        task.last_run = None;
        task.next_run = None;
        task.last_status = None;
        let wrapper = self.write_wrapper(&task)?;
        self.backend.install(&task, &wrapper)?;
        all.push(task.clone());
        self.save(&all)?;
        Ok(self.decorate(task))
    }

    fn list(&self) -> Result<Vec<Task>, TaskError> {
        let _g = self.lock.lock().unwrap();
        Ok(self.load()?.into_iter().map(|t| self.decorate(t)).collect())
    }

    fn get(&self, id: &str) -> Result<Task, TaskError> {
        let _g = self.lock.lock().unwrap();
        self.load()?
            .into_iter()
            .find(|t| t.id == id)
            .map(|t| self.decorate(t))
            .ok_or_else(|| TaskError::NotFound(id.into()))
    }

    fn update(&self, mut task: Task) -> Result<Task, TaskError> {
        let _g = self.lock.lock().unwrap();
        task.validate()?;
        let mut all = self.load()?;
        let slot = all
            .iter_mut()
            .find(|t| t.id == task.id)
            .ok_or_else(|| TaskError::NotFound(task.id.clone()))?;
        task.created_at = slot.created_at;
        task.created_by = slot.created_by.clone();
        let wrapper = self.write_wrapper(&task)?;
        self.backend.install(&task, &wrapper)?;
        *slot = task.clone();
        self.save(&all)?;
        Ok(self.decorate(task))
    }

    fn set_enabled(&self, id: &str, enabled: bool) -> Result<Task, TaskError> {
        let _g = self.lock.lock().unwrap();
        let mut all = self.load()?;
        let slot = all
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or_else(|| TaskError::NotFound(id.into()))?;
        slot.enabled = enabled;
        let t = slot.clone();
        let wrapper = self.write_wrapper(&t)?;
        self.backend.set_enabled(&t, &wrapper, enabled)?;
        self.save(&all)?;
        Ok(self.decorate(t))
    }

    fn run_now(&self, id: &str) -> Result<(), TaskError> {
        let _g = self.lock.lock().unwrap();
        let t = self
            .load()?
            .into_iter()
            .find(|t| t.id == id)
            .ok_or_else(|| TaskError::NotFound(id.into()))?;
        // A disabled job is not loaded, so there is nothing for the OS to kick; run its wrapper
        // directly. Same script, same status file, so the UI shows the result either way.
        let wrapper = self.write_wrapper(&t)?;
        if !t.enabled {
            return run_detached(&wrapper);
        }
        self.backend.start(id)
    }

    fn remove(&self, id: &str) -> Result<(), TaskError> {
        let _g = self.lock.lock().unwrap();
        let mut all = self.load()?;
        let before = all.len();
        all.retain(|t| t.id != id);
        if all.len() == before {
            return Err(TaskError::NotFound(id.into()));
        }
        self.backend.uninstall(id)?;
        for p in [self.wrapper_path(id), self.status_path(id)] {
            let _ = fs::remove_file(p);
        }
        self.save(&all)
    }
}

/// Run a wrapper directly (used for disabled tasks, which the OS has no loaded job for).
fn run_detached(wrapper: &Path) -> Result<(), TaskError> {
    Command::new(wrapper)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| TaskError::Native(format!("could not start {}: {e}", wrapper.display())))
}

fn wrapper_ext() -> &'static str {
    if cfg!(windows) {
        ".cmd"
    } else {
        ".sh"
    }
}

/// POSIX single-quote escaping.
pub(crate) fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The generated runner. It (1) cd's, (2) imports ONLY the allow-listed env names from the
/// user's login shell, (3) runs the command with output appended to the log, (4) records
/// `start end exit` for the status column. Values never touch disk.
pub(crate) fn render_wrapper(t: &Task, status: &Path, log: &Path) -> String {
    if cfg!(windows) {
        // TODO(windows): env allow-list resolution; `cmd` has no login-shell equivalent.
        let mut line = format!("\"{}\"", t.command);
        for a in &t.args {
            line.push_str(&format!(" \"{a}\""));
        }
        let cd = t
            .cwd
            .as_ref()
            .map(|c| format!("cd /d \"{c}\"\r\n"))
            .unwrap_or_default();
        return format!(
            "@echo off\r\n{cd}{line} >> \"{log}\" 2>&1\r\necho 0 0 %ERRORLEVEL% > \"{st}\"\r\n",
            log = log.display(),
            st = status.display()
        );
    }
    let mut s = String::from(
        "#!/bin/sh\n# Generated by Avada scheduled tasks. Edit the task, not this file.\n",
    );
    s.push_str("PATH=/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin:$HOME/.local/bin\nexport PATH\n");
    if let Some(cwd) = &t.cwd {
        s.push_str(&format!("cd {} || exit 126\n", sh_quote(cwd)));
    }
    for name in &t.env {
        // Name was validated as [A-Za-z_][A-Za-z0-9_]*, so interpolating it is safe.
        s.push_str(&format!(
            "_v=$(/bin/sh -lc 'printenv {name}' 2>/dev/null) && [ -n \"$_v\" ] && {{ {name}=$_v; export {name}; }}\n"
        ));
    }
    s.push_str("_s=$(date +%s)\n");
    let mut cmd = sh_quote(&t.command);
    for a in &t.args {
        cmd.push(' ');
        cmd.push_str(&sh_quote(a));
    }
    s.push_str(&format!(
        "{cmd} >> {} 2>&1\n",
        sh_quote(&log.to_string_lossy())
    ));
    s.push_str("_rc=$?\n");
    s.push_str(&format!(
        "echo \"$_s $(date +%s) $_rc\" > {}\nexit $_rc\n",
        sh_quote(&status.to_string_lossy())
    ));
    s
}

/// The host's current UTC offset in seconds (0 if it cannot be read).
fn local_utc_offset() -> i64 {
    if cfg!(windows) {
        return 0; // TODO(windows): needed only for next_run of calendar schedules.
    }
    let out = Command::new("date").arg("+%z").output().ok();
    let s = out
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    if s.len() == 5 {
        let sign = if s.starts_with('-') { -1 } else { 1 };
        let h: i64 = s[1..3].parse().unwrap_or(0);
        let m: i64 = s[3..5].parse().unwrap_or(0);
        return sign * (h * 3600 + m * 60);
    }
    0
}

/// Run a host CLI, mapping a nonzero exit to [`TaskError::Native`] carrying its stderr.
pub(crate) fn run_cli(program: &str, args: &[&str]) -> Result<String, TaskError> {
    let out = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| TaskError::Native(format!("{program}: {e}")))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(TaskError::Native(format!(
            "{program} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

/// Where the registry, wrappers, status and logs live.
pub fn default_dir() -> PathBuf {
    crate::persistence::paths::user_data_dir().join("scheduled-tasks")
}

/// The scheduler for this OS, rooted at [`default_dir`].
pub fn default_scheduler() -> Box<dyn TaskScheduler> {
    scheduler_in(default_dir())
}

pub fn scheduler_in(dir: PathBuf) -> Box<dyn TaskScheduler> {
    #[cfg(target_os = "macos")]
    {
        Box::new(Scheduler::new(dir, launchd::Launchd::new()))
    }
    #[cfg(windows)]
    {
        Box::new(Scheduler::new(dir, schtasks::Schtasks))
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        // TODO(linux): fall back to cron/at when `systemctl --user` is unavailable.
        Box::new(Scheduler::new(dir, systemd::SystemdUser::new()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_schedules() {
        assert_eq!(
            Schedule::parse("every 15m").unwrap(),
            Schedule::Interval { secs: 900 }
        );
        assert_eq!(
            Schedule::parse("*/15 * * * *").unwrap(),
            Schedule::Interval { secs: 900 }
        );
        assert_eq!(
            Schedule::parse("every 2h").unwrap(),
            Schedule::Interval { secs: 7200 }
        );
        assert_eq!(
            Schedule::parse("daily 09:30").unwrap(),
            Schedule::Calendar {
                minute: Some(30),
                hour: Some(9),
                day: None,
                month: None,
                weekday: None
            }
        );
        assert!(Schedule::parse("every 10s").is_err());
        assert!(Schedule::parse("sometimes").is_err());
        assert!(Schedule::parse("61 * * * *").is_err());
        assert_eq!(
            Schedule::parse("0 0 * * 7").unwrap(),
            Schedule::parse("0 0 * * 0").unwrap()
        );
    }

    #[test]
    fn splits_command_lines() {
        assert_eq!(
            Task::split_command_line("python3 janitor.py"),
            vec!["python3", "janitor.py"]
        );
        assert_eq!(
            Task::split_command_line("echo 'a b' \"c d\" ''"),
            vec!["echo", "a b", "c d", ""]
        );
    }

    #[test]
    fn civil_matches_known_dates() {
        // 2026-10-13 12:34:00 UTC is a Tuesday.
        assert_eq!(civil(1_791_894_840), (2026, 10, 13, 12, 34, 2));
        assert_eq!(civil(0), (1970, 1, 1, 0, 0, 4));
    }

    #[test]
    fn next_after_interval_and_calendar() {
        let i = Schedule::Interval { secs: 900 };
        assert_eq!(i.next_after(1000, 0, 0), Some(1800));
        assert_eq!(i.next_after(1800, 0, 0), Some(2700));
        let c = Schedule::Calendar {
            minute: Some(30),
            hour: Some(9),
            day: None,
            month: None,
            weekday: None,
        };
        // from 2026-10-13 12:34 UTC → next 09:30 UTC is the 14th.
        let n = c.next_after(1_791_894_840, 0, 0).unwrap();
        assert_eq!(civil(n), (2026, 10, 14, 9, 30, 3));
        // a +2h local offset shifts the UTC instant two hours earlier
        let n2 = c.next_after(1_791_894_840, 0, 7200).unwrap();
        assert_eq!(n - n2, 7200);
    }

    #[test]
    fn validation_rejects_env_values_and_relative_cwd() {
        let mut t = sample();
        assert!(t.validate().is_ok());
        t.env = vec!["TOKEN=abc".into()];
        assert!(t.validate().is_err());
        t.env = vec![];
        t.cwd = Some("relative".into());
        assert!(t.validate().is_err());
        t.cwd = None;
        t.id = "Bad Id".into();
        assert!(t.validate().is_err());
    }

    #[test]
    fn slugs() {
        assert_eq!(slugify("Janitor  (15 min)!"), "janitor-15-min");
    }

    #[test]
    #[cfg(unix)]
    fn wrapper_quotes_and_records_status() {
        let mut t = sample();
        t.args = vec!["it's".into()];
        t.env = vec!["MY_TOKEN".into()];
        let w = render_wrapper(&t, Path::new("/s/st"), Path::new("/s/log"));
        assert!(w.contains("'it'\\''s'"));
        assert!(w.contains("printenv MY_TOKEN"));
        assert!(w.contains("> '/s/st'"));
        assert!(
            w.contains("MY_TOKEN=$_v"),
            "value comes from the environment at run time: {w}"
        );
    }

    pub(crate) fn sample() -> Task {
        Task {
            id: "janitor".into(),
            name: "Janitor".into(),
            command: "python3".into(),
            args: vec!["janitor.py".into()],
            cwd: Some("/tmp".into()),
            env: vec![],
            schedule: Schedule::Interval { secs: 900 },
            enabled: true,
            created_by: "test".into(),
            created_at: 0,
            last_run: None,
            next_run: None,
            last_status: None,
        }
    }

    /// A backend that records calls, so registry/status logic is tested without any OS.
    #[derive(Default)]
    struct Fake(Mutex<Vec<String>>);
    impl NativeBackend for std::sync::Arc<Fake> {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn install(&self, t: &Task, _: &Path) -> Result<(), TaskError> {
            self.0.lock().unwrap().push(format!("install {}", t.id));
            Ok(())
        }
        fn uninstall(&self, id: &str) -> Result<(), TaskError> {
            self.0.lock().unwrap().push(format!("uninstall {id}"));
            Ok(())
        }
        fn set_enabled(&self, t: &Task, _: &Path, e: bool) -> Result<(), TaskError> {
            self.0.lock().unwrap().push(format!("enable {} {e}", t.id));
            Ok(())
        }
        fn start(&self, id: &str) -> Result<(), TaskError> {
            self.0.lock().unwrap().push(format!("start {id}"));
            Ok(())
        }
        fn is_active(&self, _: &str) -> bool {
            true
        }
    }

    #[test]
    fn registry_round_trip_with_fake_backend() {
        let dir = std::env::temp_dir().join(format!("avada-tasks-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let fake = std::sync::Arc::new(Fake::default());
        let s = Scheduler::new(dir.clone(), fake.clone());
        let t = s.create(sample()).unwrap();
        assert!(t.next_run.is_some());
        assert!(s.create(sample()).is_err(), "duplicate id refused");
        fs::write(dir.join("status/janitor"), "100 105 3\n").unwrap();
        let t = s.get("janitor").unwrap();
        assert_eq!((t.last_run, t.last_status), (Some(105), Some(3)));
        let t = s.set_enabled("janitor", false).unwrap();
        assert!(!t.enabled && t.next_run.is_none());
        s.run_now("janitor").ok(); // disabled → direct wrapper run; /tmp cwd, python may be absent
        s.remove("janitor").unwrap();
        assert!(s.list().unwrap().is_empty());
        assert!(matches!(s.get("janitor"), Err(TaskError::NotFound(_))));
        let calls = fake.0.lock().unwrap().clone();
        assert_eq!(calls.first().map(String::as_str), Some("install janitor"));
        assert_eq!(calls.last().map(String::as_str), Some("uninstall janitor"));
        let _ = fs::remove_dir_all(&dir);
    }
}
