//! "Needs Bert's attention": the one per-pane flag behind every glow surface
//! (`docs/attention-glow-spec.md`, the single authority for how it behaves).
//!
//! A glow means "something stopped and you haven't looked at it yet". [`Attention`] is that
//! flag plus the bookkeeping that decides when it turns on (a stop: the Claude `Stop` hook, an
//! awaiting-input / error liveness report, or — for a tool that sends neither — one quiet
//! period after work) and when it turns off (Bert saw the pane for about two seconds, clicked
//! or typed in it, or the manager cleared it). The pane frame, the tab chip, the drawer cards
//! and the status-tray count all read [`Attention::needs`]; none keeps its own timer.
//!
//! The logic is pure: the caller hands it one [`Tick`] of observations per frame, so the
//! rules are unit-tested here without a window or a pty.

use std::time::{Duration, Instant};

/// How long a pane must be continuously seen before its glow clears (§3.1 "about 2 seconds").
pub const SEEN_AFTER: Duration = Duration::from_secs(2);

/// Output this soon after a manager clear is the tail of the turn that was just cleared, not
/// the pane starting work again, so it does not take the "checked" note down.
const CHECKED_GRACE_MS: u64 = 2_000;

/// The session-file meta key a pane carries while it glows unseen (§5). Not under the
/// `origin.` prefix, so `pane_origin::extract` never mistakes it for provenance.
pub const META_KEY: &str = "avada.attention";
/// [`META_KEY`]'s only value.
pub const META_UNSEEN: &str = "unseen";

/// Who cleared a glow without Bert, and when (epoch ms) — the "checked by manager" note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    pub by: String,
    pub at_ms: u64,
}

/// One frame's observations of a pane, gathered by the caller.
#[derive(Debug, Clone, Copy)]
pub struct Tick {
    /// The idle-alert setting is on (§2.6). Off: nothing glows.
    pub enabled: bool,
    /// An AI is running in the pane (a tool pane, a title-sniffed agent, or a Claude helper).
    /// Plain shells never start a glow (§2.3).
    pub ai: bool,
    /// A Claude helper / sub-agent pane: only an error is a stop for it, never a plain stop
    /// (Bert ruling 2026-10-16, §7). Its parent glows when the parent's whole turn ends.
    pub errors_only: bool,
    /// The pane reports turn ends through the Claude `Stop` hook (it has a session marker),
    /// so the quiet timer is not consulted even before its first `_turn.json` (§2.4).
    pub has_hook: bool,
    /// Bert can see the pane right now: Avada is the front window, the pane's tab is active,
    /// and the pane is laid out and not behind a zoomed sibling (§3.1).
    pub watching: bool,
    /// The newest `endedAt` (epoch ms) the Claude `Stop` hook wrote for this pane, if any.
    pub turn_end_ms: Option<u64>,
    /// The liveness code (`State::liveness_ui`): 1 busy, 2 awaiting input, 4 error.
    pub liveness: i32,
    /// When the pane last produced output (epoch ms).
    pub last_output_ms: Option<u64>,
    /// The fallback quiet period, ms (the idle-alert seconds).
    pub quiet_ms: u64,
    pub now_ms: u64,
    pub now: Instant,
}

/// The per-pane attention state. `Default` is a fresh pane: dark, nothing seen yet.
#[derive(Debug, Clone, Default)]
pub struct Attention {
    /// THE flag. Every glow surface reads this and nothing else.
    pub needs: bool,
    /// The manager's note, shown on the pane until it next starts working (§3.2).
    pub checked: Option<Checked>,
    /// Bumped on every stop that lights the glow, so a drawer card for a new stop is a new card.
    pub stops: u64,
    /// A helper (subagent) pane whose transcript ended in an error. The pane is held open
    /// while that error glows instead of closing the moment the helper finishes.
    pub helper_error: bool,
    /// The finished helper's transcript has been checked for an error (once).
    pub helper_judged: bool,
    /// When the current unbroken "Bert can see it" stretch began.
    seen_since: Option<Instant>,
    /// When this state was created (epoch ms). Hook stops older than this happened before
    /// Avada knew the pane, so they are not new events.
    born_ms: Option<u64>,
    /// The newest hook `endedAt` already accounted for.
    turn_end_ms: Option<u64>,
    /// The pane has a `Stop` hook. From then on the hook alone says when a turn ends: the quiet
    /// timer would otherwise light a pane Bert watched stop a few seconds later (§2.4).
    hooked: bool,
    /// The liveness code seen last frame, so only a transition into 2/4 is a stop.
    last_liveness: i32,
    /// The pane has worked since Avada started it: reported busy or got input. The quiet timer
    /// arms only after that, so a restored pane drawing its prompt is not a stop (§5).
    worked: bool,
    /// Output up to here has already been counted as a stop by the quiet timer.
    quiet_mark: Option<u64>,
}

impl Attention {
    /// The state a pane restored from the session file starts with: glowing if it was
    /// glowing unseen when Avada went down (§5).
    pub fn restored(unseen: bool) -> Self {
        Self {
            needs: unseen,
            ..Self::default()
        }
    }

    /// Advance one frame. Returns whether anything a surface shows changed.
    pub fn step(&mut self, t: &Tick) -> bool {
        let before = (self.needs, self.checked.clone());
        let born = *self.born_ms.get_or_insert(t.now_ms);

        // ---- did the AI stop? (each source is an edge, so a stop is counted once) ----
        let mut stopped = false;
        self.hooked |= t.has_hook;
        if let Some(end) = t.turn_end_ms {
            self.hooked = true;
            if end > self.turn_end_ms.unwrap_or(born) {
                stopped = true;
            }
            self.turn_end_ms = Some(end.max(self.turn_end_ms.unwrap_or(0)));
        }
        if t.liveness != self.last_liveness {
            if t.liveness == 4 || (t.liveness == 2 && !t.errors_only) {
                stopped = true;
            }
            if t.liveness == 1 {
                self.worked = true;
                self.checked = None; // it started working again (§3.2)
            }
            self.last_liveness = t.liveness;
        }
        if t.errors_only {
            // A helper's own turn end and quiet spells are part of its parent's turn.
            stopped = stopped && t.liveness == 4;
        } else if !self.hooked {
            if let Some(out) = t.last_output_ms {
                let fresh = self.quiet_mark.is_none_or(|m| out > m);
                if !self.worked {
                    // Not armed: whatever it prints before it has been asked to do anything
                    // (a restored pane redrawing, a tool's banner) is not a turn.
                    self.quiet_mark = Some(out);
                } else if fresh && t.now_ms.saturating_sub(out) >= t.quiet_ms {
                    stopped = true;
                    self.quiet_mark = Some(out);
                }
            }
        }
        // Output well after a manager clear means the pane is at work again.
        if let (Some(c), Some(out)) = (&self.checked, t.last_output_ms) {
            if out > c.at_ms + CHECKED_GRACE_MS {
                self.checked = None;
            }
        }

        if !t.enabled {
            // §2.6: with the setting off, nothing glows (and nothing is remembered).
            self.needs = false;
            self.seen_since = None;
            return before != (self.needs, self.checked.clone());
        }

        if stopped && t.ai {
            self.checked = None;
            // §2.4: a pane Bert is watching as it stops was seen stopping.
            if !t.watching && !self.needs {
                self.needs = true;
                self.stops += 1;
            }
        }

        // ---- has Bert seen it? (§3.1: continuously, about two seconds) ----
        if t.watching {
            let since = *self.seen_since.get_or_insert(t.now);
            if self.needs && t.now.duration_since(since) >= SEEN_AFTER {
                self.needs = false;
            }
        } else {
            self.seen_since = None;
        }
        before != (self.needs, self.checked.clone())
    }

    /// Bert clicked or typed in the pane: seen at once (§3.1). Input is also work, which arms
    /// the quiet timer for tools without a stop signal.
    pub fn touch(&mut self) {
        self.needs = false;
        self.worked = true;
    }

    /// Input reached the pane from somewhere other than Bert's keyboard (an agent, the control
    /// API): the pane is at work, but nobody looked at it.
    pub fn note_work(&mut self) {
        self.worked = true;
    }

    /// The manager (or an orchestrator) decided Bert does not need this pane. Clears the glow
    /// and leaves the "checked by" note. A pane that is not glowing has nothing to clear and is
    /// left as it is. Returns whether it was glowing.
    pub fn manager_clear(&mut self, by: &str, at_ms: u64) -> bool {
        if !self.needs {
            return false;
        }
        self.needs = false;
        self.checked = Some(Checked {
            by: by.to_string(),
            at_ms,
        });
        true
    }

    /// Whether a finished helper pane must stay open: it ended in an error that has not been
    /// seen or cleared yet (or not yet been looked at by [`Self::step`]).
    pub fn holds_helper(&self) -> bool {
        self.helper_error && (self.needs || self.last_liveness != 4)
    }

    /// The pane's note, as drawn on it: `checked by manager · 14:05`. Empty when there is none.
    pub fn checked_label(&self) -> String {
        match &self.checked {
            Some(c) => format!("checked by {} · {}", c.by, local_hhmm(c.at_ms)),
            None => String::new(),
        }
    }
}

/// What the Claude hooks say about one pane, read from disk by the app (throttled) and handed
/// to [`drive`]: the newest `Stop` hook `endedAt`, and whether the pane runs a hooked Claude.
#[derive(Debug, Clone, Copy, Default)]
pub struct HookFacts {
    pub turn_end_ms: Option<u64>,
    pub has_hook: bool,
}

/// Advance every pane's flag in one window by one frame. `front` is "Avada is the frontmost
/// app and this window is not minimised"; `touched` the panes Bert typed into since the last
/// frame. Returns whether any flag or note changed (the caller repaints and re-pumps cards).
pub fn drive(
    state: &mut crate::state::State,
    mgr: &avada_core::session_manager::SessionManager,
    front: bool,
    hooks: &std::collections::HashMap<String, HookFacts>,
    touched: &std::collections::HashSet<String>,
) -> bool {
    use avada_core::tools::PaneKind;
    let enabled = state.settings.idle_alert;
    let quiet_ms = state.settings.idle_alert_seconds as u64 * 1000;
    let now = Instant::now();
    let now_ms = crate::glow::now_epoch_ms();
    // (ai, errors_only, liveness) per pane, read before the mutable tab borrow below.
    let facts: Vec<Vec<(bool, bool, i32)>> = state
        .tabs
        .iter()
        .map(|t| {
            t.panes
                .iter()
                .map(|p| {
                    if p.kind == PaneKind::Subagent {
                        let l = if p.attention.helper_error { 4 } else { 0 };
                        return (true, true, l);
                    }
                    let ai = !matches!(state.effective_kind(p), PaneKind::Terminal)
                        || crate::glow::is_ai_pane(&p.shell_title);
                    (ai, false, state.liveness_ui(&p.uid))
                })
                .collect()
        })
        .collect();
    let active = state.active;
    let mut changed = false;
    for (ti, tab) in state.tabs.iter_mut().enumerate() {
        let hidden: Vec<bool> = (0..tab.panes.len()).map(|i| tab.is_hidden(i)).collect();
        for (i, p) in tab.panes.iter_mut().enumerate() {
            let Some(&(ai, errors_only, liveness)) = facts[ti].get(i) else {
                continue;
            };
            if touched.contains(&p.uid) {
                changed |= p.attention.needs;
                p.attention.touch();
            }
            let h = hooks.get(&p.uid).copied().unwrap_or_default();
            let tick = Tick {
                enabled,
                ai,
                errors_only,
                has_hook: h.has_hook,
                watching: front && ti == active && p.visible && !hidden[i],
                turn_end_ms: h.turn_end_ms,
                liveness,
                last_output_ms: if errors_only { None } else { mgr.last_output_at(&p.uid) },
                quiet_ms,
                now_ms,
                now,
            };
            changed |= p.attention.step(&tick);
        }
    }
    if changed {
        state.dirty = true;
    }
    changed
}

/// Which tabs glow: a tab glows while any pane on it does, the active tab included, and has
/// no glow state of its own (§4). With the idle-alert setting off, none does (§2.6).
pub fn tab_needs(state: &crate::state::State) -> Vec<bool> {
    let on = state.settings.idle_alert;
    state.tabs.iter().map(|t| on && t.panes.iter().any(|p| p.attention.needs)).collect()
}

/// How many panes in `state` glow (the status-tray count reads the sum over windows).
pub fn count(state: &crate::state::State) -> usize {
    state.tabs.iter().flat_map(|t| t.panes.iter()).filter(|p| p.attention.needs).count()
}

/// Panes Bert typed into since the last frame. Keystrokes are recorded where they are
/// written to the pty (`state::note_human_input`), which has no `&mut State`; the frame
/// drains this and [`Attention::touch`]es each pane.
fn touched() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static T: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    T.get_or_init(Default::default)
}

/// Bert typed into `uid`: seen at once (§3.1).
pub fn note_touched(uid: &str) {
    if let Ok(mut t) = touched().lock() {
        t.insert(uid.to_string());
    }
}

/// The panes typed into since the last call.
pub fn take_touched() -> std::collections::HashSet<String> {
    touched().lock().map(|mut t| std::mem::take(&mut *t)).unwrap_or_default()
}

/// `HH:MM` local time for an epoch-ms instant (a recent one: the local offset is today's).
fn local_hhmm(ms: u64) -> String {
    let now_s = crate::glow::now_epoch_ms() / 1000;
    let utc_today = now_s % 86_400;
    let offset = crate::state::local_secs_since_midnight() as i64 - utc_today as i64;
    let local = ((ms / 1000) as i64 + offset).rem_euclid(86_400);
    format!("{:02}:{:02}", local / 3600, (local / 60) % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test clock: one AI pane, observed frame by frame.
    struct Rig {
        a: Attention,
        t0: Instant,
        ms0: u64,
        at: u64,
        watching: bool,
        turn_end: Option<u64>,
        liveness: i32,
        out: Option<u64>,
        enabled: bool,
        ai: bool,
        errors_only: bool,
    }

    impl Rig {
        fn new() -> Self {
            let mut r = Rig {
                a: Attention::default(),
                t0: Instant::now(),
                ms0: 1_000_000,
                at: 0,
                watching: false,
                turn_end: None,
                liveness: 0,
                out: None,
                enabled: true,
                ai: true,
                errors_only: false,
            };
            r.step();
            r
        }
        fn now_ms(&self) -> u64 {
            self.ms0 + self.at
        }
        fn step(&mut self) -> bool {
            let t = Tick {
                enabled: self.enabled,
                ai: self.ai,
                errors_only: self.errors_only,
                has_hook: false,
                watching: self.watching,
                turn_end_ms: self.turn_end,
                liveness: self.liveness,
                last_output_ms: self.out,
                quiet_ms: 5_000,
                now_ms: self.now_ms(),
                now: self.t0 + Duration::from_millis(self.at),
            };
            self.a.step(&t)
        }
        /// Advance `ms` in 100 ms frames.
        fn run(&mut self, ms: u64) {
            for _ in 0..ms / 100 {
                self.at += 100;
                self.step();
            }
        }
        /// One frame on, the Stop hook reports a turn end (a stop stamped at the very instant
        /// the pane was first seen would read as one from before Avada knew it).
        fn hook_stop(&mut self) {
            self.at += 100;
            self.out = Some(self.now_ms());
            self.turn_end = Some(self.now_ms());
            self.step();
        }
    }

    #[test]
    fn a_stop_signal_lights_the_glow_at_once() {
        let mut r = Rig::new();
        r.run(1000);
        assert!(!r.a.needs, "a pane that has not stopped is dark");
        r.hook_stop();
        assert!(r.a.needs, "the Stop hook lights the glow on the same frame, no timer");
    }

    #[test]
    fn a_pane_seen_as_it_stops_does_not_glow() {
        let mut r = Rig::new();
        r.watching = true;
        r.run(500);
        r.hook_stop();
        assert!(!r.a.needs, "Bert watched it stop");
        r.watching = false;
        r.run(20_000);
        assert!(!r.a.needs, "and the quiet timer must not light it later for a hooked pane");
    }

    #[test]
    fn two_seconds_of_looking_clears_it_and_a_flick_does_not() {
        let mut r = Rig::new();
        r.hook_stop();
        r.watching = true;
        r.run(1500);
        assert!(r.a.needs, "under two seconds is not seen");
        r.watching = false;
        r.run(200);
        r.watching = true;
        r.run(1500);
        assert!(r.a.needs, "the two seconds must be unbroken");
        r.run(600);
        assert!(!r.a.needs, "two unbroken seconds clear it");
    }

    #[test]
    fn time_passing_never_clears_it() {
        let mut r = Rig::new();
        r.hook_stop();
        r.run(60_000);
        assert!(r.a.needs);
    }

    #[test]
    fn a_click_or_keystroke_clears_it_at_once() {
        let mut r = Rig::new();
        r.hook_stop();
        r.a.touch();
        assert!(!r.a.needs);
    }

    #[test]
    fn each_stop_is_a_new_event() {
        let mut r = Rig::new();
        r.hook_stop();
        let first = r.a.stops;
        r.a.touch();
        r.run(1000);
        r.hook_stop();
        assert!(r.a.needs, "stopping again after being seen glows again");
        assert_eq!(r.a.stops, first + 1);
        r.run(1000);
        r.hook_stop();
        assert_eq!(r.a.stops, first + 1, "stopping again while unseen adds no extra glow");
    }

    #[test]
    fn a_turn_that_ended_before_avada_knew_the_pane_is_not_a_stop() {
        let mut r = Rig::new();
        r.turn_end = Some(r.ms0 - 60_000);
        r.run(500);
        assert!(!r.a.needs, "an old _turn.json on startup must not light every pane");
    }

    #[test]
    fn awaiting_input_and_error_reports_are_stops() {
        for code in [2, 4] {
            let mut r = Rig::new();
            r.liveness = 1;
            r.run(200);
            r.liveness = code;
            r.step();
            assert!(r.a.needs, "liveness {code} is a stop");
        }
    }

    #[test]
    fn the_quiet_timer_stands_in_once_per_burst_of_output() {
        let mut r = Rig::new();
        r.a.note_work();
        r.out = Some(r.now_ms());
        r.run(4_000);
        assert!(!r.a.needs, "quiet for less than the threshold");
        r.run(1_500);
        assert!(r.a.needs, "quiet past the threshold counts as a stop");
        r.a.touch();
        r.run(30_000);
        assert!(!r.a.needs, "the timer counts as ONE stop, not one per period");
        r.out = Some(r.now_ms());
        r.run(6_000);
        assert!(r.a.needs, "new output and quiet again is a new stop");
    }

    #[test]
    fn a_restored_pane_drawing_its_prompt_is_not_a_stop() {
        let mut r = Rig::new();
        r.out = Some(r.now_ms());
        r.run(30_000);
        assert!(!r.a.needs, "the timer arms only once the pane has worked");
    }

    #[test]
    fn manager_clear_stamps_a_note_that_goes_when_work_starts() {
        let mut r = Rig::new();
        r.hook_stop();
        assert!(r.a.manager_clear("manager", r.now_ms()));
        assert!(!r.a.needs);
        assert!(r.a.checked_label().starts_with("checked by manager · "));
        r.run(10_000);
        assert!(r.a.checked.is_some(), "the note stays while the pane is quiet");
        r.out = Some(r.now_ms());
        r.step();
        assert!(r.a.checked.is_none(), "output means it started working: the note goes");
    }

    #[test]
    fn manager_clear_of_a_dark_pane_changes_nothing() {
        let mut r = Rig::new();
        assert!(!r.a.manager_clear("manager", r.now_ms()));
        assert!(r.a.checked.is_none());
    }

    #[test]
    fn with_the_setting_off_nothing_glows() {
        let mut r = Rig::new();
        r.hook_stop();
        r.enabled = false;
        r.step();
        assert!(!r.a.needs);
        r.run(500);
        r.hook_stop();
        assert!(!r.a.needs);
    }

    #[test]
    fn a_plain_shell_never_glows() {
        let mut r = Rig::new();
        r.ai = false;
        r.hook_stop();
        r.liveness = 2;
        r.step();
        assert!(!r.a.needs);
    }

    #[test]
    fn a_helper_pane_glows_only_on_error() {
        let mut r = Rig::new();
        r.errors_only = true;
        r.a.note_work();
        r.hook_stop();
        r.liveness = 2;
        r.step();
        r.run(30_000);
        assert!(!r.a.needs, "a helper that simply stops (or waits) does not glow");
        r.liveness = 4;
        r.step();
        assert!(r.a.needs, "a helper that hits an error glows");
    }

    fn mgr() -> avada_core::session_manager::SessionManager {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        avada_core::session_manager::SessionManager::new(tx)
    }

    /// §4: the tab chip is derived from its panes and nothing else — the ACTIVE tab glows too
    /// (0.2.48 never lit it), and it goes dark the moment its last glowing pane is seen.
    #[tokio::test]
    async fn a_tab_glows_while_any_pane_on_it_does_even_the_active_one() {
        let m = mgr();
        let mut st = crate::state::State::new(crate::theme::load_font(1.0));
        st.settings.idle_alert = true;
        st.add_pane(&m);
        assert_eq!(tab_needs(&st), vec![false]);
        st.tabs[0].panes[0].attention.needs = true;
        assert_eq!(tab_needs(&st), vec![true], "the active tab's chip glows too");
        st.settings.idle_alert = false;
        assert_eq!(tab_needs(&st), vec![false], "setting off: nothing glows");
        st.settings.idle_alert = true;
        st.tabs[0].panes[0].attention.needs = false;
        assert_eq!(tab_needs(&st), vec![false]);
    }

    /// The frame driver: a pane Bert typed into is seen at once, and a pane on a background tab
    /// is never "watched", whatever the window focus says.
    #[tokio::test]
    async fn drive_clears_a_typed_in_pane_and_never_watches_a_background_tab() {
        let m = mgr();
        let mut st = crate::state::State::new(crate::theme::load_font(1.0));
        st.settings.idle_alert = true;
        st.add_pane(&m);
        let uid = st.tabs[0].panes[0].uid.clone();
        st.tabs[0].panes[0].attention.needs = true;
        let hooks = std::collections::HashMap::new();
        let none = std::collections::HashSet::new();
        assert!(!drive(&mut st, &m, false, &hooks, &none));
        assert!(st.tabs[0].panes[0].attention.needs, "nobody looked");
        let typed: std::collections::HashSet<String> = [uid].into_iter().collect();
        assert!(drive(&mut st, &m, true, &hooks, &typed), "a change is reported");
        assert!(!st.tabs[0].panes[0].attention.needs, "typing in it is seeing it");
        assert_eq!(count(&st), 0);
    }

    #[test]
    fn an_unseen_glow_survives_a_restart_and_a_seen_one_stays_dark() {
        let mut r = Rig::new();
        r.a = Attention::restored(true);
        r.out = Some(r.now_ms()); // the restored pane redraws
        r.run(10_000);
        assert!(r.a.needs, "unseen before the restart, unseen after it");
        r.a = Attention::restored(false);
        r.run(10_000);
        assert!(!r.a.needs, "a restart never makes a pane glow");
    }
}
