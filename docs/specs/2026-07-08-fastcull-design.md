# FastCull — Design Spec

**Date:** 2026-07-08
**Status:** Draft for review
**Author:** Yoann (with Claude)

---

## 1. Overview

FastCull is a fast, keyboard-driven **photo culling tool** for a personal
photoshooting workflow. You point it at a folder of shots, race through them
with single keypresses to sort each into a quality tier (and optionally tag
them), and on **Apply** it safely reorganizes everything into a clean
destination-folder structure.

It occupies the same niche as Photo Mechanic / FastRawViewer: the *culling*
step that happens before serious editing — done fast, with the keyboard, on
large shoots.

## 2. Goals & non-goals

**Goals**
- Open a source folder and navigate its images fast (thousands of shots, sub-frame navigation).
- Classify each shot into one of four tiers with a single keypress, plus free-form tags.
- Treat a RAW + JPEG pair as **one shot** — the RAW sibling travels with the JPEG.
- Nothing touches disk until **Apply**; decisions are resumable across sessions.
- On Apply, **safely move** files into an organized destination and write tags as XMP.
- Never lose data — verify before any deletion.

**Non-goals (v1)**
- RAW demosaicing / true RAW rendering (we always display the JPEG).
- Photo editing, adjustments, export/resize.
- Cross-platform packaging (Linux-only, personal tool).
- Cloud, catalog database, or multi-user features.

## 3. Context

- **Audience:** just the author, running on one Linux machine.
- **Formats:** shoots are `jpg + raw` pairs, or `jpg`-only. There is always a
  JPEG to display; RAW is never the display source.
- **Priorities:** build speed and raw runtime performance over UI polish and packaging.

## 4. Stack

| Layer | Choice | Rationale |
|---|---|---|
| Language | **Rust** | Fast local image work; author's choice. |
| GUI | **Slint** (Skia renderer) | Declarative `.slint` UI + Rust logic. Decoded frames fed as `SharedPixelBuffer`; `VecModel` for the filmstrip; `FocusScope` for keys. |
| JPEG decode | **turbojpeg** (libjpeg-turbo) | Scaled decode (½/¼/⅛) makes thumbnails and loupe previews cheap. `zune-jpeg` is the pure-Rust fallback if avoiding the system lib is preferred. |
| Downscale | **fast_image_resize** | SIMD resize where turbojpeg can't scale directly. |
| EXIF | **kamadak-exif** | Orientation (upright portraits) + capture-time sort. |
| Concurrency | std threads + channels; UI updates via `slint::invoke_from_event_loop` | Decode off the UI thread. |
| Persistence | **serde / serde_json** | Resumable session sidecar. |
| Tags out | **quick-xml** (hand-written XMP) | `dc:subject` keyword sidecars readable by Lightroom / darktable / Bridge. |

## 5. Domain model

A **Shot** groups all files sharing a filename stem:

- `IMG_1234.JPG` (display file, required in v1)
- `IMG_1234.CR3` (optional RAW sibling — travels with the JPEG)
- `IMG_1234.xmp` (optional pre-existing sidecar — carried along)

**Tier** (mutually exclusive, one per shot):

| Tier | Meaning | Destination bucket |
|---|---|---|
| `Culled` | **Default / residual** — looked at but not promoted. **Not deleted.** | `01_culled` |
| `Keep` | Usable. | `02_keep` |
| `Pick` | A select. | `03_picks` |
| `Best` | Portfolio / hero shot. | `04_bests` |
| `Delete` | Rejected. **Permanently deleted** on Apply (after confirm). | — (removed) |

A shot with no keypress stays `Culled`. Quality ladder:
`Delete < Culled < Keep < Pick < Best`.

**Tags:** free-form text, multiple per shot, independent of tier. Autocomplete
from previously-used tags. Written as XMP `dc:subject` keywords on Apply.

**Session:** source dir + `Vec<Shot>` + per-shot decisions (tier, tags) +
current index. Held in memory, autosaved to a JSON sidecar for resume.

## 6. Workflow

### Load
Point FastCull at a source folder → `scan` groups files into shots by stem,
detects RAW extensions and existing sidecars, sorts by EXIF capture time.

### Cull
Per shot: assign a tier (single keypress) and optionally tags. All decisions
live in memory and autosave to a sidecar — quit and resume freely.

### Apply
Nothing touches disk until here.
1. Choose a **destination folder** (must not overlap the source).
2. `plan` computes the operation list (moves + deletes + collisions) and shows a
   **preview**: counts per bucket, collision resolutions, delete count.
3. On **confirm**, `apply` executes a **safe move**:
   - Creates `01_culled / 02_keep / 03_picks / 04_bests` in the destination.
   - Moves each non-deleted shot's files (JPEG + RAW + existing sidecar) into its bucket.
   - Writes a new `.xmp` sidecar carrying that shot's tags.
   - **After all moves verify**, permanently deletes the `Delete`-marked source files.

**Consequence of move semantics:** the source folder ends up **empty** (except
any unrecognized files); the destination becomes the organized shoot. No
duplication, no extra disk.

Folder names (`01_culled` etc.) are the default and configurable.

## 7. Architecture

Two crates. All domain logic lives outside Slint so it is unit-testable and the
GUI stays swappable.

### `culler-core` (library, zero GUI deps)

| Module | Responsibility | Depends on |
|---|---|---|
| `model` | `Shot`, `Tier`, `Decision`, `Session`; pure state transitions. The heart. | — |
| `scan` | Folder → `Vec<Shot>`; pair by stem, detect RAW/sidecar, sort by capture time. | fs, kamadak-exif |
| `decode` | `(path, target_size)` → `DecodedImage { w, h, rgba }`, EXIF-oriented. Emits plain RGBA (no Slint types). | turbojpeg, fast_image_resize, kamadak-exif |
| `persist` | `Session` ⇄ JSON sidecar. | serde_json |
| `xmp` | Tags → `.xmp` string; write sidecar. (Tier-as-rating is phase 2.) | quick-xml |
| `plan` | `(Session, dest)` → `ApplyPlan` (Moves + Deletes + collisions). **Pure, no I/O.** Powers the preview. | model |
| `apply` | Execute an `ApplyPlan` safely. The only dangerous unit; most-tested. | fs, xmp |

### `culler` (binary, Slint)

| Module | Responsibility |
|---|---|
| `ui` (`.slint` + glue) | Filmstrip, loupe (fit + 1:1 zoom/pan), HUD, tag entry, Apply dialog. |
| `pipeline` | Background decode workers, bounded LRU cache, ±N neighbor prefetch; marshals `DecodedImage` → `slint::Image` via `invoke_from_event_loop`. |
| `input` | Keymap: key → action → mutate `model`. |
| `main` | Parse source dir, build/resume session, spawn workers, run event loop. |

**Data flow:** `scan → Session`; loop = `input mutates Session` + `pipeline
decodes/displays` + `persist autosaves`; Apply = `plan → preview → confirm →
apply` (+ `xmp` sidecars).

## 8. Safe-move engine

Each shot's fileset `{JPEG, RAW?, .xmp?}` is moved as a **group**.

- **Same filesystem** → `fs::rename` (atomic, instant).
- **Cross-filesystem** (`EXDEV`) → copy to `dest/.name.partial` → `fsync` →
  verify byte length (optional BLAKE3 hash as a paranoia setting) →
  `rename(.partial → final)` → only then remove source. A mid-copy failure
  leaves the source **untouched** and cleans up the partial.
- **Collisions** → auto-suffix the whole stem consistently
  (`IMG_1234` → `IMG_1234-1`) so JPEG/RAW/xmp stay matched; surfaced in the preview.
- **Group atomicity** → if a later file in a shot fails, already-moved files are
  logged and the run **stops** with a precise report; nothing is deleted.
- **Ordering guarantee** → **all** moves complete and verify **before any**
  delete-marked file is unlinked. Deletes are the final, irreversible step,
  gated behind the explicit confirm.
- **Optional safety net** → "send deletes to trash / `_deleted/` instead of
  unlink" toggle. Defaults to permanent delete (as requested).

## 9. UX & keymap

```
┌───────────────────────────────────────────────┬───────┐
│                                                │  HUD  │
│            LOUPE  (fit  ·  1:1 zoom + pan)      │ tier  │
│                                                │ tags  │
│                                                │ 47/312│
├───────────────────────────────────────────────┴───────┤
│  ▚▚▚ filmstrip, color-coded by tier ▚▚▚   ◀ ▶          │
└────────────────────────────────────────────────────────┘
```

| Key | Action |
|---|---|
| `←/→`, `space`/`backspace` | prev / next |
| `1` | Keep |
| `2` | Pick |
| `3` | Best |
| `X` | Delete |
| `` ` `` / `0` | unset → Culled |
| `T` | tag entry (autocomplete; comma-separates) |
| `Z` | toggle 1:1 zoom (focus/sharpness check); arrows/drag to pan |
| `Tab` | jump to next unrated shot |
| `A` | open Apply dialog (destination + preview + confirm) |
| `Ctrl+S` | force-save session (also autosaves) |

Filmstrip is color-coded so progress is visible at a glance:
grey = culled, green = keep, blue = pick, gold = best, red = delete.

## 10. Error handling

- Corrupt / undecodable JPEG → placeholder tile, still classifiable, logged.
- Stem with only RAW (no JPEG) → "no preview" placeholder in v1, still movable
  (embedded-preview extraction is a phase-2 item).
- Source changed under you → `plan` re-verifies existence, skips + reports stale entries.
- Permissions / disk-full / cross-FS → surfaced per file; verify-before-delete
  means a failure never loses data.
- Destination overlapping the source → detected and refused.

## 11. Testing strategy

- **Highest value — `plan` / `apply`:** temp-dir fixtures for same-FS rename,
  simulated cross-FS copy-verify-delete, collision auto-suffix, group
  atomicity, moves-before-deletes, and failure-aborts-deletes. This is where
  data loss would occur, so it gets the most coverage.
- `model` — state-transition unit tests (tier changes, residual default, counts).
- `scan` — fixture dirs → correct pairing / RAW detection / sort.
- `xmp` — generated sidecar contains expected `dc:subject`; round-trips.
- `persist` — save/load round-trip.
- `decode` — smoke tests on sample images (dimensions + orientation correctness).

## 12. Performance strategy

- turbojpeg **scaled decode** for thumbnails and loupe previews.
- Bounded **LRU cache** (memory budget) for decoded textures.
- **Prefetch** ±N neighbors so navigation is instant.
- **Virtualized** filmstrip (only visible + buffer built).
- **All decode off the UI thread**; UI never stalls.
- Target: thousands of shots, sub-frame navigation.

## 13. Phasing (YAGNI)

**v1**
- Filmstrip + loupe + 1:1 zoom.
- 4 tiers + free-form tags.
- Resumable session sidecar.
- Safe-move Apply into destination buckets, with preview + confirm.
- XMP keyword sidecars.

**Phase 2 (deferred)**
- Grid / contact-sheet view.
- RAW-only shots via embedded-preview extraction.
- Tier written as XMP rating/label.
- Trash-instead-of-delete option.
- Optional BLAKE3 verification on cross-FS moves.

## 14. Decisions log

| Question | Decision |
|---|---|
| Language | Rust |
| GUI framework | Slint (over egui / Tauri) |
| Formats | JPEG display; RAW+JPEG paired by stem; RAW-only deferred |
| Classification | Tiers {Culled, Keep, Pick, Best, Delete} + free-form tags |
| Timing | Batch; decisions in memory + resumable sidecar; Apply commits |
| Culled meaning | Residual (not promoted); **not** deleted |
| Delete meaning | Permanently deleted on Apply, after confirm |
| Output location | New destination folder chosen at Apply |
| Output op | **Move** (safe), not copy — source consumed, no duplication |
| Tags output | XMP `dc:subject` sidecars |
