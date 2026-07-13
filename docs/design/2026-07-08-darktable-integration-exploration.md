# FastCull ↔ darktable — Integration Exploration

**Date:** 2026-07-08
**Status:** Exploration / future improvement (NOT part of v1)
**Relates to:** [`docs/specs/2026-07-08-fastcull-design.md`](../specs/2026-07-08-fastcull-design.md)
**Author:** Yoann (with Claude)

---

## 0. What this is

A captured design exploration for coupling FastCull to darktable. It is **not**
scheduled for v1 — the base spec's standalone, move-based Apply stands unchanged.
This documents one specific, chosen integration shape so it can be picked up later
without re-deriving it.

The base spec already has a *loose* darktable touchpoint: it writes XMP
(`xmp:Rating` + `dc:subject`), detects darktable's `IMG_1234.CR3.xmp` sidecar
naming, and uses the `−1` reject convention. This doc goes further, to the
tightest of the coupling options considered.

## 1. Chosen model — "darktable is home"

darktable is the entry point. From the lighttable you fire a shortcut, FastCull
opens for a fast keyboard culling pass, and the decisions flow **back into
darktable** without reorganizing anything on disk.

### Decisions locked in during brainstorming

| Question | Choice | Why |
|---|---|---|
| Entry point | **darktable is home** — a Lua plugin launches FastCull | User lives in darktable; wants FastCull only for the fast cull pass |
| Back-channel | **Results manifest → darktable DB** (via Lua) | DB-native, instant, sidesteps file-move *and* XMP-merge problems |
| File moves in this mode | **None — metadata-only** | darktable tracks files by absolute path; moving them orphans the roll |
| Roll contents | **RAW + JPEG pairs** | FastCull displays the JPEG sibling as it already does — no new decoder |
| Handoff scope | **Selection if any, else whole roll** | Most natural lighttable UX |
| Direction | **One-way (FastCull → darktable) for v1** | Reading darktable metadata back in is a later extension |

### The hard constraint that shapes everything

**darktable owns the files.** Once a folder is imported as a film roll, every
image is tracked by absolute path in `library.db`. FastCull's standalone Apply
*moves files into bucket subfolders* — which in this mode would yank them out from
under darktable and orphan the entire roll ("image not found" on every tile).

Therefore, in darktable mode **FastCull must not move files**. It runs
metadata-only: cull fast, emit decisions, let darktable apply them in place.
Bucket-moving remains the *standalone*-mode behavior only.

## 2. The round trip

```
darktable lighttable
   │  select images (or none) → hit the shortcut
   ▼
fastcull.lua  ──gathers paths──▶  writes manifest.txt (the selection)
   │                                        │
   │  dt.control.execute("fastcull …")      │  (darktable stays responsive)
   ▼                                        ▼
FastCull  ── cull-only mode: NO file moves, NO XMP, NO buckets ──
   │  race through the shots with the keyboard as normal
   │  on finish → emit results.json  ◀── the contract
   ▼
fastcull.lua  ── reads results.json ──▶  write into darktable's DB:
                    image.rating · color label · tags
   ▼
lighttable shows the culling decisions; darktable persists them to its own
sidecars on its normal schedule → decisions become portable after all
```

## 3. The contract (the only thing crossing the boundary)

Keeping the interface to exactly two artifacts is what lets the Rust side and the
Lua side stay independently understandable. **FastCull gains zero knowledge of
darktable** — all darktable-awareness lives in the Lua plugin. FastCull just reads
a folder/list and writes a JSON.

### 3.1 Invocation

A single new flag, `--results <path>`, switches FastCull into **cull-only mode**
(skip the destination/move Apply flow entirely). The target is either a folder or
an explicit file list:

```sh
# roll fallback (whole folder)
fastcull <folder> --results out.json

# selection (explicit paths)
fastcull --from-list manifest.txt --results out.json
```

### 3.2 Results manifest

Keyed by **file paths** (not filename stems — a multi-folder selection could
otherwise collide), containing **only shots that received a decision**:

```json
{
  "fastcull_results_version": 1,
  "shots": [
    { "files": ["/roll/IMG_1234.CR3", "/roll/IMG_1234.JPG"],
      "tier": "best", "rating": 5, "label": "purple",
      "tags": ["portrait", "golden-hour"] }
  ]
}
```

- `files` lists every member of the shot so the plugin can match whichever one
  darktable actually imported (RAW, JPEG, or both — see §5).
- `tier` ∈ `reject | keep | pick | best`. **`rest`/undecided shots are omitted.**
- `rating`/`label` are convenience projections of `tier` (see §6) so the plugin
  needn't know FastCull's tier vocabulary.

### 3.3 Two principles that fall out

1. **Omit undecided/Rest** — darktable never zeroes out metadata for images you
   skipped or left in Rest. Same "don't clobber what exists" caution the base spec
   applies to pre-existing sidecars.
2. **One-way for v1** — FastCull does not pre-read darktable's existing stars; each
   run is a fresh culling pass. Pre-population is a future extension (§7).

## 4. FastCull side (Rust)

The darktable integration adds a thin, standalone-friendly mode. Nothing here
depends on darktable.

### 4.1 New: file-list input

`scan` today does folder → `Vec<Shot>` (flat walk). Add manifest → `Vec<Shot>`:

- Group the listed paths into shots by stem, exactly like the folder walk.
- Paths may span multiple folders.
- **Sibling probe still runs:** for each listed file, probe its own directory for
  the RAW/JPEG sibling by stem — because darktable's "RAW+JPEG" import setting may
  have handed us only the RAW while the JPEG we need to *display* sits next to it
  undisclosed. The manifest defines the *set of shots*; sibling detection still
  walks the immediate directory to complete each shot.

### 4.2 New: cull-only Apply path

When `--results` is present:

- The move/destination Apply flow is disabled. The `A` key (or a dedicated key)
  becomes **"Finish → write results & exit."**
- No bucket creation, no `plan`/`apply` move engine, no XMP writing.
- On finish (and on any clean exit), serialize decisions to the `--results` path
  **atomically** (temp + rename — same discipline as everywhere else).
- Emit only shots with `tier.is_some()` OR non-empty tags. Map tier → rating
  (+ optional label) per §6.

### 4.3 New module: `results` (culler-core)

`Session → ResultsManifest` (JSON). Pure and unit-testable, mirroring the existing
`xmp` / `plan` module style; reuses serde. The tier→rating/label mapping lives here.

### 4.4 What FastCull does NOT gain

No darktable DB access, no Lua, no knowledge of darktable's existence. This clean
separation means FastCull stays a standalone tool and the integration can't
regress its core behavior.

### 4.5 Open: session file location

The resumable `.fastcull.json` session normally lives in the source folder. A
multi-folder **selection** has no single source folder. Options: a FastCull cache
dir keyed by a hash of the manifest, or next to the results path. Roll mode (single
folder) is unaffected. Decide when building (§7).

## 5. darktable side (Lua plugin `fastcull.lua`)

Follows the established darktable Lua external-tool pattern (cf. `ext_editor.lua`
in the official lua-scripts repo).

### 5.1 Registration
- `local dt = require "darktable"`; guard with `dt.configuration.check_version(...)`.
- Register a lighttable shortcut and/or a button in a lib module ("Cull in FastCull").
- A preference for the binary path:
  `dt.preferences.register("fastcull", "bin", "string", …)` (default `fastcull`).
- JSON parsing via the repo's bundled `lib/dkjson` (older darktable Lua has no
  built-in JSON) — a dependency to note.

### 5.2 Gather target images
- `local images = dt.gui.selection()`.
- If empty → fall back to the current roll: resolve the folder from the active
  image's `film`/collection and pass it positionally (folder mode).
- Else → write the selected `image.path/image.filename` list to `manifest.txt` and
  use `--from-list` (selection mode).

### 5.3 Launch (blocking, but darktable stays responsive)
- Write `manifest.txt` and choose `results.json` under `dt.configuration.tmp_dir`.
- `dt.control.execute(bin .. " --from-list " .. manifest .. " --results " .. results)`
  runs in the Lua coroutine — blocks the *script*, not darktable's UI.
- Non-zero exit → notify, apply nothing.

### 5.4 Read results & write back
- Parse `results.json` with dkjson.
- Build a `path → image` lookup from the handed-off images.
- For each result shot, for each darktable image whose path ∈ `shot.files`:
  - `image.rating = rating`  (−1 = rejected)
  - color label: `image.red / yellow / green / blue / purple = true` (if labels on)
  - tags: `local tag = dt.tags.create(t); dt.tags.attach(tag, image)`
- Setting these updates the DB immediately; darktable writes its own XMP sidecar on
  its normal schedule → **decisions become portable through darktable itself**,
  recovering the portability the DB-only back-channel otherwise gives up.

## 6. Tier → darktable mapping

| FastCull tier | darktable rating | color label (optional) |
|---|---|---|
| **Reject** | −1 (rejected) | red |
| **Rest / undecided** | *omitted — untouched* | — |
| **Keep** | 3 | green |
| **Pick** | 4 | blue |
| **Best** | 5 | purple |

Tags (`dc:subject` keywords in standalone mode) → darktable tags via `dt.tags`.

**Open — color-label default.** Some darktable users assign their own meaning to
color labels. Safer default: **ratings always, color labels opt-in** via a plugin
preference, so the integration never stomps a user's existing label scheme.

## 7. Future improvements / open questions

- **Two-way sync (depth 4).** Pre-read darktable's existing ratings/labels so
  FastCull starts a re-cull already populated. Needs conflict resolution — the base
  spec's deferred XMP-merge concern in DB form.
- **Pushing clears/demotions.** Currently Rest is omitted, so culling a 4★ image as
  Rest leaves darktable at 4★. A `--push-clears` mode could demote explicitly.
- **Session location** for multi-folder selections (§4.5).
- **darktable groups.** How RAW+JPEG *groups* in darktable interact with shot-level
  decisions (apply to group leader vs. all members).
- **RAW-only rolls.** If the workflow ever shifts to RAW-only imports, FastCull
  needs full-size embedded-preview extraction (base-spec phase 2), *or* could read
  darktable's mipmap cache to avoid its own RAW decode (fragile, undocumented).
- **Belt-and-suspenders portability.** Optionally also write XMP directly from
  FastCull, not only rely on darktable's sidecar writer.
- **darktable Lua API version compatibility** across releases.
- **Packaging.** Install path `~/.config/darktable/lua/` + `luarc` enable line.

## 8. Why this shape (summary of the reasoning)

- **darktable owns the files** → FastCull can't move them here → metadata-only mode.
- **DB write-back beats XMP-in-place** → avoids the XMP-merge/reload problem and is
  how native darktable plugins behave; portability is recovered via darktable's own
  sidecar writer.
- **JSON-file-as-sole-contract** → the two codebases stay independently testable and
  FastCull keeps zero darktable dependency.
- **Omit-undecided** → never clobbers metadata the user didn't touch.
