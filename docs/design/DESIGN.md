# FastCull — Visual Design Reference

**This directory is the authoritative visual source for the FastCull GUI (Phase 6, Slint).**
It is the imported Claude Design project *fastcull / FastCull UI*, distilled into a
framework-agnostic spec that a Slint agent transcribes into `culler/ui/theme.slint` and
matches screen-by-screen.

The app is built in **Slint**, but the design is **HTML/CSS** — so this is a *translation*,
not a copy. Read §5 (HTML→Slint) before writing any `.slint`.

## Files in this directory

| File | What it is | Use it for |
|---|---|---|
| `screens/*.png` | Pixel screenshots of all 8 screens (2× DPR) | The visual ground truth. Any agent (incl. headless) can `Read` these. |
| `FastCull-UI.rendered.html` | Self-contained offline render (no runtime deps) | Open in any browser to inspect live: `python3 -m http.server -d docs/design` → `/FastCull-UI.rendered.html`. |
| `FastCull-UI.dc.html` | Verbatim Claude-Design source (`<x-dc>` + `support.js` runtime) | Recover *exact* values / the data model (`renderVals`). Needs the design runtime to render. |
| `DESIGN.md` | This file — tokens, `theme.slint` contract, per-screen anatomy, translation notes, scope | The contract the Slint code is held to. |

**Source-of-truth order** when values disagree: `screens/*.png` (look) → `FastCull-UI.dc.html`
(exact numbers) → this doc (the distilled contract). The live project lives at
`claude.ai/design/p/391ef3b2-cc28-4c5f-bb39-366366e887e5` (needs `/design-login`; the offline
files above are preferred so headless sub-agents and CI never need auth).

---

## 1. Design language in one paragraph

Pro-dark, image-first, middle density. Near-black neutrals; the **photo is maximized** and all
chrome floats over it as **translucent panels**. Two typefaces only: **IBM Plex Mono for every
piece of data** (filenames, counts, paths, EXIF, tags, key caps, labels) and **IBM Plex Sans for
chrome** (dialog titles, buttons, prose). One **5-tier color system** at matched chroma — grey /
green / blue / gold / red — is the single most important visual signal and appears everywhere
(filmstrip dot badges, count pills, tier badge, apply buckets, toasts). Rounded corners, soft
shadows, hairline white-alpha borders.

---

## 2. Design tokens

### 2.1 Tier palette — the core signal (matched chroma)

| Tier | Role | Hex | Code¹ | XMP rating² |
|---|---|---|---|---|
| **Reject** | rejected (→ `00_rejected`, never deleted) | `#d05f5f` | 4 | −1 |
| **Rest** | residual / undecided (→ `01_rest`) | `#63666c` | 0 | — |
| **Keep** | usable (→ `02_keep`) | `#57a86d` | 1 | 3 |
| **Pick** | a select (→ `03_picks`) | `#5a93d4` | 2 | 4 |
| **Best** | portfolio (→ `04_bests`) | `#d2a545` | 3 | 5 |
| — | highlight/hover accent (autocomplete match, link hover) | `#7fb0e4` | — | — |

¹ `code` = the `int` in `FilmstripItem.color-code` / `ui::tier_color_code` (0 rest, 1 keep, 2 pick, 3 best, 4 reject).
² Matches the spec/`Tier::xmp_rating`. Rest has no rating. Note Rest grey `#63666c` is identical to the "label/dim-text" neutral — the residual is deliberately colorless.

**Semantic accent roles** (each *is* a tier color, used by function):
- **primary action** = Pick blue `#5a93d4` (Open-folder button, current-tier `PICK` badge)
- **confirm/apply** = Keep green `#57a86d` (Move/Confirm button, progress bar, ✓ toasts)
- **warn/resume** = Best gold `#d2a545` (crash-recovery accent + Resume button)
- **danger** = Reject red `#d05f5f` (✕ toast, error text)
- Buttons with a colored fill use **dark text `#0e0f11`**.

### 2.2 Neutrals & surfaces

| Token | Value | Where |
|---|---|---|
| base bg | `#0f1012` | app background |
| toast strip bg | `#101114` | 2g container |
| startup gradient | `radial-gradient(ellipse at 50% 38%, #141519 0%, #0b0c0e 70%)` | 2a startup, 2d crash bg |
| modal scrim | `rgba(5,6,7,.6)` (`#05060799`) | dim behind 2b/2c/2e dialogs |
| HUD panel | `rgba(10,11,13,.72)` (`#0a0b0db8`) + blur(6) | 1b floating overlays |
| dialog panel | `rgba(13,14,17,.82)` (`#0d0e11d1`) + blur(12) | 2b–2f modal bodies |
| toast panel | `rgba(13,14,17,.9)` (`#0d0e11e6`) | 2g toasts |
| input inset | `rgba(0,0,0,.35–.4)` (`#00000059`) | dest field, tag input |
| chip bg | `rgba(255,255,255,.10–.12)` | tag chips, key caps |

### 2.3 Text ramp (bright → dim)

`#e8e9ea` primary · `#d6d7d9` strong-path · `#c9cacc` secondary · `#a8abb0` tertiary ·
`#9fa3a9` buttons/counts · `#8b8e94` muted · `#7d8087` position/seen · `#63666c` labels/notes ·
`#54575d` faint hints · `#45484e` RAW-badge border · `#34373d` dashed dropzone.

### 2.4 Borders

`.05` `#ffffff0d` table rows · `.08` `#ffffff14` HUD/panel · `.09` card · `.10` `#ffffff1a`
dialog/toast · `.12` `#ffffff1f` key caps/chips · `.14` `#ffffff24` inputs/key-cap border ·
`.16` tag input · `.18` `#ffffff2e` secondary button.

### 2.5 Typography

- **Families:** `"IBM Plex Sans"` (chrome) · `"IBM Plex Mono"` (all data). Rule of thumb from the
  design: *if it's a value, it's Mono; if it's a sentence/label/button, it's Sans.* Weights **400 / 500 / 600**.
- **Sizes:** wordmark 22 · dialog title 15 · primary button / dropzone 13 · body/values/buttons 12 ·
  11.5 · 11 · small 10.5 · 10 · **uppercase label 9.5** (`letter-spacing: .14em`) · RAW badge 9.
- **Tier badge** (`PICK`): Mono 600, 12px, `letter-spacing: .08em`, dark text on the tier fill.
- **Uppercase section labels** (`RECENT`, `DESTINATION`, `TIER`…): Mono 600 9.5px, color `#63666c`, `letter-spacing: .14em`.

### 2.6 Radii · shadows · spacing

- **Radii:** 3 tiles/chips/badges · 4 key caps · 5 id-badge · 6 buttons/inputs/HUD-panels/tier-badge ·
  8 cards/filmstrip/toasts/dropzone · 10 dialogs · 50% dots & the `?` button.
- **Shadows:** HUD badge `0 2px 8px /.4` · card `0 2px 10px /.5` · toast `0 4px 16px /.5` · dialog `0 16px 50px /.6`.
- **Spacing:** overlay elements sit **14px** from the card edge; panel padding 7–12 (HUD), 14 (tag panel),
  22–26 (dialogs); gaps 5–10 (tight), 14–18 (sections), 26 (startup blocks).
- **Filmstrip tile:** 84×56, `gap:5`, radius 3; tier **dot** 7px at `top:4 right:4` with a `0 0 0 2px rgba(0,0,0,.55)` ring;
  unvisited-rest tile `opacity:.4`; current tile `box-shadow: 0 0 0 2px #e8e9ea` (outline).

---

## 3. `theme.slint` contract (build this first in Phase 6)

Transcribe the tokens above into **one** `global Theme` and make every `.slint` file read from it —
**never hardcode hex** in components. Slint alpha colors are 8-digit `#RRGGBBAA`; the conversions are done for you below.

```slint
// culler/ui/theme.slint — single source of visual truth.
// Values distilled from docs/design/DESIGN.md. Do not hardcode colors elsewhere.
export global Theme {
    // ── surfaces ──
    out property <brush> bg: #0f1012;
    out property <brush> bg-radial-in: #141519;    // startup/crash gradient inner
    out property <brush> bg-radial-out: #0b0c0e;    // startup/crash gradient outer
    out property <brush> scrim: #05060799;          // modal backdrop over the image  (rgba 5,6,7,.6)
    out property <brush> panel-hud: #0a0b0db8;      // 1b floating panels             (rgba 10,11,13,.72)
    out property <brush> panel-dialog: #0d0e11d1;   // modal dialog bodies            (rgba 13,14,17,.82)
    out property <brush> panel-toast: #0d0e11e6;    // toasts                         (rgba 13,14,17,.9)
    out property <brush> input-inset: #00000059;    // input fields                   (rgba 0,0,0,.35)

    // ── borders ──
    out property <brush> border-faint: #ffffff0d;   // .05  table rows
    out property <brush> border: #ffffff14;         // .08  hud / panel
    out property <brush> border-strong: #ffffff1a;  // .10  dialog / toast
    out property <brush> chip-bg: #ffffff1f;        // .12  key caps / tag chips
    out property <brush> border-input: #ffffff24;   // .14  inputs / key caps
    out property <brush> border-btn: #ffffff2e;     // .18  secondary button

    // ── text ramp ──
    out property <brush> text: #e8e9ea;
    out property <brush> text-2: #c9cacc;
    out property <brush> text-3: #a8abb0;
    out property <brush> text-muted: #9fa3a9;
    out property <brush> text-dim: #7d8087;
    out property <brush> label: #63666c;            // uppercase labels, notes (== Rest tier)
    out property <brush> text-faint: #54575d;
    out property <brush> on-accent: #0e0f11;        // dark text on colored fills

    // ── tier palette ──
    out property <brush> tier-reject: #d05f5f;
    out property <brush> tier-rest:   #63666c;
    out property <brush> tier-keep:   #57a86d;
    out property <brush> tier-pick:   #5a93d4;
    out property <brush> tier-best:   #d2a545;
    out property <brush> accent-hi:   #7fb0e4;      // autocomplete match / hover
    // semantic roles (alias the tiers)
    out property <brush> accent-primary: self.tier-pick;
    out property <brush> accent-confirm: self.tier-keep;
    out property <brush> accent-warn:    self.tier-best;
    out property <brush> danger:         self.tier-reject;

    // ── type ──
    out property <string> font-sans: "IBM Plex Sans";
    out property <string> font-mono: "IBM Plex Mono";
    out property <length> fs-wordmark: 22px;
    out property <length> fs-title: 15px;
    out property <length> fs-body: 12px;
    out property <length> fs-small: 10.5px;
    out property <length> fs-label: 9.5px;          // uppercase, letter-spacing 0.14em

    // ── radii ──
    out property <length> r-sm: 3px;
    out property <length> r-key: 4px;
    out property <length> r-md: 6px;
    out property <length> r-lg: 8px;
    out property <length> r-xl: 10px;

    // tier code -> color  (0 rest, 1 keep, 2 pick, 3 best, 4 reject — matches ui::tier_color_code)
    pure function tier-color(code: int) -> brush {
        return code == 1 ? self.tier-keep
             : code == 2 ? self.tier-pick
             : code == 3 ? self.tier-best
             : code == 4 ? self.tier-reject
             : self.tier-rest;
    }
}
```

> This supersedes the placeholder `globals.slint`/ad-hoc hex (`#141414`, `#f0a35e`, `#99bbee`,
> `#f85149`, `#ccaa88`…) sketched in the Phase-6 tasks. Those were stand-ins; use `Theme.*`.

---

## 4. Per-screen anatomy

Each screen has a screenshot in `screens/`. `v1` column: **Build** = required by the spec ·
**Optional** = design addition, cheap, recommended · **Defer** = design addition beyond v1 scope
(see §6).

### 1b — Main culling view · `screens/1b-main.png` · Task 8–10
Photo full-bleed `fit`; chrome floats as `panel-hud` overlays inset **14px** from edges.
- **top-left** (Build): `IMG_2047.JPG` (Mono 500 12, `text`) + `RAW` badge (Mono 600 9, border `#45484e`, r-sm) + `1205/2000` (Mono 10.5, `text-dim`).
- **top-right** (Build): per-tier **counts pill** — 5× `dot(6px) + number`, order best·pick·keep·rest·reject — then the current-tier **badge** `PICK` (`accent-primary` fill, `on-accent` text, r-md, shadow `0 2px 8px/.4`). Badge color = current tier.
- **bottom-left panel** (mixed): tag chips (Build — chip-bg r-sm, `text-2`) · **histogram** 30 bars `rgba(255,255,255,.32)` (Defer) · **EXIF line** `1/250s · ƒ2.8 · ISO 400 · 85mm` (Defer).
- **bottom-right** (Optional): `?` help button — 30px circle, `panel-hud`, border `.12`.
- **bottom filmstrip** (Build): virtualized strip of 84×56 tiles, tier **dot** badge top-right, unvisited-rest dimmed `.4`, **current** tile outlined `2px #e8e9ea`; right side `seen 1204/2000` + a `accent-confirm` progress bar. Panel is `panel-hud`, r-lg.

### 2a — Startup · `screens/2a-startup.png` · Task 11
Centered 520px column on the startup gradient.
- Wordmark `fastcull` (Mono 600 22) + tagline (Build).
- **Dropzone** (Build): dashed border `#34373d` 1.5px r-lg; `Open source folder…` button (`accent-primary` fill, Sans 600 13, `on-accent`); hint line `or drop a folder here · or: fastcull ~/shoots/…`.
- **RECENT** list (Defer): label + 3 rows (path · N shots · `seen 60%`/`applied ✓`). No recents store in v1 — render the wordmark + dropzone; drop or stub the list.

### 2b — Apply dialog · `screens/2b-apply-dialog.png` · Task 12
Scrim + centered 560px `panel-dialog` r-xl, shadow `0 16px 50px/.6`. (Build.)
- Title `Apply — move 2,000 shots` (Sans 600 15) + `nothing is deleted` (Mono 11 `label`).
- `DESTINATION` label + inset field (`input-inset`, border-input) with `Browse…`; validity line green `✓ subfolder of source…` / red on error.
- **Per-bucket table**: 5 rows `dot + dir(110px) + note + count`, Mono 12, row separators `.05`.
- Preview notes (Mono 11 `text-muted`): collisions → auto-suffix, unrecognized-stay-behind, existing-sidecar skips, and a green/red free-space line.
- Footer: `Cancel` (secondary — transparent, border-btn, `text-muted`) + `Move 2,000 shots` (`accent-confirm` fill, `on-accent`). Confirm disabled unless preview ready & space ok.

### 2c — Apply in flight · `screens/2c-apply-progress.png` · Task 12
Same modal frame, 520px. Title `Moving shots…` + `763 / 2,000`; `accent-confirm` progress bar (6px, r-sm);
current file `IMG_1207.JPG + .CR3 → 02_keep/` + status line `cross-fs copy · verified · rename NOREPLACE`;
per-tier tally `X 81 · 490 K 158 P 30 B 4` + `xmp written 187`; journal path + `Stop safely` (secondary). (Build.)

### 2d — Crash recovery · `screens/2d-crash-recovery.png` · Task 11/12
Startup gradient + 520px panel with **gold** border `rgba(210,165,69,.35)`. `!` badge in a gold ring +
`Interrupted apply found`; journal path, `1,237 of 2,000 moves completed`, reassurance line; gold progress bar;
`Show report` (secondary) + `Resume apply` (`accent-warn` gold fill). (Build — detection is a Global Constraint.)

### 2e — Keymap cheat-sheet · `screens/2e-keymap.png` · (no current task — add small)
640px `panel-dialog`. `Keys` title + `? or esc to close`; a **2-column grid** of groups
**TIER / NAVIGATE / VIEW & TAGS / SESSION**, each row = key-cap (chip-bg, border-input, r-key, min-width 52,
centered) + description (`text-3`). Content mirrors the canonical keymap exactly. (Optional — a static
overlay bound to `?`; cheap and high-value. Not in the current task list — add as a small extra to Task 9/10.)

### 2f — Tag entry · `screens/2f-tag-entry.png` · Task 9
Photo **stays visible (no dim)**. A `PICK` tier badge pins top-right. Bottom-center 380px `panel-dialog` r-xl:
input row (`input-inset`, border `.16`) holding committed chips `street ×` + a live caret; **autocomplete
dropdown** — matched prefix bolded in `accent-hi`, each row `term ……… count`, the highlighted row tinted
`rgba(90,147,212,.22)`; hint `enter add · comma next tag · esc done`. (Build.)

### 2g — Toasts · `screens/2g-toasts.png` · (no current task — add small)
Stacked `panel-toast` pills (r-lg, shadow `0 4px 16px/.5`), Mono 12: `filter ≥ Pick` (pick dot),
`undo IMG_2046 → keep` (keep dot), `✓ session restored`, `✓ apply complete` (green-tinted border),
`✕ .fastcull.json was corrupt` (red-tinted border). (Optional — transient feedback for filter/undo/session/
apply/corrupt events; cheap, recommended. Add as a small extra.)

---

## 5. HTML → Slint translation notes (read before writing `.slint`)

| CSS in the design | Slint |
|---|---|
| `backdrop-filter: blur(6/12px)` | **No blur in Slint.** Render the panel as a `Rectangle { background: Theme.panel-hud; }` at the given rgba and accept the loss of frosting — do **not** fake it. If legibility over a busy photo suffers, nudge the panel alpha up ~0.08. This is a known, documented deviation. |
| `position: absolute; top/left/right/bottom` | A root `Rectangle` (the loupe/image) with children placed by explicit geometry: top-left `x:14px; y:14px`; top-right `x: parent.width - self.width - 14px`; bottom `y: parent.height - self.height - 14px`. Or center with a `VerticalLayout`/`HorizontalLayout` + `alignment: center`. |
| Modal = scrim `<div>` + centered panel | `Rectangle { background: Theme.scrim; }` filling the window, then a centered panel `Rectangle`. Gate visibility with an `if` on a model property. |
| `rgba(...)` | 8-digit `#RRGGBBAA` (values pre-computed in §3) or `<color>.with-alpha(0.72)`. |
| `border-radius` | `border-radius: Theme.r-md;` |
| `box-shadow` | `drop-shadow-blur / drop-shadow-offset-x/y / drop-shadow-color` on a `Rectangle` (single shadow only; approximate the values in §2.6). |
| `linear-gradient` (startup radial) | Slint has `@radial-gradient(circle, Theme.bg-radial-in 0%, Theme.bg-radial-out 70%)`. |
| Google-Fonts `<link>` | **Bundle the fonts.** Add IBM Plex Sans + Mono (400/500/600) `.ttf` under `culler/ui/fonts/` and register at startup (`slint::register_font_from_path`) or rely on system Fontconfig. IBM Plex is OFL — redistributable. Then `font-family: Theme.font-mono`. Do not depend on network fonts. |
| tag chips / key caps | `Rectangle { background: Theme.chip-bg; border-radius: Theme.r-sm; }` + `Text`. |
| filmstrip tile | `Image` (the thumbnail) in a `Rectangle` r-sm; overlay the tier **dot** (`Rectangle` width/height 7px, `border-radius: 50%`, plus a 2px dark ring via a slightly larger backing rect); `opacity: 0.4` when unvisited-rest; `2px Theme.text` border when current. |
| letter-spacing | `Text { letter-spacing: 0.14em * ...; }` — Slint takes a length; use e.g. `1.3px` for the 9.5px labels (~.14em). |
| colored button | `Rectangle { background: Theme.accent-primary; border-radius: Theme.r-md; }` + `Text { color: Theme.on-accent; }`. Secondary = transparent bg + `border-width:1px; border-color: Theme.border-btn`. |

**Keep `culler-core` GUI-free.** These are all binary-side (`culler`) concerns; none of this leaks into the library.

---

## 6. Scope reconciliation — design vs. v1 spec (YAGNI)

The mockup is intentionally a little richer than v1. Match the **look** of what you build, but don't
build past v1. Classification:

| Element | v1? | Note |
|---|---|---|
| Loupe fit + 1:1 zoom/pan, filmstrip (color + dots + dim + current outline + progress) | **Build** | Spec core. |
| HUD: tier badge, tags, per-tier counts, seen progress | **Build** | Spec §9. |
| Tag entry + autocomplete | **Build** | Spec §9 (`all_tags`). |
| Apply dialog (dest guard, per-bucket, collision/leftover/xmp/free-space, confirm) | **Build** | Spec §6. |
| Apply-in-flight progress; crash resume/report | **Build** | Spec §8 + Global Constraints. |
| Startup open-folder (wordmark + dropzone) | **Build** | Spec "Load". |
| Keymap cheat-sheet (2e) + `?` button | **Optional** | Static overlay, cheap; not in current tasks — add small. |
| Toasts (2g) | **Optional** | Transient feedback; cheap — add small. |
| Startup **recents** list (2a) | **Defer** | No recents store in v1 (startup = open-folder / CLI arg). Stub or omit. |
| HUD **histogram** (1b) | **Defer** | Needs a luma histogram; not in spec. |
| HUD **EXIF line** (1b) | **Defer** | Needs EXIF fields surfaced from `decode`; not in spec HUD. |

When you **Defer/omit** something visible in a screenshot, that is expected — note it so the fidelity
check (Task 13) doesn't flag it as a regression.

---

## 7. Fidelity checklist (for the Phase-6 visual verification, Task 13)

Build → run → screenshot the app → compare each surface to its `screens/*.png`:

- [ ] `theme.slint` exists; **no** component hardcodes a hex that lives in `Theme`.
- [ ] Tier colors exactly `#d05f5f / #63666c / #57a86d / #5a93d4 / #d2a545`, used consistently across filmstrip dots, counts pill, tier badge, apply buckets, toasts.
- [ ] Mono for all data, Sans for titles/buttons; IBM Plex bundled (not system-fallback).
- [ ] HUD panels translucent-dark over the photo; inset ~14px; radii per §2.6.
- [ ] Filmstrip: dot badges, unvisited-rest dimmed, current tile outlined.
- [ ] Apply dialog matches 2b (bucket table, notes, green confirm / bordered cancel).
- [ ] Buttons: primary=blue, confirm=green, resume=gold, secondary=bordered — dark text on fills.
- [ ] Any **Defer**'d element (recents/histogram/EXIF) is intentionally absent, not broken.
- [ ] List any remaining deviations (esp. the no-blur panels) and confirm they're acceptable.
