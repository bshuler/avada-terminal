//! Live transcription: the words appearing in the pane while they are still being said.
//!
//! The finished transcript is still produced the same way it always was — the WAV is
//! transcribed whole when the mic button is clicked off — and it is still the one that
//! decides what the pane ends up holding. This module only fills the gap before it, by
//! transcribing the audio *so far*, over and over, while the recording runs.
//!
//! Two things make that affordable. One [`whisper::Engine`] is loaded and kept, so the
//! 142 MB of weights are paid for once rather than once per second. And the audio handed
//! to it is a *window*, not the whole recording: everything old enough that the model has
//! stopped changing its mind about it is committed to the typed text and dropped from the
//! front, so a five-minute dictation costs the same per pass as a five-second one.
//!
//! What reaches the pane is an [`Edit`] — some backspaces and some text — because a
//! recognizer revises. "I scream" becomes "ice cream" a word later, and the only honest
//! way to show that in a terminal is to take back the characters that were wrong.

use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use super::native::Tap;
use super::whisper::{self, Segment};

/// How often the window is re-transcribed. Whisper's own cost per pass sets the floor;
/// under about a second the passes overlap and the text lags further behind the speaker
/// with every one of them.
const TICK: Duration = Duration::from_millis(1500);
/// Below this much audio a pass is skipped. Whisper is trained on 30-second windows and
/// pads what it is given; a fifth of a second of speech comes back as a hallucinated
/// "Thank you." far more often than as nothing.
const MIN_AUDIO_MS: u64 = 1000;
/// How far behind the end of the window a segment must sit before it is believed.
///
/// The last thing in the window is the thing most likely to change, because the model
/// cannot hear the rest of the word yet. Committing it early is what turns a live view
/// into a stutter of backspaces.
const SETTLE_MS: u64 = 1200;
/// The window is force-committed past this. Cost per pass grows with window length, so
/// an unbroken monologue must not be allowed to grow one without bound — better a
/// slightly early commit than a live view that falls further behind every minute.
const MAX_WINDOW_MS: u64 = 20_000;
/// Samples per millisecond at [`super::native::OUT_RATE`].
const PER_MS: usize = super::native::OUT_RATE as usize / 1000;

/// What to change in the pane to bring it from the last thing shown to the current one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Edit {
    /// Characters to take back first.
    pub backspaces: usize,
    /// Text to type after that.
    pub insert: String,
}

impl Edit {
    #[tracing::instrument(level = "debug", ret)]
    pub fn is_empty(&self) -> bool {
        self.backspaces == 0 && self.insert.is_empty()
    }
}

/// The text this dictation has put into the pane, and the diff that keeps it current.
///
/// Every backspace this produces is one it typed itself: the count can never exceed the
/// length of what is recorded here. That is the whole safety property — a recognizer that
/// suddenly revises its entire transcript can still only eat its own output.
#[derive(Debug, Default)]
pub struct Typed {
    typed: String,
}

impl Typed {
    #[tracing::instrument(level = "debug", ret)]
    pub fn new() -> Self {
        Self::default()
    }

    /// A pane that already holds `text` from this dictation.
    ///
    /// The stop-time reconcile is the same diff as every live pass — pick up what live
    /// typing left, ask for the final transcript, and take back only the difference.
    #[tracing::instrument(level = "debug", ret)]
    pub fn already(text: &str) -> Self {
        Self {
            typed: text.to_string(),
        }
    }

    /// What is believed to be in the pane.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn text(&self) -> &str {
        &self.typed
    }

    /// Move the pane from what is typed to `want`, and record having done so.
    ///
    /// Diffed by character and not by byte: a backspace takes back a character, so
    /// counting the bytes of a revised "café" would eat a letter too many.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn update(&mut self, want: &str) -> Edit {
        let shared = common_prefix(&self.typed, want);
        let backspaces = self.typed[shared..].chars().count();
        let insert = want[shared..].to_string();
        self.typed = want.to_string();
        Edit { backspaces, insert }
    }
}

/// The byte length of the longest prefix `a` and `b` share, always on a char boundary.
#[tracing::instrument(level = "debug", ret)]
fn common_prefix(a: &str, b: &str) -> usize {
    let mut n = 0;
    for (x, y) in a.chars().zip(b.chars()) {
        if x != y {
            break;
        }
        n += x.len_utf8();
    }
    n
}

/// How the driver should transcribe, and how to shape the result.
pub struct Config {
    /// The Whisper model, which must already be on disk — see
    /// [`whisper::cached_model`].
    pub model: PathBuf,
    /// Applied to the assembled text before it is diffed against the pane.
    ///
    /// A function pointer rather than a call into the control layer, so that `stt` keeps
    /// knowing nothing about panes. What matters is that the caller passes the *same*
    /// cleaning the final transcript gets: the stop-time reconcile diffs live text
    /// against final text, and two different cleanings would show up there as a screenful
    /// of backspaces correcting nothing.
    pub clean: fn(&str) -> String,
}

/// A running live transcription.
pub struct Live {
    stop: Sender<()>,
    join: Option<JoinHandle<()>>,
    typed: Arc<Mutex<Typed>>,
}

impl Live {
    /// Start transcribing whatever `tap` captures, reporting each change to `sink`.
    ///
    /// Fails only if the model will not load. Everything after that is best-effort by
    /// design: live text is a courtesy, and no failure of it may cost the recording.
    #[tracing::instrument(level = "debug", skip(tap, cfg, sink))]
    pub fn start(
        tap: Arc<Tap>,
        cfg: Config,
        mut sink: Box<dyn FnMut(Edit) + Send>,
    ) -> Result<Self, String> {
        let engine = whisper::Engine::load(&cfg.model)?;
        let typed = Arc::new(Mutex::new(Typed::new()));
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let shared = Arc::clone(&typed);
        let clean = cfg.clean;
        let join = std::thread::Builder::new()
            .name("hp-live-stt".into())
            .spawn(move || {
                let mut window: Vec<f32> = Vec::new();
                let mut committed = String::new();
                loop {
                    let quit = matches!(
                        stop_rx.recv_timeout(TICK),
                        Ok(()) | Err(RecvTimeoutError::Disconnected)
                    );
                    // Drained even on the way out, but not transcribed: the caller is
                    // already on its way to the real transcript of the whole file, and
                    // one more pass over a partial window would only race it.
                    for s in tap.drain() {
                        window.push(f32::from(s) / 32768.0);
                    }
                    if quit {
                        return;
                    }
                    if window.len() < MIN_AUDIO_MS as usize * PER_MS {
                        continue;
                    }
                    let segments = match engine.transcribe(&window) {
                        Ok(s) => s,
                        Err(e) => {
                            // Logged once per pass and otherwise ignored. The microphone
                            // is still recording, and the transcript that matters is
                            // still going to be made from the file at the end.
                            tracing::debug!(error = %e, "live transcription pass failed");
                            continue;
                        }
                    };
                    let window_ms = (window.len() / PER_MS) as u64;
                    let settled = settled_count(&segments, window_ms);
                    if let Some(last) = settled.checked_sub(1).and_then(|i| segments.get(i)) {
                        for s in &segments[..settled] {
                            push_word(&mut committed, &s.text);
                        }
                        let cut = (last.end_ms as usize * PER_MS).min(window.len());
                        window.drain(..cut);
                    }
                    let mut want = committed.clone();
                    for s in &segments[settled..] {
                        push_word(&mut want, &s.text);
                    }
                    let want = clean(&want);
                    let edit = shared
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .update(&want);
                    if !edit.is_empty() {
                        sink(edit);
                    }
                }
            })
            .map_err(|e| format!("starting live transcription: {e}"))?;
        Ok(Live {
            stop: stop_tx,
            join: Some(join),
            typed,
        })
    }

    /// Stop the driver, wait for it, and report what it left in the pane.
    ///
    /// Joined rather than detached, and joined before the final transcript is delivered:
    /// a pass still in flight would type over the reconcile that is about to correct it.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn finish(mut self) -> String {
        let _ = self.stop.send(());
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        self.typed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .text()
            .to_string()
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// How many leading segments are old enough to type and forget.
///
/// A segment is settled when the model has heard at least [`SETTLE_MS`] past the end of
/// it, because that is the point at which another pass stops rewriting it. The exception
/// is a window that has grown past [`MAX_WINDOW_MS`]: there the cost of another pass is
/// the greater risk, and everything but the final segment is committed regardless.
#[tracing::instrument(level = "debug", ret)]
fn settled_count(segments: &[Segment], window_ms: u64) -> usize {
    let settled = segments
        .iter()
        .take_while(|s| s.end_ms + SETTLE_MS <= window_ms)
        .count();
    if window_ms > MAX_WINDOW_MS {
        return settled.max(segments.len().saturating_sub(1));
    }
    settled
}

/// Append a segment's text, space-separated, the way the batch transcript is assembled.
#[tracing::instrument(level = "debug")]
fn push_word(out: &mut String, text: &str) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    if !out.is_empty() {
        out.push(' ');
    }
    out.push_str(text);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(text: &str, start_ms: u64, end_ms: u64) -> Segment {
        Segment {
            text: text.to_string(),
            start_ms,
            end_ms,
        }
    }

    // ---- Typed / Edit ----

    #[test]
    fn the_first_words_are_typed_with_nothing_taken_back() {
        let mut t = Typed::new();
        assert_eq!(
            t.update("hello there"),
            Edit {
                backspaces: 0,
                insert: "hello there".into()
            }
        );
        assert_eq!(t.text(), "hello there");
    }

    #[test]
    fn growing_text_only_types_the_new_tail() {
        let mut t = Typed::new();
        t.update("open the");
        assert_eq!(
            t.update("open the file"),
            Edit {
                backspaces: 0,
                insert: " file".into()
            }
        );
    }

    #[test]
    fn a_revision_takes_back_exactly_the_characters_that_changed() {
        // The reason this feature needs backspaces at all: whisper revises.
        let mut t = Typed::new();
        t.update("i scream");
        let e = t.update("ice cream");
        assert_eq!(e.backspaces, 7, "'i' is shared, ' scream' is not");
        assert_eq!(e.insert, "ce cream");
        assert_eq!(t.text(), "ice cream");
    }

    #[test]
    fn a_backspace_takes_back_a_character_not_a_byte() {
        let mut t = Typed::new();
        t.update("caf\u{e9} au lait");
        let e = t.update("caf\u{e9} noir");
        assert_eq!(e.backspaces, "au lait".chars().count());
        assert_eq!(e.insert, "noir");
    }

    #[test]
    fn nothing_changing_is_nothing_typed() {
        let mut t = Typed::new();
        t.update("steady");
        let e = t.update("steady");
        assert!(e.is_empty());
    }

    #[test]
    fn a_full_retraction_can_never_exceed_what_was_typed() {
        // The safety property: we can only ever eat our own output, so a recognizer that
        // throws away everything it said still cannot reach the user's own prompt.
        let mut t = Typed::new();
        t.update("a whole sentence");
        let e = t.update("");
        assert_eq!(e.backspaces, "a whole sentence".chars().count());
        assert!(e.insert.is_empty());
    }

    #[test]
    fn the_stop_time_reconcile_only_corrects_the_tail_that_was_wrong() {
        // The point of carrying live text into delivery: the pane already holds most of
        // the transcript, and re-typing it would double it.
        let mut t = Typed::already("open the file and run the tets");
        let e = t.update("open the file and run the tests");
        assert_eq!(
            e.backspaces, 2,
            "'tets' and 'tests' first differ at the 't' of 'ts'"
        );
        assert_eq!(e.insert, "sts");
    }

    #[test]
    fn a_dictation_with_no_live_typing_delivers_the_whole_transcript() {
        // The degenerate case, and the one every process-recorder dictation takes.
        let mut t = Typed::already("");
        let e = t.update("the whole thing");
        assert_eq!(e.backspaces, 0);
        assert_eq!(e.insert, "the whole thing");
    }

    #[test]
    fn cleaning_is_applied_before_the_diff_not_after() {
        // Live text and the final transcript have to be shaped identically, or the
        // stop-time reconcile shows up as a screenful of backspaces correcting nothing.
        let mut t = Typed::new();
        let clean: fn(&str) -> String = |s| s.trim().to_string();
        t.update(&clean("  hello  "));
        assert_eq!(t.text(), "hello");
    }

    // ---- settled_count ----

    #[test]
    fn the_newest_segment_is_never_committed() {
        // It is the one the model is still hearing the end of.
        let segs = [seg("hello", 0, 800), seg("there", 800, 1600)];
        assert_eq!(settled_count(&segs, 1700), 0);
    }

    #[test]
    fn a_segment_the_model_has_moved_well_past_is_committed() {
        let segs = [seg("hello", 0, 800), seg("there", 800, 1600)];
        assert_eq!(
            settled_count(&segs, 2500),
            1,
            "the second is still too fresh"
        );
        assert_eq!(settled_count(&segs, 3000), 2);
    }

    #[test]
    fn an_unbroken_monologue_still_commits_so_the_window_stays_bounded() {
        // Every segment here is inside the settle margin, so the ordinary rule commits
        // nothing and the window would grow for as long as the person keeps talking.
        let segs = [
            seg("one", 0, 21_000),
            seg("two", 21_000, 21_500),
            seg("three", 21_500, 22_000),
        ];
        assert_eq!(settled_count(&segs, 22_100), 2);
    }

    #[test]
    fn nothing_heard_is_nothing_committed() {
        assert_eq!(settled_count(&[], 5000), 0);
    }

    // ---- push_word ----

    #[test]
    fn segments_join_with_one_space_like_the_batch_transcript() {
        let mut s = String::new();
        push_word(&mut s, " hello ");
        push_word(&mut s, "");
        push_word(&mut s, "there");
        assert_eq!(s, "hello there");
    }
}
