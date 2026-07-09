# FastCull — Design Spec

**Date:** 2026-07-08
**Status:** Draft — rev 3 (2026-07-09: §8 cross-FS durability ordering fixed, journal lifecycle + resume reconciliation specified, in-flight-apply breadcrumb added; rev 2 folded v1 review feedback)
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
- Classify each shot with a single keypress — Keep / Pick / Best / Reject, or
  leave it in the residual **Rest** — plus free-form tags.
- Treat a RAW + JPEG pair as **one shot** — the RAW sibling travels with the JPEG.
- Nothing touches disk until **Apply**; decisions are resumable across sessions.
- On Apply, **safely move** files into an organized destination; write tags and
  a tier rating as XMP.
- Never lose data — **v1 performs no deletions at all**; every operation is a
  verified move, and a crash mid-apply is recoverable from a journal.

**Non-goals (v1)**
- RAW demosaicing / true RAW rendering (we always display the JPEG).
- Photo editing, adjustments, export/resize.
- Cross-platform packaging (Linux-only, personal tool).
- Cloud, catalog database, or multi-user features.
- Color management (AdobeRGB JPEGs render unmanaged and look slightly flat —
  acceptable for culling decisions).
- Permanent deletion (phase-2 opt-in; v1 never unlinks anything).

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
| EXIF | **kamadak-exif** | Orientation (upright portraits), capture-time sort, embedded thumbnail extraction. |
| Safe fs ops | **rustix** (or `nix`) | `renameat2(RENAME_NOREPLACE)` for no-clobber renames — std doesn't expose it. |
| Concurrency | std threads + channels; UI updates via `slint::invoke_from_event_loop` | Decode off the UI thread. |
| Persistence | **serde / serde_json** | Resumable session sidecar + apply journal. |
| Tags out | **quick-xml** (hand-written XMP) | `dc:subject` keywords + `xmp:Rating` sidecars readable by Lightroom / darktable / Bridge. |

## 5. Domain model

A **Shot** groups all files sharing a filename stem:

- `IMG_1234.JPG` (display file, required in v1)
- `IMG_1234.CR3` (optional RAW sibling — travels with the JPEG)
- optional pre-existing sidecar — **both** `IMG_1234.xmp` (Adobe convention)
  **and** `IMG_1234.CR3.xmp` (darktable convention) are detected and carried.

**Decision** (per shot):

```rust
struct Decision {
    tier: Option<Tier>,   // None = undecided → lands in 01_rest on Apply
    tags: Vec<String>,
    visited: bool,        // set the first time the shot is shown in the loupe
}
enum Tier { Reject, Keep, Pick, Best }
```

| Tier | Meaning | Destination bucket | XMP rating |
|---|---|---|---|
| *(none)* / **Rest** | **Default / residual** — never keyed, or explicitly cleared. | `01_rest` | — |
| `Keep` | Usable. | `02_keep` | 3 |
| `Pick` | A select. | `03_picks` | 4 |
| `Best` | Portfolio / hero shot. | `04_bests` | 5 |
| `Reject` | Rejected. **Moved to `00_rejected` on Apply — never deleted by v1.** | `00_rejected` | −1 |

Quality ladder: `Reject < Rest < Keep < Pick < Best`.

**Why `Option<Tier>` + `visited`:** "never looked at" and "looked at, left in
the residual" must be distinguishable — `Tab` (next unvisited) and the progress
HUD depend on it. Both undecided and explicitly-cleared shots land in `01_rest`
at Apply; `visited` is what separates seen from unseen.

**Naming note:** rev 1 called the residual tier *Culled*. Renamed **Rest**
because "culled" conventionally means *rejected*, and a `01_culled` folder
would read as "safe to delete" six months later.

**Tags:** free-form text, multiple per shot, independent of tier. Autocomplete
from previously-used tags. Written as XMP `dc:subject` keywords on Apply.

**Undo:** the model keeps a bounded undo stack of `(shot index, previous
Decision)`; `U` reverts the last tier/tag change. Cheap because state
transitions are pure.

**Session:** source dir + `Vec<Shot>` + decisions + current index + the
in-flight Apply destination (`pending_apply`, normally absent; set just before
an apply starts and cleared on success — the crash-detection breadcrumb,
rev 3). Held in memory, autosaved to **`.fastcull.json` in the source
folder**. Saves are
atomic (write temp + rename — same discipline as file moves). Decisions are
keyed by filename stem so resume re-attaches them after a rescan. A corrupt
session file is renamed to `.fastcull.json.bad` and reported; a fresh session
starts rather than silently overwriting evidence.

## 6. Workflow

### Load
Point FastCull at a source folder → `scan` walks it **flat** (non-recursive;
subdirectories ignored), groups files into shots by stem (extension matching
case-insensitive), detects RAW siblings and both sidecar conventions, and
sorts by **(EXIF DateTimeOriginal, SubSecTimeOriginal, filename)** — the
tiebreakers keep burst order stable across sessions, so the filmstrip never
shuffles on resume.

### Cull
Per shot: assign a tier (single keypress, **auto-advances** to the next shot;
default on, `--no-auto-advance` to disable) and optionally tags. `F` filters
the working set by tier for second passes (refine Keeps into Picks, Picks into
Bests, review Rejects). `Tab` jumps to the next unvisited shot. `U` undoes.
All decisions live in memory and autosave to the session sidecar — quit and
resume freely.

### Apply
Nothing touches disk until here.
1. Choose a **destination folder** — any folder **except the source itself**.
   A fresh subfolder of the source is allowed and gives in-place organizing
   (`shoot/sorted/`).
2. `plan` computes the operation list and shows a **preview**: counts per
   bucket (including rejects), collision resolutions, the number of
   **unrecognized files that stay behind** (videos, voice memos, …), and a
   free-space check when the move crosses filesystems.
3. On **confirm**, `apply` executes a **safe move**:
   - Creates `00_rejected / 01_rest / 02_keep / 03_picks / 04_bests` in the
     destination.
   - **Journals first:** the in-flight destination is recorded in the session
     sidecar (breadcrumb), then the plan is serialized to
     `dest/.fastcull-apply.json` before the first move, and each operation is
     marked complete as it executes. Because the journal lives in the
     *destination* while the next launch opens the *source*, the breadcrumb is
     what makes "detected on next launch" actually work: a crash mid-apply is
     detected via the session's pending destination and offered as
     resume-or-report — never a forensic mystery. On success the journal is
     removed and the breadcrumb cleared.
   - Moves each shot's fileset (JPEG + RAW + existing sidecar) into its bucket
     — **including rejects into `00_rejected`. Nothing is unlinked.**
   - Writes a fresh `.xmp` sidecar (tags + `xmp:Rating`) for each shot that
     has a tier or tags — **unless the shot already had a sidecar**, which is
     carried unmodified and the skipped tag-write reported (merging into
     existing XMP is phase 2; overwriting someone's edit history is data loss
     through the front door).
   - On success, the session file moves into the destination as the audit
     record of what was decided.

**Consequence of move semantics:** the source folder ends up empty except
unrecognized files (and the destination itself, if you chose a subfolder).
Rejects sit in `00_rejected` for one last human glance — delete that folder by
hand, or use the phase-2 opt-in. No duplication, no extra disk.

Bucket names above are defaults, overridable by **CLI flags** (no config file
in v1).

## 7. Architecture

Two crates. All domain logic lives outside Slint so it is unit-testable and the
GUI stays swappable.

### `culler-core` (library, zero GUI deps)

| Module | Responsibility | Depends on |
|---|---|---|
| `model` | `Shot`, `Tier`, `Decision` (`Option<Tier>` + `visited`), `Session`, undo stack; pure state transitions. The heart. | — |
| `scan` | Folder → `Vec<Shot>`; flat walk, pair by stem, detect RAW + both sidecar conventions, stable capture-time sort. | fs, kamadak-exif |
| `decode` | `(path, target_size)` → `DecodedImage { w, h, rgba }`, EXIF-oriented; also extracts embedded EXIF thumbnails. Emits plain RGBA (no Slint types). | turbojpeg, fast_image_resize, kamadak-exif |
| `persist` | `Session` ⇄ JSON sidecar; atomic writes. | serde_json |
| `xmp` | Tags + rating → `.xmp` string; write sidecar. | quick-xml |
| `plan` | `(Session, dest)` → `ApplyPlan` (moves + collisions + stale/skipped + leftover report). **Pure, no I/O.** Powers the preview. | model |
| `apply` | Execute an `ApplyPlan` safely through a small **`FsOps` trait** (rename / copy / fsync / mkdir) so tests can inject EXDEV, ENOSPC, and surprise collisions; maintains the journal. The only dangerous unit; most-tested. | fs, xmp |

### `culler` (binary, Slint)

| Module | Responsibility |
|---|---|
| `ui` (`.slint` + glue) | Filmstrip, loupe (fit + sticky 1:1 zoom/pan), HUD (tier, tags, per-tier counts, visited progress), tag entry, filter state, Apply dialog. |
| `pipeline` | Background decode workers, bounded LRU cache, ±N neighbor prefetch, **generation-counter latest-wins scheduling** (stale requests dropped at dequeue and at delivery); marshals `DecodedImage` → `slint::Image` via `invoke_from_event_loop`. |
| `input` | Keymap: key → action → mutate `model`. |
| `main` | Parse source dir + flags, build/resume session, spawn workers, run event loop. |

**Data flow:** `scan → Session`; loop = `input mutates Session` + `pipeline
decodes/displays` + `persist autosaves`; Apply = `plan → preview → confirm →
journal → apply` (+ `xmp` sidecars).

## 8. Safe-move engine

Each shot's fileset `{JPEG, RAW?, .xmp?}` is moved as a **group**.

- **Same filesystem** → `renameat2(RENAME_NOREPLACE)` (atomic, instant,
  no-clobber). Plan-time collision checks are advisory only — a file that
  appears in the destination between plan and apply must **fail loudly**,
  never be silently overwritten.
- **Cross-filesystem** (`EXDEV`) → copy to `dest/.name.partial` opened with
  `create_new` → `fsync` file → verify byte length (BLAKE3 hash is a phase-2
  paranoia setting) → `rename(.partial → final)` with `NOREPLACE` → **`fsync`
  the parent directory** → only then remove source. The directory fsync must
  come *after* the publish rename, not before: the source unlink happens on a
  *different filesystem*, so power loss could otherwise persist the unlink
  while the rename is lost — leaving the data reachable only as a hidden
  `.partial`. A mid-copy failure leaves the source **untouched** and cleans up
  the partial. *(rev 3: rev 2 ordered the dir fsync before the rename, which
  made the `.partial` entry durable instead of the final one.)*
- **Preflight** → before the first cross-FS copy, check destination free space
  (`statvfs`) against the plan's total byte count; refuse rather than abort
  halfway.
- **Journal** → the serialized plan in the destination records per-file status
  as apply proceeds; a crashed or aborted run is resumable. **Lifecycle
  (rev 3):** before the first move, the session sidecar records the in-flight
  destination (breadcrumb) so the next launch on the source folder can find
  the journal; on full success the journal is **removed** and the breadcrumb
  cleared (the journal is FastCull's own bookkeeping, not user data — the §2
  no-deletion guarantee protects photos, not our metadata). A journal
  containing any non-`Done` entry marks a crashed run; an all-`Done` journal
  must never be mistaken for one.
- **Resume reconciliation (rev 3)** → a crash can land between a completed
  move and its journal update (or the reverse: journal fsynced, rename lost).
  `resume` therefore reconciles the journal against the disk in both
  directions before executing: a `Pending` move whose source is gone and
  whose destination exists is treated as done; a `Done` move whose
  destination is missing while the source still exists is re-executed. Resume
  must never fail with a spurious error on work a crashed run already did.
- **Sidecar writes are no-clobber too** → freshly written `.xmp` sidecars are
  published exactly like moves (write temp → rename with `NOREPLACE`); no
  destination write path may silently overwrite. Re-running a sidecar write on
  resume skips an already-present target instead of failing.
- **Collisions** → auto-suffix the whole stem consistently
  (`IMG_1234` → `IMG_1234-1`) so JPEG/RAW/xmp stay matched; surfaced in the preview.
- **Group atomicity** → if a later file in a shot fails, already-moved files
  are recorded in the journal and the run **stops** with a precise report.
- **No deletion step exists in v1.** Rejects are moved like every other
  bucket, so the rev-1 "all moves verify before any delete" ordering machinery
  is gone. Opt-in permanent deletion of `00_rejected` is phase 2.

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
| `1` | Keep (auto-advance) |
| `2` | Pick (auto-advance) |
| `3` | Best (auto-advance) |
| `X` | Reject (auto-advance) |
| `` ` `` / `0` | clear → undecided (Rest) |
| `U` | undo last tier/tag change |
| `T` | tag entry (autocomplete; comma-separates) |
| `Z` | toggle 1:1 zoom; **zoom level and pan position persist across prev/next** so a burst can be flipped through comparing focus on the same spot |
| `F` | cycle tier filter: All → ≥Keep → ≥Pick → ≥Best → Rejects |
| `Tab` | jump to next **unvisited** shot |
| `A` | open Apply dialog (destination + preview + confirm) |
| `Ctrl+S` | force-save session (also autosaves) |

HUD shows the current shot's tier and tags, per-tier counts, and visited
progress (e.g. `seen 1200/2000`) — real completion, not just position.

Filmstrip is color-coded so progress is visible at a glance: grey =
rest/undecided (unvisited rendered dimmer), green = keep, blue = pick,
gold = best, red = reject.

## 10. Error handling

- Corrupt / undecodable JPEG → placeholder tile, still classifiable, logged.
- Stem with only RAW (no JPEG) → "no preview" placeholder in v1, still movable
  (embedded-preview extraction is a phase-2 item).
- Source changed under you → `plan` re-verifies existence, skips + reports stale entries.
- Permissions / disk-full / cross-FS → surfaced per file; the journal makes a
  partial run resumable, and since **nothing is ever deleted**, no failure
  mode loses data.
- Destination = the source folder itself → detected and refused (a subfolder
  is fine).
- Shot already has a sidecar + new tags to write → sidecar carried untouched,
  tag-write skipped and reported (merge is phase 2).
- Corrupt `.fastcull.json` → renamed to `.bad`, fresh session, reported.
- AdobeRGB JPEGs render unmanaged (slightly desaturated) — known v1 limitation.

## 11. Testing strategy

- **Highest value — `plan` / `apply`:** temp-dir fixtures for same-FS rename,
  injected-`EXDEV` copy-verify path, collision auto-suffix, **collision
  appearing between plan and apply (NOREPLACE fails loudly)**, group
  atomicity, **crash-mid-apply → journal recovery resumes correctly**,
  **crash *between* a move and its journal update → resume reconciles instead
  of erroring (both directions)**, **sidecar writes refuse to clobber and are
  skip-idempotent on resume**, **journal removed on success (an all-`Done`
  journal never triggers a recovery offer or hijacks a later apply into the
  same dest)**, and ENOSPC / permission failures injected via the `FsOps`
  trait. This is where data loss would occur, so it gets the most coverage.
- `model` — state-transition unit tests: tier changes, `Option<Tier>` +
  `visited` semantics, undo stack, per-tier counts.
- `scan` — fixture dirs → correct pairing / RAW detection / both sidecar
  conventions / stable burst ordering.
- `xmp` — generated sidecar contains expected `dc:subject` + `xmp:Rating`;
  round-trips; existing-sidecar shots are skipped, not overwritten.
- `persist` — save/load round-trip; truncated/corrupt file rejected cleanly.
- `decode` — smoke tests on sample images (dimensions + orientation correctness).

## 12. Performance strategy

- turbojpeg **scaled decode** for thumbnails and loupe previews.
- **Embedded EXIF thumbnails** for filmstrip first paint — they live in the
  first few KB of each file, so a 2,000-shot strip appears near-instantly even
  from slow media; real scaled decodes refine tiles lazily.
- Bounded **LRU cache** (memory budget) for fit-size textures. **1:1 decodes
  bypass it** — a 45 MP frame is ~180 MB of RGBA, so full-res gets a single
  slot (current shot only) rather than evicting every prefetched neighbor.
- **Prefetch** ±N neighbors so navigation is instant.
- **Latest-wins scheduling:** a generation counter invalidates stale decode
  requests at dequeue and at delivery, so holding `→` through 50 frames never
  builds a backlog that rubber-bands the UI.
- Loupe shows the best already-cached scale immediately (upscaled thumbnail if
  that's all we have), replaced when the target decode lands.
- **Virtualized** filmstrip (only visible + buffer built).
- **All decode off the UI thread**; UI never stalls.
- Target: thousands of shots, sub-frame navigation.

## 13. Phasing (YAGNI)

**v1**
- Filmstrip + loupe + sticky 1:1 zoom.
- 4 explicit tiers + residual Rest + free-form tags; auto-advance; tier
  filter; undo; visited tracking.
- Resumable session sidecar (atomic saves).
- Safe-move Apply into destination buckets — journaled, crash-recoverable,
  no-clobber renames, preview + confirm. **Zero deletions.**
- XMP sidecars: `dc:subject` keywords + tier as `xmp:Rating`.

**Phase 2 (deferred)**
- Grid / contact-sheet view.
- RAW-only shots via embedded-preview extraction.
- **Opt-in permanent deletion of `00_rejected`** (the only irreversible
  operation, deliberately excluded from v1).
- Merging tags into pre-existing XMP sidecars.
- Optional BLAKE3 verification on cross-FS moves.
- Color management for AdobeRGB previews.

## 14. Decisions log

| Question | Decision |
|---|---|
| Language | Rust |
| GUI framework | Slint (over egui / Tauri) |
| Formats | JPEG display; RAW+JPEG paired by stem; RAW-only deferred |
| Classification | Tiers {Rest (residual), Keep, Pick, Best, Reject} + free-form tags |
| Undecided vs. residual | `Decision.tier: Option<Tier>` + `visited` flag — distinguishes "never seen" from "seen, left in Rest"; `Tab` targets unvisited; HUD shows real progress *(rev 2)* |
| Residual naming | "Culled" → "Rest" — *culled* reads as *rejected*; `01_culled` would invite a mistaken `rm` later *(rev 2)* |
| Timing | Batch; decisions in memory + resumable sidecar; Apply commits |
| Reject meaning | rev 1: permanently deleted on Apply after confirm → **rev 2: moved to `00_rejected`; v1 never unlinks; permanent delete is a phase-2 opt-in.** Rationale: reuses the safe-move engine, removes all irreversible ops from v1, collapses the moves-before-deletes machinery |
| Output location | New destination folder chosen at Apply — anywhere except the source root itself; a source subfolder enables in-place organizing *(rev 2)* |
| Output op | **Move** (safe), not copy — source consumed, no duplication |
| Tags output | XMP `dc:subject` sidecars |
| Rating output | Tier → `xmp:Rating` {Keep 3, Pick 4, Best 5, Reject −1} in v1 — the sidecar writer exists anyway; −1 is the Bridge/darktable reject convention *(rev 2)* |
| Sidecar convention | Write `stem.xmp` (Adobe style); scan detects both `stem.xmp` and `file.ext.xmp` (darktable); pre-existing sidecars are carried untouched and tag-writes skipped + reported — verify one real import in the downstream editor *(rev 2)* |
| Auto-advance | On by default; `--no-auto-advance` *(rev 2)* |
| Crash safety | Apply journal in destination; `RENAME_NOREPLACE` everywhere; free-space preflight; atomic session saves *(rev 2)* |
| Crash-safety hardening | Cross-FS dir fsync moved *after* the publish rename; resume reconciles journal↔disk in both directions; sidecar writes no-clobber + skip-idempotent; journal removed on success; `pending_apply` breadcrumb in the session makes next-launch crash detection real *(rev 3)* |
| Configuration | Bucket names via CLI flags; no config file in v1 *(rev 2)* |
