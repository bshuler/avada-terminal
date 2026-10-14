# Report — bars configurability + tab-strip lurch

## Done
- **Bars pinned by default.** Top welcome bar and bottom status tray now stay in view. New
  preference "Auto-hide the top and bottom bars" (one toggle for both, off by default) brings
  back the old hover-and-slide behaviour. Pinned bars take their own space, so they never cover
  terminal text; auto-hide keeps the overlay behaviour.
- **Drag to resize.** A 6px handle on each bar's inner edge (gold line on hover). Limits 48–280px.
  Heights are saved in the normal settings blob on mouse release (`topBarH`, `bottomBarH`;
  old settings files load fine).
- **Tab lurch fixed.** The "hide subagents" chip is always in the strip at a fixed width; on a
  tab with no subagents it dims and ignores clicks (still drags the window). Tabs no longer
  move when switching. Auto-scroll logic reviewed: it already scrolls only the minimum; the
  B9 wordmark guard is untouched.

## Verified
- `cargo check` / `cargo test --bins` on hp2 (not the Mac). New tests in `src/uitest/bars.rs`:
  pinned default, drag + clamp + persist callback, auto-hide still slides, tab x-positions
  identical with/without subagents. Subagent-toggle test updated for the in-place chip.

## NEEDS HUMAN
- Eyeball it in the real app: needs the macOS GUI bundle, which only builds on the Mac, only on
  Bert's go (`-j4`). Not built; no Mac target dirs were created.
- Installing the new build restarts the app (steals focus) — Bert's call.
