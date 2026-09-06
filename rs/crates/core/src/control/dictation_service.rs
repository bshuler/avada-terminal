//! The control layer's dictation surface: persisted [`SttSettings`], the per-pane
//! [`Dictation`] state machine, and the one rule that makes a transcript safe to type
//! into a live terminal.
//!
//! Unlike its sibling [`speech_service`](crate::control::speech_service), this has no
//! background loop. Dictation is entirely user-driven — a click starts it, a click stops
//! it — so there is nothing to poll and an install where nobody ever presses the mic does
//! no work at all.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::control::input::SUBMIT_DELAY_MS;
use crate::control::server::Shared;
use crate::permissions::{self, Grant, Right};
use crate::stt::dictation::{Dictation, LiveOptions, Transcript};
use crate::stt::live::{Edit, Typed};
use crate::stt::{self, SttSettings};

/// What one backspace is, on the wire.
///
/// DEL (0x7f), not BS (0x08): DEL is what the Backspace key sends on macOS and on every
/// terminal whose `erase` is the default, and it is what readline, Ink and the shells all
/// treat as "rub out the character behind the cursor". BS moves the cursor left in some
/// of them and deletes in others, which is the worst of both.
const BACKSPACE: &str = "\u{7f}";

/// A point-in-time snapshot for `/state`'s `dictation` field.
pub struct DictationStatus {
    pub recorder: String,
    pub transcriber: String,
    pub recording_panes: Vec<String>,
    /// Where finished recordings and their transcripts are kept.
    pub kept_in: String,
}

/// Owns dictation settings and every pane's recording state.
pub struct DictationService {
    settings_path: PathBuf,
    settings: Mutex<SttSettings>,
    dictation: Dictation,
}

impl DictationService {
    /// `settings_path` is `stt.json`'s path.
    ///
    /// Capture goes to a pid-scoped temp directory, so two instances never share a
    /// scratch file. The finished recording and its transcript then move to
    /// `<state>/dictation`, which is the whole point: a transcript that came out wrong is
    /// only fixable if the words are still somewhere the user can open and paste from,
    /// and temp is not that place.
    #[tracing::instrument(level = "debug")]
    pub fn new(settings_path: PathBuf) -> Self {
        let settings = stt::load(&settings_path);
        let wav_dir =
            std::env::temp_dir().join(format!("hyperpanes-dictation-{}", std::process::id()));
        let archive = crate::persistence::paths::state_dir().join("dictation");
        DictationService {
            settings_path,
            settings: Mutex::new(settings),
            dictation: Dictation::new_with_archive(wav_dir, archive),
        }
    }

    /// Where finished recordings and their transcripts are kept.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn archive_dir(&self) -> std::path::PathBuf {
        self.dictation.archive_dir().to_path_buf()
    }

    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn settings_snapshot(&self) -> SttSettings {
        self.settings
            .lock()
            .expect("stt settings lock poisoned")
            .clone()
    }

    /// Re-read `stt.json` — the settings routes call this after a user edits it, so a new
    /// `transcribeTemplate` takes effect without a restart.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn reload(&self) {
        let fresh = stt::load(&self.settings_path);
        *self.settings.lock().expect("stt settings lock poisoned") = fresh;
    }

    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn recording_panes(&self) -> Vec<String> {
        self.dictation.recording_panes()
    }

    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn is_recording(&self, pane_id: &str) -> bool {
        self.dictation.is_recording(pane_id)
    }

    /// Whether a finished transcript should be submitted (Enter) as well as typed.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn submit_after_insert(&self) -> bool {
        self.settings_snapshot().submit
    }

    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn start(&self, pane_id: &str) -> Result<&'static str, String> {
        self.start_live(pane_id, None)
    }

    /// As [`Self::start`], but typing the words into the pane as they are spoken.
    #[tracing::instrument(level = "debug", ret, skip(self, opts))]
    pub fn start_live(
        &self,
        pane_id: &str,
        opts: Option<LiveOptions>,
    ) -> Result<&'static str, String> {
        // Raise the OS's own consent dialog from the feature that needs it, at the moment it
        // needs it — macOS shows each one once ever, so a mic prompt spent on a settings list
        // is one dictation never gets. Where the OS has no dialog this is a status read.
        if permissions::prompt(Right::Microphone) == Grant::Denied {
            return Err(format!(
                "{} access is denied — grant it in system settings",
                Right::Microphone.label()
            ));
        }
        self.dictation
            .start_live(pane_id, &self.settings_snapshot(), opts)
    }

    /// Take the user to the OS's microphone setting.
    ///
    /// This exists because "nothing was recorded" is, on macOS, far more often a denied mic
    /// than a broken recorder — and [`permissions::status`] there is `Undetermined`, since
    /// reading the real answer means loading AVFoundation. Offering the door is honest;
    /// asserting a grant we cannot read would not be.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn open_microphone_settings(&self) -> Result<(), String> {
        permissions::request(Right::Microphone)
    }

    /// Stop and transcribe. **Blocking** — seconds, on a cold model — so the HTTP layer
    /// runs it on a blocking task rather than the async executor.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn stop(&self, pane_id: &str) -> Result<Transcript, String> {
        self.dictation.stop(pane_id, &self.settings_snapshot())
    }

    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn cancel(&self, pane_id: &str) {
        self.dictation.cancel(pane_id);
    }

    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn cancel_all(&self) {
        self.dictation.cancel_all();
    }

    #[tracing::instrument(level = "debug", skip(self))]
    pub fn status(&self) -> DictationStatus {
        let s = self.settings_snapshot();
        DictationStatus {
            recorder: crate::stt::backend::detect_recorder(&s).name().to_string(),
            transcriber: crate::stt::backend::detect_transcriber(&s)
                .name()
                .to_string(),
            recording_panes: self.dictation.recording_panes(),
            kept_in: self.archive_dir().display().to_string(),
        }
    }
}

/// Start `pane_id` recording, typing what is heard into `uid`'s pty as it is heard.
///
/// The counterpart of [`stop_and_deliver`], and it exists for the same reason: the sink
/// needs the session table and the pane's uid, which the `stt` layer knows nothing about.
/// Both of the app's mic buttons go through here so that live typing cannot be on in one
/// of them and off in the other.
#[tracing::instrument(level = "debug", ret, skip(shared))]
pub fn start_dictation(
    shared: &Arc<Shared>,
    pane_id: &str,
    uid: &str,
) -> Result<&'static str, String> {
    let sunk = Arc::clone(shared);
    let target = uid.to_string();
    shared.dictation.start_live(
        pane_id,
        Some(LiveOptions {
            clean: clean_for_pane,
            // Failures are swallowed on purpose. A pane that has gone away, or a program
            // that has closed its input, must not take the recording down with it — the
            // words are still going into the WAV, and the transcript at the end is still
            // going to be delivered or reported.
            sink: Box::new(move |edit| {
                let _ = apply(&sunk, &target, &edit);
            }),
        }),
    )
}

/// Bring a pane from what it currently holds to what an [`Edit`] says it should.
///
/// The backspaces go out as a plain write and the text as a paste, and that split is the
/// point. A backspace has to arrive as a *keystroke* — inside a bracketed paste it is
/// literal content, and a TUI that honours bracketed paste would insert it rather than
/// erase with it. The insert, meanwhile, wants to be a paste for the reason the batch
/// delivery does: it can be longer than the tty's 1024-byte input queue.
#[tracing::instrument(level = "debug", ret, skip(shared))]
fn apply(shared: &Shared, uid: &str, edit: &Edit) -> Result<(), String> {
    if edit.backspaces > 0 {
        shared
            .sessions
            .write(uid, &BACKSPACE.repeat(edit.backspaces))
            .map_err(|e| format!("the pane did not accept a correction: {e}"))?;
    }
    if !edit.insert.is_empty() {
        shared
            .sessions
            .paste(uid, &edit.insert)
            .map_err(|e| format!("the pane did not accept the transcript: {e}"))?;
    }
    Ok(())
}

/// The shaping every word gets on its way into a pane, live or final.
///
/// One function, used by both, because the stop-time reconcile diffs one against the
/// other: any difference between the two cleanings would show up in the pane as a burst
/// of backspaces correcting text that was already right.
#[tracing::instrument(level = "debug", ret)]
fn clean_for_pane(raw: &str) -> String {
    sanitize_for_pane(&crate::stt::backend::clean_transcript(raw))
}

/// What a finished dictation put into a pane.
pub struct Delivered {
    pub text: String,
    pub backend: &'static str,
    pub submitted: bool,
    /// The kept transcript, for a caller that wants to tell the user where their words
    /// are. `None` only when the archive could not be written.
    pub kept: Option<PathBuf>,
}

/// Stop `pane_id`'s recording, transcribe it, and type the result into `uid`'s pty.
///
/// **Blocking** — a cold transcriber is seconds. Both callers run it off their own thread:
/// the control route on `spawn_blocking`, the GUI's mic button on a worker, which is also
/// why the submit delay here is a plain sleep rather than a timer. They share this function
/// so a transcript can never reach a pane by two subtly different routes.
#[tracing::instrument(level = "debug", skip(shared))]
pub fn stop_and_deliver(shared: &Shared, pane_id: &str, uid: &str) -> Result<Delivered, String> {
    let transcript = shared.dictation.stop(pane_id)?;
    let text = sanitize_for_pane(&transcript.text);
    // Not "type the transcript" but "correct the pane into the transcript". With no live
    // typing the pane holds nothing, the edit is the whole text with no backspaces, and
    // this is exactly the delivery it always was. With live typing the pane already holds
    // most of it, and only the tail the recognizer got wrong is taken back.
    let mut typed = Typed::already(&transcript.typed_live);
    let edit = typed.update(&text);
    if text.is_empty() {
        // Nothing was said — but something may already have been typed, and leaving a
        // stray hallucinated word in someone's prompt is worse than the empty result.
        let _ = apply(shared, uid, &edit);
        return Err("no speech in the recording".to_string());
    }
    let want_submit = shared.dictation.submit_after_insert();
    // Paired with the `dictation transcribed` line: the transcript's length against what
    // was handed to the pty is the difference between "the model lost it" and "the pane
    // did". Sanitizing only ever replaces characters, so these should match.
    tracing::info!(
        pane = %pane_id,
        transcript_chars = transcript.text.len(),
        delivered_chars = text.len(),
        typed_live_chars = transcript.typed_live.len(),
        backspaces = edit.backspaces,
        submit = want_submit,
        "delivering dictation to the pane"
    );
    // The recording is already consumed by this point, so a failed write means the user's
    // speech is simply gone. Saying so is the only useful thing left to do — reporting a
    // successful delivery would leave them looking for words that were never typed.
    // `paste`, not `write`. A minute of speech is well past the tty's 1024-byte input
    // queue, so the pane's program receives it in several reads no matter what we do, and
    // a TUI is entitled to treat each read as a separate event. This is not theoretical:
    // a 1277-character dictation arrived as its last 254 characters, the first 1023 gone,
    // with every log line on the way reporting success. Bracketing (when the program asked
    // for it) makes the split invisible to the reader.
    apply(shared, uid, &edit)?;
    let mut submitted = false;
    if want_submit {
        // A separate, later write — exactly as `/panes/{id}/input` does it — so a
        // bracketed-paste TUI reads the Enter as a keypress and not as pasted content.
        std::thread::sleep(Duration::from_millis(SUBMIT_DELAY_MS));
        submitted = shared.sessions.write(uid, "\r").is_ok();
    }
    Ok(Delivered {
        text,
        backend: transcript.backend,
        submitted,
        kept: transcript.kept,
    })
}

/// Make a transcript safe to write into a live terminal.
///
/// Whisper transcribes what it hears, and what it hears is arbitrary sound in a room. A
/// transcript is therefore untrusted input on its way to a shell: a stray control
/// character could execute a line the user never finished dictating, and an embedded
/// newline could submit it. Both are flattened to spaces — the human presses Enter.
#[tracing::instrument(level = "debug", ret)]
pub fn sanitize_for_pane(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_space = false;
    for c in text.chars() {
        let c = if c.is_control() || c == '\u{7f}' {
            ' '
        } else {
            c
        };
        if c == ' ' {
            if !last_space && !out.is_empty() {
                out.push(' ');
            }
            last_space = true;
        } else {
            out.push(c);
            last_space = false;
        }
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_settings(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "hp-dictation-svc-{}-{tag}.json",
            std::process::id()
        ))
    }

    #[test]
    fn a_fresh_service_is_recording_nothing() {
        let d = DictationService::new(temp_settings("fresh"));
        assert!(d.recording_panes().is_empty());
        assert!(!d.is_recording("p1"));
        assert!(
            !d.submit_after_insert(),
            "dictation never auto-submits by default"
        );
    }

    #[test]
    fn status_names_both_halves_even_with_nothing_installed() {
        let d = DictationService::new(temp_settings("status"));
        let s = d.status();
        assert!(!s.recorder.is_empty());
        assert!(!s.transcriber.is_empty());
        assert!(s.recording_panes.is_empty());
        assert!(
            s.kept_in.ends_with("dictation"),
            "recordings should be kept somewhere nameable: {}",
            s.kept_in
        );
    }

    #[test]
    fn reload_picks_up_an_edited_settings_file() {
        let p = temp_settings("reload");
        let _ = std::fs::remove_file(&p);
        let d = DictationService::new(p.clone());
        assert!(!d.submit_after_insert());

        stt::save(
            &p,
            &SttSettings {
                submit: true,
                ..Default::default()
            },
        )
        .unwrap();
        d.reload();
        assert!(d.submit_after_insert());
        let _ = std::fs::remove_file(&p);
    }

    // ---- sanitize_for_pane ----

    #[test]
    fn a_newline_in_a_transcript_never_submits_the_line() {
        // The whole risk of dictation into a shell: a CR is Enter.
        assert_eq!(sanitize_for_pane("rm -rf /\nyes"), "rm -rf / yes");
        assert_eq!(sanitize_for_pane("a\rb"), "a b");
    }

    #[test]
    fn control_characters_are_flattened_not_passed_through() {
        assert_eq!(sanitize_for_pane("hi\u{1b}[2Jthere"), "hi [2Jthere");
        assert_eq!(sanitize_for_pane("tab\there"), "tab here");
        assert_eq!(sanitize_for_pane("del\u{7f}x"), "del x");
    }

    #[test]
    fn ordinary_prose_is_left_exactly_as_dictated() {
        assert_eq!(
            sanitize_for_pane("open the file and run the tests"),
            "open the file and run the tests"
        );
    }

    #[test]
    fn leading_and_repeated_whitespace_collapses() {
        assert_eq!(sanitize_for_pane("   two    words  "), "two words");
        assert_eq!(sanitize_for_pane(""), "");
    }

    #[test]
    fn non_ascii_speech_survives() {
        // Whisper transcribes many languages; nothing here is ASCII-only.
        assert_eq!(sanitize_for_pane("café ☕ 日本語"), "café ☕ 日本語");
    }
}
