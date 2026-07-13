# FastCull — HUD & UX batch (8 items)

**Date:** 2026-07-13
**Status:** In implementation (branch `feat/hud-ux-batch`)
**Relates to:** [`docs/design/DESIGN.md`](../../design/DESIGN.md) §4 (screen 1b/2f), §6 (scope)

A batch of eight small-to-moderate GUI improvements requested in one pass. Two of
them (histogram, EXIF line) are elements the v1 design explicitly **Deferred**
(DESIGN §6) — they are now being built, matching the `screens/1b-main.png` mockup
exactly. Delivery is one feature branch, integrated + tested at the end.

## Decisions locked in (from brainstorming)

| Question | Choice |
|---|---|
| "Spectrogram" | **Luma histogram** — 30 bars, white @ .32 alpha, from the decoded fit-preview (Rec.601). Matches 1b. |
| Focus mode | **Hide all chrome + a faint "press enter to exit" hint.** Toggle = **Enter**. Distinct from F11. |
| Fullscreen | **F11** → OS window fullscreen (Slint `Window::set_fullscreen`). Composes with focus mode. |
| Filter-switch snap | Snap to **nearest passing at/after current** (else before, else stay). Mirrors the filmstrip. |
| EXIF missing fields | Omitted gracefully (no blanks/zeros). |
| Delivery | Partitioned-parallel: `culler-core` in a subagent; coupled UI/glue by one hand; one branch. |

## Architecture

Rust workspace: `culler-core` (pure, GUI-free) + `culler` (Slint 1.17 GUI, winit + Skia).
`kamadak-exif 0.6` already a dep. The split for this batch:

- **`culler-core` (pure, unit-tested in isolation):**
  - `histogram::luma_histogram(&DecodedImage, bins) -> Vec<f32>` — normalized 0..1 bar heights.
  - `model::ExifSummary { exposure:(u32,u32)?, f_number:f32?, iso:u32?, focal_length_mm:f32? }`
    + `hud_line() -> String` (`1/250s · ƒ2.8 · ISO 400 · 85mm`, missing fields dropped).
  - `Shot.exif: Option<ExifSummary>`, parsed in `scan.rs` beside capture time.
  - `Session::tag_counts() -> Vec<(String, usize)>` (count desc, name asc).
- **`culler` Slint + glue (compile-coupled through generated bindings + `main.rs`):**
  everything visual and every key/dispatch wiring.

## Per-item design

1. **Luma histogram** — new `HudHistogram`-style bar row inside the bottom-left panel; AppWindow
   `histogram: [float]` (30) fed from the decoded current image (`ui::set_loupe` path). Bars are
   `rgba(255,255,255,.32)`.
2. **`?` hint** — `HudHelpHint` (bottom-right pill "? shortcuts"); the `?`→KeySheet binding already
   exists. Hidden in focus mode. Static, no Rust.
3. **Filter-switch snap** — `input::nearest_passing`; `main.rs` `CycleFilter` snaps `current` +
   `request_current()` when the current shot is filtered out. Unit-tested.
4. **F11 fullscreen** — forwarded in the `keyscope`; `Key::F11`→`Action::ToggleFullscreen`→
   `window().set_fullscreen(!is_fullscreen())`. Loupe context.
5. **Dead space** — filmstrip row `130px`→`72px` (content is ~70px). No other element below it.
6. **EXIF line** — AppWindow `hud-exif: string` from `ExifSummary::hud_line()`, drawn in the
   bottom-left panel above the tags. Omits missing fields.
7. **Tag entry keyboard nav + counts + all-by-default** — suggestions become a struct
   `{prefix, rest, count}`; `suggest_tags` returns all tags (count desc) when the segment is empty;
   a `tag-active-index` drives a highlighted row (`rgba(90,147,212,.22)`); **Tab/⇧Tab/↑/↓** move it,
   **Enter** accepts the highlighted row into the current comma-segment (else commits all). Counts
   right-aligned; matched prefix bolded in `accent-hi`.
8. **Focus mode** — AppWindow `focus-mode: bool`; `Enter`→`Action::ToggleFocus`. HUD overlays bound
   `visible: !focus-mode`; the filmstrip switched to `if !focus-mode` (reclaims the row). `HudFocusHint`
   shows the exit reminder. Non-modal (navigation still works).

## Keybindings added
`F11` fullscreen · `Enter` focus-mode toggle · `Tab`/`⇧Tab`/`↑`/`↓` tag-suggestion nav (tag entry only).
All added to the KeySheet (`2e`).

## Testing
- `culler-core`: unit tests for histogram (black/white/gradient/normalization), `ExifSummary::hud_line`
  (full/partial/empty/≥1s/integer-f), `tag_counts`.
- `culler` pure: `nearest_passing`, F11/Enter keymap, `to_key` normalization.
- Integration: `cargo build && cargo test`; run the app and eyeball each surface vs `screens/*.png`.

## Non-goals / preserved behavior
- The bottom-left panel keeps its existing filter-label + `seen X/Y` progress (added below the design's
  histogram/EXIF/tags) rather than removing them.
- No file-move, session, or apply behavior changes. No `culler-core` GUI dependency introduced.
