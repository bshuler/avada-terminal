//! Agent-tip harvest: capture the "✦ Heads up · …" notice an agent TUI raises, keep it with
//! its context, dismiss it so the agent keeps working, and hand it to the Manager.
//!
//! Everything here is plain data and file IO. Nothing in this module (or the review it
//! runs) ever deletes: stale tips and stale Open Questions are MOVED into an archive that
//! stays readable and restorable. It also never acts on a tip — money, deploys and
//! deletions are out of scope, the text is only recorded.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

/// A tip older than this is stale at the hourly review.
pub const TIP_STALE_MS: i64 = 24 * 3600 * 1000;
/// An Open Questions inbox file older than this is stale at the hourly review.
pub const QUESTION_STALE_MS: i64 = 72 * 3600 * 1000;
/// How often the review runs.
pub const REVIEW_EVERY_MS: i64 = 3600 * 1000;
/// The same (pane, text) seen again inside this window is the same notice, not a new tip.
const DEDUPE_MS: i64 = 10 * 60 * 1000;
/// Context lines kept above the notice — what the agent was doing.
const CONTEXT_LINES: usize = 8;
/// The key that picks "Dismiss" in the notice's menu.
pub const DISMISS_KEY: &str = "0";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Tip {
    pub id: i32,
    pub pane_uid: String,
    pub pane_title: String,
    pub text: String,
    /// The rendered lines above the notice: what the agent was doing when it appeared.
    pub context: String,
    pub captured_ms: i64,
    pub archived_ms: Option<i64>,
    pub archive_reason: String,
}

impl Tip {
    pub fn archived(&self) -> bool {
        self.archived_ms.is_some()
    }
}

/// A notice found on a rendered screen.
#[derive(Clone, Debug, PartialEq)]
pub struct Notice {
    pub text: String,
    pub context: String,
    /// The `❯` input line is empty, so a key sent now cannot land in a half-typed message.
    pub input_empty: bool,
}

/// Find a tip notice on `screen`: a `✦ Heads up · <text>` line (the text may wrap) followed
/// by the `1: Learn more   2: Knew this already   0: Dismiss` menu. Both must be present —
/// the menu line is what proves it is the live notice and not text quoted in a transcript.
pub fn parse_notice(screen: &str) -> Option<Notice> {
    let lines: Vec<&str> = screen.lines().collect();
    let menu = lines
        .iter()
        .rposition(|l| l.contains("1: Learn more") && l.contains("Dismiss"))?;
    let head = lines[..menu].iter().rposition(|l| l.contains("Heads up"))?;
    if menu - head > 6 {
        return None;
    }
    let mut text = String::new();
    for (i, l) in lines[head..menu].iter().enumerate() {
        let part = if i == 0 {
            l.split_once('·').map(|x| x.1).unwrap_or(l)
        } else {
            l
        };
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(part);
    }
    if text.is_empty() {
        return None;
    }
    let context: Vec<&str> = lines[..head]
        .iter()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(CONTEXT_LINES)
        .copied()
        .collect();
    let context = context.into_iter().rev().collect::<Vec<_>>().join("\n");
    let input_empty = lines[menu + 1..]
        .iter()
        .find(|l| l.trim_start().starts_with('❯'))
        .is_some_and(|l| l.trim_start().trim_start_matches('❯').trim().is_empty());
    Some(Notice {
        text,
        context,
        input_empty,
    })
}

#[derive(Default, Serialize, Deserialize)]
pub struct TipStore {
    pub next_id: i32,
    pub tips: Vec<Tip>,
    #[serde(skip)]
    pub last_review_ms: i64,
}

impl TipStore {
    /// Record a notice. Returns the new tip's id, or `None` for a repeat of a recent one.
    pub fn capture(&mut self, uid: &str, title: &str, n: &Notice, now_ms: i64) -> Option<i32> {
        if self.tips.iter().any(|t| {
            t.pane_uid == uid && t.text == n.text && now_ms - t.captured_ms < DEDUPE_MS
        }) {
            return None;
        }
        self.next_id += 1;
        let id = self.next_id;
        self.tips.push(Tip {
            id,
            pane_uid: uid.to_string(),
            pane_title: title.to_string(),
            text: n.text.clone(),
            context: n.context.clone(),
            captured_ms: now_ms,
            archived_ms: None,
            archive_reason: String::new(),
        });
        Some(id)
    }

    pub fn archive(&mut self, id: i32, reason: &str, now_ms: i64) -> bool {
        match self.tips.iter_mut().find(|t| t.id == id && !t.archived()) {
            Some(t) => {
                t.archived_ms = Some(now_ms);
                t.archive_reason = reason.to_string();
                true
            }
            None => false,
        }
    }

    pub fn restore(&mut self, id: i32) -> bool {
        match self.tips.iter_mut().find(|t| t.id == id && t.archived()) {
            Some(t) => {
                t.archived_ms = None;
                t.archive_reason.clear();
                true
            }
            None => false,
        }
    }

    /// Archive every active tip older than [`TIP_STALE_MS`]. Returns how many moved.
    pub fn archive_stale(&mut self, now_ms: i64) -> usize {
        let mut n = 0;
        for t in &mut self.tips {
            if !t.archived() && now_ms - t.captured_ms >= TIP_STALE_MS {
                t.archived_ms = Some(now_ms);
                t.archive_reason = "stale (hourly review)".into();
                n += 1;
            }
        }
        n
    }

    #[cfg(test)]
    pub fn active(&self) -> impl Iterator<Item = &Tip> {
        self.tips.iter().filter(|t| !t.archived())
    }
}

pub fn dir() -> PathBuf {
    avada_core::persistence::paths::user_data_dir().join("tips")
}

fn store_path(dir: &Path) -> PathBuf {
    dir.join("tips.json")
}

pub fn load(dir: &Path) -> TipStore {
    std::fs::read(store_path(dir))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save(dir: &Path, store: &TipStore) {
    if let Ok(b) = serde_json::to_vec_pretty(store) {
        let _ = avada_core::persistence::paths::write_atomic(&store_path(dir), &b);
    }
}

/// Append one captured tip to the Manager's feed (`tips/feed.jsonl`, one JSON object per
/// line) — the file the Manager reads; the app never needs it back.
pub fn append_feed(dir: &Path, t: &Tip) {
    use std::io::Write;
    let Ok(line) = serde_json::to_string(t) else {
        return;
    };
    let _ = std::fs::create_dir_all(dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("feed.jsonl"))
    {
        let _ = writeln!(f, "{line}");
    }
}

/// Move Open Questions inbox files older than [`QUESTION_STALE_MS`] into `archive/`.
/// A MOVE: nothing is deleted, and an existing archive file is never overwritten (the
/// name gets a numeric suffix). Returns the names moved.
pub fn archive_stale_questions(oq_dir: &Path, now_ms: i64) -> Vec<String> {
    let inbox = oq_dir.join("inbox");
    let archive = oq_dir.join("archive");
    let mut moved = Vec::new();
    let Ok(rd) = std::fs::read_dir(&inbox) else {
        return moved;
    };
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_file() {
            continue;
        }
        let Some(mtime) = e
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
        else {
            continue;
        };
        if now_ms - mtime < QUESTION_STALE_MS {
            continue;
        }
        let name = e.file_name().to_string_lossy().to_string();
        if std::fs::create_dir_all(&archive).is_err() {
            break;
        }
        let mut dest = archive.join(&name);
        let mut k = 1;
        while dest.exists() {
            dest = archive.join(format!("{name}.{k}"));
            k += 1;
        }
        if std::fs::rename(&p, &dest).is_ok() {
            moved.push(name);
        }
    }
    moved
}

/// The process-wide store: every window shares one tip list.
pub fn store() -> &'static Mutex<TipStore> {
    static S: OnceLock<Mutex<TipStore>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(load(&dir())))
}

/// Command handlers: mutate the shared store and persist.
pub fn archive_cmd(id: i32) {
    let mut s = store().lock().expect("tip store lock");
    if s.archive(id, "archived by hand", crate::glow::now_epoch_ms() as i64) {
        save(&dir(), &s);
    }
}

pub fn restore_cmd(id: i32) {
    let mut s = store().lock().expect("tip store lock");
    if s.restore(id) {
        save(&dir(), &s);
    }
}

pub fn open_questions_dir() -> PathBuf {
    avada_core::persistence::paths::user_data_dir().join("open-questions")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: &str = "● Editing the parser\n  ⎿ done\n\n✦ Heads up · Press Ctrl+O to see\n  the full transcript\n  1: Learn more   2: Knew this already   0: Dismiss\n──────\n❯ \n";

    #[test]
    fn parses_a_wrapped_notice_with_context() {
        let n = parse_notice(SCREEN).unwrap();
        assert_eq!(n.text, "Press Ctrl+O to see the full transcript");
        assert!(n.context.contains("Editing the parser"));
        assert!(n.input_empty);
    }

    #[test]
    fn a_typed_input_line_is_not_empty() {
        let s = SCREEN.replace("❯ \n", "❯ hello\n");
        assert!(!parse_notice(&s).unwrap().input_empty);
    }

    #[test]
    fn needs_the_menu_to_count() {
        assert!(parse_notice("✦ Heads up · quoted in a transcript\n").is_none());
        assert!(parse_notice("no tip here").is_none());
    }

    #[test]
    fn repeats_dedupe_then_expire() {
        let n = parse_notice(SCREEN).unwrap();
        let mut s = TipStore::default();
        assert!(s.capture("p1", "t", &n, 1000).is_some());
        assert!(s.capture("p1", "t", &n, 2000).is_none());
        assert!(s.capture("p2", "t", &n, 2000).is_some());
        assert!(s.capture("p1", "t", &n, 1000 + DEDUPE_MS).is_some());
    }

    #[test]
    fn archive_moves_and_restore_brings_back() {
        let n = parse_notice(SCREEN).unwrap();
        let mut s = TipStore::default();
        let id = s.capture("p", "t", &n, 0).unwrap();
        assert_eq!(s.archive_stale(TIP_STALE_MS - 1), 0);
        assert_eq!(s.archive_stale(TIP_STALE_MS), 1);
        assert_eq!(s.active().count(), 0);
        assert_eq!(s.tips.len(), 1, "archived, never removed");
        assert!(s.restore(id));
        assert_eq!(s.active().count(), 1);
    }

    #[test]
    fn store_round_trips_through_disk() {
        let d = tempdir("rt");
        let n = parse_notice(SCREEN).unwrap();
        let mut s = TipStore::default();
        s.capture("p", "t", &n, 5);
        save(&d, &s);
        assert_eq!(load(&d).tips, s.tips);
    }

    fn tempdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("avada-tips-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn stale_questions_move_and_fresh_stay() {
        let d = tempdir("oq");
        std::fs::create_dir_all(d.join("inbox")).unwrap();
        std::fs::write(d.join("inbox/old.md"), "q").unwrap();
        std::fs::write(d.join("inbox/new.md"), "q").unwrap();
        let now = crate::glow::now_epoch_ms() as i64;
        // Nothing is stale yet.
        assert!(archive_stale_questions(&d, now).is_empty());
        // Far in the future, both are.
        let moved = archive_stale_questions(&d, now + QUESTION_STALE_MS + 60_000);
        assert_eq!(moved.len(), 2);
        assert!(d.join("archive/old.md").exists());
        assert!(!d.join("inbox/old.md").exists());
    }
}
