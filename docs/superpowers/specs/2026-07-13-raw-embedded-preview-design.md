# FastCull — RAW embedded-preview (Fuji RAF) + JPEG⇄RAW toggle

**Date:** 2026-07-13
**Status:** Approved 2026-07-13 — ready for implementation planning
**Relates to:** [`docs/design/DESIGN.md`](../../design/DESIGN.md) §4 (screen 1b), §10 (placeholder tile) · [`docs/specs/2026-07-08-fastcull-design.md`](../../specs/2026-07-08-fastcull-design.md) (base spec, phase-2 "embedded-preview extraction") · [`docs/design/2026-07-08-darktable-integration-exploration.md`](../../design/2026-07-08-darktable-integration-exploration.md) §7 ("RAW-only rolls")

Adds a RAW **preview** to the culler by extracting the **full-size embedded JPEG**
that a Fuji **RAF** file carries in its header, then decoding it through the
existing JPEG pipeline. No demosaic, **no new dependency**, no change to build
portability — the embedded preview rides the same `turbojpeg` decoder the app
already uses, so a future Windows port is not made any harder by this work.

Two capabilities (scope = **both**, chosen in brainstorming):
1. **RAF-only shots become cullable** — a stem with a RAF but no JPEG sibling is
   promoted to a real `Shot`, shown via its embedded preview. Today `scan_report`
   detects these and drops them.
2. **`r` toggles JPEG⇄RAW in a pair** — when a shot has both a JPEG and a
   (previewable) RAW, `r` switches the loupe between them. When there is nothing
   to switch to, a transient warning toast fires; a RAW-only shot carries a
   standing on-screen "RAW ONLY" badge.

## Decisions locked in (from brainstorming)

| Question | Choice |
|---|---|
| Preview fidelity | **Embedded JPEG only** — extract + reuse the existing decode path. No demosaic, no libraw/rawloader. |
| Formats (first) | **Fuji RAF only.** Extraction is format-gated behind `raw::preview_supported(ext)` so more can be added later. |
| Scope | **Both** — promote RAF-only stems *and* add the in-pair `r` toggle. |
| Toggle key | **`r` / `R`** → `Action::ToggleRawPreview`. |
| Toggle scope | **Sticky global "prefer RAW" mode** (persists across navigation, like `Z` zoom). Per-shot reset is the noted open alternative (§10). |
| No-RAW / RAW-only feedback | **Transient toast** on `r` when nothing to toggle; **persistent HUD badge** "RAW ONLY" while a JPEG-less shot is current. |
| RAF-only sort order | **Capture time from the embedded JPEG's EXIF** (keeps burst order), filename tiebreak falls back to the RAF name. |
| Portability | **Unchanged** — no new native deps; only pure-Rust byte parsing + the existing `turbojpeg` path. |

## Architecture

Same workspace split. RAF parsing is pure and lives in `culler-core`; the decode
dispatch, scan promotion, and metadata read all reuse it. GUI wiring is confined
to `culler`.

- **`culler-core` (pure, unit-tested in isolation):**
  - **New `raw` module** (`culler-core/src/raw.rs`):
    - `preview_supported(ext_lower: &str) -> bool` — `true` for `"raf"` today; the one extensibility seam.
    - `embedded_jpeg(data: &[u8]) -> Option<&[u8]>` — parse the RAF header, return the embedded-JPEG slice. Pure, zero-copy, bounds-checked, never panics.
  - **`decode`**: dispatch on container. `is_jpeg` → today's path; else `raw::embedded_jpeg` → run the *same* `decompress_scaled` + `read_orientation` + `apply_orientation` on the extracted slice; else `Unsupported`.
  - **`model::Shot`**: `jpeg: Option<PathBuf>` (was required `PathBuf`) + invariant "jpeg or raw present" + `display_path()` / `is_raw_only()` helpers.
  - **`scan`**: promote RAF-only stems to shots; read capture-time/EXIF from the display file (extracting the RAF's embedded JPEG first).
- **`culler` (Slint + glue):** the `r` action, `request_current` path routing, the HUD badge states, and the warning toasts.

## RAF container format

A RAF file is a fixed header pointing straight at a full-size embedded JPEG
(the camera's own render, with its own EXIF — orientation *and* capture metadata).

```
offset  field
0       "FUJIFILMCCD-RAW " (16-byte ASCII magic)
16      format version "0201" (4)
20      camera id number (8)
28      camera model string (32)
60      dir version (4) + unknown (20)
84      embedded-JPEG byte offset   (u32, big-endian)   ← preview
88      embedded-JPEG byte length   (u32, big-endian)   ← preview
92      CFA (sensor) header offset/length, CFA offset/length …
```

`raw::embedded_jpeg`:
1. Reject if `data` doesn't start with the 16-byte magic → `None`.
2. Read `off = u32::from_be_bytes(data[84..88])`, `len = u32::from_be_bytes(data[88..92])`.
3. `end = off.checked_add(len)`; require `len > 0` and `end <= data.len()` → else `None`.
4. `slice = &data[off..end]`; require `is_jpeg(slice)` (SOI `FF D8 FF`) → else `None`; return `Some(slice)`.

The slice then flows through the **unchanged** decode internals, so the
`MAX_DECODE_PIXELS` decompression-bomb guard, EXIF orientation, and the
`Fit/Full/Scaled` targets all apply to RAF previews for free.

> **Implementation note:** offsets 84/88 are the documented, stable layout
> (dcraw / exiftool / libopenraw agree), but the RED test's synthetic fixture
> and one real `.RAF` must both be verified byte-exact before the module is
> considered done.

## Model change — `Shot.jpeg` becomes optional

```rust
pub struct Shot {
    pub stem: String,
    #[serde(default)]
    pub jpeg: Option<std::path::PathBuf>, // display JPEG, if the stem has one
    pub raw: Option<std::path::PathBuf>,
    pub sidecar: Option<std::path::PathBuf>,
    pub capture: CaptureTime,
    #[serde(default)]
    pub exif: Option<ExifSummary>,
}
```

**Invariant (enforced in `scan`): `jpeg.is_some() || raw.is_some()`.**

New helpers localize the "which file do we show/name" decision so no consumer
open-codes the fallback:

```rust
impl Shot {
    /// The file the loupe decodes by default: the JPEG if present, else the RAW.
    /// Never None — the scan invariant guarantees one is present.
    pub fn display_path(&self) -> &std::path::Path { /* jpeg.as_deref().or(raw.as_deref()).expect(...) */ }
    pub fn is_raw_only(&self) -> bool { self.jpeg.is_none() }
    // files(): push jpeg? then raw? then sidecar? (was jpeg unconditionally)
}
```

**Serde compat:** old sessions store `"jpeg": "/path"` → deserializes to `Some`.
`#[serde(default)]` covers any missing key. New RAF-only shots serialize
`"jpeg": null`. `reattach` rebuilds `shots` from a fresh scan each launch and
merges decisions by stem, so the stored path is informational either way.

**Blast radius (the complete list — all flow through `display_path()`/`files()`):**

| Site | Change |
|---|---|
| `model::Shot::files()` (`model.rs:195`) | push `jpeg` only when `Some`. |
| `scan::sort_key` (`scan.rs:303`) | filename tiebreak from `display_path()`, not `&shot.jpeg`. |
| `scan::scan_report` (`scan.rs:107`) | promotion + EXIF-from-display (below). |
| `ui::hud_text` filename (`ui.rs:193`) | from the *displayed* file (§UI). |
| `main::request_current` (`main.rs:259,269`) | route JPEG vs RAW (§toggle). |
| `plan.rs:109`, `applyflow.rs:58,100` | already call `shot.files()` — no direct change. |

## Scan change — promotion + metadata

In `scan_report`, the `group.jpeg == None` arm currently pushes the RAW to the
`raw_only` report. New logic:

```
None (no JPEG) =>
  if let Some(raw) = group.raw, and raw::preview_supported(ext_of(raw)):
      // promote: a previewable RAW-only shot
      let (capture, exif) = read_exif_from_display(&raw);   // extracts embedded JPEG first
      shots.push(Shot { stem, jpeg: None, raw: Some(raw), sidecar: group.sidecar, capture, exif });
  else if let Some(raw) = group.raw:
      raw_only.push(raw);   // unchanged: CR3/NEF/… with no extractor stay non-cullable
```

`read_exif_data` (`scan.rs:188`) generalizes to `read_exif_from_display(path)`:
read the file; if it's a RAF, `raw::embedded_jpeg` first and run
`exif::Reader::read_from_container` on **the extracted JPEG bytes** (a `Cursor`);
otherwise read the JPEG directly as today. `kamadak-exif` does **not** parse RAF
containers, hence the extract-first step. Undecodable/EXIF-less → default
`CaptureTime` + `None` summary (never fails the scan), exactly as now.

**Result:** RAF-only shots sort by real capture time (burst order preserved) and
show a normal EXIF HUD line. Non-RAF RAW-only stems behave exactly as before.

## Decode change — dispatch

`decode(path, target)` (`decode.rs:238`) today: read file → `is_jpeg` else
`Unsupported`. New:

```
let data = read(path)?;
let jpeg: &[u8] =
    if is_jpeg(&data) { &data }
    else if let Some(slice) = raw::embedded_jpeg(&data) { slice }   // RAF
    else { return Err(Unsupported) };
// …identical from here: read_orientation(jpeg), target match on jpeg, apply_orientation…
```

`embedded_thumbnail` (`decode.rs:263`, the filmstrip instant-paint path) gains the
same extract-first step: for a RAF, pull the embedded JPEG, then run the existing
IFD1-thumbnail logic on it (the embedded JPEG carries its own small EXIF
thumbnail). If absent → `None`, and the full decode fills the tile as usual.

## Display-source toggle (`r`) + routing

**Input** (`input.rs`): add `Action::ToggleRawPreview`; map `Key::Char('r') |
Key::Char('R')`; list it among the UI-only no-ops in `apply_action` (like
`ToggleZoom`, it changes *view* state, not the model).

**State** (`main.rs`): `let show_raw = Rc::new(Cell::new(false));` alongside
`zoom`. Sticky "prefer RAW" mode.

**Routing** — `request_current` picks the current shot's decode path:

```rust
let use_raw = show_raw.get()
    && shot.raw.as_deref().is_some_and(|r| raw::preview_supported(ext_lower(r)));
let path = if use_raw { shot.raw.clone().unwrap() } else { shot.display_path().to_path_buf() };
```

So even in global RAW mode a **non-previewable** RAW (e.g. a CR3 sibling) falls
back to its JPEG rather than decode-failing to a placeholder. Neighbors always
prefetch `display_path()` (their default). Reuses the existing generation/
staleness machinery — this is the documented `Z`-toggle case (`pipeline.rs:97`:
same index, bumped generation), so no new freshness logic.

**`r` handler** (in `on_key_pressed`, mirroring `ToggleZoom`): branch on the
current shot and fire the right feedback —

| Current shot | Action |
|---|---|
| JPEG **and** previewable RAW | flip `show_raw`; `request_current()`; toast `"showing RAW"` / `"showing JPEG"` (code −1). |
| JPEG **and** non-previewable RAW | no flip; toast `"RAW preview unsupported (.cr3)"`. |
| JPEG, **no** RAW | no flip; toast `"no RAW for this shot"`. |
| RAW-only (no JPEG) | no flip; toast `"RAW only — no JPEG to switch to"`. |

## UI surfaces

**HUD top-left badge** (`hud.slint` `HudTopLeft`, already renders a `has-raw`
"RAW" badge). Three visual states driven by two new `AppWindow` props
(`hud-showing-raw: bool`, `hud-raw-only: bool`) beside the existing
`hud-has-raw`:

- **RAW sibling exists, showing JPEG** → outlined/dim "RAW" (press `r` to view) — today's styling.
- **Showing RAW** (pair toggled to RAW) → filled/accent "RAW".
- **RAW-only shot** → amber **"RAW ONLY"** badge — the standing on-screen warning the user asked for (persists the whole time a JPEG-less shot is current).

**Filename** reflects the *file actually being decoded* — the `.JPG` name
normally, the `.RAF` name for a RAW-only shot or a pair toggled to RAW — so the
top-left line always tells the truth about what's on screen.

**Toasts** reuse the existing `show_toast(text, code)` pill (2500 ms, code −1 =
no color dot). No new toast component.

`ui::hud_text` gains a `show_raw: bool` param and returns `has_raw`, `raw_only`,
`showing_raw` (`= raw_only || (show_raw && has_raw && previewable)`) plus the
display-file `filename`; `refresh_view` pushes all three to the AppWindow.

## Keybindings added

`r` / `R` → toggle RAW⇄JPEG preview (loupe context only). Added to the KeySheet
(`keysheet.slint`, screen `2e`) and the `?` help.

## Edge cases / fallbacks

- **RAF with zero-length / out-of-bounds / non-JPEG preview pointer** →
  `embedded_jpeg` returns `None` → `decode` returns `Unsupported`/`Decode` →
  existing **placeholder tile** (`pipeline::placeholder_image`, DESIGN §10). A
  RAF-only shot whose preview won't extract is still a classifiable shot.
- **Corrupt embedded JPEG** → maps to `DecodeError::Decode` like any bad JPEG.
- **Apply/move** — a RAF-only shot's `files()` = `[raf, sidecar?]`; `plan`/`apply`
  move it into buckets unchanged (they already iterate `files()`).
- **`MAX_DECODE_PIXELS`** guard applies to RAF previews unchanged (same code path).
- **darktable `IMG.RAF.xmp` sidecar** naming already handled by `scan::sidecar_stem`.

## Testing

- **`culler-core::raw` (pure):** magic accept/reject; correct offset/length read
  (big-endian); zero-length, past-EOF, and non-JPEG-slice pointers all → `None`;
  a real `.RAF` fixture yields a decodable slice. Synthetic-RAF fixture builder
  (magic + header + a `synth_jpeg` payload at a chosen offset), mirroring
  `decode.rs`'s existing synthetic-JPEG helpers.
- **`decode`:** `decode(raf, Full/Fit/Scaled)` returns oriented RGBA of the
  embedded preview; bomb guard still fires via a patched embedded-JPEG SOF0;
  `embedded_thumbnail(raf)` extracts or `None`.
- **`model`:** `jpeg: None` serde round-trip + old-session (`"jpeg":"…"`) still
  loads; `display_path`/`is_raw_only`/`files()` for jpeg-only, raw-only, and pairs.
- **`scan`:** RAF-only stem → promoted shot with capture time from the embedded
  JPEG; CR3-only stem → still `raw_only`; jpeg+raf pair → one shot; sort order
  across mixed jpeg/raf-only shots.
- **`culler` pure:** `r` keymap; `hud_text` `has_raw`/`raw_only`/`showing_raw`
  matrix; `request_current` path selection (unit-testable helper extracted).
- **Integration:** `cargo build && cargo test`; run on a folder of jpeg+raf pairs
  and raf-only files — verify the badge states, the `r` toggle, both warnings,
  and that a raf-only shot culls + applies (moves the `.RAF`).

## Non-goals / preserved behavior

- **No RAW demosaic.** Preview = the camera's embedded JPEG only.
- **No new dependency; no build/portability change.** RAF parse is pure Rust;
  decode stays on the existing `turbojpeg`.
- **Non-RAF RAW formats unchanged** — CR3/NEF/ARW/… only gain preview when a
  future `raw::embedded_jpeg` branch + `preview_supported` entry is added.
- No file-move, session-schema (beyond `jpeg` optionality), or apply-flow change.
- No `culler-core` GUI dependency introduced.

## Resolved decisions

All three review questions were confirmed on 2026-07-13:

1. **Toggle behavior — sticky (confirmed).** `r` is a **sticky global "prefer
   RAW" mode** that persists across navigation, consistent with `Z` zoom — *not*
   per-shot reset.
2. **RAW-only badge — amber (confirmed).** "RAW ONLY" in an informational amber,
   drawn from the DESIGN palette (`theme.slint`), not error-red.
3. **Toast copy — confirmed as written**: `"showing RAW"` / `"showing JPEG"`
   (pair toggle), `"RAW preview unsupported (.<ext>)"` (non-previewable RAW),
   `"no RAW for this shot"` (JPEG-only), `"RAW only — no JPEG to switch to"`
   (RAW-only). Code −1 (no color dot) on all.
