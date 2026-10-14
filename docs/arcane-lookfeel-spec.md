# Avada Terminal — "Arcane" look & feel + auto-hiding chrome

Source: Bert's mockup screenshot of an "AvadaTerminal" window (the "Arcane" theme).
Goal: adopt this aesthetic in the real Avada terminal AND add two mouse-edge
auto-hide drawers. Bert's exact asks are the MUST list below.

## MUST (Bert's requirements, verbatim intent)

1. **Keep the left and right panels.** The mockup has NO side panels, but the
   extension panels we are building on the left and right MUST stay. The new
   look applies to the center terminal column only; it must not remove or cover
   the side rails. Drawers overlay the center column ONLY, never the rails.
2. **Top welcome banner → edge-reveal drawer.** The welcome header slides in
   when the mouse hits the TOP boundary; slides away when the mouse leaves.
   Scope of the drawer = header + tagline + metadata block (version/last
   update/uptime/host) + glyph art. The `# Initialize AI session…` init log is
   terminal CONTENT, not part of the drawer — at most a one-shot splash on
   session start, not auto-hidden.
3. **Bottom status area → notifications/attention drawer.** A strip of
   **notification + status cards**: items that need Bert's response or report
   live state. Slides in on the BOTTOM boundary; slides away on leave.
4. **Discoverability lip (both drawers).** Hidden-by-default must not hide that
   something is waiting. Each drawer keeps a thin always-visible lip (~2–4px);
   the bottom lip carries a count badge for needs-response items. A drawer stays
   open while hovered, and the bottom drawer stays open while it has an unread
   needs-response item.
5. **Apply the Arcane aesthetic** (palette, typography, glyphs, dividers) to the
   center terminal surface + titlebar. Rails untouched.
6. **Build/verify constraint.** Implement and verify in a dev build / second
   instance. Do NOT restart the Avada that hosts the live fleet — it picks up
   the change on Bert's next restart. The mockup image IS on disk:
   `docs/arcane-mockup.jpg`. It is canonical. Compare every change against it
   (sample colours from it, do not eyeball them); the prose below is only a summary.

## Look & feel from the mockup (reference, not pixel-exact)

- Background: near-black (~#0A0A0F / #0B0B12). Monospace throughout.
- Accents: gold/amber (titles, headers), magenta/purple (accent rules, glyphs,
  response headers), green (prompt arrow, OK status, success), muted
  grey-lavender (labels, body).
- Decorative `✦ / ✧ / ✸ / ➜` sparkle + arrow glyphs; thin horizontal rule
  dividers with a diamond/▪ terminator.
- **Titlebar:** traffic-light dots at left, "A" logo, centered gold
  `✦ AvadaTerminal ✦`, window controls right.

### Top banner (the "really cool welcome message")
- Large header `✦ AVADATERMINAL ✦` + subtitle "Premium Magical AI Terminal •
  Power. Precision. Purpose."
- Metadata block: version / last update / uptime / host (values in accent
  color), with a decorative compass-star glyph art to the right.
- An init log block: `# Initialize AI session…` → `➜ ~ avada ai init --profile
  arcane` → a list of steps with green ⊘/✓ "OK" markers → "AI core online" line.
- This whole block is what reveals on the TOP edge.

### Bottom status cards (repurposed as notifications/attention)
- Mockup shows: INTENT · AI MODEL · CONTEXT · SECURITY · FOCUS, each with an
  icon and a two-line label/value, separated by thin vertical rules.
- Repurpose the card row as: (a) notifications, (b) status items that need
  Bert's response. Keep the iconed, vertically-ruled card styling.

## Defaults (use these; don't bounce them back)

Per working-with-bert, implement with these unless you have a concrete reason;
raise to Bert via AskUserQuestion (4 options, recommended first) only if a
choice changes something he'd actually notice:

- Reveal: JS mousemove edge hotzone (~4px), ~150ms reveal delay, ~200ms slide
  in, hide ~300ms after the mouse leaves. Easing: ease-out.
- Focus: NEVER change terminal focus. Typing in the terminal while a drawer is
  revealed must keep working. Clicking a bottom card → focus that pane.
- Overlay the center column only; never clip or cover the side rails.

## Phase order

0. **Recon (no code):** locate the center-column renderer and the existing
   attention sources — supervisor ticket API, pane status, pending
   AskUserQuestions / notifications feed. Report findings first.
1. Theme tokens as CSS variables; apply to center column + titlebar; rails
   untouched.
2. One `EdgeDrawer` primitive; two instances (top banner, bottom cards).
3. Bottom drawer card schema `{id, pane, kind: notification|needs-response,
   title, detail}` wired to the real attention sources from phase 0.
4. Top banner from real values (app version, build date, uptime, hostname).

## Handoff
Owner: **repaint** pane (the look-and-feel agent). It knows the Avada/hyperpane
UI codebase. Bert wants an advisor ruling on the plan first, then repaint
implements. Do NOT rebuild/relaunch Avada as part of this.
