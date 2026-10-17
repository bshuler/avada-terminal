# Attention glow: when panes and tabs glow, and when they stop

**Source:** Bert, direct direction 2026-10-16, plus his answers to five clarifying questions the
same day. This file is the authoritative statement of *how the glow is supposed to behave*. Code,
tests and other docs reconcile to it, not the other way round. Change it only on Bert's word,
and record the change in §7.

---

## 1. The rule in one paragraph

A glow means **"something stopped and you haven't looked at it yet."** A pane glows when the AI
in it stops working. A tab glows when any pane on it is glowing. The glow ends when **Bert has
seen the pane**, or when **the manager or orchestrator has looked at it and decided Bert does not
need to**. Once it ends, it stays off until that AI stops *again*. A glow never means "this pane
is idle". An idle pane that has been seen is dark, however long it stays idle.

## 2. When a pane starts to glow

1. **Trigger:** the AI's own "turn finished" signal (for Claude, the `Stop` hook event, and the
   awaiting-input and error states the hooks report). A pane starts glowing as soon as that
   signal arrives. It does not wait out a timer.
2. **Fallback:** for an AI tool that sends no stop signal, today's quiet timer stands in. No output
   for the configured idle-alert seconds counts as a stop. The timer counts as **one** stop, the
   same as a signal: the pane does not glow again until it has produced output and gone quiet
   once more.
3. **Which panes:** **every AI pane**. That includes Bert's own agents, orchestrators, builders
   and other workers that report to the manager, not only the panes that answer to Bert. A plain
   shell with no AI in it never glows.
   - *Interpretation, confirm with Bert:* a Claude **subagent** pane (a Task sub-run shown inside
     its parent) is part of its parent's turn. It does not glow on its own; the parent glows when
     the parent's turn ends.
4. **Already watching:** if the pane is **already seen** at the moment it stops (see §3: Avada is
   the front app and the pane is visible on the active tab), it **does not start glowing**. Bert
   watched it stop. If Bert has stepped away, Avada will not be the front app, so the pane glows.
5. **Each stop is a new event.** A pane that was seen, then worked again, then stopped again,
   glows again. A pane that stops several times before Bert looks just keeps glowing; it does not
   get "more" glow.
6. The idle-alert setting (on/off, style) still governs: with it off, nothing glows.

## 3. When a pane stops glowing

The glow clears on **either** of these, whichever comes first:

1. **Bert has seen it.** A pane counts as seen when all of these hold continuously for
   **about 2 seconds**:
   - Avada is the frontmost app and its window is not minimised;
   - the pane's tab is the active tab in that window;
   - the pane is visible on that tab (not hidden behind another zoomed pane, not collapsed).

   No click or keystroke is needed. Flicking past a tab in under 2 seconds does **not** count.
   Clicking or typing in the pane also counts as seen, at once.
2. **The manager or orchestrator cleared it.** The manager (or an orchestrator, for its own
   workers) may look at a stopped pane, decide it does not need Bert, and clear the glow. The
   pane then shows a small, visible **"checked by manager"** note (who cleared it, and when), so
   Bert can tell a pane that was handled from one that never asked. The note goes away the next
   time that pane starts working.

Nothing else clears a glow. Time passing does not, and output from *other* panes does not.

## 4. Tabs

1. A tab glows while **any** pane on it is glowing, and stops the moment none is.
2. A tab has no glow state of its own. It is always derived from its panes, so it can never get
   out of step with them.
3. Looking at a tab clears only the panes on it that meet the §3.1 rule. A pane hidden on that tab
   (behind a zoomed pane, say) keeps glowing, so the tab keeps glowing too.

### 4.4 Off-screen tabs: the overflow arrows

When there are more tabs than fit, the strip scrolls and shows a left/right overflow arrow (‹ ›)
for the tabs hidden past each edge. A glowing tab scrolled off-screen would otherwise hide its
glow behind that arrow.

1. When a glowing tab is scrolled off one side of the strip, the overflow arrow on **that** side
   glows and pulses with the **same** gold breath as the chip — the identical colour and
   animation, so the arrow reads as "there is a glow this way".
2. If glowing tabs are hidden off **both** sides, **both** arrows glow.
3. The arrow has **no glow state of its own**. It is derived from the hidden tabs' flags exactly
   as a tab is derived from its panes (§4.2), so it can never get out of step with them.
4. The arrow stops glowing the moment the tab scrolls back into view (its chip then shows the
   glow directly) or stops needing attention. A tab even partially in view is "in view": its chip
   is visible, so its arrow stays dark.
5. The front window's active tab is always kept in view, and §2.4 keeps a front-visible pane from
   glowing at all, so the arrow is only ever a proxy for a genuinely hidden, still-unseen glow.

## 5. Restart

Avada **remembers unseen glows** across a restart, whether an upgrade, a crash, or a reboot. A pane that was
glowing and unseen when Avada went down glows again when it comes back. A pane that had been seen
stays dark. A restart never makes every pane glow, and it never wipes out glows Bert has not
dealt with.

## 6. One state, every surface

"Needs Bert's attention" is **one** per-pane flag, and every surface reads that same flag:

- the pane's glowing frame;
- the tab chip's glow;
- the tab-strip overflow arrow, when the glowing tab is scrolled off-screen (§4.4);
- the bottom drawer's attention cards;
- the status-tray / menu-bar count.

When any surface clears it (Bert looks at the pane, or clicks its attention card and then looks at
the pane, or the manager clears it), they all clear together. No surface keeps its own timer or
its own idea of "waiting".

## 7. How this differs from the code as of 0.2.48, and change log

As of 0.2.48 (`crates/app/src/paneview.rs`, `drive_tab_attention` and the pane idle glow):

- The glow is **"quiet for N seconds"**, not "stopped and not yet seen". It never clears when Bert
  looks, so a pane glows forever until its agent prints again. **Fix:** §2, §3.
- The tab glow re-derives "quiet for N seconds" itself instead of reading the panes' flag.
  **Fix:** §4, §6.
- Panes that report to the manager (orchestrator, builder) never glow (`answers_to_human`).
  **Fix:** §2.3. They glow, and the manager may clear them (§3.2).
- Nothing is persisted. **Fix:** §5.
- There is no way for the manager to clear a glow. **Fix:** §3.2.

| Date | Change | By |
|---|---|---|
| 2026-10-16 | Written from Bert's direction and his answers on "seen", stop signal, which panes, already-watching, and restart | Bert |
| 2026-10-16 | Bert ruling 2026-10-16: helpers glow only on error. A Claude helper / sub-agent pane does not glow when it simply stops; it glows only if it hits an error. The parent pane still glows when its whole turn ends. Settles the §2.3 interpretation. | Bert |
| 2026-10-16 | §4.4 added: when a glowing tab is scrolled off the overflowing strip, the overflow arrow on that side glows and pulses with the same gold breath as the chip (both arrows if hidden glows on both sides). The arrow has no state of its own — derived from the hidden tabs' flags, stops when the tab scrolls into view or stops glowing. | Bert direction, relayed |
