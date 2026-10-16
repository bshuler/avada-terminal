//! Ctrl+← / Ctrl+→ after a dictation: step through a model's rewrites of what was heard.
//!
//! Speech recognition gets homophones wrong ("right a test for the pain"), and the human is
//! the only one who knows which reading was meant. So after a dictation lands in a pane and
//! is not sent, the pane holds a short list: position 0 is the transcript exactly as
//! delivered, and every later position is a rewrite a model suggested. Ctrl+→ moves right,
//! asking for more when it runs off the end; Ctrl+← moves back, all the way to the
//! words as dictated. Each step swaps the pane's text with the same backspace-and-paste diff
//! live dictation uses, so it can only ever take back text it typed itself.
//!
//! The list lives until the message is sent (Enter) and no longer. Any other key ends it too:
//! once the human has edited the line, the diff no longer knows what the pane holds, and a
//! backspace counted against an edited line would eat the human's own words.
//!
//! Who suggests is a choice, made with Ctrl+↑/↓ while the list is open: a model on this Mac
//! first (Ollama), then Haiku, Sonnet, Opus and Fable. Every list starts on the local model,
//! and only the local model is asked before a keypress: the first batch is requested the
//! moment a dictation lands, so nothing leaves the machine unless the human picks a cloud
//! model and presses Ctrl+→. The cloud models run through the `claude` CLI the human
//! already has (`claude -p --model …`), so they use the existing login and need no key.
//!
//! Background threads only fetch; every write to the pane happens on the GUI thread
//! ([`step`] and [`poll`]), so a suggestion can never interleave with a keystroke.

use std::collections::{BTreeMap, HashMap};
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::control::dictation_service::{apply, sanitize_for_pane};
use crate::control::server::Shared;
use crate::stt::live::Typed;

/// Suggestions asked for per model call.
const BATCH: usize = 3;
/// When stepping right leaves fewer than this many unseen suggestions, ask for more.
const PREFETCH_WHEN_LEFT: usize = 1;
/// A model call that has not answered by now is abandoned.
const ASK_TIMEOUT: Duration = Duration::from_secs(60);
/// The list stops growing here; a human does not page through more than this.
const MAX_ITEMS: usize = 40;
/// Where the local model is served (`ollama serve`'s default).
const OLLAMA: &str = "http://127.0.0.1:11434";
/// The local model. Picked by measuring it against Haiku on mis-heard sentences on an M4.
const LOCAL_MODEL: &str = "gemma3:4b";

/// Who writes the suggestions, in Ctrl+↓ order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Local,
    Haiku,
    Sonnet,
    Opus,
    Fable,
}

const PROVIDERS: [Provider; 5] = [
    Provider::Local,
    Provider::Haiku,
    Provider::Sonnet,
    Provider::Opus,
    Provider::Fable,
];

impl Provider {
    /// The name the toast shows.
    pub fn name(self) -> &'static str {
        match self {
            Provider::Local => "on this Mac",
            Provider::Haiku => "Haiku",
            Provider::Sonnet => "Sonnet",
            Provider::Opus => "Opus",
            Provider::Fable => "Fable",
        }
    }

    /// The `claude --model` value, for the cloud ones.
    fn claude_model(self) -> Option<&'static str> {
        match self {
            Provider::Local => None,
            Provider::Haiku => Some("haiku"),
            Provider::Sonnet => Some("sonnet"),
            Provider::Opus => Some("opus"),
            Provider::Fable => Some("fable"),
        }
    }
}

const SYSTEM_PROMPT: &str = "You correct speech-to-text transcripts. The user dictated a \
message, usually to an AI coding assistant, and the recognizer may have picked the wrong \
homophone or mis-heard a word (pane/pain, write/right, branch/brunch, their/there, \
cache/cash, commit/comment). Propose alternative readings of the WHOLE transcript that fix \
likely recognition errors, and punctuation where it helps. Keep the speaker's wording, \
meaning and length; do not answer, summarise or add anything. Each alternative must differ \
from the transcript and from every earlier suggestion; prefer the most likely fix first, and \
when the obvious fixes are used up, try less likely readings. Output only the alternatives, \
separated by a line containing exactly ===, with no numbering and no commentary.";

/// One pane's list.
#[derive(Debug)]
struct Carousel {
    /// `items[0]` is the transcript as delivered; the rest are suggestions, in arrival order.
    items: Vec<String>,
    /// Who wrote each item; `None` for `items[0]`.
    from: Vec<Option<Provider>>,
    /// Who the next request goes to (Ctrl+↑/↓).
    provider: Provider,
    /// The item the pane holds now.
    at: usize,
    /// What this carousel has typed into the pane, for the next diff.
    typed: Typed,
    /// The providers with a request in flight.
    asking: Vec<Provider>,
    /// The human pressed → at the end of the list: move as soon as more arrive.
    want_next: bool,
    /// Distinguishes this carousel from an earlier one on the same pane, so a late answer
    /// for a list that already ended is dropped instead of grafted onto the new one.
    generation: u64,
}

/// What a background model call hands back to the GUI thread.
struct Answer {
    uid: String,
    generation: u64,
    provider: Provider,
    result: Result<Vec<String>, String>,
}

fn carousels() -> &'static Mutex<HashMap<String, Carousel>> {
    static C: OnceLock<Mutex<HashMap<String, Carousel>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

fn answers() -> &'static Mutex<Vec<Answer>> {
    static A: OnceLock<Mutex<Vec<Answer>>> = OnceLock::new();
    A.get_or_init(|| Mutex::new(Vec::new()))
}

fn next_generation() -> u64 {
    static G: AtomicU64 = AtomicU64::new(1);
    G.fetch_add(1, Ordering::Relaxed)
}

/// A dictation of `text` was just typed into `uid` and not sent: start its list and ask
/// the local model for the first batch.
#[tracing::instrument(level = "debug")]
pub fn begin(uid: &str, text: &str) {
    if text.trim().is_empty() {
        return;
    }
    let generation = next_generation();
    let mut c = Carousel {
        items: vec![text.to_string()],
        from: vec![None],
        provider: Provider::Local,
        at: 0,
        typed: Typed::already(text),
        asking: Vec::new(),
        want_next: false,
        generation,
    };
    ask(uid, &mut c);
    carousels()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(uid.to_string(), c);
}

/// Drop `uid`'s list (a new dictation is starting, or the pane went away).
#[tracing::instrument(level = "debug")]
pub fn end(uid: &str) {
    carousels()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(uid);
}

/// A key the human typed into `uid` (Ctrl+←/→ never get here while a list is open). Enter
/// sends the message, which is what clears the list; any other key edits the line, after
/// which the list cannot safely rewrite it.
#[tracing::instrument(level = "debug")]
pub fn note_keystroke(uid: &str, _data: &str) {
    end(uid);
}

/// Whether `uid` has a list open (the key handler only claims Ctrl+←/→ when it does).
pub fn is_open(uid: &str) -> bool {
    carousels()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(uid)
}

/// Ctrl+→ (`right`) or Ctrl+← in `uid`. `None` when the pane has no list, so the key goes
/// to the pane as usual; otherwise the toast to show.
#[tracing::instrument(level = "debug", skip(shared))]
pub fn step(shared: &Shared, uid: &str, right: bool) -> Option<String> {
    let mut all = carousels().lock().unwrap_or_else(|e| e.into_inner());
    let c = all.get_mut(uid)?;
    if right {
        if c.at + 1 < c.items.len() {
            c.at += 1;
            c.want_next = false;
            if let Err(e) = show(shared, uid, c) {
                all.remove(uid);
                return Some(e);
            }
            if c.items.len() - 1 - c.at < PREFETCH_WHEN_LEFT {
                ask(uid, c);
            }
            return Some(label(c));
        }
        if c.items.len() >= MAX_ITEMS {
            return Some(format!("{} · no more suggestions", label(c)));
        }
        c.want_next = true;
        ask(uid, c);
        return Some(format!("{} · asking {}…", label(c), c.provider.name()));
    }
    c.want_next = false;
    if c.at == 0 {
        return Some(label(c));
    }
    c.at -= 1;
    if let Err(e) = show(shared, uid, c) {
        all.remove(uid);
        return Some(e);
    }
    Some(label(c))
}

/// Ctrl+↓ (`down`, towards Fable) or Ctrl+↑ (towards the local model) in `uid`: pick who
/// the next suggestions come from, and ask it for a batch. `None` when the pane has no list.
#[tracing::instrument(level = "debug")]
pub fn switch(uid: &str, down: bool) -> Option<String> {
    let mut all = carousels().lock().unwrap_or_else(|e| e.into_inner());
    let c = all.get_mut(uid)?;
    let i = PROVIDERS.iter().position(|p| *p == c.provider).unwrap_or(0);
    let j = if down {
        (i + 1).min(PROVIDERS.len() - 1)
    } else {
        i.saturating_sub(1)
    };
    c.provider = PROVIDERS[j];
    // Choosing a provider is the human asking for its suggestions: start fetching now, so
    // they are likely there by the time Ctrl+→ reaches the end of the list.
    if j != i {
        ask(uid, c);
    }
    let order: Vec<String> = PROVIDERS
        .iter()
        .map(|p| {
            if *p == c.provider {
                format!("[{}]", p.name())
            } else {
                p.name().to_string()
            }
        })
        .collect();
    Some(format!("Suggestions from: {}", order.join(" · ")))
}

/// Take in whatever a model has answered since the last call, and move any pane whose human
/// was waiting at the end of its list. Called from the GUI thread's tick; returns the toasts
/// to show, as `(uid, message)`.
pub fn poll(shared: &Shared) -> Vec<(String, String)> {
    let came: Vec<Answer> = std::mem::take(&mut *answers().lock().unwrap_or_else(|e| e.into_inner()));
    if came.is_empty() {
        return Vec::new();
    }
    let mut notices = Vec::new();
    let mut all = carousels().lock().unwrap_or_else(|e| e.into_inner());
    for a in came {
        let Some(c) = all.get_mut(&a.uid) else { continue };
        if c.generation != a.generation {
            continue;
        }
        c.asking.retain(|p| *p != a.provider);
        let added = match a.result {
            Ok(new) => merge(c, new, a.provider),
            Err(e) => {
                tracing::warn!(uid = %a.uid, provider = a.provider.name(), error = %e, "dictation revisions failed");
                if c.want_next && a.provider == c.provider {
                    c.want_next = false;
                    notices.push((a.uid.clone(), format!("{}: {e}", a.provider.name())));
                }
                continue;
            }
        };
        if !c.want_next || a.provider != c.provider {
            continue;
        }
        c.want_next = false;
        if added == 0 {
            notices.push((
                a.uid.clone(),
                format!("{} · {} has no other suggestions", label(c), a.provider.name()),
            ));
            continue;
        }
        c.at += 1;
        match show(shared, &a.uid, c) {
            Ok(()) => notices.push((a.uid.clone(), label(c))),
            Err(e) => {
                all.remove(&a.uid);
                notices.push((a.uid, e));
            }
        }
    }
    notices
}

/// Put `c.items[c.at]` in the pane.
fn show(shared: &Shared, uid: &str, c: &mut Carousel) -> Result<(), String> {
    let edit = c.typed.update(&c.items[c.at]);
    apply(shared, uid, &edit)
}

/// "2/4 · Haiku", or "1/4 · as dictated" at the start.
fn label(c: &Carousel) -> String {
    let whose = c.from[c.at].map(Provider::name).unwrap_or("as dictated");
    format!("{}/{} · {whose}", c.at + 1, c.items.len())
}

/// Append the suggestions in `new` that are not already listed; how many were added.
fn merge(c: &mut Carousel, new: Vec<String>, from: Provider) -> usize {
    let before = c.items.len();
    for s in new {
        if c.items.len() >= MAX_ITEMS {
            break;
        }
        if !c.items.iter().any(|i| i == &s) {
            c.items.push(s);
            c.from.push(Some(from));
        }
    }
    c.items.len() - before
}

/// Ask the selected provider for another batch for `c`, unless it is already asked.
fn ask(uid: &str, c: &mut Carousel) {
    let provider = c.provider;
    if c.asking.contains(&provider) || c.items.len() >= MAX_ITEMS {
        return;
    }
    c.asking.push(provider);
    let (uid, generation) = (uid.to_string(), c.generation);
    let original = c.items[0].clone();
    let prompt = build_prompt(&c.items);
    std::thread::spawn(move || {
        let out = match provider.claude_model() {
            Some(model) => run_claude(model, &prompt),
            None => run_local(&prompt),
        };
        let result = out.map(|out| parse_suggestions(&out, &original));
        answers()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Answer {
                uid,
                generation,
                provider,
                result,
            });
    });
}

/// The user turn: the transcript, and every suggestion already made so the new ones differ.
fn build_prompt(items: &[String]) -> String {
    let mut p = format!("Transcript:\n{}\n", items[0]);
    if items.len() > 1 {
        p.push_str("\nAlready suggested (give different ones):\n");
        for s in &items[1..] {
            p.push_str(s);
            p.push_str("\n===\n");
        }
    }
    p.push_str(&format!("\nGive {BATCH} new alternatives."));
    p
}

/// A model's answer as clean, pane-safe suggestions. Anything that cannot be a rewrite of the
/// transcript (empty, identical, or far longer than what was said) is dropped: the text goes
/// straight into someone's prompt.
fn parse_suggestions(out: &str, original: &str) -> Vec<String> {
    let longest = original.chars().count() * 2 + 40;
    let mut found: Vec<String> = Vec::new();
    // Small local models often put the separator inline ("a === b") or forget it and give
    // one reading per line; a one-line transcript makes either readable.
    let chunks: Vec<&str> = if out.contains("===") {
        out.split("===").collect()
    } else if !original.contains('\n') {
        out.lines().collect()
    } else {
        vec![out]
    };
    for chunk in chunks {
        let chunk = chunk.trim_matches('=');
        let s = sanitize_for_pane(chunk.trim());
        let s = s.trim().to_string();
        if s.is_empty() || s == original || s.chars().count() > longest || found.contains(&s) {
            continue;
        }
        found.push(s);
    }
    found
}

/// The `claude` program to run: the real binary on PATH or in its usual places first, and the
/// Preferences → Tools override only when there is no such binary (an override is usually a
/// wrapper meant for interactive sessions).
fn claude_binary() -> Option<PathBuf> {
    let tool = crate::tools::registry::by_id("claude")?;
    if let Some(r) = crate::tools::detect::resolve(tool, &BTreeMap::new()) {
        return Some(r.path);
    }
    let path = crate::persistence::paths::user_data_dir().join("native-settings.json");
    let json = std::fs::read_to_string(path).unwrap_or_default();
    let overrides = crate::tools::detect::tool_paths_from_settings(&json);
    crate::tools::detect::resolve(tool, &overrides).map(|r| r.path)
}

/// One `claude -p --model <model>` call with `prompt` on stdin; its stdout.
///
/// No tools, no session file, no MCP servers and none of the human's settings, so none of
/// their hooks fire for it (a Stop hook would otherwise make some pane glow). Run from the
/// temp directory so no project's CLAUDE.md is read. The Anthropic environment overrides a
/// shell may carry are removed, the same ones `claude_auto` removes, so the call goes to the
/// human's own login.
fn run_claude(model: &str, prompt: &str) -> Result<String, String> {
    let bin = claude_binary().ok_or("the claude CLI was not found")?;
    let mut child = Command::new(&bin)
        .args([
            "-p",
            "--model",
            model,
            "--tools",
            "",
            "--no-session-persistence",
            "--setting-sources",
            "",
            "--strict-mcp-config",
            "--system-prompt",
            SYSTEM_PROMPT,
        ])
        .current_dir(std::env::temp_dir())
        .env_remove("ANTHROPIC_BASE_URL")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_MODEL")
        .env_remove("ANTHROPIC_SMALL_FAST_MODEL")
        .env_remove("ANTHROPIC_DEFAULT_HAIKU_MODEL")
        .env_remove("CLAUDE_CONFIG_DIR")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", bin.display()))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(prompt.as_bytes())
            .map_err(|e| format!("could not ask {model}: {e}"))?;
    }
    // Read on a thread so a full pipe can never stall the wait below.
    let mut stdout = child.stdout.take().ok_or("no output pipe")?;
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut stdout, &mut s);
        s
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if started.elapsed() > ASK_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("no answer within a minute".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(200)),
            Err(e) => return Err(format!("lost the claude process: {e}")),
        }
    };
    let out = reader.join().unwrap_or_default();
    if !status.success() {
        return Err(format!("claude exited with {status}"));
    }
    Ok(out)
}

/// One request to the local model through Ollama; its text.
///
/// A fresh single-threaded runtime per call: this runs on a worker thread that has none, and
/// a call is seconds long, so the runtime's cost is noise. The model is kept loaded for a
/// while afterwards, because the next Ctrl+→ is likely to follow soon.
fn run_local(prompt: &str) -> Result<String, String> {
    let body = serde_json::json!({
        "model": LOCAL_MODEL,
        "stream": false,
        "keep_alive": "15m",
        "messages": [
            { "role": "system", "content": SYSTEM_PROMPT },
            { "role": "user", "content": prompt },
        ],
        "options": { "temperature": 0.7 },
    });
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("no runtime: {e}"))?;
    rt.block_on(async {
        let res = reqwest::Client::new()
            .post(format!("{OLLAMA}/api/chat"))
            .json(&body)
            .timeout(ASK_TIMEOUT)
            .send()
            .await
            .map_err(|e| {
                if e.is_connect() {
                    "the local model is not running (start Ollama)".to_string()
                } else {
                    e.to_string()
                }
            })?;
        let status = res.status();
        if status.as_u16() == 404 {
            return Err(format!("the local model is not installed (ollama pull {LOCAL_MODEL})"));
        }
        if !status.is_success() {
            return Err(format!("Ollama answered {status}"));
        }
        let v: serde_json::Value = res.json().await.map_err(|e| e.to_string())?;
        v.pointer("/message/content")
            .and_then(|c| c.as_str())
            .map(str::to_string)
            .ok_or_else(|| "Ollama sent no text".to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggestions_are_split_cleaned_and_filtered() {
        let out = "I want to write a test for the pane that glows\n===\n\
                   right a test for the pain that glows\n===\n\
                   Write a test for the pane\u{1b}[31m that glows.\n===\n\
                   I want to write a test for the pane that glows\n===\n\n";
        let got = parse_suggestions(out, "right a test for the pain that glows");
        assert_eq!(
            got,
            vec![
                "I want to write a test for the pane that glows".to_string(),
                "Write a test for the pane [31m that glows.".to_string(),
            ]
        );
    }

    #[test]
    fn inline_separators_and_bare_lines_both_split() {
        let t = "their going to merge the brunch";
        assert_eq!(
            parse_suggestions("=== they're going to merge the branch === there going ===", t),
            vec!["they're going to merge the branch", "there going"]
        );
        assert_eq!(
            parse_suggestions("they're going to merge the branch\n\nthere going\n", t),
            vec!["they're going to merge the branch", "there going"]
        );
    }

    #[test]
    fn a_runaway_answer_is_not_a_suggestion() {
        let essay = "word ".repeat(100);
        assert!(parse_suggestions(&essay, "a short line").is_empty());
    }

    #[test]
    fn the_prompt_lists_what_was_already_suggested() {
        let p = build_prompt(&["their going".into(), "they're going".into()]);
        assert!(p.starts_with("Transcript:\ntheir going\n"));
        assert!(p.contains("Already suggested"));
        assert!(p.contains("they're going\n==="));
        let first = build_prompt(&["their going".into()]);
        assert!(!first.contains("Already suggested"));
    }

    #[test]
    fn merging_keeps_order_and_skips_repeats() {
        let mut c = carousel(&["a", "b"]);
        assert_eq!(merge(&mut c, vec!["b".into(), "c".into(), "c".into()], Provider::Opus), 1);
        assert_eq!(c.items, vec!["a", "b", "c"]);
        assert_eq!(c.from.len(), 3);
        assert_eq!(c.from[2], Some(Provider::Opus));
    }

    #[test]
    fn a_keystroke_or_enter_closes_the_list() {
        let uid = "revisions-test-pane";
        carousels().lock().unwrap().insert(
            uid.into(),
            carousel(&["x"]),
        );
        assert!(is_open(uid));
        note_keystroke(uid, "\r");
        assert!(!is_open(uid));
    }

    #[test]
    fn labels_say_whose_words_the_pane_holds() {
        let mut c = carousel(&["x"]);
        merge(&mut c, vec!["y".into()], Provider::Haiku);
        merge(&mut c, vec!["z".into()], Provider::Local);
        assert_eq!(label(&c), "1/3 · as dictated");
        c.at = 1;
        assert_eq!(label(&c), "2/3 · Haiku");
        c.at = 2;
        assert_eq!(label(&c), "3/3 · on this Mac");
    }

    #[test]
    fn up_and_down_walk_the_providers_and_stop_at_the_ends() {
        let uid = "revisions-test-switch";
        carousels().lock().unwrap().insert(uid.into(), carousel(&["x"]));
        let now = |uid: &str| carousels().lock().unwrap()[uid].provider;
        assert_eq!(now(uid), Provider::Local);
        assert!(switch(uid, false).unwrap().contains("[on this Mac]"));
        assert_eq!(now(uid), Provider::Local);
        for want in [Provider::Haiku, Provider::Sonnet, Provider::Opus, Provider::Fable, Provider::Fable] {
            switch(uid, true);
            assert_eq!(now(uid), want);
        }
        assert!(switch(uid, false).unwrap().contains("[Opus]"));
        assert!(switch("revisions-no-such-pane", true).is_none());
        end(uid);
    }

    #[test]
    fn only_the_local_model_stays_on_this_machine() {
        assert_eq!(Provider::Local.claude_model(), None);
        for p in &PROVIDERS[1..] {
            assert!(p.claude_model().is_some());
        }
    }

    fn carousel(items: &[&str]) -> Carousel {
        Carousel {
            items: items.iter().map(|s| s.to_string()).collect(),
            from: std::iter::once(None)
                .chain(std::iter::repeat(Some(Provider::Haiku)))
                .take(items.len())
                .collect(),
            provider: Provider::Local,
            at: 0,
            typed: Typed::already(items[0]),
            asking: Vec::new(),
            want_next: false,
            generation: 0,
        }
    }
}
