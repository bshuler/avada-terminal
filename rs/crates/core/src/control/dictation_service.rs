//! The control layer's dictation surface: persisted [`SttSettings`], the per-pane
//! [`Dictation`] state machine, and the one rule that makes a transcript safe to type
//! into a live terminal.
//!
//! Unlike its sibling [`speech_service`](crate::control::speech_service), this has no
//! background loop. Dictation is entirely user-driven — a click starts it, a click stops
//! it — so there is nothing to poll and an install where nobody ever presses the mic does
//! no work at all.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
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
        let wav_dir = std::env::temp_dir().join(format!("avada-dictation-{}", std::process::id()));
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

/// A human key reached the pane during this dictation (see [`note_human_keystroke`]).
const TOUCHED: u8 = 1;
/// …and one of them was Enter: the line, live text and all, has been sent.
const SENT: u8 = 2;

/// Per-pane-uid record of a human typing into a pane while its dictation is still able to
/// edit it — from the mic going on until the final transcript has been delivered.
///
/// It exists because every edit dictation makes is a diff against what it *believes* the
/// pane holds ([`Typed`]), and that belief is only true while dictation is the pane's sole
/// writer. One keystroke from the human and it is false. The case that made this visible:
/// mic off, "Transcribing…", Enter pressed on the live text — the prompt is now empty, the
/// reconcile's backspaces have nothing to eat, and the whole transcript is pasted back
/// into the box a couple of seconds after it was sent. Once a human has typed, dictation
/// stops editing the pane: what is there is theirs.
fn watched() -> &'static Mutex<HashMap<String, Arc<AtomicU8>>> {
    static W: OnceLock<Mutex<HashMap<String, Arc<AtomicU8>>>> = OnceLock::new();
    W.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record that the human typed `data` into pane `uid`. Cheap when nothing is dictating.
///
/// Called by the GUI for every human keystroke and paste. The bytes are not kept; only
/// whether they held an Enter.
#[tracing::instrument(level = "debug", skip_all)]
pub fn note_human_keystroke(uid: &str, data: &str) {
    crate::control::revisions::note_keystroke(uid, data);
    let w = watched().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(flag) = w.get(uid) {
        let bits = if data.contains(['\r', '\n']) {
            TOUCHED | SENT
        } else {
            TOUCHED
        };
        flag.fetch_or(bits, Ordering::SeqCst);
    }
}

/// Whether the human pressed Enter in `uid` during its dictation, answered once: the GUI
/// stops a mic that is still on when this is true, since the message it was for has gone,
/// and must not stop it twice. The pane stays marked as typed-into.
#[tracing::instrument(level = "debug", ret)]
pub fn take_sent_during_dictation(uid: &str) -> bool {
    watched()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(uid)
        .is_some_and(|f| f.fetch_and(!SENT, Ordering::SeqCst) & SENT != 0)
}

/// Start watching `uid` for human keys, fresh, and return the flag.
fn watch(uid: &str) -> Arc<AtomicU8> {
    let flag = Arc::new(AtomicU8::new(0));
    watched()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(uid.to_string(), Arc::clone(&flag));
    flag
}

/// The flag for `uid`'s dictation; a fresh one when it was never watched.
fn watching(uid: &str) -> Arc<AtomicU8> {
    watched()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(uid)
        .cloned()
        .unwrap_or_default()
}

fn unwatch(uid: &str) {
    watched()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(uid);
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
    crate::control::revisions::end(uid); // a new dictation replaces the last one's list
    let touched = watch(uid);
    shared.dictation.start_live(
        pane_id,
        Some(LiveOptions {
            clean: clean_for_pane,
            // Failures are swallowed on purpose. A pane that has gone away, or a program
            // that has closed its input, must not take the recording down with it — the
            // words are still going into the WAV, and the transcript at the end is still
            // going to be delivered or reported.
            //
            // A pane the human has typed into is no longer dictation's to edit (see
            // [`watched`]): from then on the live words are only recorded, not typed.
            sink: Box::new(move |edit| {
                if touched.load(Ordering::SeqCst) != 0 {
                    return;
                }
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
pub(crate) fn apply(shared: &Shared, uid: &str, edit: &Edit) -> Result<(), String> {
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
    /// The human typed into the pane before the transcript was ready, so nothing was typed
    /// or corrected: the pane holds what they left in it. The transcript is still kept.
    pub taken_over: bool,
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
    // Taken before the stop and checked after the transcriber: the keystroke that matters
    // most is the Enter pressed while "Transcribing…" is on screen.
    let touched = watching(uid);
    let transcript = shared.dictation.stop(pane_id);
    unwatch(uid);
    deliver(shared, pane_id, uid, transcript?, touched.load(Ordering::SeqCst) != 0)
}

/// The second half of [`stop_and_deliver`]: correct the pane into `transcript`, unless the
/// human has `touched` it since the mic went on.
#[tracing::instrument(level = "debug", skip(shared, transcript))]
fn deliver(
    shared: &Shared,
    pane_id: &str,
    uid: &str,
    transcript: Transcript,
    touched: bool,
) -> Result<Delivered, String> {
    let text = sanitize_for_pane(&transcript.text);
    // Not "type the transcript" but "correct the pane into the transcript". With no live
    // typing the pane holds nothing, the edit is the whole text with no backspaces, and
    // this is exactly the delivery it always was. With live typing the pane already holds
    // most of it, and only the tail the recognizer got wrong is taken back.
    if touched {
        // Every backspace below would be counted against text the human may already have
        // sent or changed. Typing nothing is the only edit that cannot be wrong.
        tracing::info!(
            pane = %pane_id,
            transcript_chars = transcript.text.len(),
            typed_live_chars = transcript.typed_live.len(),
            "dictation not delivered: the human typed into the pane first"
        );
        return Ok(Delivered {
            text,
            backend: transcript.backend,
            submitted: false,
            taken_over: true,
            kept: transcript.kept,
        });
    }
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
    if !submitted {
        // Left in the pane for the human to send: Ctrl+←/→ can now swap it for a rewrite.
        crate::control::revisions::begin(uid, &text);
    }
    Ok(Delivered {
        text,
        backend: transcript.backend,
        submitted,
        taken_over: false,
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

    /// A recorder that *opens* — so the pane counts as recording — but writes no WAV and
    /// simply waits to be stopped. Because no capture file is ever created, cancelling the
    /// recording finds nothing to move into the real dictation archive
    /// (`crate::stt::archive::keep` bails before it can rename or prune), so these tests
    /// leave `~/Library/Application Support/avada/dictation` untouched and can never evict a
    /// real recording. Same `{wav}`-placeholder shape as the recorder in `stt::dictation`.
    #[cfg(unix)]
    fn idle_recorder() -> Vec<String> {
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "sleep 30".into(),
            "sh".into(),
            "{wav}".into(),
        ]
    }

    /// A `DictationService` whose settings name [`idle_recorder`], so `start` really opens a
    /// (silent) recording rather than reporting "no microphone".
    #[cfg(unix)]
    fn recording_service(tag: &str) -> DictationService {
        let p = temp_settings(tag);
        stt::save(
            &p,
            &SttSettings {
                record_template: Some(idle_recorder()),
                ..Default::default()
            },
        )
        .unwrap();
        DictationService::new(p)
    }

    /// A `Shared` with an empty session table: every `write`/`paste` to it fails with
    /// `NotFound`, which is exactly what the `apply` error-branch tests want to observe.
    fn shared_for(dir: &std::path::Path) -> Arc<Shared> {
        use crate::session_manager::SessionManager;
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        Shared::new(
            Arc::new(SessionManager::new(tx)),
            true,
            "0.0.0-test",
            dir.join("control.json"),
            dir.join("speech.json"),
        )
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

    // ---- recording lifecycle ----

    /// One strong test over `start`/`start_live`/`is_recording`/`recording_panes`/`cancel`:
    /// a real (silent) recording opens, is visible in exactly the right places, and cancel
    /// actually tears it down.
    #[cfg(unix)]
    #[test]
    fn starting_opens_a_recording_visible_everywhere_it_should_be() {
        let d = recording_service("start");

        // `start` names the recorder that actually opened — the custom one we configured.
        // A stubbed `Ok("")` / `Ok("xyzzy")` start would return the wrong string, and the
        // `== Denied` permission gate flipped to `!= Denied` would make this an `Err`
        // (prompt(Microphone) is `Undetermined`, never `Denied`, on this platform), so the
        // `.unwrap()` alone would then fail.
        assert_eq!(d.start("pane-A").unwrap(), "custom");

        assert!(
            d.is_recording("pane-A"),
            "the pane that started is recording"
        );
        assert!(
            !d.is_recording("pane-B"),
            "a pane that never started must read as not recording"
        );
        assert_eq!(d.recording_panes(), vec!["pane-A".to_string()]);

        // `cancel` really stops it; a stubbed no-op `cancel` would leave it recording.
        d.cancel("pane-A");
        assert!(!d.is_recording("pane-A"), "cancel stops the recording");
        assert!(d.recording_panes().is_empty());
    }

    /// `cancel_all` stops every recording, not just one. A no-op `cancel_all` leaves them up.
    #[cfg(unix)]
    #[test]
    fn cancel_all_stops_every_recording_at_once() {
        let d = recording_service("cancelall");
        d.start("pane-1").unwrap();
        d.start("pane-2").unwrap();
        assert_eq!(d.recording_panes().len(), 2, "both panes are recording");

        d.cancel_all();
        assert!(
            d.recording_panes().is_empty(),
            "cancel_all must clear every pane"
        );
    }

    /// `start_dictation` opens a recording through the control-server `Shared` state and
    /// reports the recorder that opened. A stubbed `Ok("")` / `Ok("xyzzy")` would not.
    #[cfg(unix)]
    #[test]
    fn start_dictation_opens_a_recording_through_shared_state() {
        let dir =
            std::env::temp_dir().join(format!("hp-dictation-svc-{}-startdict", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // `Shared::new` derives its `stt.json` from the speech-settings path's directory, so
        // the fake recorder has to be on disk there before `Shared::new` reads it.
        stt::save(
            &dir.join("stt.json"),
            &SttSettings {
                record_template: Some(idle_recorder()),
                ..Default::default()
            },
        )
        .unwrap();
        let shared = shared_for(&dir);

        assert_eq!(
            start_dictation(&shared, "pane-X", "uid-X").unwrap(),
            "custom"
        );
        assert!(shared.dictation.is_recording("pane-X"));
        shared.dictation.cancel_all();
    }

    // ---- a human typing during dictation ----

    fn transcript(text: &str, typed_live: &str) -> Transcript {
        Transcript {
            text: text.to_string(),
            backend: "test",
            kept: None,
            typed_live: typed_live.to_string(),
        }
    }

    /// The reported bug: live text sent with Enter while "Transcribing…" was up, then the
    /// reconcile pasted the whole transcript back into the empty prompt. With the pane
    /// typed into, delivery writes nothing — the session table here is empty, so any
    /// write at all would come back as an `Err`.
    #[test]
    fn a_pane_the_human_sent_is_never_retyped_into() {
        let dir = std::env::temp_dir().join(format!("hp-dictation-svc-{}-sent", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let shared = shared_for(&dir);
        let d = deliver(&shared, "p", "no-such-uid", transcript("Hello world.", "hello world"), true)
            .expect("nothing may be written to a pane the human has taken over");
        assert!(d.taken_over);
        assert!(!d.submitted);
        assert_eq!(d.text, "Hello world.", "the transcript is still reported and kept");
    }

    /// The same delivery to an untouched pane does write — the guard above is the only
    /// thing that stopped it.
    #[test]
    fn an_untouched_pane_still_gets_its_correction() {
        let dir = std::env::temp_dir().join(format!("hp-dictation-svc-{}-untouched", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let shared = shared_for(&dir);
        let err = deliver(&shared, "p", "no-such-uid", transcript("Hello world.", "hello world"), false)
            .err()
            .expect("an untouched pane is corrected, which here fails on the missing session");
        assert!(err.contains("correction"), "{err}");
    }

    #[test]
    fn enter_marks_the_pane_sent_once_and_other_keys_only_touch_it() {
        let uid = format!("watch-test-{}", std::process::id());
        note_human_keystroke(&uid, "\r");
        assert!(!take_sent_during_dictation(&uid), "an unwatched pane records nothing");

        let flag = watch(&uid);
        note_human_keystroke(&uid, "x");
        assert_eq!(flag.load(Ordering::SeqCst), TOUCHED);
        assert!(!take_sent_during_dictation(&uid));
        note_human_keystroke(&uid, "\r");
        assert!(take_sent_during_dictation(&uid));
        assert!(!take_sent_during_dictation(&uid), "answered once, so the mic stops once");
        assert_eq!(flag.load(Ordering::SeqCst), TOUCHED, "still typed-into");

        let fresh = watch(&uid);
        assert_eq!(fresh.load(Ordering::SeqCst), 0, "a new dictation starts clean");
        unwatch(&uid);
        assert_eq!(watching(&uid).load(Ordering::SeqCst), 0);
    }

    // ---- apply ----

    /// A backspace whose write is refused surfaces as a *correction* error — and never
    /// touches the paste branch when there is nothing to insert.
    ///
    /// Pins the `edit.backspaces > 0` guard with a positive count: `> 0` is true, so the
    /// write runs and fails; `==`/`<` would both skip it and return `Ok(())`, and the
    /// whole-body `Ok(())` mutant loses the `Err` entirely.
    #[test]
    fn apply_reports_a_refused_backspace_as_a_correction() {
        let dir =
            std::env::temp_dir().join(format!("hp-dictation-svc-{}-apply-bs", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let shared = shared_for(&dir);

        let err = apply(
            &shared,
            "no-such-uid",
            &Edit {
                backspaces: 2,
                insert: String::new(),
            },
        )
        .unwrap_err();
        assert!(err.contains("correction"), "wrong branch/message: {err}");
        assert!(
            !err.contains("transcript"),
            "an empty insert must not reach the paste branch: {err}"
        );
    }

    /// With nothing to do — no backspaces, no insert — `apply` is a clean `Ok(())`.
    ///
    /// This is the boundary the `>=`/`==` mutants get wrong: `0 > 0` is false so the write
    /// is skipped, but `0 >= 0` / `0 == 0` are true, so those mutants would run the write
    /// against the unknown uid and return `Err`.
    #[test]
    fn apply_with_nothing_to_do_succeeds() {
        let dir = std::env::temp_dir().join(format!(
            "hp-dictation-svc-{}-apply-noop",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let shared = shared_for(&dir);

        assert!(
            apply(
                &shared,
                "no-such-uid",
                &Edit {
                    backspaces: 0,
                    insert: String::new(),
                },
            )
            .is_ok(),
            "an empty edit writes nothing, so an unknown uid is never touched"
        );
    }

    /// An insert whose paste is refused surfaces as a *transcript* error.
    ///
    /// Pins the `!edit.insert.is_empty()` guard: a non-empty insert takes the paste branch
    /// and fails; deleting the `!` would flip the test so a non-empty insert is *skipped*
    /// and `apply` returns `Ok(())` instead.
    #[test]
    fn apply_reports_a_refused_insert_as_a_transcript() {
        let dir =
            std::env::temp_dir().join(format!("hp-dictation-svc-{}-apply-ins", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let shared = shared_for(&dir);

        let err = apply(
            &shared,
            "no-such-uid",
            &Edit {
                backspaces: 0,
                insert: "hello".to_string(),
            },
        )
        .unwrap_err();
        assert!(err.contains("transcript"), "wrong branch/message: {err}");
    }

    // ---- clean_for_pane ----

    /// The pane-shaping every dictated word gets: whisper timestamps stripped (from
    /// `clean_transcript`) and control bytes flattened (from `sanitize_for_pane`), in one
    /// call. An exact result, so a stubbed `String::new()` or constant `"xyzzy"` both fail.
    #[test]
    fn clean_for_pane_strips_timestamps_and_flattens_control_bytes() {
        assert_eq!(
            clean_for_pane("[00:00.000 --> 00:02.000]  open the\u{7f} file"),
            "open the file"
        );
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
