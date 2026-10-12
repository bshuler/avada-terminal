# Avada Terminal — Brand Design Requirements

Decomposition of `AvadaTerminal_Brand_Guidelines.pdf` (in this folder) into concrete,
per-surface requirements that cover the whole `avada-terminal` repo. This document only
**decomposes** the brand into requirements; it does not implement them.

- **Source of truth:** `docs/brand/AvadaTerminal_Brand_Guidelines.pdf`. Reference extracts
  beside it: `guideline-page-{1..5}.png` (raster) and `guideline-page-{1..5}.svg`
  (the page chrome — rules, wordmark text — is real vector art, ~56 paths on page 1; the
  ~56 paths are outlined text and rules, **not** the mark). The **blade-A mark itself is a
  painterly raster**, embedded as a JPEG on pages 1–3 (the page-3 app-icon render is the
  highest-fidelity version). It is therefore not clean line art and cannot be lifted as
  vector paths; the icon is built from that raster. `build/icon.svg` is accordingly a
  **raster-backed master** — it embeds the rendered PNG — read by the icon pipeline
  (see BR-12, BR-20). Since 0.2.40 the art is the chosen concept "Blade A, Sharpest"
  (concept 37 of the brand chat export): its old tile border masked out, the mark
  centred on an 824 px rounded black tile inside a transparent 1024 canvas (the macOS
  icon grid). Desktop and Android use that tile; iOS uses the art full-bleed and opaque.
- **Status:** requirements only. Each `BR-NN` names the file(s) to change and an acceptance
  check. Nothing here is built yet.
- **Brand gaps:** the brand book is silent on several values the repo must set. Each gap is
  resolved inline with a **Default:** line. All chosen defaults are reversible; none needed a
  decision escalation.

---

## 1. Brand facts (from the guidelines)

### Colour system
| Role | Name | Hex |
|---|---|---|
| Primary background / icon field | Void Black | `#000000` |
| Primary accent — active / success / left energy | Spell Green | `#39FF14` |
| Secondary accent — AI / magic / right energy | Arcane Purple | `#7B2CBF` |
| Accent — subtle outlines, wordmark highlight | Gold | `#D4AF37` |
| Text primary | Soft White | `#F5F5F5` |
| Text muted | Dim | `#A0A0A0` |

Brand CSS tokens: `--avada-void / --avada-green / --avada-purple / --avada-gold /
--avada-white / --avada-muted`.

**Dual-energy semantics (binding rule):** green = active / success / prompt (left energy);
purple = AI / magic states (right energy); gold = subtle premium accent, **never dominant**;
the green-left → purple-right direction must never be reversed.

### Typography
- **Display / wordmark:** Cinzel or Cinzel Decorative (or a custom sharp geometric face).
- **UI:** Inter or Geist.
- **Terminal / code:** JetBrains Mono or Geist Mono.
- **Recommended pairing:** Cinzel (wordmark) + Inter (UI) + JetBrains Mono (terminal).

### Terminal theme (the brand's own Hyper-compatible spec)
bg `#000000`, fg `#F5F5F5`, cursor `#39FF14`, green `#39FF14`, magenta/purple `#7B2CBF`,
yellow/gold `#D4AF37`, selection `#7B2CBF` @ 30 % opacity.

### Logo / icon
Sharp dual-blade "A" monogram, hard edges, green (left) → purple (right), legible small; the
icon unfolds into the A (and complementary V). **Launch/loading animation:** folded dual
blade unfolds into the full A, ~0.3–0.5 s smooth exponential ease. **Usage:** always on pure
black / very dark bg; clear space ≥ ¼ icon height; favicon = simplified sharp A at
16/32/48/**64**/128/256/512 px.

### Supporting system
Secondary marks: sharp dual chevron (prompt), simplified crystal (AI/living), horizontal
energy beam — all green→purple hard-edge. Taglines: "AI Terminal • Magical Intelligence" /
"Terminal. Magic. Evolved." Icon package deliverables: primary A monogram, wordmark lockups
(horizontal + stacked), favicon set, secondary UI icon set.

---

## 2. How colour works in the repo today (so the requirements are accurate)

- **Single source for UI colour:** `rs/crates/app/ui/theme.slint` — every UI colour is an
  `in-out property <color>` here (currently Catppuccin Mocha). Rule in that file: **no colour
  literal anywhere else** except the 8 project-swatch hues in `sidebar.slint` and one
  `#ffffff` on a danger fill. 14 Slint files consume `Theme.accent`: askbrowser, overlays,
  confirmclose, contextmenu, prefs_rights, addproject, leftpanel, placeholder, sidebar,
  topbar, paneview, gridpane, newpane, newgoal.
- **Runtime palettes:** `rs/crates/app/src/theme.rs` holds `UI_PALETTES: [UiPalette; 5]`
  (fields: `name, bg, mantle, surface, surface2, border, text, subtext, faint, accent,
  danger, ok, warn, link, scrim, veil` — all `u32` ARGB) and `TERMINAL_THEMES: [(name,
  [[u8;3];16]); 4]` (base16: index 0=bg, 7=fg, 1–6 ANSI, 8–15 bright). These overwrite the
  Slint defaults at runtime via the `state::Setting::UiPalette` picker.
- **Documented invariant:** `UI_PALETTES[0]` ("Mocha") is **byte-identical** to the literal
  defaults in `theme.slint`, and `TERMINAL_THEMES[0]` ("Dark") likewise. Any brand default
  that becomes index 0 must change **both** places together, or the invariant breaks.
- **Cursor is drawn by TRUE invert** (`terminal-widget/src/render.rs:266`), not from a colour
  token. A branded cursor colour (`#39FF14`) is therefore a **code** change, not a palette
  entry (BR-07). Selection colour: confirm whether it is a token or also derived (BR-08).
- **`palette.rs`** in the app crate is the Ctrl+Shift+P **command** palette — a name
  collision, not colour. Out of scope for colour work.

---

## 3. Requirements

Grouped by surface. Each: **file(s)**, the requirement, **acceptance** check, and (where the
brand is silent) a **Default:**.

### A. Colour tokens — UI

**BR-01 — Add an "Avada" UI palette and make it the default.**
`rs/crates/app/src/theme.rs` (`UI_PALETTES`), `rs/crates/app/ui/theme.slint` (literal
defaults). Add a `UiPalette { name: "Avada", … }` built from the brand colours and place it at
**index 0**, updating `theme.slint`'s property defaults byte-for-byte to match (preserve the
documented invariant). Map: `text` = `#F5F5F5`, `subtext`/`faint` = `#A0A0A0` (two tiers,
see default), `ok` = `#3EC46D` (a muted leaf green — derived, **not** Spell Green: the
syntax highlighter paints keywords with `accent` and strings with `ok`, and the prefs-rights
matrix uses `accent` and `ok` as adjacent levels, so the two must stay visibly distinct),
`link` = `#5FE0A0` (green-tinted), `danger` stays a conventional error red.
*Acceptance:* picker shows "Avada" first and selected on a fresh profile; `theme.slint`
literals equal `UI_PALETTES[0]`; `cargo test -p app` palette tests green.
**Default (bg tiers):** brand gives one background (Void `#000000`). Derive the five tiers as
near-black so existing depth cues survive: `bg #000000`, `mantle #050507`, `surface #0b0b10`,
`surface2 #15151d`, `border #26262f`. **Default (subtext/faint):** `subtext #A0A0A0`
(brand Dim), `faint #6E6E78` (darker derived). **Default (scrim/veil):** keep near-current
opacities over pure black (`scrim 0x8c000000`, `veil 0xcc000000`).

**BR-02 — Split the single `accent` into dual energy (green active / purple AI).**
`UiPalette` has one `accent`; the brand has two. Add a second accent token —
`accent2` (purple) alongside `accent` (green) — as a new `UiPalette` field **and** a new
`theme.slint` property, pushed from `theme.rs` for every palette. Audit the 14 `Theme.accent`
consumers and point AI/magic affordances (AI pane states, the ask-browser/AI surfaces) at
`accent2`, leaving active/selected/prompt affordances on green `accent`.
*Acceptance:* AI-related UI renders purple, active/selected renders green, on the Avada
palette; no `Theme.accent` consumer is left ambiguous; other palettes supply a sensible
`accent2` (default = their existing accent).
**Default (non-Avada palettes' accent2):** reuse each palette's existing `accent` so only the
Avada palette carries the green/purple split initially.

**BR-03 — Gold is a subtle accent, not `warn`.**
`theme.rs`, `theme.slint`. Do **not** map Gold `#D4AF37` onto `warn` — `warn` paints every
"agent awaiting input" badge and would make gold dominant, violating "gold subtle never
dominant." Introduce gold as a dedicated subtle token (`premium`/`gold`) used only for thin
outlines and wordmark highlight.
*Acceptance:* `warn` remains an amber outside the brand green/purple/gold triad; gold appears
only on outlines/highlights; no large gold fills.
**Default (warn):** keep an amber (`#E8B339`) distinct from brand Gold so attention states
stay legible and gold stays reserved.
*Superseded 2026-10-11 (8b2d629):* `warn` = Gold `#D4AF37`. See [`color-audit.md`](color-audit.md).

**BR-04 — Rename the token comment vars `--hp-*` → `--avada-*`.**
`rs/crates/app/ui/theme.slint` (comments like `--hp-bg`). Align the documentary CSS-var names
with the brand tokens.
*Acceptance:* no `--hp-` reference remains in `theme.slint`; names match `--avada-*`.

**BR-05 — Re-check the 8 sidebar swatch hues and the `#ffffff` danger-fill literal.**
`rs/crates/app/ui/sidebar.slint` (8 project-swatch hues), and the single sanctioned
`#ffffff` on a danger fill. Verify the 8 swatches read clearly on pure-black bg and do not
clash with the green/purple pair; change the `#ffffff` fill to Soft White `#F5F5F5`.
*Acceptance:* swatches legible on `#000000`; no raw `#ffffff` remains; both exceptions still
the only literals outside `theme.slint`.
*Done in 0.2.41:* swatches are the Avada frame palette (brand hues); `#ffffff` → `#f5f5f5`.
See [`color-audit.md`](color-audit.md).
**Default (swatches):** keep hues but lift lightness where any fails contrast on black;
replace any that collide with `#39FF14`/`#7B2CBF`.

### B. Colour tokens — terminal

**BR-06 — Add an "Avada" terminal base16 theme and make it default.**
`rs/crates/app/src/theme.rs` (`TERMINAL_THEMES`). Add at **index 0**: bg `#000000`,
fg `#F5F5F5`, green `#39FF14`, magenta `#7B2CBF`, yellow `#D4AF37`, per the brand. Update any
code that assumes the old index-0 "Dark".
*Acceptance:* terminal renders pure-black bg, soft-white fg, neon-green/arcane-purple/gold on
the brand slots; existing base16 tests green.
**Default (undefined ANSI slots):** brand defines only bg/fg/green/magenta/yellow. Choose:
red `#FF5C57` (conventional error red), blue `#57C7FF`, cyan `#9AEDFE`, white `#F5F5F5`,
bright-black `#585B70`; bright variants = brightened versions of each base. Red intentionally
stays a true error red — not re-themed to brand green/purple.

**BR-07 — Terminal cursor colour = Spell Green `#39FF14`.**
`rs/crates/terminal-widget/src/render.rs` (cursor is TRUE invert today, no colour token).
Add a themeable cursor colour and default it to `#39FF14`.
*Acceptance:* block cursor paints neon green (or green-aware invert) under the Avada theme;
cursor still legible over any cell; render tests green.

**BR-08 — Selection = Arcane Purple `#7B2CBF` @ 30 %.**
`rs/crates/terminal-widget/src/` (selection fill — confirm whether token or derived). Set the
selection highlight to purple at ~30 % opacity.
*Acceptance:* dragged selection shows a translucent purple wash; selected text stays readable.

**BR-09 — Prompt / AI-response / status dual-energy accents in the terminal.**
`rs/crates/core/src/session/osc133.rs` (prompt marks), terminal render path. Where Avada marks
prompts (OSC 133) or renders AI-response/status affordances, use green for prompt/active and
purple for AI/magic, matching the dual-energy rule.
*Acceptance:* prompt marks render green; any AI/status accent renders purple; no reversed
direction.
**Default:** if no prompt-mark rendering exists yet, record this as a forward requirement (a
"no change today, applies when prompt-mark UI lands") rather than inventing UI.

### C. Typography

**BR-10 — Bundle and apply the brand fonts.**
`rs/crates/app/assets/fonts/` (has `JetBrainsMono-Regular.ttf`, `FiraCode-Regular.ttf`),
`rs/crates/app/ui/app.slint` (`default-font-size`, font-family). Add **Cinzel** (wordmark)
and **Inter** (UI); keep **JetBrains Mono** for the terminal (brand-approved ✓). Apply Cinzel
to the wordmark, Inter as the UI default font, JetBrains Mono to the terminal grid.
*Acceptance:* wordmark renders in Cinzel, UI chrome in Inter, terminal in JetBrains Mono; app
builds with the fonts embedded.
**Default (face/weights):** Inter Regular + Medium; Cinzel Regular + SemiBold; licensing is
SIL OFL for all three (verify at bundle time — see BR-21).

**BR-11 — Wordmark string is "AVADA TERMINAL".**
`rs/crates/app/ui/topbar.slint` (title rendering), any about/splash string. Ensure the
product wordmark presents as "AVADA TERMINAL" in the display face where a wordmark is shown
(not tab titles, which are per-pane).
*Acceptance:* the brand wordmark appears as "AVADA TERMINAL" in Cinzel wherever a brand
wordmark is displayed.

### D. Icons & app identity

**BR-12 — Rebuild the app icon as the sharp dual-blade A (green→purple).**
The **source** is the checked-in `build/icon.svg` — a raster-backed master that embeds the
brand mark's painterly render (→ `build/icon.png` 1024×1024 retina master, beside `build/icon.ico`). The macOS iconset under
`rs/packaging/out/macos-stage/avada.iconset/` is **generated output** (`rs/packaging/out` is
gitignored): `rs/packaging/macos/bundle.sh` derives every size with `sips` from
`build/icon.png`, through the full-resolution 512×512@2x (1024) Retina entry. So: author the blade A into `build/icon.svg`, re-render `build/icon.png`,
and let `bundle.sh` do the rest. Then regenerate the icons that are checked in separately:
Linux `rs/packaging/linux/icons/hicolor/{16,32,48,64,128,256,512}/apps/avada.png` +
`avada.desktop`, and iOS `mobile/hyperpanes_mobile/ios/Runner/Assets.xcassets/AppIcon.appiconset/`
(through 1024). Pure black, hard edges, green left → purple right.
*Acceptance:* `build/icon.svg`/`.png` is the blade A; `bundle.sh` produces a macOS iconset
from it; Linux hicolor PNGs and the iOS appiconset are regenerated to match; direction never
reversed; small sizes legible.

**BR-13 — Favicon set, adding 64 px.**
`docs/favicon-{16,32,48}.png` + `docs/favicon.ico` (currently 16/32/48 only). Produce the
full brand favicon spec: **16/32/48/64/128/256/512** px as simplified sharp A, plus `.ico`.
*Acceptance:* all seven PNG sizes exist incl. the new 64 px; `.ico` bundles the small sizes.

**BR-14 — Replace the current `docs/logo.png`.**
`docs/logo.png` (exists — current mark). Swap for the new blade-A + wordmark lockup.
*Acceptance:* `docs/logo.png` is the new brand mark on black.

### E. Motion

**BR-15 — Launch / loading unfold animation.**
`rs/crates/app/ui/placeholder.slint` and `newpane.slint` (loading/placeholder states),
app-launch/splash path. Animate the folded dual blade unfolding into the full A over
~0.3–0.5 s with a smooth exponential ease, for app launch and loading states.
*Acceptance:* launch and pane-loading states play the unfold once at ~0.3–0.5 s ease-out; no
jank; respects reduced-motion if the platform signals it.
**Default (reduced motion):** if a reduce-motion preference is set, show the final A with no
animation.

### F. Supporting marks

**BR-16 — Secondary marks as assets.**
`docs/brand/` (new asset home). Produce the sharp dual chevron (prompt glyph), simplified
crystal (AI/living), and horizontal energy beam — all green→purple hard-edge — as reusable
SVG/PNG assets, ready for the prompt, AI-state, and divider affordances.
*Acceptance:* three secondary-mark assets exist in `docs/brand/`, on-brand direction and
palette.

### G. Website / hero (new surface — currently a gap)

**BR-17 — Brand website / landing hero.**
No website/landing/marketing HTML exists in the repo. The brand specifies a website
application with a hero. Record this as a **new deliverable** (not an existing surface to
edit): pure-black bg, blade-A + wordmark in the header, neon-green active elements/prompts,
arcane-purple AI states, subtle gold premium; taglines "AI Terminal • Magical Intelligence" /
"Terminal. Magic. Evolved."; hero plays the unfold animation.
*Acceptance:* a landing page meeting the above exists (location TBD — likely `docs/` Pages
site or a separate `web/` surface); flagged here as the one net-new surface the brand forces.

### H. Documentation & packaging identity

**BR-18 — README / docs header + tagline.**
`README.md`, docs headers. Use the blade-A/logo and a brand tagline in the README header.
*Acceptance:* README leads with the brand mark and an approved tagline.

**BR-19 — Bundle display name + metadata.**
`rs/packaging/macos/bundle.sh` (`Avada.app`, `CFBundleName`/`CFBundleDisplayName`),
`rs/packaging/linux/avada.desktop` (`Name=`), iOS `Info.plist` display name. Confirm the
displayed app name matches the brand ("Avada" / "Avada Terminal"); set consistently.
*Acceptance:* every platform's displayed app name matches the brand decision.
**Default:** app/bundle name "Avada Terminal" where a display name is shown; executable stays
`avada`.

**BR-20 — Master logo asset + icon package.**
`docs/brand/`, `build/icon.svg`. The blade-A mark is a painterly raster (see §1 note), not
line art, so the master is the rendered image rather than retraced paths: `build/icon.svg` is
a raster-backed master (embeds the PNG) that the whole icon pipeline reads.
Deliver: the raster-backed master A (in `build/icon.svg`), wordmark lockups
(horizontal + stacked), the favicon set (BR-13), and a small secondary UI icon set (BR-16) —
the brand's "Icon Package."
*Acceptance:* the master A + both wordmark lockups live in `docs/brand/`, and
`build/icon.svg` is the dual-blade A; BR-12/13/14/16 all source from them.

**BR-21 — Font licensing record.**
`docs/` (note: `docs/licensing.md` is about commercial *module* license tokens, **not** font
licensing — do not conflate). Record the bundled-font licenses (JetBrains Mono, Fira Code,
Inter, Cinzel, Nerd Font) in a fonts license note alongside the assets.
*Acceptance:* a font-license note exists listing each bundled face and its license (OFL etc.).

### I. Mobile

**BR-22 — Flutter theme + launch screen.**
`mobile/hyperpanes_mobile/lib/main.dart` (+ `src/`) `ThemeData`, and iOS
`ios/Runner/Base.lproj/LaunchScreen.storyboard`. Apply the brand palette to the Flutter
`ThemeData` (black bg, green/purple accents, soft-white text) and the blade-A launch screen.
*Acceptance:* the mobile app chrome and launch screen are on-brand.

### J. Usage-rule enforcement (cross-cutting)

**BR-23 — Enforce the brand usage rules.**
All icon/surface work above. Encode as review rules: always pure-black/very-dark bg behind the
mark; clear space ≥ ¼ icon height in every lockup; never reverse green-left → purple-right;
gold subtle, never a dominant fill.
*Acceptance:* a short "usage rules" checklist in `docs/brand/` that BR-12…BR-20 are checked
against.

---

## 4. Out of scope (stated explicitly)

- `rs/spikes/` (e.g. `spikes/terminal-render/ui/app.slint`) — experimental, not shipped.
- `worktrees/*` — working copies, not source.
- The agent-supervisor **tier3** web page — a **different repo** (`~/code/agent-supervisor`),
  not `avada-terminal`.
- `rs/crates/app/src/palette.rs` — the **command** palette (Ctrl+Shift+P), not colour.
- Implementation itself — this document decomposes requirements only.

---

## 5. Traceability

### 5a. Every brand element → requirement(s)
| Brand element | Requirement(s) |
|---|---|
| Void Black bg | BR-01, BR-06 |
| Spell Green (active/success/prompt) | BR-01, BR-02, BR-06, BR-07, BR-09 |
| Arcane Purple (AI/magic) | BR-02, BR-06, BR-08, BR-09 |
| Gold (subtle) | BR-03, BR-05 |
| Soft White text | BR-01, BR-05, BR-06 |
| Dim muted text | BR-01 |
| Dual-energy semantics | BR-02, BR-09, BR-23 |
| CSS var names `--avada-*` | BR-04 |
| Cursor `#39FF14` | BR-07 |
| Selection purple @30% | BR-08 |
| Cinzel wordmark | BR-10, BR-11 |
| Inter UI | BR-10 |
| JetBrains Mono terminal | BR-10 |
| Blade-A icon (all sizes) | BR-12, BR-13, BR-14, BR-20 |
| Favicon incl. 64px | BR-13 |
| Unfold launch/loading animation | BR-15, BR-17 |
| Secondary marks (chevron/crystal/beam) | BR-16 |
| Website / hero | BR-17 |
| Taglines | BR-17, BR-18 |
| Wordmark lockups + icon package | BR-20 |
| Usage rules (bg, clear space, direction, gold) | BR-23 |

### 5b. Every repo surface → requirement or "no change"
| Surface | Requirement |
|---|---|
| `rs/crates/app/ui/theme.slint` | BR-01, BR-02, BR-03, BR-04 |
| `rs/crates/app/src/theme.rs` (`UI_PALETTES`) | BR-01, BR-02, BR-03 |
| `rs/crates/app/src/theme.rs` (`TERMINAL_THEMES`) | BR-06 |
| 14 `Theme.accent` consumer slints | BR-02 |
| `rs/crates/app/ui/sidebar.slint` (swatches) | BR-05 |
| `rs/crates/terminal-widget/src/render.rs` (cursor) | BR-07 |
| `rs/crates/terminal-widget/src/` (selection) | BR-08 |
| `rs/crates/core/src/session/osc133.rs` (prompt marks) | BR-09 |
| `rs/crates/app/assets/fonts/` | BR-10 |
| `rs/crates/app/ui/app.slint` (fonts/size) | BR-10 |
| `rs/crates/app/ui/topbar.slint` (wordmark) | BR-11 |
| macOS iconset + `bundle.sh` | BR-12, BR-19 |
| `build/icon.svg` / `build/icon.png` / `build/icon.ico` (icon source) | BR-12, BR-20 |
| Linux hicolor icons + `avada.desktop` | BR-12, BR-19 |
| iOS `AppIcon.appiconset` | BR-12 |
| `docs/favicon-*.png` + `favicon.ico` | BR-13 |
| `docs/logo.png` | BR-14 |
| `rs/crates/app/ui/placeholder.slint`, `newpane.slint` | BR-15 |
| `docs/brand/` (assets) | BR-16, BR-20, BR-23 |
| (new) website / hero | BR-17 |
| `README.md` / docs headers | BR-18 |
| `docs/` font-license note | BR-21 |
| `mobile/hyperpanes_mobile/lib/main.dart` + iOS launch screen | BR-22 |
| `rs/spikes/*`, `worktrees/*`, tier3, `palette.rs` | no change (§4) |

### 5c. Brand-silent gaps resolved by default (all reversible)
| Gap | Where | Chosen default |
|---|---|---|
| bg tiers (brand has 1 black) | BR-01 | near-black mantle/surface/surface2/border derivations |
| subtext/faint second muted tier | BR-01 | subtext `#A0A0A0`, faint `#6E6E78` |
| `warn` colour (gold would dominate) | BR-03 | keep amber `#E8B339`, gold reserved |
| non-Avada palettes' `accent2` | BR-02 | reuse their existing accent |
| undefined ANSI slots (red/blue/cyan/bright) | BR-06 | conventional red `#FF5C57`, blue/cyan/bright set |
| font faces/weights + licenses | BR-10, BR-21 | Inter+Cinzel OFL, Regular+Medium/SemiBold |
| reduced-motion fallback | BR-15 | static final A, no animation |
| display app name | BR-19 | "Avada Terminal" |
| website location | BR-17 | TBD (docs Pages or `web/`), flagged net-new |
