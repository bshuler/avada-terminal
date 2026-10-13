//! Windows: Task Scheduler through `schtasks`. Argument construction is unit-tested; the
//! commands themselves have not been run on a Windows host.
//!
//! TODO(windows): `at` fallback for hosts without Task Scheduler; `/SC` mapping for
//! multi-field calendar schedules (weekday + hour) beyond DAILY/HOURLY/MINUTE.

use std::path::Path;

use super::{run_cli, NativeBackend, Schedule, Task, TaskError, LABEL_PREFIX};

pub struct Schtasks;

fn name(id: &str) -> String {
    format!("{LABEL_PREFIX}{id}")
}

/// `/SC` + `/MO` / `/ST` arguments for a schedule.
pub fn schedule_args(s: &Schedule) -> Result<Vec<String>, TaskError> {
    Ok(match s {
        Schedule::Interval { secs } if secs % 86400 == 0 => vec![
            "/SC".into(),
            "DAILY".into(),
            "/MO".into(),
            (secs / 86400).to_string(),
        ],
        Schedule::Interval { secs } if secs % 3600 == 0 => vec![
            "/SC".into(),
            "HOURLY".into(),
            "/MO".into(),
            (secs / 3600).to_string(),
        ],
        Schedule::Interval { secs } => vec![
            "/SC".into(),
            "MINUTE".into(),
            "/MO".into(),
            (secs.div_ceil(60)).to_string(),
        ],
        Schedule::Calendar {
            minute,
            hour: Some(h),
            day: None,
            month: None,
            weekday: None,
        } => {
            vec![
                "/SC".into(),
                "DAILY".into(),
                "/ST".into(),
                format!("{:02}:{:02}", h, minute.unwrap_or(0)),
            ]
        }
        Schedule::Calendar {
            minute,
            hour: None,
            day: None,
            month: None,
            weekday: None,
        } => {
            vec![
                "/SC".into(),
                "HOURLY".into(),
                "/ST".into(),
                format!("00:{:02}", minute.unwrap_or(0)),
            ]
        }
        Schedule::Calendar { .. } => {
            return Err(TaskError::Native(
                "this calendar shape is not yet mapped to schtasks".into(),
            ))
        }
    })
}

impl NativeBackend for Schtasks {
    fn name(&self) -> &'static str {
        "schtasks"
    }

    fn install(&self, task: &Task, wrapper: &Path) -> Result<(), TaskError> {
        let mut args: Vec<String> = vec![
            "/Create".into(),
            "/F".into(),
            "/TN".into(),
            name(&task.id),
            "/TR".into(),
            format!("\"{}\"", wrapper.display()),
        ];
        args.extend(schedule_args(&task.schedule)?);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        run_cli("schtasks", &refs)?;
        let flag = if task.enabled { "/ENABLE" } else { "/DISABLE" };
        run_cli("schtasks", &["/Change", "/TN", &name(&task.id), flag]).map(|_| ())
    }

    fn uninstall(&self, id: &str) -> Result<(), TaskError> {
        let _ = run_cli("schtasks", &["/Delete", "/F", "/TN", &name(id)]);
        Ok(())
    }

    fn set_enabled(&self, task: &Task, _w: &Path, enabled: bool) -> Result<(), TaskError> {
        let flag = if enabled { "/ENABLE" } else { "/DISABLE" };
        run_cli("schtasks", &["/Change", "/TN", &name(&task.id), flag]).map(|_| ())
    }

    fn start(&self, id: &str) -> Result<(), TaskError> {
        run_cli("schtasks", &["/Run", "/TN", &name(id)]).map(|_| ())
    }

    fn is_active(&self, id: &str) -> bool {
        run_cli("schtasks", &["/Query", "/TN", &name(id)]).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_schedules() {
        assert_eq!(
            schedule_args(&Schedule::Interval { secs: 900 }).unwrap(),
            ["/SC", "MINUTE", "/MO", "15"]
        );
        assert_eq!(
            schedule_args(&Schedule::Interval { secs: 7200 }).unwrap(),
            ["/SC", "HOURLY", "/MO", "2"]
        );
        let d = Schedule::Calendar {
            minute: Some(5),
            hour: Some(9),
            day: None,
            month: None,
            weekday: None,
        };
        assert_eq!(schedule_args(&d).unwrap(), ["/SC", "DAILY", "/ST", "09:05"]);
    }
}
