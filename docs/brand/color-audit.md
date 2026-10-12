# Colour audit — every colour the app paints, traced to the brand (0.2.41)

Source of truth: the brand colour system in [`design-requirements.md`](design-requirements.md)
§1 — Void Black `#000000`, Spell Green `#39FF14` (active / success), Arcane Purple `#7B2CBF`
(AI / magic), Gold `#D4AF37` (subtle outlines, never dominant), Soft White `#F5F5F5`,
Dim `#A0A0A0`.

Trigger (2026-10-12): the idle glow around a pane and the red record button "don't go with
the rest of the colours". Both were real: neither came from the brand.

## What was wrong

| Surface | Was | Why it clashed | Now |
|---|---|---|---|
| Pane idle glow (pulse / firefly / blink ring) | the pane's frame accent, from the **Muted** frame palette (Tailwind red/orange/blue/pink/teal/yellow) | the only saturated non-brand hues on a black/green/purple UI | Arcane Purple, lifted (`Theme.accent2.brighter(0.35)`) so a 2 px ring reads on black — the AI energy, since the glow means "this agent has gone quiet" |
| Dictation record button | `Theme.danger` red fill, black ink | recording is not an error; red read as an alarm | Arcane Purple fill, Soft White ink |
| Focused pane border, header tint, title dot | Muted palette | same as the glow | Avada frame palette (below) |
| Project colours (sidebar dot, pane frames) and the recolor swatches | Muted palette, hashed in core `PROJECT_COLORS` | same | Avada frame palette |
| Light ink on red (Remove, trash hover, window close hover) | `#ffffff` | BR-05: pure white is not a brand colour | Soft White `#f5f5f5` |

## The Avada frame palette (new default, `FRAME_PALETTES[0]`)

Eight per-pane / per-project colours, all brand colours or lifts of one so they stay
distinct from each other on black:

| Slot | Name | Hex | Derivation |
|---|---|---|---|
| 0 | Spell Green | `#39ff14` | brand |
| 1 | Gold | `#d4af37` | brand (a thin frame line — "subtle outline" is exactly its job) |
| 2 | Mint | `#5fe0a0` | the existing `link` token, a cool green |
| 3 | Amethyst | `#9d4edd` | Arcane Purple, lifted |
| 4 | Arcane Purple | `#7b2cbf` | brand |
| 5 | Lavender | `#c77dff` | Arcane Purple, lifted further |
| 6 | Leaf | `#3ec46d` | the existing `ok` token |
| 7 | Soft White | `#f5f5f5` | brand |

The old palettes are still selectable in Preferences (Muted is now index 1, Vivid 2,
Neon 3, Grayscale 4). A stored `framePalette: 0` therefore means Avada.

**Migration.** Colours saved before 0.2.41 (`projects.json`, `last-workspace.json`,
control-API recolors) are Muted hexes. While the Avada palette is active, any colour that
exactly equals a Muted slot loads as the Avada colour of the same slot
(`theme::legacy_to_brand`, applied in `state::parse_hex`), so every project keeps its
distinct slot; the next save writes the brand hex back. Under any other palette nothing
is rewritten.

## Every token, checked

`ui/theme.slint` and `UI_PALETTES[0]` (a test keeps them byte-identical):

| Token | Hex | Brand source |
|---|---|---|
| bg | `#000000` | Void Black |
| mantle / surface / surface2 / border | `#050507` `#0b0b10` `#15151d` `#26262f` | Void Black, lifted for layering |
| text | `#f5f5f5` | Soft White |
| subtext | `#a0a0a0` | Dim |
| faint | `#6e6e78` | Dim, darkened |
| accent | `#39ff14` | Spell Green |
| accent2 | `#7b2cbf` | Arcane Purple |
| ok | `#3ec46d` | Spell Green, softened for large fills |
| warn | `#d4af37` | Gold — a deliberate 2026-10-11 decision (8b2d629) that supersedes BR-03's amber default |
| link | `#5fe0a0` | Spell Green, cooled |
| **danger** | `#e5484d` | **not brand — the one exception** |
| scrim / veil | black at 55 % / 80 % | Void Black |

Terminal theme `TERMINAL_THEMES[0]` "Avada" was already brand (BR-06); its ANSI red
`#FF5C57` stays because programs print errors in red and expect it to look like red.

## Deliberate exceptions

1. **`danger` red `#e5484d`.** The brand has no colour for "this destroys something".
   It is used only for destructive actions (Remove project, close window, delete) and
   errors — never for state such as recording. Making it green or purple would make
   "delete" look like "go". Kept; flagged for Bert's call.
2. **Tool marks** (`core/src/tools/registry.rs` brand tints — Claude orange etc.): these
   are third-party logos identifying which tool runs in a pane, not Avada chrome.
3. **ANSI colours** inside the terminal (see above).
4. `terminal-widget/src/bin/demo.rs` is a dev demo, not shipped UI.

## Rule going forward

No colour literal outside `ui/theme.slint` / `src/theme.rs` except the eight swatch hues in
`sidebar.slint` (which equal `AVADA_FRAME`, pinned by a test) and Soft White ink on a
`danger` fill.
