# FastCull Phase 6 — Slint GUI Binary — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans. Steps use `- [ ]`. Canonical types + keymap in [README.md](README.md). **Depends on Phases 1–5 (all of culler-core).**

**Goal:** Build the `culler` binary — a keyboard-driven Slint GUI (loupe, color-coded virtualized filmstrip, HUD, tag entry, Apply dialog) that renders `culler-core` off the UI thread and never touches disk until Apply.

**Architecture:** Thin Slint glue over unit-tested pure logic. All keymap/filter/scheduling/apply-preview decisions live in pure Rust functions (`key_to_action`, `apply_action`, `passes`, `next_filter`, `Scheduler`, `LruCache`, `prefetch_set`, `dest_is_source_root`, `gather_apply_inputs`, `build_preview`, `find_crashed_apply`) with real `#[test]`s; only rendering, threading, and `slint::invoke_from_event_loop` marshaling are manually verified. The core stays swappable — the binary only translates UI events into `culler-core` calls and marshals `DecodedImage → slint::Image` at the boundary.

**Tech Stack:** Rust 2021, Slint 1.8 (Skia renderer), std threads + channels, clap (derive) for args, serde_json for journal reads, tempfile (dev) for temp-dir tests.

## Visual design source (authoritative)

The look of this GUI is fixed by the imported Claude Design project, vendored at **[`docs/design/`](../../../design/)**. That directory — **not** this plan's placeholder colors — is the authoritative visual source. Before writing any `.slint`:

1. Read **[`docs/design/DESIGN.md`](../../../design/DESIGN.md)** — design tokens, the `theme.slint` contract (§3), per-screen anatomy (§4), the **HTML→Slint translation notes** (§5, read before your first `.slint`), and the **v1 scope reconciliation** (§6).
2. Open the matching **`docs/design/screens/*.png`** for the surface you're building (or `FastCull-UI.rendered.html` in a browser).
3. Pull every color/size/font from `Theme` (`culler/ui/theme.slint`, built in **Task 1b**). **Never hardcode a hex** that lives in `Theme`. The ad-hoc hex in the task code below (`#141414`, `#f0a35e`, `#99bbee`, `#f85149`, `#ccaa88`, …) are **placeholders** — replace them with `Theme.*`.

**Task → screen map:**

| Task | Builds | Design screen(s) |
|---|---|---|
| **1b** | `theme.slint` token global | all (the token source) |
| **8** | loupe + filmstrip | `1b-main` |
| **9** | HUD + tag entry | `1b-main` (HUD), `2f-tag-entry` |
| **10** | sticky 1:1 zoom/pan | `1b-main` |
| **11** | startup scan/resume, crash detection | `2a-startup`, `2d-crash-recovery` |
| **12** | Apply dialog / progress / resume | `2b-apply-dialog`, `2c-apply-progress`, `2d-crash-recovery` |
| **13** | visual-fidelity pass | all |

Two design surfaces — **`2e` keymap sheet** and **`2g` toasts** — have no dedicated task; they are cheap, recommended **Optional** additions (DESIGN.md §6). Fold them into Task 9/10 if time allows. Elements marked **Defer** in DESIGN.md §6 (startup recents, HUD histogram, HUD EXIF line) are intentionally out of v1 — don't build them, and don't let the Task 13 fidelity check flag their absence.

## Global Constraints

Copied verbatim from [README.md](README.md); every task's requirements implicitly include this section.

- **Language / edition:** Rust, edition 2021. Workspace with two member crates: `culler-core` (lib) and `culler` (bin). This phase builds `culler`.
- **`culler-core` has zero GUI dependencies.** No `slint`, no Slint types, in the library. `decode` emits plain `Vec<u8>` RGBA, never `slint::Image`. The binary is the *only* place `DecodedImage → slint::Image` marshaling happens (via `SharedPixelBuffer`), at the boundary.
- **v1 performs no deletions of user data.** Rejects are **moved** to `00_rejected`, never deleted. The binary never unlinks a source shot; it only calls `culler_core::apply`, which owns the sole cross-FS source-removal path.
- **Nothing touches disk until Apply.** All culling decisions live in memory + the autosaved session sidecar. `plan` is pure and performs **no I/O**; the binary gathers `existing`/`sizes` via readdir/stat and hands them in.
- **Atomic writes everywhere:** session saves and journal writes use write-temp-then-rename (owned by core). The binary never writes the session or journal by hand except relocating the session file into `dest` on success.
- **A destination file appearing between plan and apply must fail loudly** (NOREPLACE returns `EEXIST`) — surfaced by `apply`, reported by the binary; never silently overwrite.
- **Decisions are keyed by filename stem** so resume re-attaches them after a rescan. Corrupt session file → renamed to `.fastcull.json.bad`, reported, fresh session started (handled by `load_or_fresh`).
- **Destination = source root itself is refused** (a source *subfolder* is allowed). Pure guard, unit-tested.
- **Crash detection:** on launch / when a dest is chosen, if `dest/.fastcull-apply.json` exists, offer resume-or-report (`resume()` or a formatted report). At minimum detect + surface.
- **Platform:** Linux only. `rustix`/`renameat2`/`statvfs` live in core; the binary uses `culler_core::RealFs` for filesystem probes (`same_filesystem`, `free_space`).
- **TDD, DRY, YAGNI, frequent commits.** Every logic task: failing test → run-it-fails → minimal impl → run-it-passes → commit. Conventional-commit messages (`feat:`, `test:`). Purely-visual tasks replace Steps 1–4 with a Manual verification checklist but still show complete code.
- **No v1 config file.** All configurable names/behaviors are CLI flags (bucket names, `--no-auto-advance`, destination typed in the Apply dialog).
- **Auto-advance default ON**; `--no-auto-advance` disables it. The flag is parsed in `main` and threaded into `apply_action`.

**Assumption (one place to adjust):** `culler-core` re-exports its public surface at the crate root (`pub use` in `lib.rs`), so this plan imports `culler_core::{Session, Shot, Decision, Tier, CaptureTime, TierCountsPlan, ApplyPlan, Journal, OpState, ApplyReport, DecodedImage, TargetSize, RealFs, FsOps, plan, apply, resume, save, load_or_fresh, scan, decode, embedded_thumbnail, BUCKET_*, SESSION_FILE, JOURNAL_FILE, UNDO_LIMIT}`. If core instead nests these in modules (`culler_core::model::Session`, `culler_core::plan::plan`, …), fix only the `use` lines — nothing else changes.

---

### Task 1: `culler` crate wiring + a Slint window that opens

**Files:** Modify `Cargo.toml` (workspace root); Create `culler/Cargo.toml`, `culler/build.rs`, `culler/ui/app.slint`, `culler/ui/globals.slint`, `culler/src/main.rs`.
**Interfaces:** Consumes: nothing yet (just links `culler-core`). Produces: a runnable `culler` binary that opens an empty FastCull window.

This is a pure-visual bring-up task — Steps 1–4 are replaced by a Manual verification checklist, but the full code is shown.

- [ ] **Step 1–4 (Manual verification):** After writing the files below, run `cargo run -p culler -- /tmp` and confirm:
  - A titled window "FastCull" opens (~1280×820) with the placeholder text visible.
  - `cargo build -p culler` completes with the Skia renderer (`slint` feature `renderer-skia`).
  - Closing the window exits the process cleanly (exit code 0).

- [ ] **Step 3 (code):** workspace root `Cargo.toml` — ensure both members are listed:
```toml
[workspace]
members = ["culler-core", "culler"]
resolver = "2"
```

`culler/Cargo.toml`:
```toml
[package]
name = "culler"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "culler"
path = "src/main.rs"

[dependencies]
culler-core = { path = "../culler-core" }
slint = { version = "1.8", features = ["backend-winit", "renderer-skia"] }
clap = { version = "4", features = ["derive"] }
serde_json = "1"

[build-dependencies]
slint-build = "1.8"

[dev-dependencies]
tempfile = "3"
```

`culler/build.rs`:
```rust
fn main() {
    slint_build::compile("ui/app.slint").expect("compile ui/app.slint");
}
```

`culler/ui/globals.slint` (shared structs used by later tasks; safe to create now):
```slint
export struct FilmstripItem {
    thumb: image,
    color-code: int,   // 0 rest/grey, 1 keep/green, 2 pick/blue, 3 best/gold, 4 reject/red
    dim: bool,         // unvisited rest -> rendered dimmer
    selected: bool,
}

export struct ApplyPreviewUi {
    rejected: int,
    rest: int,
    keep: int,
    picks: int,
    bests: int,
    collisions: int,
    skipped-sidecars: int,
    stale: int,
    leftovers: int,
    total-mb: int,
    cross-fs: bool,
    enough-space: bool,
}
```

`culler/ui/app.slint` (minimal for now; extended in Tasks 8–12):
```slint
export component AppWindow inherits Window {
    title: "FastCull";
    preferred-width: 1280px;
    preferred-height: 820px;
    background: #141414;
    Text {
        text: "FastCull — point me at a folder of shots";
        color: #cccccc;
        font-size: 18px;
    }
}
```

`culler/src/main.rs`:
```rust
slint::include_modules!();

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app = AppWindow::new()?;
    app.run()?;
    Ok(())
}
```

- [ ] **Step 5: Commit**
```bash
git add Cargo.toml culler/Cargo.toml culler/build.rs culler/ui/ culler/src/main.rs
git commit -m "feat(main): scaffold culler Slint binary with an empty window"
```

---

### Task 1b: `theme.slint` — design-token global (from `docs/design/DESIGN.md`)

**Files:** Create `culler/ui/theme.slint`, `culler/ui/fonts/` (bundled IBM Plex `.ttf`); Modify `culler/ui/app.slint` (import `Theme`, use `Theme.bg`), `culler/src/main.rs` (register fonts).
**Interfaces:** Consumes: nothing. Produces: `export global Theme` (surfaces, borders, text ramp, tier palette, type, radii, `pure function tier-color(int) -> brush`) — imported by every subsequent `.slint`.
**Design ref:** [`docs/design/DESIGN.md` §3](../../../design/DESIGN.md) is the verbatim source of the `Theme` block; §2 explains every token.

Pure-visual setup task (no unit tests). This is **the single source of visual truth** for Phase 6 — author it **before** Tasks 8–12 so every component reads `Theme.*` instead of ad-hoc hex. It supersedes the placeholder colors sketched in later tasks.

- [ ] **Step 1: Bundle fonts.** Place IBM Plex Sans + Mono (weights 400/500/600) `.ttf` under `culler/ui/fonts/` (IBM Plex is OFL — redistributable). Register at startup so they render regardless of system fonts, in `main()` after `slint::include_modules!();`:
```rust
// register each bundled weight of Sans + Mono before building the window
slint::register_font_from_path(std::path::Path::new("culler/ui/fonts/IBMPlexMono-Regular.ttf")).ok();
// … Medium/SemiBold, and IBMPlexSans-Regular/Medium/SemiBold.  (Or embed via build.rs / rely on system Fontconfig.)
```

- [ ] **Step 2: Create `culler/ui/theme.slint`** — copy the `export global Theme { … }` block **verbatim** from [DESIGN.md §3](../../../design/DESIGN.md). Tokens cover surfaces, borders, the text ramp, the 5-tier palette, type, radii, and `tier-color(code)` (0 rest / 1 keep / 2 pick / 3 best / 4 reject — matches `ui::tier_color_code` and `FilmstripItem.color-code`).

- [ ] **Step 3: Wire it into `app.slint`.** Add `import { Theme } from "theme.slint";` and replace the placeholder `background: #141414;` with `background: Theme.bg;`. From here on every `.slint` component pulls color/size/font from `Theme` — never a literal hex that lives in `Theme`.

- [ ] **Step 4 (Manual verification):** `cargo run -p culler -- /tmp` opens the window on `Theme.bg` (`#0f1012`); text renders in IBM Plex (not a system fallback — verify the glyphs match `screens/*.png`). `theme.slint` compiles (it's referenced by `app.slint`).

- [ ] **Step 5: Commit**
```bash
git add culler/ui/theme.slint culler/ui/fonts culler/ui/app.slint culler/src/main.rs
git commit -m "feat(ui): theme.slint design-token global from docs/design + bundled IBM Plex fonts"
```

---

### Task 2: `input.rs` — keymap (`Key`, `Action`, `key_to_action`, `to_key`)

**Files:** Create `culler/src/input.rs`; Modify `culler/src/main.rs` (add `mod input;`).
**Interfaces:** Consumes: `culler_core::Tier`. Produces: `Key`, `Modifiers`, `InputContext`, `Action`, `pub fn key_to_action(Key, Modifiers, InputContext) -> Option<Action>`, `pub fn to_key(&str) -> Option<Key>`.

Covers the full §9 canonical keymap. `to_key` turns the strings the `.slint` `FocusScope` forwards (special keys pre-normalized to `"Left"/"Right"/"Tab"/"Backspace"`, printables as-is) into a semantic `Key`; `key_to_action` maps `Key`+modifiers to an `Action`, gated by `InputContext` so the loupe keymap goes inert while a text field / dialog owns the keyboard.

- [ ] **Step 1: Write the failing test** (append to `culler/src/input.rs`):
```rust
#[cfg(test)]
mod key_tests {
    use super::*;
    use culler_core::Tier;

    const LOUPE: InputContext = InputContext::Loupe;
    fn m() -> Modifiers { Modifiers::default() }

    #[test]
    fn arrows_space_backspace_navigate() {
        assert_eq!(key_to_action(Key::Left, m(), LOUPE), Some(Action::Prev));
        assert_eq!(key_to_action(Key::Backspace, m(), LOUPE), Some(Action::Prev));
        assert_eq!(key_to_action(Key::Right, m(), LOUPE), Some(Action::Next));
        assert_eq!(key_to_action(Key::Space, m(), LOUPE), Some(Action::Next));
    }

    #[test]
    fn tier_keys_map_to_settier_some() {
        assert_eq!(key_to_action(Key::Char('1'), m(), LOUPE), Some(Action::SetTier(Some(Tier::Keep))));
        assert_eq!(key_to_action(Key::Char('2'), m(), LOUPE), Some(Action::SetTier(Some(Tier::Pick))));
        assert_eq!(key_to_action(Key::Char('3'), m(), LOUPE), Some(Action::SetTier(Some(Tier::Best))));
        assert_eq!(key_to_action(Key::Char('x'), m(), LOUPE), Some(Action::SetTier(Some(Tier::Reject))));
        assert_eq!(key_to_action(Key::Char('X'), m(), LOUPE), Some(Action::SetTier(Some(Tier::Reject))));
    }

    #[test]
    fn clear_keys_map_to_settier_none() {
        assert_eq!(key_to_action(Key::Char('`'), m(), LOUPE), Some(Action::SetTier(None)));
        assert_eq!(key_to_action(Key::Char('0'), m(), LOUPE), Some(Action::SetTier(None)));
    }

    #[test]
    fn command_keys_cover_the_keymap() {
        assert_eq!(key_to_action(Key::Char('u'), m(), LOUPE), Some(Action::Undo));
        assert_eq!(key_to_action(Key::Char('t'), m(), LOUPE), Some(Action::OpenTagEntry));
        assert_eq!(key_to_action(Key::Char('z'), m(), LOUPE), Some(Action::ToggleZoom));
        assert_eq!(key_to_action(Key::Char('f'), m(), LOUPE), Some(Action::CycleFilter));
        assert_eq!(key_to_action(Key::Tab, m(), LOUPE), Some(Action::NextUnvisited));
        assert_eq!(key_to_action(Key::Char('a'), m(), LOUPE), Some(Action::OpenApply));
    }

    #[test]
    fn ctrl_s_force_saves_but_plain_s_does_not() {
        let ctrl = Modifiers { control: true, ..Default::default() };
        assert_eq!(key_to_action(Key::Char('s'), ctrl, LOUPE), Some(Action::ForceSave));
        assert_eq!(key_to_action(Key::Char('S'), ctrl, LOUPE), Some(Action::ForceSave));
        assert_eq!(key_to_action(Key::Char('s'), m(), LOUPE), None);
    }

    #[test]
    fn keymap_is_inert_outside_the_loupe() {
        assert_eq!(key_to_action(Key::Char('1'), m(), InputContext::TagEntry), None);
        assert_eq!(key_to_action(Key::Left, m(), InputContext::ApplyDialog), None);
    }

    #[test]
    fn to_key_normalizes_specials_and_printables() {
        assert_eq!(to_key("Left"), Some(Key::Left));
        assert_eq!(to_key("Right"), Some(Key::Right));
        assert_eq!(to_key("Tab"), Some(Key::Tab));
        assert_eq!(to_key("Backspace"), Some(Key::Backspace));
        assert_eq!(to_key(" "), Some(Key::Space));
        assert_eq!(to_key("a"), Some(Key::Char('a')));
        assert_eq!(to_key(""), None);
    }
}
```

- [ ] **Step 2: Run to verify it fails** Run: `cargo test -p culler key_tests` Expected: FAIL "cannot find type `Key`/`Action` in this scope" (nothing implemented yet).

- [ ] **Step 3: Minimal implementation** (top of `culler/src/input.rs`):
```rust
use culler_core::Tier;

/// A semantic key, decoded from the string the Slint FocusScope forwards.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Left,
    Right,
    Space,
    Backspace,
    Tab,
    Char(char),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Modifiers {
    pub control: bool,
    pub shift: bool,
    pub alt: bool,
}

/// Which surface currently owns the keyboard. The loupe keymap is inert unless `Loupe`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InputContext {
    Loupe,
    TagEntry,
    ApplyDialog,
}

/// One user intent. Model-mutating variants are executed by `apply_action`;
/// UI-only variants (OpenTagEntry, ToggleZoom, CycleFilter, OpenApply, ForceSave)
/// are dispatched by the ui glue.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Action {
    Prev,
    Next,
    NextUnvisited,
    SetTier(Option<Tier>), // Some(t) = keep/pick/best/reject; None = clear -> Rest
    Undo,
    OpenTagEntry,
    ToggleZoom,
    CycleFilter,
    OpenApply,
    ForceSave,
}

/// Decode the FocusScope-forwarded text into a semantic `Key`.
/// The `.slint` side normalizes special keys to these names; printable text passes through.
pub fn to_key(text: &str) -> Option<Key> {
    match text {
        "Left" => Some(Key::Left),
        "Right" => Some(Key::Right),
        "Tab" => Some(Key::Tab),
        "Backspace" => Some(Key::Backspace),
        " " => Some(Key::Space),
        _ => text.chars().next().map(Key::Char),
    }
}

/// Pure §9 keymap. Returns None when a modal (`TagEntry`/`ApplyDialog`) owns keys,
/// or when the key is unbound.
pub fn key_to_action(key: Key, mods: Modifiers, ctx: InputContext) -> Option<Action> {
    if ctx != InputContext::Loupe {
        return None;
    }
    if mods.control {
        return match key {
            Key::Char('s') | Key::Char('S') => Some(Action::ForceSave),
            _ => None,
        };
    }
    match key {
        Key::Left | Key::Backspace => Some(Action::Prev),
        Key::Right | Key::Space => Some(Action::Next),
        Key::Tab => Some(Action::NextUnvisited),
        Key::Char('1') => Some(Action::SetTier(Some(Tier::Keep))),
        Key::Char('2') => Some(Action::SetTier(Some(Tier::Pick))),
        Key::Char('3') => Some(Action::SetTier(Some(Tier::Best))),
        Key::Char('x') | Key::Char('X') => Some(Action::SetTier(Some(Tier::Reject))),
        Key::Char('`') | Key::Char('0') => Some(Action::SetTier(None)),
        Key::Char('u') | Key::Char('U') => Some(Action::Undo),
        Key::Char('t') | Key::Char('T') => Some(Action::OpenTagEntry),
        Key::Char('z') | Key::Char('Z') => Some(Action::ToggleZoom),
        Key::Char('f') | Key::Char('F') => Some(Action::CycleFilter),
        Key::Char('a') | Key::Char('A') => Some(Action::OpenApply),
        _ => None,
    }
}
```
Add `mod input;` to `culler/src/main.rs`.

- [ ] **Step 4: Run to verify pass** Run: `cargo test -p culler key_tests` Expected: PASS (7 tests).

- [ ] **Step 5: Commit**
```bash
git add culler/src/input.rs culler/src/main.rs
git commit -m "feat(input): pure §9 keymap key_to_action + to_key with tests"
```

---

### Task 3: `input.rs` — filter cycle, filter-confined navigation, tag parsing

**Files:** Modify `culler/src/input.rs`.
**Interfaces:** Consumes: `culler_core::{Session, Shot, Decision, Tier, CaptureTime}`. Produces: `Filter`, `pub fn next_filter(Filter) -> Filter`, `pub fn passes(Filter, &Decision) -> bool`, `pub fn step_filtered(&Session, Filter, bool) -> Option<usize>`, `pub fn parse_tags(&str) -> Vec<String>`.

`F` cycles `All → ≥Keep → ≥Pick → ≥Best → Rejects` (§9). `passes` is the pure predicate; `step_filtered` walks forward/back to the next index whose decision passes (with `Filter::All` it degenerates to plain ±1) so a second pass stays inside the working set. `parse_tags` turns the comma-separated tag-entry text into a clean, deduped `Vec<String>`.

- [ ] **Step 1: Write the failing test** (append to `culler/src/input.rs`):
```rust
#[cfg(test)]
mod filter_tests {
    use super::*;
    use culler_core::{CaptureTime, Decision, Session, Shot, Tier};

    fn mk_session(tiers: &[Option<Tier>]) -> Session {
        let mut shots = Vec::new();
        let mut decisions = std::collections::HashMap::new();
        for (i, t) in tiers.iter().enumerate() {
            let stem = format!("IMG_{i:04}");
            shots.push(Shot {
                stem: stem.clone(),
                jpeg: std::path::PathBuf::from(format!("/src/{stem}.JPG")),
                raw: None,
                sidecar: None,
                capture: CaptureTime::default(),
            });
            decisions.insert(stem, Decision { tier: *t, tags: vec![], visited: false });
        }
        Session {
            source_dir: "/src".into(),
            shots,
            decisions,
            current: 0,
            undo: Vec::new(),
        }
    }

    #[test]
    fn filter_cycles_all_keep_pick_best_rejects() {
        assert_eq!(next_filter(Filter::All), Filter::Keep);
        assert_eq!(next_filter(Filter::Keep), Filter::Pick);
        assert_eq!(next_filter(Filter::Pick), Filter::Best);
        assert_eq!(next_filter(Filter::Best), Filter::Rejects);
        assert_eq!(next_filter(Filter::Rejects), Filter::All);
    }

    #[test]
    fn passes_respects_quality_ladder() {
        let none = Decision::default();
        let keep = Decision { tier: Some(Tier::Keep), ..Default::default() };
        let pick = Decision { tier: Some(Tier::Pick), ..Default::default() };
        let best = Decision { tier: Some(Tier::Best), ..Default::default() };
        let rej = Decision { tier: Some(Tier::Reject), ..Default::default() };

        for d in [&none, &keep, &pick, &best, &rej] {
            assert!(passes(Filter::All, d));
        }
        // >= Keep : keep, pick, best (never rest/none or reject)
        assert!(!passes(Filter::Keep, &none));
        assert!(!passes(Filter::Keep, &rej));
        assert!(passes(Filter::Keep, &keep));
        assert!(passes(Filter::Keep, &pick));
        assert!(passes(Filter::Keep, &best));
        // >= Pick
        assert!(!passes(Filter::Pick, &keep));
        assert!(passes(Filter::Pick, &pick));
        assert!(passes(Filter::Pick, &best));
        // >= Best (only best)
        assert!(!passes(Filter::Best, &pick));
        assert!(passes(Filter::Best, &best));
        // Rejects (only reject)
        assert!(passes(Filter::Rejects, &rej));
        assert!(!passes(Filter::Rejects, &keep));
        assert!(!passes(Filter::Rejects, &none));
    }

    #[test]
    fn step_filtered_skips_non_passing_forward_and_back() {
        // Keep, None, Pick, None, Best
        let mut s = mk_session(&[
            Some(Tier::Keep),
            None,
            Some(Tier::Pick),
            None,
            Some(Tier::Best),
        ]);
        s.current = 0;
        assert_eq!(step_filtered(&s, Filter::Pick, true), Some(2)); // next >=Pick after Keep@0
        s.current = 2;
        assert_eq!(step_filtered(&s, Filter::Pick, true), Some(4)); // Best@4
        s.current = 4;
        assert_eq!(step_filtered(&s, Filter::Pick, true), None); // nothing after
        s.current = 4;
        assert_eq!(step_filtered(&s, Filter::Pick, false), Some(2)); // back to Pick@2
    }

    #[test]
    fn step_filtered_all_is_plain_pm1() {
        let mut s = mk_session(&[None, None, None]);
        s.current = 1;
        assert_eq!(step_filtered(&s, Filter::All, true), Some(2));
        assert_eq!(step_filtered(&s, Filter::All, false), Some(0));
        s.current = 0;
        assert_eq!(step_filtered(&s, Filter::All, false), None); // clamp at start
    }

    #[test]
    fn parse_tags_splits_trims_dedupes() {
        assert_eq!(
            parse_tags("sky, tree ,  sky , , water"),
            vec!["sky".to_string(), "tree".to_string(), "water".to_string()]
        );
        assert!(parse_tags("   ").is_empty());
        assert!(parse_tags("").is_empty());
    }
}
```

- [ ] **Step 2: Run to verify it fails** Run: `cargo test -p culler filter_tests` Expected: FAIL "cannot find type `Filter`".

- [ ] **Step 3: Minimal implementation** (append to the non-test region of `culler/src/input.rs`):
```rust
use culler_core::{Decision, Session};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Filter {
    All,
    Keep,    // >= Keep
    Pick,    // >= Pick
    Best,    // >= Best (only Best)
    Rejects, // only Reject
}

/// §9 filter cycle: All -> >=Keep -> >=Pick -> >=Best -> Rejects -> All.
pub fn next_filter(f: Filter) -> Filter {
    match f {
        Filter::All => Filter::Keep,
        Filter::Keep => Filter::Pick,
        Filter::Pick => Filter::Best,
        Filter::Best => Filter::Rejects,
        Filter::Rejects => Filter::All,
    }
}

/// Pure predicate: does this decision pass the active filter?
/// Ladder is Reject(-1) < Rest/None(0) < Keep(1) < Pick(2) < Best(3).
pub fn passes(filter: Filter, d: &Decision) -> bool {
    match filter {
        Filter::All => true,
        Filter::Keep => d.tier.map_or(false, |t| t.rank() >= 1),
        Filter::Pick => d.tier.map_or(false, |t| t.rank() >= 2),
        Filter::Best => d.tier.map_or(false, |t| t.rank() >= 3),
        Filter::Rejects => d.tier == Some(culler_core::Tier::Reject),
    }
}

/// Next/previous index whose decision passes `filter`. With `Filter::All`
/// this is a plain +/-1 (first candidate always passes). None at either end.
pub fn step_filtered(session: &Session, filter: Filter, forward: bool) -> Option<usize> {
    let n = session.shots.len();
    if n == 0 {
        return None;
    }
    let mut i = session.current;
    loop {
        if forward {
            if i + 1 >= n {
                return None;
            }
            i += 1;
        } else {
            if i == 0 {
                return None;
            }
            i -= 1;
        }
        if passes(filter, session.decision(i)) {
            return Some(i);
        }
    }
}

/// Turn comma-separated tag-entry text into clean, order-preserving, deduped tags.
pub fn parse_tags(input: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in input.split(',') {
        let t = raw.trim();
        if !t.is_empty() && !out.iter().any(|e| e == t) {
            out.push(t.to_string());
        }
    }
    out
}
```

- [ ] **Step 4: Run to verify pass** Run: `cargo test -p culler filter_tests` Expected: PASS (5 tests).

- [ ] **Step 5: Commit**
```bash
git add culler/src/input.rs
git commit -m "feat(input): filter cycle, filter-confined nav, tag parsing with tests"
```

---

### Task 4: `input.rs` — `apply_action` (model mutation)

**Files:** Modify `culler/src/input.rs`.
**Interfaces:** Consumes: `culler_core::Session` methods `set_tier`, `undo`, `mark_visited`, `next_unvisited`, `decision`. Produces: `pub fn apply_action(Action, &mut Session, auto_advance: bool, filter: Filter)`.

Executes the model-mutating actions and moves `session.current`. `SetTier(Some(_))` auto-advances (filter-confined) when `auto_advance` is on; `SetTier(None)` (clear→Rest) never advances. Navigation marks the shot it lands on as visited. UI-only actions are no-ops here (the ui glue handles them). **Deliberate extension of the sketch signature:** `filter` is threaded in so prev/next stay inside the filtered working set (§6 second passes) — the one place this plan widens the README's `apply_action(action, &mut Session, auto_advance)` sketch; noted for reviewers.

- [ ] **Step 1: Write the failing test** (append to `culler/src/input.rs`):
```rust
#[cfg(test)]
mod action_tests {
    use super::*;
    use culler_core::{CaptureTime, Decision, Session, Shot, Tier};

    fn mk_session(tiers: &[Option<Tier>]) -> Session {
        let mut shots = Vec::new();
        let mut decisions = std::collections::HashMap::new();
        for (i, t) in tiers.iter().enumerate() {
            let stem = format!("IMG_{i:04}");
            shots.push(Shot {
                stem: stem.clone(),
                jpeg: std::path::PathBuf::from(format!("/src/{stem}.JPG")),
                raw: None,
                sidecar: None,
                capture: CaptureTime::default(),
            });
            decisions.insert(stem, Decision { tier: *t, tags: vec![], visited: false });
        }
        Session { source_dir: "/src".into(), shots, decisions, current: 0, undo: Vec::new() }
    }

    #[test]
    fn settier_some_records_and_autoadvances() {
        let mut s = mk_session(&[None, None, None]);
        apply_action(Action::SetTier(Some(Tier::Keep)), &mut s, true, Filter::All);
        assert_eq!(s.decision(0).tier, Some(Tier::Keep));
        assert_eq!(s.current, 1); // advanced
        assert!(s.decision(1).visited);
    }

    #[test]
    fn settier_some_no_autoadvance_when_disabled() {
        let mut s = mk_session(&[None, None]);
        apply_action(Action::SetTier(Some(Tier::Pick)), &mut s, false, Filter::All);
        assert_eq!(s.current, 0);
    }

    #[test]
    fn clear_never_autoadvances_even_when_enabled() {
        let mut s = mk_session(&[Some(Tier::Keep), None]);
        apply_action(Action::SetTier(None), &mut s, true, Filter::All);
        assert_eq!(s.decision(0).tier, None);
        assert_eq!(s.current, 0);
    }

    #[test]
    fn undo_reverts_last_tier_change() {
        let mut s = mk_session(&[None]);
        apply_action(Action::SetTier(Some(Tier::Best)), &mut s, false, Filter::All);
        apply_action(Action::Undo, &mut s, false, Filter::All);
        assert_eq!(s.decision(0).tier, None);
    }

    #[test]
    fn next_prev_move_and_mark_visited() {
        let mut s = mk_session(&[None, None]);
        apply_action(Action::Next, &mut s, false, Filter::All);
        assert_eq!(s.current, 1);
        assert!(s.decision(1).visited);
        apply_action(Action::Prev, &mut s, false, Filter::All);
        assert_eq!(s.current, 0);
    }

    #[test]
    fn autoadvance_respects_active_filter() {
        // Keep, None, Keep : tiering @0 with >=Keep filter should skip None@1 to Keep@2
        let mut s = mk_session(&[None, None, None]);
        // set up so 2 already passes >=Keep, 1 does not
        apply_action(Action::SetTier(Some(Tier::Keep)), &mut s, false, Filter::All); // s.current stays 0
        s.decisions.get_mut("IMG_0002").unwrap().tier = Some(Tier::Keep);
        s.current = 0;
        apply_action(Action::SetTier(Some(Tier::Keep)), &mut s, true, Filter::Keep);
        assert_eq!(s.current, 2); // skipped the un-tiered @1
    }

    #[test]
    fn ui_only_actions_do_not_mutate_model() {
        let mut s = mk_session(&[None, None]);
        for a in [Action::OpenTagEntry, Action::ToggleZoom, Action::CycleFilter, Action::OpenApply, Action::ForceSave] {
            apply_action(a, &mut s, true, Filter::All);
        }
        assert_eq!(s.current, 0);
        assert_eq!(s.decision(0), &Decision::default());
    }
}
```

- [ ] **Step 2: Run to verify it fails** Run: `cargo test -p culler action_tests` Expected: FAIL "cannot find function `apply_action`".

- [ ] **Step 3: Minimal implementation** (append to the non-test region of `culler/src/input.rs`):
```rust
/// Execute a model-mutating action. UI-only actions are no-ops (handled by the ui glue).
/// `filter` confines prev/next and auto-advance to the working set; `auto_advance`
/// only affects `SetTier(Some(_))` (clear never advances).
pub fn apply_action(action: Action, session: &mut Session, auto_advance: bool, filter: Filter) {
    match action {
        Action::Prev => {
            if let Some(i) = step_filtered(session, filter, false) {
                session.current = i;
                session.mark_visited(i);
            }
        }
        Action::Next => {
            if let Some(i) = step_filtered(session, filter, true) {
                session.current = i;
                session.mark_visited(i);
            }
        }
        Action::NextUnvisited => {
            if let Some(i) = session.next_unvisited(session.current) {
                session.current = i;
                session.mark_visited(i);
            }
        }
        Action::SetTier(tier) => {
            let idx = session.current;
            session.set_tier(idx, tier);
            if tier.is_some() && auto_advance {
                if let Some(i) = step_filtered(session, filter, true) {
                    session.current = i;
                    session.mark_visited(i);
                }
            }
        }
        Action::Undo => {
            session.undo();
        }
        // UI-only — the ui glue handles these; no model mutation here.
        Action::OpenTagEntry
        | Action::ToggleZoom
        | Action::CycleFilter
        | Action::OpenApply
        | Action::ForceSave => {}
    }
}
```

- [ ] **Step 4: Run to verify pass** Run: `cargo test -p culler action_tests` Expected: PASS (7 tests).

- [ ] **Step 5: Commit**
```bash
git add culler/src/input.rs
git commit -m "feat(input): apply_action model mutation with auto-advance + filter-confined nav"
```

---

### Task 5: `pipeline.rs` — `Scheduler` (generation-counter latest-wins staleness)

**Files:** Create `culler/src/pipeline.rs`; Modify `culler/src/main.rs` (add `mod pipeline;`).
**Interfaces:** Consumes: nothing (pure). Produces: `Request { index, generation }`, `Scheduler { generation }` with `new`, `advance() -> u64`, `request(index, gen) -> Request`, and the associated `is_stale(&Request, current_gen) -> bool`.

The pure core of §12 latest-wins scheduling. Each navigation calls `advance()` once (bumping `generation`); every request stamped afterward carries that generation. A request is **stale** if a newer generation has since been issued — checked both at DEQUEUE (worker) and at DELIVERY (event loop) so holding `→` through 50 frames never backlogs. Threading/atomics live in Task 7; here it is a plain counter.

- [ ] **Step 1: Write the failing test** (append to `culler/src/pipeline.rs`):
```rust
#[cfg(test)]
mod scheduler_tests {
    use super::*;

    #[test]
    fn a_request_is_fresh_until_a_newer_generation_is_issued() {
        let mut sch = Scheduler::new();
        let g = sch.advance();
        let r = sch.request(5, g);
        assert!(!Scheduler::is_stale(&r, sch.generation));
        let g2 = sch.advance();
        assert_eq!(g2, g + 1);
        assert!(Scheduler::is_stale(&r, sch.generation)); // superseded
    }

    #[test]
    fn a_batch_shares_one_generation_and_goes_stale_together() {
        let mut sch = Scheduler::new();
        let g = sch.advance();
        let a = sch.request(10, g);
        let b = sch.request(11, g);
        assert!(!Scheduler::is_stale(&a, sch.generation));
        assert!(!Scheduler::is_stale(&b, sch.generation));
        sch.advance();
        assert!(Scheduler::is_stale(&a, sch.generation));
        assert!(Scheduler::is_stale(&b, sch.generation));
    }

    #[test]
    fn generation_starts_at_zero() {
        let sch = Scheduler::new();
        assert_eq!(sch.generation, 0);
    }
}
```

- [ ] **Step 2: Run to verify it fails** Run: `cargo test -p culler scheduler_tests` Expected: FAIL "cannot find type `Scheduler`".

- [ ] **Step 3: Minimal implementation** (top of `culler/src/pipeline.rs`):
```rust
/// A decode request tagged with the generation that was current when it was stamped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Request {
    pub index: usize,
    pub generation: u64,
}

/// Pure latest-wins scheduler. `generation` bumps once per navigation event.
#[derive(Clone, Copy, Debug, Default)]
pub struct Scheduler {
    pub generation: u64,
}

impl Scheduler {
    pub fn new() -> Self {
        Self { generation: 0 }
    }

    /// Advance to a new generation (call once per navigation) and return it.
    pub fn advance(&mut self) -> u64 {
        self.generation += 1;
        self.generation
    }

    /// Stamp a request for `index` with generation `gen` (typically `self.generation`).
    pub fn request(&self, index: usize, gen: u64) -> Request {
        Request { index, generation: gen }
    }

    /// True if a newer generation has been issued since this request was stamped.
    /// Checked at dequeue and at delivery.
    pub fn is_stale(req: &Request, current_gen: u64) -> bool {
        req.generation < current_gen
    }
}
```
Add `mod pipeline;` to `culler/src/main.rs`.

- [ ] **Step 4: Run to verify pass** Run: `cargo test -p culler scheduler_tests` Expected: PASS (3 tests).

- [ ] **Step 5: Commit**
```bash
git add culler/src/pipeline.rs culler/src/main.rs
git commit -m "feat(pipeline): pure generation-counter Scheduler with staleness tests"
```

---

### Task 6: `pipeline.rs` — `LruCache` + `prefetch_set`

**Files:** Modify `culler/src/pipeline.rs`.
**Interfaces:** Consumes: `culler_core::DecodedImage`, `std::sync::Arc`. Produces: `LruCache` with `new(budget)`, `get`, `put`, `contains`, `len`, `used_bytes`; `pub fn prefetch_set(current, n, len) -> Vec<usize>`.

Memory-budgeted LRU of fit-size RGBA textures (§12). `put` evicts least-recently-used entries until `used_bytes <= budget`; `get` touches an entry to MRU. Entry byte cost = `image.rgba.len()`. **1:1/Full decodes bypass this cache entirely** (Task 7 keeps a single dedicated slot for the current shot so a ~180 MB frame never evicts prefetched neighbors). `prefetch_set` yields the current index first, then forward-biased ±1, ±2 … within bounds (users hold `→`).

- [ ] **Step 1: Write the failing test** (append to `culler/src/pipeline.rs`):
```rust
#[cfg(test)]
mod cache_tests {
    use super::*;
    use culler_core::DecodedImage;
    use std::sync::Arc;

    fn img(bytes: usize) -> Arc<DecodedImage> {
        Arc::new(DecodedImage { w: 1, h: bytes as u32, rgba: vec![0u8; bytes] })
    }

    #[test]
    fn lru_evicts_least_recently_used_over_budget() {
        let mut c = LruCache::new(300);
        c.put(0, img(100));
        c.put(1, img(100));
        c.put(2, img(100));
        assert_eq!(c.len(), 3);
        assert!(c.get(0).is_some()); // touch 0 -> now MRU; 1 becomes LRU
        c.put(3, img(100)); // over budget -> evict LRU (1)
        assert!(c.contains(0));
        assert!(!c.contains(1));
        assert!(c.contains(2));
        assert!(c.contains(3));
        assert!(c.used_bytes() <= 300);
    }

    #[test]
    fn lru_get_absent_is_none() {
        let mut c = LruCache::new(100);
        assert!(c.get(42).is_none());
    }

    #[test]
    fn lru_put_same_key_updates_not_duplicates() {
        let mut c = LruCache::new(1000);
        c.put(7, img(100));
        c.put(7, img(200));
        assert_eq!(c.len(), 1);
        assert_eq!(c.used_bytes(), 200);
    }

    #[test]
    fn prefetch_is_forward_biased_and_clamped() {
        assert_eq!(prefetch_set(5, 2, 100), vec![5, 6, 4, 7, 3]);
        assert_eq!(prefetch_set(0, 2, 100), vec![0, 1, 2]); // clamp at start
        assert_eq!(prefetch_set(99, 2, 100), vec![99, 98, 97]); // clamp at end
        assert_eq!(prefetch_set(0, 3, 0), Vec::<usize>::new()); // empty set
    }
}
```

- [ ] **Step 2: Run to verify it fails** Run: `cargo test -p culler cache_tests` Expected: FAIL "cannot find type `LruCache`".

- [ ] **Step 3: Minimal implementation** (append to the non-test region of `culler/src/pipeline.rs`):
```rust
use culler_core::DecodedImage;
use std::collections::HashMap;
use std::sync::Arc;

struct CacheEntry {
    image: Arc<DecodedImage>,
    bytes: usize,
}

/// Memory-budgeted LRU of fit-size RGBA textures, keyed by shot index.
/// Full/1:1 decodes never enter here (see Task 7's dedicated slot).
pub struct LruCache {
    budget: usize,
    used: usize,
    order: Vec<usize>, // front = LRU, back = MRU
    map: HashMap<usize, CacheEntry>,
}

impl LruCache {
    pub fn new(budget: usize) -> Self {
        Self { budget, used: 0, order: Vec::new(), map: HashMap::new() }
    }

    fn touch(&mut self, key: usize) {
        if let Some(pos) = self.order.iter().position(|&k| k == key) {
            self.order.remove(pos);
            self.order.push(key);
        }
    }

    pub fn get(&mut self, key: usize) -> Option<Arc<DecodedImage>> {
        if self.map.contains_key(&key) {
            self.touch(key);
            self.map.get(&key).map(|e| e.image.clone())
        } else {
            None
        }
    }

    pub fn put(&mut self, key: usize, image: Arc<DecodedImage>) {
        let bytes = image.rgba.len();
        if let Some(old) = self.map.remove(&key) {
            self.used -= old.bytes;
            if let Some(pos) = self.order.iter().position(|&k| k == key) {
                self.order.remove(pos);
            }
        }
        self.used += bytes;
        self.map.insert(key, CacheEntry { image, bytes });
        self.order.push(key);
        self.evict();
    }

    fn evict(&mut self) {
        while self.used > self.budget && self.order.len() > 1 {
            let lru = self.order.remove(0);
            if let Some(e) = self.map.remove(&lru) {
                self.used -= e.bytes;
            }
        }
    }

    pub fn contains(&self, key: usize) -> bool {
        self.map.contains_key(&key)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn used_bytes(&self) -> usize {
        self.used
    }
}

/// Indices to prefetch around `current`: current first, then +1,-1,+2,-2,... within [0,len).
/// Forward-biased because navigation usually goes right.
pub fn prefetch_set(current: usize, n: usize, len: usize) -> Vec<usize> {
    let mut out = Vec::new();
    if len == 0 {
        return out;
    }
    let current = current.min(len - 1);
    out.push(current);
    for d in 1..=n {
        if current + d < len {
            out.push(current + d);
        }
        if current >= d {
            out.push(current - d);
        }
    }
    out
}
```

- [ ] **Step 4: Run to verify pass** Run: `cargo test -p culler cache_tests` Expected: PASS (4 tests).

- [ ] **Step 5: Commit**
```bash
git add culler/src/pipeline.rs
git commit -m "feat(pipeline): budgeted LruCache + forward-biased prefetch_set with tests"
```

---

### Task 7: `pipeline.rs` — worker threads, channels, event-loop marshaling

**Files:** Modify `culler/src/pipeline.rs`.
**Interfaces:** Consumes: `culler_core::{decode, embedded_thumbnail, DecodedImage, TargetSize}`, `slint::{SharedPixelBuffer, Rgba8Pixel, Image, invoke_from_event_loop}`. Produces: `Pipeline` (worker pool + shared atomic generation), `DecodeRequest`, `DecodeResult`, `pub fn to_slint_image(&DecodedImage) -> slint::Image`.

The thin threading glue: N worker threads pull `DecodeRequest`s off an `mpsc` channel, **drop stale requests at dequeue** (compare against the shared `AtomicU64` generation via `Scheduler::is_stale`), decode via core, and hand results back through the `on_ready` callback — which marshals on the event loop and **drops stale results at delivery**. Fit-size results go through the `LruCache`; `TargetSize::Full` (1:1) bypasses the cache into a dedicated single slot (wired in Task 10). `to_slint_image` is the *only* `DecodedImage → slint::Image` conversion in the app.

`to_slint_image` size is unit-testable (no window needed); the threading is manually verified.

- [ ] **Step 1: Write the failing test** (append to `culler/src/pipeline.rs`):
```rust
#[cfg(test)]
mod marshal_tests {
    use super::*;
    use culler_core::DecodedImage;

    #[test]
    fn to_slint_image_preserves_dimensions() {
        let d = DecodedImage { w: 4, h: 3, rgba: vec![0u8; 4 * 3 * 4] };
        let img = to_slint_image(&d);
        assert_eq!(img.size().width, 4);
        assert_eq!(img.size().height, 3);
    }
}
```

- [ ] **Step 2: Run to verify it fails** Run: `cargo test -p culler marshal_tests` Expected: FAIL "cannot find function `to_slint_image`".

- [ ] **Step 3: Minimal implementation** (append to the non-test region of `culler/src/pipeline.rs`):
```rust
use culler_core::{decode, embedded_thumbnail, TargetSize};
use slint::{Image, Rgba8Pixel, SharedPixelBuffer};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::Mutex;

/// A job for a worker. `req` carries the generation for latest-wins dropping.
pub struct DecodeRequest {
    pub req: Request,
    pub path: PathBuf,
    pub target: TargetSize,
    pub thumb_first: bool, // filmstrip: try embedded EXIF thumbnail for instant first paint
}

/// A decoded result handed back to the event loop.
pub struct DecodeResult {
    pub req: Request,
    pub target: TargetSize,
    pub image: Arc<DecodedImage>,
}

/// Marshal straight-RGBA8 into a `slint::Image` (the ONLY such conversion in the app).
pub fn to_slint_image(img: &DecodedImage) -> Image {
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(img.w, img.h);
    let bytes = buf.make_mut_bytes();
    let n = bytes.len().min(img.rgba.len());
    bytes[..n].copy_from_slice(&img.rgba[..n]);
    Image::from_rgba8(buf)
}

/// Worker pool + shared generation counter. Requests stamp the current generation;
/// stale ones are dropped at dequeue here and at delivery in `on_ready`.
pub struct Pipeline {
    tx: Sender<DecodeRequest>,
    pub generation: Arc<AtomicU64>,
}

impl Pipeline {
    /// Spawn `workers` decode threads. `on_ready` runs on a worker thread; it should
    /// re-check staleness and marshal onto the event loop via `invoke_from_event_loop`.
    pub fn spawn<F>(workers: usize, on_ready: F) -> Self
    where
        F: Fn(DecodeResult) + Send + Sync + 'static,
    {
        let (tx, rx) = channel::<DecodeRequest>();
        let rx = Arc::new(Mutex::new(rx));
        let generation = Arc::new(AtomicU64::new(0));
        let on_ready = Arc::new(on_ready);
        for _ in 0..workers.max(1) {
            let rx = rx.clone();
            let generation = generation.clone();
            let on_ready = on_ready.clone();
            std::thread::spawn(move || loop {
                let job = {
                    let guard = rx.lock().unwrap();
                    guard.recv()
                };
                let Ok(job) = job else { break }; // channel closed -> exit
                // DROP AT DEQUEUE
                if Scheduler::is_stale(&job.req, generation.load(Ordering::SeqCst)) {
                    continue;
                }
                // Filmstrip fast path: embedded EXIF thumbnail first, refined later.
                if job.thumb_first {
                    if let Some(t) = embedded_thumbnail(&job.path) {
                        on_ready(DecodeResult {
                            req: job.req,
                            target: job.target,
                            image: Arc::new(t),
                        });
                    }
                }
                match decode(&job.path, job.target) {
                    Ok(img) => on_ready(DecodeResult {
                        req: job.req,
                        target: job.target,
                        image: Arc::new(img),
                    }),
                    Err(e) => eprintln!("decode {:?} failed: {:?}", job.path, e),
                }
            });
        }
        Pipeline { tx, generation }
    }

    /// Bump the shared generation (call once per navigation) and return the new value.
    pub fn bump(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Enqueue a request stamped with the current generation.
    pub fn enqueue(&self, index: usize, path: PathBuf, target: TargetSize, thumb_first: bool) {
        let gen = self.generation.load(Ordering::SeqCst);
        let _ = self.tx.send(DecodeRequest {
            req: Request { index, generation: gen },
            path,
            target,
            thumb_first,
        });
    }
}
```

- [ ] **Step 4 (test + Manual verification):** Run: `cargo test -p culler marshal_tests` Expected: PASS (1 test). Then manually verify the threading once it is wired in Task 8:
  - Holding `→` through ~50 shots shows no backlog/rubber-band: the loupe tracks the current shot; intermediate decodes are dropped (add a temporary `eprintln!` counting dropped-at-dequeue vs delivered to confirm dropping happens).
  - The filmstrip paints embedded thumbnails near-instantly on a large folder, then refines.
  - The UI thread never stalls during decode (window stays responsive while scrubbing).

- [ ] **Step 5: Commit**
```bash
git add culler/src/pipeline.rs
git commit -m "feat(pipeline): worker pool, latest-wins dequeue drop, RGBA->slint marshaling"
```

---

### Task 8: `.slint` loupe + color-coded virtualized filmstrip + `ui.rs` model glue

**Files:** Create `culler/src/ui.rs`, `culler/ui/loupe.slint`, `culler/ui/filmstrip.slint`; Modify `culler/ui/app.slint`, `culler/src/main.rs` (add `mod ui;`).
**Interfaces:** Consumes: `culler_core::{Session, Decision, Tier}`, `input::Filter`, `pipeline::{Pipeline, to_slint_image, prefetch_set}`. Produces: `pub fn tier_color_code(&Decision) -> i32`, `pub fn dim_flag(&Decision) -> bool`, `pub fn build_filmstrip_window(&Session, Filter, usize) -> (Vec<usize>, usize)`, plus the `.slint` Loupe + Filmstrip components and the Rust glue that populates a `VecModel<FilmstripItem>`.
**Design ref:** [`screens/1b-main.png`](../../../design/screens/1b-main.png) — full-bleed loupe; filmstrip tiles 84×56 with tier **dot** badges, unvisited-rest dimmed (opacity .4), current tile outlined 2px `Theme.text`. All color/size/font from `Theme` (Task 1b); see [DESIGN.md](../../../design/DESIGN.md) §4 (1b) + §5 (filmstrip tile recipe).

Loupe = fit + sticky 1:1 zoom/pan (pan bound to persistent `AppWindow` properties, Task 10). Filmstrip is **virtualized**: `build_filmstrip_window` computes only the visible+buffer indices around current, and the ui glue rebuilds a `VecModel<FilmstripItem>` holding just that slice — grey/green/blue/gold/red per tier, unvisited-rest rendered dimmer. The color/dim mapping is pure and unit-tested; the rendering + VecModel wiring is manually verified.

- [ ] **Step 1: Write the failing test** (append to `culler/src/ui.rs`):
```rust
#[cfg(test)]
mod color_tests {
    use super::*;
    use culler_core::{CaptureTime, Decision, Session, Shot, Tier};
    use crate::input::Filter;

    #[test]
    fn tier_color_code_maps_every_tier() {
        assert_eq!(tier_color_code(&Decision::default()), 0); // rest/grey
        assert_eq!(tier_color_code(&Decision { tier: Some(Tier::Keep), ..Default::default() }), 1);
        assert_eq!(tier_color_code(&Decision { tier: Some(Tier::Pick), ..Default::default() }), 2);
        assert_eq!(tier_color_code(&Decision { tier: Some(Tier::Best), ..Default::default() }), 3);
        assert_eq!(tier_color_code(&Decision { tier: Some(Tier::Reject), ..Default::default() }), 4);
    }

    #[test]
    fn only_unvisited_rest_is_dim() {
        assert!(dim_flag(&Decision { tier: None, visited: false, ..Default::default() }));
        assert!(!dim_flag(&Decision { tier: None, visited: true, ..Default::default() }));
        assert!(!dim_flag(&Decision { tier: Some(Tier::Keep), visited: false, ..Default::default() }));
    }

    fn mk(n: usize) -> Session {
        let mut shots = Vec::new();
        let mut decisions = std::collections::HashMap::new();
        for i in 0..n {
            let stem = format!("IMG_{i:04}");
            shots.push(Shot { stem: stem.clone(), jpeg: format!("/s/{stem}.JPG").into(), raw: None, sidecar: None, capture: CaptureTime::default() });
            decisions.insert(stem, Decision::default());
        }
        Session { source_dir: "/s".into(), shots, decisions, current: 0, undo: Vec::new() }
    }

    #[test]
    fn filmstrip_window_is_buffered_and_reports_current_offset() {
        let mut s = mk(100);
        s.current = 50;
        let (indices, cur_off) = build_filmstrip_window(&s, Filter::All, 5);
        assert_eq!(indices, vec![45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55]);
        assert_eq!(indices[cur_off], 50);
    }

    #[test]
    fn filmstrip_window_clamps_at_edges() {
        let mut s = mk(4);
        s.current = 0;
        let (indices, cur_off) = build_filmstrip_window(&s, Filter::All, 5);
        assert_eq!(indices, vec![0, 1, 2, 3]);
        assert_eq!(cur_off, 0);
    }

    #[test]
    fn filmstrip_window_respects_filter() {
        // Only even indices are Keep; a >=Keep filter keeps only those in the window.
        let mut s = mk(10);
        for i in (0..10).step_by(2) {
            let stem = format!("IMG_{i:04}");
            s.decisions.get_mut(&stem).unwrap().tier = Some(Tier::Keep);
        }
        s.current = 4;
        let (indices, cur_off) = build_filmstrip_window(&s, Filter::Keep, 5);
        assert_eq!(indices, vec![0, 2, 4, 6, 8]);
        assert_eq!(indices[cur_off], 4);
    }
}
```

- [ ] **Step 2: Run to verify it fails** Run: `cargo test -p culler color_tests` Expected: FAIL "cannot find function `tier_color_code`".

- [ ] **Step 3: Minimal implementation** (top of `culler/src/ui.rs`):
```rust
use crate::input::{passes, Filter};
use culler_core::{Decision, Session};

/// Filmstrip color bucket: 0 rest/grey, 1 keep/green, 2 pick/blue, 3 best/gold, 4 reject/red.
pub fn tier_color_code(d: &Decision) -> i32 {
    match d.tier {
        None => 0,
        Some(culler_core::Tier::Keep) => 1,
        Some(culler_core::Tier::Pick) => 2,
        Some(culler_core::Tier::Best) => 3,
        Some(culler_core::Tier::Reject) => 4,
    }
}

/// Unvisited residual shots render dimmer so progress is visible at a glance (§9).
pub fn dim_flag(d: &Decision) -> bool {
    d.tier.is_none() && !d.visited
}

/// Virtualized filmstrip window: indices (respecting `filter`) within `buffer` of current,
/// plus the offset of the current index inside that returned slice.
pub fn build_filmstrip_window(session: &Session, filter: Filter, buffer: usize) -> (Vec<usize>, usize) {
    let n = session.shots.len();
    if n == 0 {
        return (Vec::new(), 0);
    }
    // Collect all passing indices (cheap: it's a Vec walk, only the built VecModel is windowed).
    let passing: Vec<usize> = (0..n).filter(|&i| passes(filter, session.decision(i))).collect();
    if passing.is_empty() {
        return (Vec::new(), 0);
    }
    // Locate current (or nearest passing) in the passing list.
    let cur_pos = passing
        .iter()
        .position(|&i| i >= session.current)
        .unwrap_or(passing.len() - 1);
    let lo = cur_pos.saturating_sub(buffer);
    let hi = (cur_pos + buffer + 1).min(passing.len());
    let indices: Vec<usize> = passing[lo..hi].to_vec();
    let cur_off = cur_pos - lo;
    (indices, cur_off)
}
```
Add `mod ui;` to `culler/src/main.rs`.

- [ ] **Step 4a: Run to verify pass** Run: `cargo test -p culler color_tests` Expected: PASS (5 tests).

- [ ] **Step 4b (Manual verification of the visuals):** wire the `.slint` below and confirm on a real folder:
  - Loupe shows the current shot fit-to-window; the filmstrip below is horizontal and color-coded (grey/green/blue/gold/red).
  - Unvisited residual tiles are visibly dimmer than visited ones.
  - The current tile has a thicker highlighted border and stays centered as you navigate.
  - With a filter active (`F`), only passing tiles appear in the strip.
  - Clicking a tile jumps the loupe to that shot.

`culler/ui/loupe.slint`:
```slint
export component Loupe inherits Rectangle {
    in property <image> source;
    in property <bool> zoomed;
    in-out property <length> pan-x;
    in-out property <length> pan-y;
    background: #1a1a1a;
    clip: true;

    // Fit mode: whole frame, aspect-preserved.
    Image {
        visible: !root.zoomed;
        source: root.source;
        width: parent.width;
        height: parent.height;
        image-fit: contain;
    }
    // 1:1 zoom mode: native pixels, panned; pan bound to persistent root props (Task 10).
    Image {
        visible: root.zoomed;
        source: root.source;
        image-fit: ImageFit.preserve;
        x: root.pan-x;
        y: root.pan-y;
    }
    drag := TouchArea {
        enabled: root.zoomed;
        property <length> anchor-x;
        property <length> anchor-y;
        pointer-event(ev) => {
            if (ev.kind == PointerEventKind.down) {
                self.anchor-x = root.pan-x;
                self.anchor-y = root.pan-y;
            }
        }
        moved => {
            root.pan-x = self.anchor-x + (self.mouse-x - self.pressed-x);
            root.pan-y = self.anchor-y + (self.mouse-y - self.pressed-y);
        }
    }
}
```

`culler/ui/filmstrip.slint`:
```slint
import { FilmstripItem } from "globals.slint";

export component Filmstrip inherits Rectangle {
    in property <[FilmstripItem]> items;
    in property <int> current; // offset of the current shot within `items`
    callback clicked(int);
    background: #101010;
    clip: true;

    HorizontalLayout {
        alignment: center;
        spacing: 4px;
        padding: 6px;
        for item[i] in root.items: Rectangle {
            width: 96px;
            height: 108px;
            border-width: i == root.current ? 3px : 1px;
            border-color: item.color-code == 0 ? #808080
                : item.color-code == 1 ? #3fb950
                : item.color-code == 2 ? #4098ff
                : item.color-code == 3 ? #d4a72c
                : #f85149;
            opacity: item.dim ? 0.45 : 1.0;
            Image {
                source: item.thumb;
                width: 90px;
                height: 90px;
                y: 3px;
                image-fit: contain;
            }
            TouchArea {
                clicked => { root.clicked(i); }
            }
        }
    }
}
```

`culler/ui/app.slint` (replace the minimal body from Task 1; imports + loupe/filmstrip composition + key FocusScope):
```slint
import { Loupe } from "loupe.slint";
import { Filmstrip } from "filmstrip.slint";
import { FilmstripItem } from "globals.slint";

export component AppWindow inherits Window {
    title: "FastCull";
    preferred-width: 1280px;
    preferred-height: 820px;
    background: #141414;

    in property <image> current-image;
    in-out property <bool> zoomed;
    in-out property <length> pan-x;
    in-out property <length> pan-y;
    in property <[FilmstripItem]> film-items;
    in property <int> film-current;

    callback key-pressed(string, bool) -> bool; // (text, ctrl)
    callback film-clicked(int);

    forward-focus: keyscope;
    keyscope := FocusScope {
        key-pressed(event) => {
            if (event.text == Key.LeftArrow) { return root.key-pressed("Left", event.modifiers.control) ? accept : reject; }
            if (event.text == Key.RightArrow) { return root.key-pressed("Right", event.modifiers.control) ? accept : reject; }
            if (event.text == Key.Backspace) { return root.key-pressed("Backspace", event.modifiers.control) ? accept : reject; }
            if (event.text == Key.Tab) { return root.key-pressed("Tab", event.modifiers.control) ? accept : reject; }
            return root.key-pressed(event.text, event.modifiers.control) ? accept : reject;
        }
        VerticalLayout {
            Loupe {
                source: root.current-image;
                zoomed: root.zoomed;
                pan-x <=> root.pan-x;
                pan-y <=> root.pan-y;
            }
            Filmstrip {
                height: 130px;
                items: root.film-items;
                current: root.film-current;
                clicked(i) => { root.film-clicked(i); }
            }
        }
    }
}
```

`culler/src/ui.rs` glue (append; builds the windowed VecModel and wires clicks/keys — manual-verified):
```rust
use crate::input::{self, Action, InputContext};
use crate::pipeline::{to_slint_image, Pipeline};
use slint::{Model, ModelRc, VecModel};
use std::cell::RefCell;
use std::rc::Rc;

/// Rebuild the filmstrip VecModel to hold only the current window (virtualization).
/// `thumbs` provides an already-decoded thumbnail per shot index (grey placeholder if absent).
pub fn refresh_filmstrip(
    app: &crate::AppWindow,
    session: &Session,
    filter: Filter,
    buffer: usize,
    thumb_for: &dyn Fn(usize) -> slint::Image,
) {
    let (indices, cur_off) = build_filmstrip_window(session, filter, buffer);
    let items: Vec<crate::FilmstripItem> = indices
        .iter()
        .map(|&i| {
            let d = session.decision(i);
            crate::FilmstripItem {
                thumb: thumb_for(i),
                color_code: tier_color_code(d),
                dim: dim_flag(d),
                selected: i == session.current,
            }
        })
        .collect();
    let model = Rc::new(VecModel::from(items));
    app.set_film_items(ModelRc::from(model));
    app.set_film_current(cur_off as i32);
}

/// Marshal a decoded fit image onto the loupe (called from the event loop).
pub fn set_loupe(app: &crate::AppWindow, img: &culler_core::DecodedImage) {
    app.set_current_image(to_slint_image(img));
}
```
The full event wiring (key dispatch calling `input::key_to_action` + `input::apply_action`, click → `session.current = window_index`, then re-request decode via `Pipeline::bump`/`enqueue`) is assembled in `main.rs` (Task 11) and manually verified per the checklist above.

- [ ] **Step 5: Commit**
```bash
git add culler/src/ui.rs culler/ui/ culler/src/main.rs
git commit -m "feat(ui): loupe + virtualized color-coded filmstrip with pure color/window tests"
```

---

### Task 9: HUD + tag entry with autocomplete

**Files:** Modify `culler/src/ui.rs`, `culler/ui/app.slint`; Create `culler/ui/hud.slint`.
**Interfaces:** Consumes: `culler_core::Session` methods `counts()`, `visited_count()`, `all_tags()`, `decision()`. Produces: `pub fn suggest_tags(&[String], &str) -> Vec<String>`, `pub fn hud_text(&Session, Filter) -> HudText`, plus the Hud + TagEntry `.slint` components.
**Design ref:** [`screens/1b-main.png`](../../../design/screens/1b-main.png) (HUD: current-tier badge = `Theme.tier-color`, per-tier counts pill, tag chips) + [`screens/2f-tag-entry.png`](../../../design/screens/2f-tag-entry.png) (translucent tag input + autocomplete, matched prefix bolded in `Theme.accent-hi`, **photo not dimmed**). Tokens from `Theme`; [DESIGN.md](../../../design/DESIGN.md) §4. NB the 1b **histogram + EXIF line are Defer** (§6) — don't build them.

HUD shows the current shot's tier + tags, per-tier counts, and real visited progress `seen X/Y` (§9). Tag entry autocompletes from `Session::all_tags()`; `suggest_tags` filters that list by prefix. Both string builders are pure and unit-tested; the `.slint` widgets are manually verified.

- [ ] **Step 1: Write the failing test** (append to `culler/src/ui.rs`):
```rust
#[cfg(test)]
mod hud_tests {
    use super::*;
    use culler_core::{CaptureTime, Decision, Session, Shot, Tier};
    use crate::input::Filter;

    fn mk(tiers: &[Option<Tier>]) -> Session {
        let mut shots = Vec::new();
        let mut decisions = std::collections::HashMap::new();
        for (i, t) in tiers.iter().enumerate() {
            let stem = format!("IMG_{i:04}");
            shots.push(Shot { stem: stem.clone(), jpeg: format!("/s/{stem}.JPG").into(), raw: None, sidecar: None, capture: CaptureTime::default() });
            decisions.insert(stem, Decision { tier: *t, tags: vec![], visited: t.is_some() });
        }
        Session { source_dir: "/s".into(), shots, decisions, current: 0, undo: Vec::new() }
    }

    #[test]
    fn suggest_tags_prefix_filters_case_insensitively() {
        let all = vec!["sky".to_string(), "skyline".to_string(), "sea".to_string(), "Sunset".to_string()];
        assert_eq!(suggest_tags(&all, "sk"), vec!["sky".to_string(), "skyline".to_string()]);
        assert_eq!(suggest_tags(&all, "SU"), vec!["Sunset".to_string()]);
        assert!(suggest_tags(&all, "").is_empty()); // no prefix -> no noise
        // an exact match is not re-suggested
        assert!(suggest_tags(&all, "sky").iter().all(|s| s != "sky"));
    }

    #[test]
    fn hud_text_reports_tier_counts_and_progress() {
        let s = mk(&[Some(Tier::Keep), Some(Tier::Reject), None]);
        let h = hud_text(&s, Filter::All);
        assert_eq!(h.tier, "Keep"); // current @0
        assert!(h.counts.contains("keep 1"));
        assert!(h.counts.contains("reject 1"));
        assert!(h.progress.contains("seen 2/3")); // two tiered => visited
        assert_eq!(h.filter_label, "filter: All");
    }

    #[test]
    fn hud_text_shows_rest_for_undecided_current() {
        let mut s = mk(&[None, None]);
        s.current = 0;
        let h = hud_text(&s, Filter::Keep);
        assert_eq!(h.tier, "Rest");
        assert_eq!(h.filter_label, "filter: >=Keep");
    }
}
```

- [ ] **Step 2: Run to verify it fails** Run: `cargo test -p culler hud_tests` Expected: FAIL "cannot find function `suggest_tags`".

- [ ] **Step 3: Minimal implementation** (append to the non-test region of `culler/src/ui.rs`):
```rust
/// Autocomplete suggestions: entries of `all` whose text starts with `prefix`
/// (case-insensitive), excluding an exact match. Capped for a tidy popup.
pub fn suggest_tags(all: &[String], prefix: &str) -> Vec<String> {
    let p = prefix.trim().to_lowercase();
    if p.is_empty() {
        return Vec::new();
    }
    all.iter()
        .filter(|t| {
            let lt = t.to_lowercase();
            lt.starts_with(&p) && lt != p
        })
        .cloned()
        .take(8)
        .collect()
}

/// Pre-rendered HUD strings.
pub struct HudText {
    pub tier: String,
    pub tags: String,
    pub counts: String,
    pub progress: String,
    pub filter_label: String,
}

pub fn hud_text(session: &Session, filter: Filter) -> HudText {
    let d = session.decision(session.current);
    let tier = match d.tier {
        None => "Rest".to_string(),
        Some(culler_core::Tier::Keep) => "Keep".to_string(),
        Some(culler_core::Tier::Pick) => "Pick".to_string(),
        Some(culler_core::Tier::Best) => "Best".to_string(),
        Some(culler_core::Tier::Reject) => "Reject".to_string(),
    };
    let c = session.counts();
    let counts = format!(
        "reject {}  rest {}  keep {}  pick {}  best {}",
        c.rejected, c.rest, c.keep, c.picks, c.bests
    );
    let progress = format!("seen {}/{}", session.visited_count(), session.shots.len());
    let filter_label = match filter {
        Filter::All => "filter: All",
        Filter::Keep => "filter: >=Keep",
        Filter::Pick => "filter: >=Pick",
        Filter::Best => "filter: >=Best",
        Filter::Rejects => "filter: Rejects",
    }
    .to_string();
    HudText {
        tier,
        tags: d.tags.join(", "),
        counts,
        progress,
        filter_label,
    }
}
```

- [ ] **Step 4a: Run to verify pass** Run: `cargo test -p culler hud_tests` Expected: PASS (3 tests).

- [ ] **Step 4b (Manual verification):** wire the `.slint` below and confirm:
  - HUD shows the current tier, comma-joined tags, per-tier counts, and `seen X/Y` that increases only as new shots are actually viewed (not merely by position).
  - Pressing `T` opens the tag entry focused; typing a prefix lists matching prior tags; clicking one or pressing Enter commits comma-separated tags to the current shot; Esc cancels.
  - The filter indicator updates as `F` cycles.

`culler/ui/hud.slint`:
```slint
import { LineEdit } from "std-widgets.slint";

export component Hud inherits Rectangle {
    in property <string> tier;
    in property <string> tags;
    in property <string> counts;
    in property <string> progress;
    in property <string> filter-label;
    background: #202020;
    VerticalLayout {
        padding: 12px;
        spacing: 8px;
        Text { text: "TIER"; color: #888; font-size: 11px; }
        Text { text: root.tier; color: white; font-size: 22px; }
        Text { text: "TAGS"; color: #888; font-size: 11px; }
        Text { text: root.tags; color: #dddddd; wrap: word-wrap; }
        Rectangle { height: 1px; background: #333; }
        Text { text: root.counts; color: #cccccc; wrap: word-wrap; }
        Text { text: root.progress; color: #99bbee; font-size: 15px; }
        Text { text: root.filter-label; color: #ccaa88; }
    }
}

export component TagEntry inherits Rectangle {
    in-out property <string> text;
    in property <[string]> suggestions;
    callback changed(string);
    callback committed(string);
    callback cancelled();
    background: #000000cc;

    Rectangle {
        width: 480px;
        height: 240px;
        background: #262626;
        border-radius: 8px;
        VerticalLayout {
            padding: 16px;
            spacing: 8px;
            Text { text: "Tags (comma-separated)"; color: white; }
            edit := LineEdit {
                text <=> root.text;
                placeholder-text: "sky, portrait, keeper";
                edited(t) => { root.changed(t); }
                accepted(t) => { root.committed(t); }
            }
            for s[i] in root.suggestions: Rectangle {
                height: 20px;
                Text { text: "  " + s; color: #88bbdd; }
                TouchArea { clicked => { root.committed(s); } }
            }
        }
        esc := FocusScope {
            key-pressed(event) => {
                if (event.text == Key.Escape) { root.cancelled(); return accept; }
                return reject;
            }
        }
    }
    init => { edit.focus(); }
}
```

Add to `culler/ui/app.slint`: `import { Hud, TagEntry } from "hud.slint";`, HUD properties (`hud-tier`, `hud-tags`, `hud-counts`, `hud-progress`, `filter-label`, `tag-open`, `tag-text`, `tag-suggestions`) and callbacks (`tag-changed`, `tag-committed`, `tag-cancelled`), place `Hud` beside `Loupe` in a `HorizontalLayout` (Hud `width: 240px`), and add the overlay `if root.tag-open : TagEntry { ... }` inside `keyscope`.

- [ ] **Step 5: Commit**
```bash
git add culler/src/ui.rs culler/ui/hud.slint culler/ui/app.slint
git commit -m "feat(ui): HUD strings + tag autocomplete with pure suggest_tags/hud_text tests"
```

---

### Task 10: Sticky 1:1 zoom + pan persistence across prev/next

**Files:** Modify `culler/src/ui.rs`, `culler/src/pipeline.rs` (Full-decode dedicated slot).
**Interfaces:** Consumes: `pipeline::Pipeline`, `culler_core::TargetSize`. Produces: `ZoomState { zoomed, pan_x, pan_y }` with pure `toggle()` and `on_navigate()` (pan **persists** across shot changes), plus a dedicated full-res slot on the pipeline glue.
**Design ref:** [`screens/1b-main.png`](../../../design/screens/1b-main.png) — sticky 1:1 zoom/pan adds no new chrome; keep the Task 8–9 HUD overlays intact over the zoomed photo.

`Z` toggles 1:1 zoom; **zoom level and pan position persist across prev/next** (§9) so a burst can be flipped through comparing focus on the same spot. When zoomed, the loupe requests `TargetSize::Full` for the current shot, which **bypasses the LRU** into a single dedicated slot (a ~180 MB frame must not evict prefetched neighbors, §12). `ZoomState` is pure and unit-tested; the loupe pan binding + full-decode slot are manually verified.

- [ ] **Step 1: Write the failing test** (append to `culler/src/ui.rs`):
```rust
#[cfg(test)]
mod zoom_tests {
    use super::*;

    #[test]
    fn toggle_flips_zoom_but_keeps_pan() {
        let mut z = ZoomState::default();
        assert!(!z.zoomed);
        z.pan_x = 120.0;
        z.pan_y = -40.0;
        z.toggle();
        assert!(z.zoomed);
        // toggling zoom must NOT reset the pan
        assert_eq!(z.pan_x, 120.0);
        assert_eq!(z.pan_y, -40.0);
    }

    #[test]
    fn navigation_preserves_zoom_and_pan() {
        let mut z = ZoomState { zoomed: true, pan_x: 200.0, pan_y: 55.0 };
        z.on_navigate(); // moving to another shot
        assert!(z.zoomed); // still zoomed
        assert_eq!(z.pan_x, 200.0); // pan sticky across prev/next
        assert_eq!(z.pan_y, 55.0);
    }
}
```

- [ ] **Step 2: Run to verify it fails** Run: `cargo test -p culler zoom_tests` Expected: FAIL "cannot find type `ZoomState`".

- [ ] **Step 3: Minimal implementation** (append to the non-test region of `culler/src/ui.rs`):
```rust
/// Sticky loupe zoom/pan. Both `zoomed` and pan persist across prev/next so the
/// same crop can be compared through a burst (§9).
#[derive(Clone, Copy, Debug, Default)]
pub struct ZoomState {
    pub zoomed: bool,
    pub pan_x: f32,
    pub pan_y: f32,
}

impl ZoomState {
    /// `Z`: flip 1:1 zoom, leaving pan untouched.
    pub fn toggle(&mut self) {
        self.zoomed = !self.zoomed;
    }

    /// Navigating to another shot keeps zoom + pan exactly as they were.
    pub fn on_navigate(&mut self) {
        // Intentionally a no-op: persistence IS the behavior. Kept explicit so a
        // future "reset pan on navigate" option has an obvious home.
    }

    /// Which decode target the loupe needs right now.
    pub fn target(&self, fit_w: u32, fit_h: u32) -> culler_core::TargetSize {
        if self.zoomed {
            culler_core::TargetSize::Full // bypasses the LRU cache (dedicated slot)
        } else {
            culler_core::TargetSize::Fit(fit_w, fit_h)
        }
    }
}
```

Add to `culler/src/pipeline.rs` a dedicated full-res slot (append to non-test region):
```rust
/// Single dedicated slot for the current 1:1/Full frame, kept OUT of the LRU so a
/// ~180 MB RGBA decode never evicts prefetched fit-size neighbors (§12).
#[derive(Default)]
pub struct FullSlot {
    pub index: Option<usize>,
    pub image: Option<Arc<DecodedImage>>,
}

impl FullSlot {
    /// Store the full decode for `index`, replacing any prior one.
    pub fn set(&mut self, index: usize, image: Arc<DecodedImage>) {
        self.index = Some(index);
        self.image = Some(image);
    }

    /// The stored full image iff it is for `index`.
    pub fn get(&self, index: usize) -> Option<Arc<DecodedImage>> {
        if self.index == Some(index) {
            self.image.clone()
        } else {
            None
        }
    }
}
```

- [ ] **Step 4a: Run to verify pass** Run: `cargo test -p culler zoom_tests` Expected: PASS (2 tests).

- [ ] **Step 4b (Manual verification):**
  - Press `Z` on a shot: it snaps to 1:1; drag pans; the view stays put.
  - Navigate `→`/`←` while zoomed: the next shot appears at the **same** zoom + pan (compare focus on the same spot across a burst).
  - Press `Z` again: back to fit. Pan is remembered if you re-zoom.
  - While zoomed, scrubbing many neighbors does not blow memory — the full frame occupies its own slot; the LRU still holds fit-size neighbors (add a temporary `eprintln!` of `LruCache::used_bytes()` to confirm it does not spike with full-res sizes).

- [ ] **Step 5: Commit**
```bash
git add culler/src/ui.rs culler/src/pipeline.rs
git commit -m "feat(ui): sticky ZoomState (pan persists across nav) + full-res dedicated slot"
```

---

### Task 11: `main.rs` — CLI parse, startup scan/resume/reattach, dest==source guard, event loop wiring

**Files:** Create `culler/src/startup.rs`; Modify `culler/src/main.rs` (add `mod startup;`, real `fn main`).
**Interfaces:** Consumes: `culler_core::{scan, load_or_fresh, save, Session, Shot, BUCKET_*, SESSION_FILE}`, all of `input`/`ui`/`pipeline`. Produces: `Cli` (clap), `pub fn default_buckets() -> [String;5]`, `pub fn resolve_buckets(&Cli) -> [String;5]`, `pub fn reattach(&Path, Vec<Shot>, Option<Session>) -> Session`, `pub fn dest_is_source_root(&Path, &Path) -> bool`, and the assembled event loop.
**Design ref:** [`screens/2a-startup.png`](../../../design/screens/2a-startup.png) (wordmark `fastcull` + dashed dropzone; the **recents list is Defer**, §6 — render wordmark + dropzone, stub/omit recents) + [`screens/2d-crash-recovery.png`](../../../design/screens/2d-crash-recovery.png) (gold-accented resume/report on crash detection). Tokens from `Theme` — startup gradient = `@radial-gradient(circle, Theme.bg-radial-in 0%, Theme.bg-radial-out 70%)`.

Startup (§7/§6): parse `source` (positional) + flags (`--no-auto-advance`, bucket-name overrides); `load_or_fresh(source)` (corrupt → `.bad` + fresh, handled by core); always `scan(source)` and re-attach prior decisions by stem via `reattach` (clamping `current`); a fresh scan gets a new session. `dest_is_source_root` refuses dest == source **root** (a source subfolder is allowed). Guard + reattach are pure and unit-tested; the event loop (key dispatch → `input`, decode requests → `pipeline`, autosave via `save`) is manually verified.

- [ ] **Step 1: Write the failing test** (append to `culler/src/startup.rs`):
```rust
#[cfg(test)]
mod startup_tests {
    use super::*;
    use culler_core::{CaptureTime, Decision, Session, Shot, Tier};

    fn shot(stem: &str, dir: &std::path::Path) -> Shot {
        Shot {
            stem: stem.into(),
            jpeg: dir.join(format!("{stem}.JPG")),
            raw: None,
            sidecar: None,
            capture: CaptureTime::default(),
        }
    }

    #[test]
    fn dest_equal_source_root_is_refused_subfolder_allowed() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path();
        assert!(dest_is_source_root(src, src)); // exact root -> refused
        let sub = src.join("sorted");
        assert!(!dest_is_source_root(src, &sub)); // subfolder -> allowed
        let elsewhere = tmp.path().parent().unwrap();
        assert!(!dest_is_source_root(src, elsewhere));
    }

    #[test]
    fn reattach_keeps_prior_decisions_by_stem_and_clamps_current() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let mut prev_decisions = std::collections::HashMap::new();
        prev_decisions.insert("IMG_0001".to_string(), Decision { tier: Some(Tier::Best), tags: vec!["hero".into()], visited: true });
        let prev = Session {
            source_dir: dir.to_path_buf(),
            shots: vec![shot("IMG_0001", dir), shot("IMG_0002", dir)],
            decisions: prev_decisions,
            current: 5, // stale index from before a rescan removed shots
            undo: vec![],
        };
        let scanned = vec![shot("IMG_0001", dir)]; // only one shot remains on disk
        let s = reattach(dir, scanned, Some(prev));
        assert_eq!(s.shots.len(), 1);
        assert_eq!(s.current, 0); // clamped into range
        assert_eq!(s.decision(0).tier, Some(Tier::Best)); // re-attached by stem
        assert_eq!(s.decision(0).tags, vec!["hero".to_string()]);
        assert!(s.undo.is_empty()); // undo not restored across sessions
    }

    #[test]
    fn reattach_none_starts_fresh() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let s = reattach(dir, vec![shot("IMG_0001", dir)], None);
        assert_eq!(s.current, 0);
        assert!(s.decisions.is_empty());
        assert_eq!(s.source_dir, dir);
    }

    #[test]
    fn resolve_buckets_defaults_and_overrides() {
        let d = default_buckets();
        assert_eq!(d, ["00_rejected", "01_rest", "02_keep", "03_picks", "04_bests"].map(String::from));
        let cli = Cli {
            source: "/x".into(),
            no_auto_advance: false,
            bucket_rejected: Some("trash".into()),
            bucket_rest: None,
            bucket_keep: None,
            bucket_picks: None,
            bucket_bests: None,
        };
        let b = resolve_buckets(&cli);
        assert_eq!(b[0], "trash");
        assert_eq!(b[1], "01_rest");
    }
}
```

- [ ] **Step 2: Run to verify it fails** Run: `cargo test -p culler startup_tests` Expected: FAIL "cannot find function `dest_is_source_root`".

- [ ] **Step 3: Minimal implementation** (top of `culler/src/startup.rs`):
```rust
use culler_core::{Session, Shot, BUCKET_BESTS, BUCKET_KEEP, BUCKET_PICKS, BUCKET_REJECTED, BUCKET_REST};
use std::path::{Path, PathBuf};

#[derive(clap::Parser, Debug)]
#[command(name = "culler", about = "FastCull — keyboard-driven photo culling")]
pub struct Cli {
    /// Source folder of shots (scanned flat, non-recursive).
    pub source: PathBuf,
    /// Disable single-key auto-advance (tiering stays on the current shot).
    #[arg(long)]
    pub no_auto_advance: bool,
    #[arg(long)]
    pub bucket_rejected: Option<String>,
    #[arg(long)]
    pub bucket_rest: Option<String>,
    #[arg(long)]
    pub bucket_keep: Option<String>,
    #[arg(long)]
    pub bucket_picks: Option<String>,
    #[arg(long)]
    pub bucket_bests: Option<String>,
}

/// Bucket names in canonical index order [rejected, rest, keep, picks, bests].
pub fn default_buckets() -> [String; 5] {
    [BUCKET_REJECTED, BUCKET_REST, BUCKET_KEEP, BUCKET_PICKS, BUCKET_BESTS].map(String::from)
}

pub fn resolve_buckets(cli: &Cli) -> [String; 5] {
    let d = default_buckets();
    [
        cli.bucket_rejected.clone().unwrap_or_else(|| d[0].clone()),
        cli.bucket_rest.clone().unwrap_or_else(|| d[1].clone()),
        cli.bucket_keep.clone().unwrap_or_else(|| d[2].clone()),
        cli.bucket_picks.clone().unwrap_or_else(|| d[3].clone()),
        cli.bucket_bests.clone().unwrap_or_else(|| d[4].clone()),
    ]
}

/// Build the working session from a fresh scan + any prior decisions (keyed by stem).
/// Prior `current` is clamped into the new shot range; the undo stack is not restored.
pub fn reattach(source: &Path, scanned: Vec<Shot>, prev: Option<Session>) -> Session {
    match prev {
        Some(p) => {
            let current = if scanned.is_empty() { 0 } else { p.current.min(scanned.len() - 1) };
            Session {
                source_dir: source.to_path_buf(),
                shots: scanned,
                decisions: p.decisions, // stem-keyed; survives a rescan
                current,
                undo: Vec::new(),
            }
        }
        None => Session {
            source_dir: source.to_path_buf(),
            shots: scanned,
            decisions: std::collections::HashMap::new(),
            current: 0,
            undo: Vec::new(),
        },
    }
}

/// True iff `dest` resolves to the source ROOT itself (which is refused).
/// A source subfolder is allowed. A not-yet-created dest can't be the existing root.
pub fn dest_is_source_root(source: &Path, dest: &Path) -> bool {
    match (source.canonicalize(), dest.canonicalize()) {
        (Ok(s), Ok(d)) => s == d,
        _ => source == dest,
    }
}
```

- [ ] **Step 4a: Run to verify pass** Run: `cargo test -p culler startup_tests` Expected: PASS (4 tests).

- [ ] **Step 4b (real `main.rs` + Manual verification):** wire the event loop and confirm the whole §9 keymap end-to-end (Manual Test Checklist at the end of this file). `culler/src/main.rs`:
```rust
slint::include_modules!();

mod input;
mod pipeline;
mod startup;
mod ui;
mod applyflow; // Task 12

use clap::Parser;
use culler_core::{load_or_fresh, save, scan, TargetSize};
use input::{apply_action, key_to_action, next_filter, parse_tags, to_key, Filter, InputContext, Action};
use pipeline::{prefetch_set, to_slint_image, FullSlot, Pipeline};
use startup::{dest_is_source_root, reattach, resolve_buckets, Cli};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

const PREFETCH_N: usize = 4;
const FILMSTRIP_BUFFER: usize = 8;
const CACHE_BUDGET: usize = 512 * 1024 * 1024; // 512 MB of fit-size textures

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let source = cli.source.clone();
    let auto_advance = !cli.no_auto_advance;
    let buckets = resolve_buckets(&cli);

    // Startup: load-or-fresh, always rescan, reattach by stem.
    let prev = load_or_fresh(&source)?; // corrupt -> .bad + Ok(None), reported by core
    let scanned = scan(&source)?;
    let session = reattach(&source, scanned, prev);

    // Crash detection on the source dir (in case it was a prior destination) — Task 13.
    if let Some(j) = startup::find_crashed_apply(&source) {
        eprintln!("{}", startup::journal_report(&j).unwrap_or_default());
        // UI surfaces resume-or-report; see Task 13.
    }

    let app = AppWindow::new()?;
    let session = Rc::new(RefCell::new(session));
    let filter = Rc::new(RefCell::new(Filter::All));
    let zoom = Rc::new(RefCell::new(ui::ZoomState::default()));
    let cache = Arc::new(Mutex::new(pipeline::LruCache::new(CACHE_BUDGET)));
    let full_slot = Arc::new(Mutex::new(FullSlot::default()));

    // Decode pipeline. on_ready drops stale results at delivery, then updates cache/loupe.
    let weak = app.as_weak();
    let cache_w = cache.clone();
    let full_w = full_slot.clone();
    let pipeline = Arc::new(Pipeline::spawn(3, move |res| {
        let weak = weak.clone();
        let cache_w = cache_w.clone();
        let full_w = full_w.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(app) = weak.upgrade() {
                match res.target {
                    TargetSize::Full => {
                        full_w.lock().unwrap().set(res.req.index, res.image.clone());
                        app.set_current_image(to_slint_image(&res.image));
                    }
                    _ => {
                        cache_w.lock().unwrap().put(res.req.index, res.image.clone());
                        // Only paint if still the current shot (delivery-time freshness).
                        app.set_current_image(to_slint_image(&res.image));
                    }
                }
            }
        });
    }));

    // Helper: (re)request current + prefetch neighbors after any navigation.
    let request_current = {
        let session = session.clone();
        let zoom = zoom.clone();
        let cache = cache.clone();
        let full_slot = full_slot.clone();
        let pipeline = pipeline.clone();
        let app_w = app.as_weak();
        move || {
            let s = session.borrow();
            if s.shots.is_empty() { return; }
            let (fw, fh) = (1600u32, 1000u32);
            pipeline.bump(); // latest-wins: supersede in-flight requests
            let cur = s.current;
            let z = *zoom.borrow();
            // Show the best already-cached scale immediately.
            if let Some(app) = app_w.upgrade() {
                if z.zoomed {
                    if let Some(img) = full_slot.lock().unwrap().get(cur) {
                        app.set_current_image(to_slint_image(&img));
                    }
                } else if let Some(img) = cache.lock().unwrap().get(cur) {
                    app.set_current_image(to_slint_image(&img));
                }
            }
            // Request the exact target for current.
            pipeline.enqueue(cur, s.shots[cur].jpeg.clone(), z.target(fw, fh), false);
            // Prefetch neighbors (fit-size only).
            for idx in prefetch_set(cur, PREFETCH_N, s.shots.len()) {
                if idx != cur && !cache.lock().unwrap().contains(idx) {
                    pipeline.enqueue(idx, s.shots[idx].jpeg.clone(), TargetSize::Fit(fw, fh), false);
                }
            }
        }
    };

    // Refresh HUD + filmstrip from current state.
    let refresh_view = {
        let session = session.clone();
        let filter = filter.clone();
        let cache = cache.clone();
        let app_w = app.as_weak();
        move || {
            let Some(app) = app_w.upgrade() else { return };
            let s = session.borrow();
            let h = ui::hud_text(&s, *filter.borrow());
            app.set_hud_tier(h.tier.into());
            app.set_hud_tags(h.tags.into());
            app.set_hud_counts(h.counts.into());
            app.set_hud_progress(h.progress.into());
            app.set_filter_label(h.filter_label.into());
            let mut cache = cache.lock().unwrap();
            let grey = pipeline::grey_thumb();
            let thumb_for = |i: usize| cache.get(i).map(|im| to_slint_image(&im)).unwrap_or_else(|| grey.clone());
            ui::refresh_filmstrip(&app, &s, *filter.borrow(), FILMSTRIP_BUFFER, &thumb_for);
        }
    };

    // Key dispatch: pure map -> action -> mutate model or UI state, then refresh.
    {
        let session = session.clone();
        let filter = filter.clone();
        let zoom = zoom.clone();
        let request_current = request_current.clone();
        let refresh_view = refresh_view.clone();
        let app_w = app.as_weak();
        let source = source.clone();
        app.on_key_pressed(move |text, ctrl| {
            let Some(app) = app_w.upgrade() else { return false };
            let ctx = if app.get_tag_open() {
                InputContext::TagEntry
            } else if app.get_apply_open() {
                InputContext::ApplyDialog
            } else {
                InputContext::Loupe
            };
            let Some(key) = to_key(&text) else { return false };
            let mods = input::Modifiers { control: ctrl, ..Default::default() };
            let Some(action) = key_to_action(key, mods, ctx) else { return false };
            match action {
                Action::CycleFilter => {
                    let nf = next_filter(*filter.borrow());
                    *filter.borrow_mut() = nf;
                    refresh_view();
                }
                Action::ToggleZoom => {
                    zoom.borrow_mut().toggle();
                    app.set_zoomed(zoom.borrow().zoomed);
                    request_current();
                }
                Action::OpenTagEntry => {
                    let s = session.borrow();
                    app.set_tag_text(s.decision(s.current).tags.join(", ").into());
                    app.set_tag_open(true);
                }
                Action::OpenApply => { app.set_apply_open(true); }
                Action::ForceSave => {
                    let _ = save(&session.borrow(), &source.join(culler_core::SESSION_FILE));
                }
                other => {
                    let before = session.borrow().current;
                    apply_action(other, &mut session.borrow_mut(), auto_advance, *filter.borrow());
                    if session.borrow().current != before {
                        zoom.borrow_mut().on_navigate();
                        request_current();
                    }
                    // Autosave after every model change.
                    let _ = save(&session.borrow(), &source.join(culler_core::SESSION_FILE));
                    refresh_view();
                }
            }
            true
        });
    }

    // Filmstrip click -> jump.
    {
        let session = session.clone();
        let filter = filter.clone();
        let request_current = request_current.clone();
        let refresh_view = refresh_view.clone();
        app.on_film_clicked(move |offset| {
            let (indices, _) = ui::build_filmstrip_window(&session.borrow(), *filter.borrow(), FILMSTRIP_BUFFER);
            if let Some(&idx) = indices.get(offset as usize) {
                session.borrow_mut().current = idx;
                session.borrow_mut().mark_visited(idx);
                request_current();
                refresh_view();
            }
        });
    }

    // Tag entry commit / cancel.
    {
        let session = session.clone();
        let refresh_view = refresh_view.clone();
        let source = source.clone();
        let app_w = app.as_weak();
        app.on_tag_committed(move |text| {
            let Some(app) = app_w.upgrade() else { return };
            let idx = session.borrow().current;
            session.borrow_mut().set_tags(idx, parse_tags(&text));
            app.set_tag_open(false);
            let _ = save(&session.borrow(), &source.join(culler_core::SESSION_FILE));
            refresh_view();
        });
    }
    {
        let app_w = app.as_weak();
        app.on_tag_cancelled(move || { if let Some(a) = app_w.upgrade() { a.set_tag_open(false); } });
    }
    {
        let session = session.clone();
        let app_w = app.as_weak();
        app.on_tag_changed(move |text| {
            if let Some(app) = app_w.upgrade() {
                let all = session.borrow().all_tags();
                let last = text.rsplit(',').next().unwrap_or("").to_string();
                let sugg: Vec<slint::SharedString> =
                    ui::suggest_tags(&all, &last).into_iter().map(Into::into).collect();
                app.set_tag_suggestions(std::rc::Rc::new(slint::VecModel::from(sugg)).into());
            }
        });
    }

    // Apply dialog callbacks are wired in Task 12 (applyflow).
    applyflow::wire_apply_dialog(&app, session.clone(), buckets.clone());

    // First paint.
    {
        let mut s = session.borrow_mut();
        if !s.shots.is_empty() {
            let cur = s.current;
            s.mark_visited(cur);
        }
    }
    request_current();
    refresh_view();
    app.run()?;
    Ok(())
}
```
Also add to `culler/src/pipeline.rs` a tiny grey placeholder helper (append, non-test):
```rust
/// A 1x1 grey placeholder image for filmstrip tiles not yet decoded.
pub fn grey_thumb() -> Image {
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(1, 1);
    buf.make_mut_bytes().copy_from_slice(&[128, 128, 128, 255]);
    Image::from_rgba8(buf)
}
```

- [ ] **Step 5: Commit**
```bash
git add culler/src/startup.rs culler/src/main.rs culler/src/pipeline.rs
git commit -m "feat(main): CLI parse, startup scan/reattach, dest guard, event-loop wiring"
```

---

### Task 12: Apply dialog — gather inputs, plan preview, confirm/apply, session relocation, crash-journal resume

**Files:** Create `culler/src/applyflow.rs`, `culler/ui/applydialog.slint`; Modify `culler/src/startup.rs` (add `find_crashed_apply`, `journal_report`), `culler/ui/app.slint` (apply-dialog properties/callbacks + overlay).
**Interfaces:** Consumes: `culler_core::{plan, apply, resume, save, RealFs, FsOps, ApplyPlan, Journal, OpState, TierCountsPlan, ApplyReport, JOURNAL_FILE, SESSION_FILE}`, `startup::{dest_is_source_root, default_buckets}`. Produces: `pub fn gather_apply_inputs(&Session, &Path, &[String;5]) -> (BTreeSet<String>, HashMap<String,u64>, usize)`, `ApplyPreview`, `pub fn build_preview(&ApplyPlan, usize, bool, Option<u64>) -> ApplyPreview`, `pub fn compute_preview(&Session, &Path, &[String;5]) -> (ApplyPlan, ApplyPreview)`, `pub fn run_apply(&Session, &Path, &[String;5]) -> Result<ApplyReport,String>`, `pub fn wire_apply_dialog(&AppWindow, Rc<RefCell<Session>>, [String;5])`; and in `startup`: `pub fn find_crashed_apply(&Path) -> Option<PathBuf>`, `pub fn journal_report(&Path) -> io::Result<String>`.
**Design ref:** [`screens/2b-apply-dialog.png`](../../../design/screens/2b-apply-dialog.png), [`screens/2c-apply-progress.png`](../../../design/screens/2c-apply-progress.png), [`screens/2d-crash-recovery.png`](../../../design/screens/2d-crash-recovery.png) — scrim (`Theme.scrim`) + centered `Theme.panel-dialog` modal (r-xl), per-bucket table with tier dots, **green confirm** (`Theme.accent-confirm`) / **bordered cancel**, **gold resume** (`Theme.accent-warn`). **Replace the placeholder hex in the `.slint` below** (`#262626`, `#f0a35e`, `#99bbee`, `#f85149`, `#ccaa88`, `#000000cc`, …) with `Theme.*`; [DESIGN.md](../../../design/DESIGN.md) §4 (2b–2d).

The full §6 Apply workflow. `A` (already mapped) sets `apply_open`. The dialog takes a destination path; the **dest == source root** guard is `startup::dest_is_source_root` from Task 11 (reused, not duplicated — a source subfolder is allowed). `gather_apply_inputs` reads `existing` names + per-stem `sizes` + the leftover unrecognized-file count from disk so `plan` stays pure; `build_preview` folds in collision resolutions (`ShotOp.suffix`), skipped-sidecar and stale counts, and a cross-filesystem free-space check. Confirm calls `apply` (journaling to `dest/.fastcull-apply.json`) — or `resume` if a journal is already there — then relocates `source/.fastcull.json` into `dest` as the audit record (the responsibility Phase 4 left to `main`). Launch-time crash detection referenced in Task 11's `main` is implemented here (`find_crashed_apply` + `journal_report`) and the dialog also offers resume-or-report for the chosen dest.

**Crash-journal resume-or-report landed in Task 12** (functions live in `startup.rs`, called from Task 11's `main` and from this dialog).

- [ ] **Step 1: Write the failing tests**

Append to `culler/src/applyflow.rs`:
```rust
#[cfg(test)]
mod applyflow_tests {
    use super::*;
    use culler_core::{CaptureTime, Decision, Session, Shot, Tier};

    fn mk_shot(stem: &str, dir: &std::path::Path) -> Shot {
        Shot {
            stem: stem.into(),
            jpeg: dir.join(format!("{stem}.JPG")),
            raw: None,
            sidecar: None,
            capture: CaptureTime::default(),
        }
    }

    #[test]
    fn gather_and_preview_counts_leftovers_bytes_and_buckets() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path();
        std::fs::write(src.join("IMG_1.JPG"), b"aaaa").unwrap(); // 4 bytes -> Keep
        std::fs::write(src.join("IMG_2.JPG"), b"bbbbbb").unwrap(); // 6 bytes -> Reject
        std::fs::write(src.join("clip.MOV"), b"zz").unwrap(); // unrecognized -> leftover

        let mut decisions = std::collections::HashMap::new();
        decisions.insert("IMG_1".to_string(), Decision { tier: Some(Tier::Keep), tags: vec![], visited: true });
        decisions.insert("IMG_2".to_string(), Decision { tier: Some(Tier::Reject), tags: vec![], visited: true });
        let session = Session {
            source_dir: src.to_path_buf(),
            shots: vec![mk_shot("IMG_1", src), mk_shot("IMG_2", src)],
            decisions,
            current: 0,
            undo: vec![],
        };
        let buckets = crate::startup::default_buckets();
        let dest = src.join("sorted"); // a source subfolder is allowed; not created yet

        let (existing, sizes, leftovers) = gather_apply_inputs(&session, &dest, &buckets);
        assert!(existing.is_empty()); // dest buckets do not exist yet
        assert_eq!(sizes["IMG_1"], 4);
        assert_eq!(sizes["IMG_2"], 6);
        assert_eq!(leftovers, 1); // clip.MOV stays behind

        let planned = culler_core::plan(&session, &dest, &buckets, &existing, &sizes);
        let preview = build_preview(&planned, leftovers, false, None);
        assert_eq!(preview.per_bucket.keep, 1);
        assert_eq!(preview.per_bucket.rejected, 1);
        assert_eq!(preview.leftovers, 1);
        assert_eq!(preview.total_bytes, 10);
        assert_eq!(preview.collisions, 0);
        assert!(preview.enough_space); // same-fs -> no space gate
    }

    #[test]
    fn build_preview_gates_on_free_space_when_cross_fs() {
        let planned = culler_core::ApplyPlan {
            dest: "/mnt/other/sorted".into(),
            buckets: crate::startup::default_buckets(),
            ops: vec![],
            per_bucket_counts: culler_core::TierCountsPlan::default(),
            skipped_sidecar_writes: vec![],
            stale: vec![],
            total_bytes: 1_000,
        };
        let ok = build_preview(&planned, 0, true, Some(2_000));
        assert!(ok.cross_fs && ok.enough_space);
        let tight = build_preview(&planned, 0, true, Some(500));
        assert!(tight.cross_fs && !tight.enough_space); // 500 < 1000
    }
}
```

Append to `culler/src/startup.rs`:
```rust
#[cfg(test)]
mod crash_tests {
    use super::*;

    #[test]
    fn find_and_report_an_interrupted_apply() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path();
        assert!(find_crashed_apply(dest).is_none()); // clean

        let plan = culler_core::ApplyPlan {
            dest: dest.to_path_buf(),
            buckets: default_buckets(),
            ops: vec![],
            per_bucket_counts: culler_core::TierCountsPlan::default(),
            skipped_sidecar_writes: vec![],
            stale: vec![],
            total_bytes: 0,
        };
        let journal = culler_core::Journal {
            plan,
            statuses: vec![culler_core::OpState::Done, culler_core::OpState::Pending],
        };
        std::fs::write(
            dest.join(culler_core::JOURNAL_FILE),
            serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();

        let found = find_crashed_apply(dest).expect("journal detected");
        let report = journal_report(&found).unwrap();
        assert!(report.contains("done: 1"));
        assert!(report.contains("pending: 1"));
        assert!(report.contains("failed: 0"));
    }
}
```

- [ ] **Step 2: Run to verify it fails** Run: `cargo test -p culler applyflow_tests crash_tests` Expected: FAIL "cannot find function `gather_apply_inputs`" / "`find_crashed_apply`".

- [ ] **Step 3: Minimal implementation**

Append to the non-test region of `culler/src/startup.rs`:
```rust
/// Detect an interrupted apply: a journal left in `dir` by a prior crashed run.
pub fn find_crashed_apply(dir: &Path) -> Option<PathBuf> {
    let j = dir.join(culler_core::JOURNAL_FILE);
    if j.is_file() {
        Some(j)
    } else {
        None
    }
}

/// Human-readable per-op status summary for the resume-or-report prompt.
pub fn journal_report(journal_path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(journal_path)?;
    let journal: culler_core::Journal = serde_json::from_slice(&bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let count = |want: culler_core::OpState| journal.statuses.iter().filter(|s| **s == want).count();
    Ok(format!(
        "Interrupted apply into {}\n  done: {}  pending: {}  failed: {}",
        journal.plan.dest.display(),
        count(culler_core::OpState::Done),
        count(culler_core::OpState::Pending),
        count(culler_core::OpState::Failed),
    ))
}
```

`culler/src/applyflow.rs` (top of file):
```rust
use crate::AppWindow;
use culler_core::{apply, plan, resume, save, ApplyPlan, ApplyReport, FsOps, RealFs, Session};
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::rc::Rc;

/// Everything the preview shows.
pub struct ApplyPreview {
    pub per_bucket: culler_core::TierCountsPlan,
    pub collisions: usize,       // ops whose whole stem was auto-suffixed
    pub skipped_sidecars: usize, // pre-existing sidecar carried, tag-write skipped
    pub stale: usize,            // stems that vanished from disk
    pub leftovers: usize,        // unrecognized source files that stay behind
    pub total_bytes: u64,
    pub cross_fs: bool,
    pub free_bytes: Option<u64>,
    pub enough_space: bool,
}

/// Gather the plan's I/O-derived inputs so `plan` itself stays pure:
///  - `existing`: file names already under any dest bucket (collision detection)
///  - `sizes`: stem -> total bytes of the shot's files (free-space preflight)
///  - leftover count: source files belonging to no shot (they stay behind)
pub fn gather_apply_inputs(
    session: &Session,
    dest: &Path,
    buckets: &[String; 5],
) -> (BTreeSet<String>, HashMap<String, u64>, usize) {
    let mut existing = BTreeSet::new();
    for b in buckets {
        if let Ok(rd) = std::fs::read_dir(dest.join(b)) {
            for e in rd.flatten() {
                if let Some(name) = e.file_name().to_str() {
                    existing.insert(name.to_string());
                }
            }
        }
    }

    let mut sizes = HashMap::new();
    let mut shot_names = BTreeSet::new();
    for shot in &session.shots {
        let mut total = 0u64;
        for f in shot.files() {
            if let Ok(md) = std::fs::metadata(&f) {
                total += md.len();
            }
            if let Some(name) = f.file_name().and_then(|n| n.to_str()) {
                shot_names.insert(name.to_string());
            }
        }
        sizes.insert(shot.stem.clone(), total);
    }

    let mut leftovers = 0usize;
    if let Ok(rd) = std::fs::read_dir(&session.source_dir) {
        for e in rd.flatten() {
            if e.path().is_file() {
                if let Some(name) = e.file_name().to_str() {
                    if name.starts_with('.') {
                        continue; // dotfiles incl. the session sidecar
                    }
                    if !shot_names.contains(name) {
                        leftovers += 1;
                    }
                }
            }
        }
    }

    (existing, sizes, leftovers)
}

/// Assemble the preview from a computed plan + gathered facts.
pub fn build_preview(
    planned: &ApplyPlan,
    leftovers: usize,
    cross_fs: bool,
    free_bytes: Option<u64>,
) -> ApplyPreview {
    let collisions = planned.ops.iter().filter(|o| o.suffix.is_some()).count();
    let enough_space = match (cross_fs, free_bytes) {
        (true, Some(free)) => free >= planned.total_bytes,
        _ => true, // same-fs (rename) never needs a space gate
    };
    ApplyPreview {
        per_bucket: planned.per_bucket_counts,
        collisions,
        skipped_sidecars: planned.skipped_sidecar_writes.len(),
        stale: planned.stale.len(),
        leftovers,
        total_bytes: planned.total_bytes,
        cross_fs,
        free_bytes,
        enough_space,
    }
}

/// True when the move crosses filesystems (probes the nearest existing ancestor of dest).
fn probe_cross_fs(fs: &RealFs, source: &Path, dest: &Path) -> bool {
    let mut probe = dest;
    while !probe.exists() {
        match probe.parent() {
            Some(p) => probe = p,
            None => return false,
        }
    }
    fs.same_filesystem(source, probe).map(|same| !same).unwrap_or(false)
}

/// Gather + plan + preview in one step (used by the dialog's "Compute preview").
pub fn compute_preview(session: &Session, dest: &Path, buckets: &[String; 5]) -> (ApplyPlan, ApplyPreview) {
    let (existing, sizes, leftovers) = gather_apply_inputs(session, dest, buckets);
    let planned = plan(session, dest, buckets, &existing, &sizes);
    let fs = RealFs;
    let cross_fs = probe_cross_fs(&fs, &session.source_dir, dest);
    let free_bytes = if cross_fs { fs.free_space(dest).ok() } else { None };
    let preview = build_preview(&planned, leftovers, cross_fs, free_bytes);
    (planned, preview)
}

/// Move the session sidecar into dest as the audit record (Phase 4 left this to main).
fn relocate_session(session: &Session, dest: &Path) -> std::io::Result<()> {
    let from = session.source_dir.join(culler_core::SESSION_FILE);
    let to = dest.join(culler_core::SESSION_FILE);
    if std::fs::rename(&from, &to).is_err() {
        // cross-fs or already-moved: write a fresh copy of the record instead
        save(session, &to)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("{e:?}")))?;
    }
    Ok(())
}

/// Fresh apply: gather -> plan -> journaled apply -> relocate session record.
pub fn run_apply(session: &Session, dest: &Path, buckets: &[String; 5]) -> Result<ApplyReport, String> {
    let (existing, sizes, _leftovers) = gather_apply_inputs(session, dest, buckets);
    let planned = plan(session, dest, buckets, &existing, &sizes);
    let fs = RealFs;
    let journal_path = dest.join(culler_core::JOURNAL_FILE);
    let report = apply(&planned, &fs, &journal_path).map_err(|e| format!("{e:?}"))?;
    relocate_session(session, dest).map_err(|e| format!("session relocation: {e}"))?;
    Ok(report)
}

fn to_ui_preview(p: &ApplyPreview) -> crate::ApplyPreviewUi {
    crate::ApplyPreviewUi {
        rejected: p.per_bucket.rejected as i32,
        rest: p.per_bucket.rest as i32,
        keep: p.per_bucket.keep as i32,
        picks: p.per_bucket.picks as i32,
        bests: p.per_bucket.bests as i32,
        collisions: p.collisions as i32,
        skipped_sidecars: p.skipped_sidecars as i32,
        stale: p.stale as i32,
        leftovers: p.leftovers as i32,
        total_mb: (p.total_bytes / (1024 * 1024)) as i32,
        cross_fs: p.cross_fs,
        enough_space: p.enough_space,
    }
}

/// Wire the Apply dialog callbacks: dest validation + crash-journal probe, preview,
/// confirm (apply or resume) + session relocation, cancel.
pub fn wire_apply_dialog(app: &AppWindow, session: Rc<RefCell<Session>>, buckets: [String; 5]) {
    // Destination typed: guard dest==source root, probe for a crashed journal.
    {
        let session = session.clone();
        let app_w = app.as_weak();
        app.on_dest_changed(move |dest_str| {
            let Some(app) = app_w.upgrade() else { return };
            let dest = std::path::PathBuf::from(dest_str.to_string());
            let source = session.borrow().source_dir.clone();
            app.set_preview_ready(false);
            if dest.as_os_str().is_empty() {
                app.set_dest_error("".into());
                return;
            }
            if crate::startup::dest_is_source_root(&source, &dest) {
                app.set_dest_error(
                    "Destination cannot be the source folder itself (a subfolder is fine).".into(),
                );
                return;
            }
            if let Some(j) = crate::startup::find_crashed_apply(&dest) {
                let report = crate::startup::journal_report(&j).unwrap_or_default();
                app.set_dest_error(
                    format!("{report}\nConfirm to RESUME this interrupted apply.").into(),
                );
            } else {
                app.set_dest_error("".into());
            }
        });
    }

    // Compute preview.
    {
        let session = session.clone();
        let buckets = buckets.clone();
        let app_w = app.as_weak();
        app.on_apply_refresh(move || {
            let Some(app) = app_w.upgrade() else { return };
            let dest = std::path::PathBuf::from(app.get_dest_path().to_string());
            let source = session.borrow().source_dir.clone();
            if dest.as_os_str().is_empty() || crate::startup::dest_is_source_root(&source, &dest) {
                app.set_preview_ready(false);
                return;
            }
            let (_planned, preview) = compute_preview(&session.borrow(), &dest, &buckets);
            app.set_preview(to_ui_preview(&preview));
            app.set_preview_ready(true);
        });
    }

    // Confirm: resume a crashed run if a journal exists, else fresh apply.
    {
        let session = session.clone();
        let buckets = buckets.clone();
        let app_w = app.as_weak();
        app.on_apply_confirmed(move || {
            let Some(app) = app_w.upgrade() else { return };
            let dest = std::path::PathBuf::from(app.get_dest_path().to_string());
            let source = session.borrow().source_dir.clone();
            if crate::startup::dest_is_source_root(&source, &dest) {
                return;
            }
            let journal_path = dest.join(culler_core::JOURNAL_FILE);
            let result = if journal_path.exists() {
                resume(&journal_path, &RealFs)
                    .map_err(|e| format!("{e:?}"))
                    .and_then(|r| {
                        relocate_session(&session.borrow(), &dest)
                            .map(|_| r)
                            .map_err(|e| format!("session relocation: {e}"))
                    })
            } else {
                run_apply(&session.borrow(), &dest, &buckets)
            };
            match result {
                Ok(report) => {
                    app.set_dest_error(
                        format!(
                            "Applied: {} shots, {} files moved, {} sidecars written.",
                            report.moved_shots, report.moved_files, report.sidecars_written
                        )
                        .into(),
                    );
                    app.set_apply_open(false);
                }
                Err(e) => app.set_dest_error(format!("Apply failed: {e}").into()),
            }
        });
    }

    // Cancel.
    {
        let app_w = app.as_weak();
        app.on_apply_cancelled(move || {
            if let Some(a) = app_w.upgrade() {
                a.set_apply_open(false);
            }
        });
    }
}
```

- [ ] **Step 4a: Run to verify pass** Run: `cargo test -p culler applyflow_tests crash_tests` Expected: PASS (3 tests).

- [ ] **Step 4b (Manual verification of the dialog UI):** wire the `.slint` below and confirm the §6 Apply flow end-to-end on a real folder:
  - `A` opens the dialog; typing the source root itself shows the refusal error and disables Confirm; a source *subfolder* (e.g. `sorted`) is accepted.
  - "Compute preview" shows per-bucket counts including `00_rejected`, collision auto-suffix count, skipped-sidecar count, stale count, unrecognized-files-left-behind count, and total MB.
  - Choosing a destination on another filesystem shows a free-space line; Confirm is disabled when space is insufficient.
  - Confirm executes the move: buckets appear in dest, rejects land in `00_rejected` (never deleted), the session `.fastcull.json` is relocated into dest, and the source is left with only the unrecognized files.
  - Pointing the dialog at a dest that already holds a `.fastcull-apply.json` shows the interrupted-apply report and Confirm **resumes** it (verify against a deliberately half-written journal).
  - Launch-time: starting `culler` on a folder that contains a leftover journal prints the report to stderr (Task 11 `main`), confirming detection.

`culler/ui/applydialog.slint`:
```slint
import { LineEdit, Button } from "std-widgets.slint";
import { ApplyPreviewUi } from "globals.slint";

export component ApplyDialog inherits Rectangle {
    in-out property <string> dest;
    in property <string> dest-error;
    in property <bool> preview-ready;
    in property <ApplyPreviewUi> preview;
    callback dest-changed(string);
    callback refresh();
    callback confirmed();
    callback cancelled();
    background: #000000cc;

    Rectangle {
        width: 640px;
        height: 480px;
        background: #262626;
        border-radius: 8px;
        VerticalLayout {
            padding: 20px;
            spacing: 10px;
            Text { text: "Apply — reorganize into destination"; color: white; font-size: 18px; }
            LineEdit {
                text <=> root.dest;
                placeholder-text: "/path/to/destination (a source subfolder is allowed)";
                edited(t) => { root.dest-changed(t); }
            }
            if root.dest-error != "": Text { text: root.dest-error; color: #f0a35e; wrap: word-wrap; }
            Button { text: "Compute preview"; clicked => { root.refresh(); } }
            if root.preview-ready: VerticalLayout {
                spacing: 4px;
                Text {
                    color: #cccccc;
                    text: "00_rejected: " + root.preview.rejected
                        + "   01_rest: " + root.preview.rest
                        + "   02_keep: " + root.preview.keep
                        + "   03_picks: " + root.preview.picks
                        + "   04_bests: " + root.preview.bests;
                }
                Text {
                    color: #ccaa88;
                    text: "collisions auto-suffixed: " + root.preview.collisions
                        + "   sidecar-writes skipped: " + root.preview.skipped-sidecars;
                }
                Text {
                    color: #ccaa88;
                    text: "stale (vanished): " + root.preview.stale
                        + "   unrecognized files left behind: " + root.preview.leftovers;
                }
                Text {
                    color: root.preview.enough-space ? #99bbee : #f85149;
                    text: "total: " + root.preview.total-mb + " MB"
                        + (root.preview.cross-fs
                            ? (root.preview.enough-space ? "  (cross-FS: space OK)" : "  (cross-FS: NOT ENOUGH SPACE)")
                            : "  (same filesystem)");
                }
            }
            HorizontalLayout {
                alignment: end;
                spacing: 8px;
                Button { text: "Cancel"; clicked => { root.cancelled(); } }
                Button {
                    text: "Confirm move";
                    enabled: root.preview-ready && root.preview.enough-space;
                    clicked => { root.confirmed(); }
                }
            }
        }
    }
}
```

Add to `culler/ui/app.slint`: `import { ApplyDialog } from "applydialog.slint";` and `import { ApplyPreviewUi } from "globals.slint";`, the properties/callbacks:
```slint
    in-out property <bool> apply-open;
    in-out property <string> dest-path;
    in property <string> dest-error;
    in property <bool> preview-ready;
    in property <ApplyPreviewUi> preview;
    callback dest-changed(string);
    callback apply-refresh();
    callback apply-confirmed();
    callback apply-cancelled();
```
and the overlay inside `keyscope`:
```slint
        if root.apply-open: ApplyDialog {
            dest <=> root.dest-path;
            dest-error: root.dest-error;
            preview-ready: root.preview-ready;
            preview: root.preview;
            dest-changed(d) => { root.dest-changed(d); }
            refresh() => { root.apply-refresh(); }
            confirmed() => { root.apply-confirmed(); }
            cancelled() => { root.apply-cancelled(); }
        }
```

- [ ] **Step 5: Commit**
```bash
git add culler/src/applyflow.rs culler/src/startup.rs culler/ui/applydialog.slint culler/ui/app.slint
git commit -m "feat(main): Apply dialog flow — gather/preview, confirm/apply/resume, session relocation, crash-journal detection"
```

---

### Task 13: Visual-fidelity pass against `docs/design/`

**Files:** touch-ups across `culler/ui/*.slint` (+ `theme.slint`); no new modules.
**Interfaces:** Consumes: the running binary + [`docs/design/screens/*.png`](../../../design/screens/). Produces: a deviation list and the fixes for it.
**Design ref:** all screens + the **fidelity checklist** in [DESIGN.md §7](../../../design/DESIGN.md).

Pure-visual verification task (no unit tests). The GUI is now complete; this holds it to the imported design.

- [ ] **Step 1: Run & capture.** `cargo run -p culler -- <a test folder of shots>`; drive each surface — loupe, filmstrip, HUD, tag entry (`T`), Apply (`A`), and, if built, keymap (`?`) and toasts — and screenshot each.
- [ ] **Step 2: Compare** each surface side-by-side with its `docs/design/screens/*.png` (per the Task→screen map at the top) and run [DESIGN.md §7](../../../design/DESIGN.md): tier colors exact (`#d05f5f/#63666c/#57a86d/#5a93d4/#d2a545`); Mono-for-data / Sans-for-chrome with IBM Plex bundled; translucent HUD panels inset ~14px; filmstrip dots + unvisited-dim + current outline; Apply dialog matches 2b; button roles blue/green/gold/bordered with dark text on fills.
- [ ] **Step 3: Fix** deviations by pointing components at the correct `Theme.*` tokens. Confirm **no** component hardcodes a hex that lives in `Theme` (grep the `.slint` for stray `#` literals). Elements marked **Defer** in [DESIGN.md §6](../../../design/DESIGN.md) (recents, histogram, EXIF) are expected to be absent — do not add them; the known **no-backdrop-blur** difference (§5) is accepted.
- [ ] **Step 4: Commit**
```bash
git add culler/ui
git commit -m "fix(ui): visual-fidelity pass — match docs/design screens, tokens via Theme"
```

---

## Phase 6 done — definition of done

Phase 6 delivers the running `culler` binary — the last piece of FastCull v1, matched to the imported design in [`docs/design/`](../../../design/) (tokens in `theme.slint`, verified against `docs/design/screens/` in Task 13).

**Modules / files delivered**
- `culler/Cargo.toml`, `culler/build.rs`, workspace `Cargo.toml` member — crate wiring (Slint + Skia, clap, serde_json; tempfile dev-dep).
- `culler/src/input.rs` — keymap + filter + tag/action logic.
- `culler/src/pipeline.rs` — scheduler, LRU, prefetch, worker pool, marshaling.
- `culler/src/ui.rs` — color/window/HUD/zoom pure helpers + Slint model glue.
- `culler/src/startup.rs` — CLI, bucket resolution, reattach, dest guard, crash-journal detection.
- `culler/src/applyflow.rs` — Apply gather/preview/confirm/apply/resume + session relocation.
- `culler/src/main.rs` — startup + event-loop wiring.
- `culler/ui/{app,globals,theme,loupe,filmstrip,hud,applydialog}.slint` + `culler/ui/fonts/` — the UI, styled from `theme.slint` (the design-token global built in Task 1b from [`docs/design/DESIGN.md`](../../../design/DESIGN.md)) and bundled IBM Plex fonts.

**Unit-tested pure/public symbols** (run headless, no window): `to_key`, `key_to_action` (full §9 keymap incl. `Ctrl+S`, modal gating); `next_filter`, `passes`, `step_filtered`, `parse_tags`; `apply_action` (auto-advance on/off, clear-never-advances, filter-confined nav, undo, UI-only no-ops); `Scheduler` (generation-counter staleness); `LruCache` (budget eviction, MRU touch, update-in-place), `prefetch_set` (forward-biased, clamped); `tier_color_code`, `dim_flag`, `build_filmstrip_window` (virtualized window + filter); `suggest_tags`, `hud_text`; `ZoomState` (sticky zoom + pan across nav); `default_buckets`, `resolve_buckets`, `reattach`, `dest_is_source_root`; `gather_apply_inputs` + `build_preview` (temp-dir assembly), `find_crashed_apply` + `journal_report` (temp-dir). `to_slint_image` dimensions are asserted without a window.

**Manual-checklist (visual) parts**: window bring-up; loupe fit/zoom/pan rendering; color-coded virtualized filmstrip; HUD + tag-entry autocomplete widgets; latest-wins scrubbing with no backlog/rubber-band; embedded-thumbnail first paint; full-res dedicated slot not spiking the LRU; the Apply dialog (dest guard, preview, confirm/apply/resume) end-to-end; launch-time crash-journal surfacing.

**Consumes from `culler-core` (Phases 1–5):** `Session`, `Shot`, `Decision`, `Tier`, `CaptureTime`, `TierCounts`, `TierCountsPlan`, `ApplyPlan`, `Journal`, `OpState`, `ApplyReport`, `DecodedImage`, `TargetSize`, `RealFs`, `FsOps`; functions `scan`, `load_or_fresh`, `save`, `plan`, `apply`, `resume`, `decode`, `embedded_thumbnail`; `Session` methods `decision`, `set_tier`, `set_tags`, `add_tag`, `mark_visited`, `undo`, `counts`, `visited_count`, `next_unvisited`, `all_tags`; constants `BUCKET_*`, `SESSION_FILE`, `JOURNAL_FILE`.

**Phase 6 completes FastCull v1**: a fast, keyboard-driven culler that classifies shots into tiers with single keypresses, keeps decisions resumable in memory + sidecar, and on Apply safely reorganizes everything into destination buckets — journaled, crash-recoverable, and deletion-free.
