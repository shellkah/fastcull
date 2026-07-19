# FastCull macOS Port — Design Spec

**Date:** 2026-07-18
**Status:** Approved (brainstorming complete; ready for implementation plan)
**Author:** brainstorming session (shellkah)

## Goal

Add **macOS** as a second first-class target for FastCull, alongside the existing Linux
build, without changing Linux behavior. Deliver a self-contained, double-clickable
`FastCull.app` distributed as a **universal** (`x86_64` + `arm64`) `.dmg` via GitHub
Releases, built and tested entirely on GitHub Actions macOS runners.

## Context

FastCull is a Rust + Slint photo-culling GUI (workspace: pure-logic `culler-core` +
GUI/CLI `culler`, binary `fastcull`). It currently targets Linux only, with a shipped
v0.1.0 release (per-arch `x86_64`/`aarch64` tarballs) and CI on both Linux arches.

**A codebase audit found the app is almost entirely portable already.** All dangerous
filesystem effects funnel through one trait (`culler-core::fsops::FsOps`) over `rustix`;
the session sidecar (`.fastcull.json`) and crash journal (`.fastcull-apply.json`) live
inside user-chosen folders (no hardcoded system paths, no `XDG_*`, no `dirs`/`directories`
crate); there is **zero** `#[cfg(target_os)]` branching; no production `Command::new`, no
shell-out, no inotify. Slint's `backend-winit` + `renderer-skia` support macOS natively.

Two load-bearing assumptions were verified against primary sources before this spec:

1. **`rustix::fs::renameat_with(RenameFlags::NOREPLACE)` works on macOS**, mapping to
   `renameatx_np(RENAME_EXCL)` on Apple targets. This is the atomic no-clobber move
   primitive under every apply (`fsops.rs:45`) and every sidecar write (`xmp.rs:108`).
   Source: rustix `renameat_with` docs; rust-lang/libs-team #131.
2. **libjpeg-turbo 3.x retains the legacy TurboJPEG `tj*` API** (`tjInitDecompress`,
   `tjDecompress2`, `tjCompress2`, …) as deprecated wrappers over the new `tj3*` API, so
   the pinned `turbojpeg` 0.5.x (which binds the legacy API via `turbojpeg-sys` 0.2.x)
   links against Homebrew's current `jpeg-turbo` **3.2.0**. `turbojpeg-sys` 0.2.x's
   pkg-config probe requires `libturbojpeg >= 2.0`; 3.2.0 satisfies it. **No `turbojpeg`
   version change is needed.** Source: libjpeg-turbo TurboJPEG API docs; Homebrew formula.

### Hard constraint: no local macOS

Neither the maintainer nor the agent has a Mac. **GitHub Actions macOS runners are the
only macOS build/test environment.** The Linux `cargo build/test/fmt/clippy` loop remains
the fast inner loop and MUST stay green throughout. macOS validation happens exclusively in
CI. The release workflow gains a `workflow_dispatch` trigger so the full macOS packaging
pipeline can be dry-run on a branch before a real `v*` tag is pushed.

### Design invariants preserved (from CLAUDE.md)

- **Non-destructive**: no delete/unlink path added; Reject still moves to `00_rejected`.
- **Nothing touches disk until Apply**; crash recovery via journal preserved.
- **Display source is always a JPEG** (RAW embedded preview); unchanged.
- `culler-core` stays UI-free and testable without Slint.

## Non-goals / deliberately deferred

Documented as known gaps, not v1 blockers:

- **Notarization / Developer ID signing.** Chosen approach is **ad-hoc signing**
  (`codesign -s -`) plus a documented Gatekeeper workaround. No Apple Developer account,
  no CI secrets. (A future upgrade path to notarization is noted in Risks.)
- **Native Mac keyboard shortcuts.** `Ctrl+S` (force-save) and `F11` (fullscreen) keep
  their Linux bindings; `⌘S` / green-button fullscreen are later polish. The `Ctrl` key
  still exists on macOS, so `Ctrl+S` works — it's just not the platform idiom.
- **`F_FULLFSYNC`.** macOS `fsync(2)` does not force a drive-cache flush the way Linux
  does; the journal + reconcile model already tolerates an unsynced tail, so the weaker
  guarantee is acceptable for v1. Not switching to `F_FULLFSYNC`.
- **Exotic filesystems** (FAT/exFAT, some network mounts) where `renameatx_np` may return
  `ENOTSUP`: apply stops **loudly** (safe — no data loss, surfaces as `ApplyError::Fs`),
  rather than silently degrading. APFS/HFS+ (normal Mac volumes) support it.
- **Folder drag-onto-app-icon.** Double-click launches the existing no-arg folder-picker
  screen; accepting a dropped folder via `application:openFiles:` is future work.
- **Windows.** Out of scope.

## Global constraints

- Package/crate name stays **`culler`**; binary stays **`fastcull`** (`-p culler` selects
  the package; the artifact is `target/release/fastcull`).
- `turbojpeg` stays pinned to **0.5.x** with `default-features = false, features =
  ["pkg-config"]` — unchanged on both platforms (see Context fact #2). Do not "upgrade" it
  for macOS.
- Linux CI/release jobs, their `apt` dependency lists, and their artifact names/notes are
  unchanged.
- macOS deployment floor: **macOS 11.0** (`MACOSX_DEPLOYMENT_TARGET=11.0`,
  `LSMinimumSystemVersion=11.0`). Bump only if the Slint/Skia toolchain demands it (CI
  reveals this).
- Bundle identifier: **`com.shellkah.fastcull`**.
- Runner pinning: **`macos-13`** = Intel/`x86_64`, **`macos-14`** = Apple Silicon/`arm64`.

## Detailed design

### A. Dependency change — split `rfd` by target (`culler/Cargo.toml`)

The current single dependency uses the Linux-only `xdg-portal` backend (pulls in `ashpd`
/ D-Bus, which does not compile on macOS). Replace the one `rfd` line with two
target-specific stanzas. No Rust `#[cfg]` is required — `rfd::FileDialog::new()
.pick_folder()` (the synchronous picker at `culler/src/main.rs:842`) compiles under both:

```toml
[target.'cfg(target_os = "linux")'.dependencies]
rfd = { version = "0.15", default-features = false, features = ["xdg-portal", "async-std"] }

[target.'cfg(target_os = "macos")'.dependencies]
rfd = { version = "0.15", default-features = false }
```

Rationale: macOS's rfd backend (AppKit `NSOpenPanel`) is compiled unconditionally on
Apple targets and is genuinely synchronous, so it needs neither `xdg-portal` nor an
`async-std`/`tokio` runtime feature. Linux's `xdg-portal` backend is async internally,
which is why Linux keeps `async-std`.

This is the **only** manifest/code change required for the app to build and run on macOS.

### B. Optional doc-comment accuracy edits (low priority, no behavior change)

A few comments assert Linux-specific facts that are now cross-platform. Optional cleanup;
may be folded into the same commit or skipped:

- `culler-core/src/fsops.rs` — `RealFs` "over the real Linux filesystem"; `fsync_dir`
  "on Linux". Reword to note macOS parity via rustix + the `F_FULLFSYNC` caveat.
- `culler-core/src/decode.rs` — the `MAX_DECODE_PIXELS` comment's `vm.overcommit_memory`
  rationale is Linux-specific; note the guard is platform-agnostic.
- `culler-core/tests/turbojpeg_probe.rs` — "this machine has no cmake/nasm" is
  Linux-build-host specific; generalize to "system-linked libjpeg-turbo".

### C. Build environment (macOS runners)

Every macOS build/test step needs, before `cargo`:

```sh
brew install jpeg-turbo
export PKG_CONFIG_PATH="$(brew --prefix jpeg-turbo)/lib/pkgconfig"
export MACOSX_DEPLOYMENT_TARGET=11.0
```

- `brew install jpeg-turbo` provides `libturbojpeg.dylib` + `lib/pkgconfig/libturbojpeg.pc`.
- `PKG_CONFIG_PATH` is set explicitly (belt-and-suspenders across `/usr/local` on Intel and
  `/opt/homebrew` on Apple Silicon), so `turbojpeg-sys`'s pkg-config probe resolves
  deterministically regardless of the runner's default pkg-config search path.
- No fontconfig, no `libxkbcommon`/wayland/x11 packages (macOS uses native windowing +
  CoreText). Xcode Command Line Tools (with `clang`/`libclang` for skia-safe's bindgen)
  are preinstalled on GitHub macOS runners.

The existing `culler-core/tests/turbojpeg_probe.rs` link-probe validates that the system
libjpeg-turbo actually links — this is the automated tripwire if the pkg-config setup is
wrong on macOS.

### D. CI workflow (`.github/workflows/ci.yml`)

Add two macOS entries to the existing `check` matrix (which currently holds
`ubuntu-24.04` x86_64 and `ubuntu-24.04-arm` aarch64):

```
- { os: macos,  arch: x86_64,  runner: macos-13 }
- { os: macos,  arch: aarch64, runner: macos-14 }
```

- The "Install system dependencies" step must branch on OS: `apt-get …` on Linux,
  `brew install jpeg-turbo` + export `PKG_CONFIG_PATH`/`MACOSX_DEPLOYMENT_TARGET` on macOS.
  Cleanest is a per-OS conditional step (`if: runner.os == 'Linux'` /
  `if: runner.os == 'macOS'`).
- **Build + test** run on all four rows. The non-GUI test suite runs headless on macOS
  exactly as it does on Linux today (no test constructs a Slint window; Linux CI already
  runs without xvfb, confirming this). The `applyflow` `0o555` permission tests
  runtime-probe and self-skip if the mode doesn't block writes.
- **fmt + clippy stay x86_64-linux only** (`matrix.os == 'linux' && matrix.arch ==
  'x86_64'`). They are platform-agnostic gates, and the port introduces no
  `#[cfg(target_os)]` Rust code for them to miss. (If any macOS-gated Rust is added later,
  add a clippy-on-macOS row.)
- `Swatinem/rust-cache` key extended to include the OS so the four caches stay distinct.

### E. Release workflow (`.github/workflows/release.yml`)

Keep the existing Linux `build` matrix + `release` publish job. Add:

**Trigger:** add `workflow_dispatch` alongside the `push: tags: ['v*.*.*']` trigger, so the
whole pipeline can be exercised manually. When run via dispatch (no tag), derive a
placeholder version (e.g. `0.0.0-dev` or the short SHA) and **skip** the publish step
(guard the `release` job on `github.event_name == 'push'`).

**New macOS build jobs (two, native per arch):** `macos-13` (x86_64) and `macos-14`
(arm64). Each:
1. Sets up the macOS build env (section C).
2. `cargo build --release --locked -p culler`.
3. Discovers the linked libturbojpeg via `otool -L target/release/fastcull` (grep for
   `turbojpeg`) — do **not** hardcode the dylib basename/version.
4. Uploads two artifacts: the arch's `fastcull` binary and its `libturbojpeg.*.dylib`
   slice (copied out of the Homebrew prefix at the discovered basename).

**New combine/package job** (runs on `macos-14`, needs both build jobs):
1. Downloads both arches' `fastcull` + dylib slices.
2. `lipo -create` the two `fastcull` slices → **universal** `fastcull`; `lipo -create` the
   two dylib slices → **universal** `libturbojpeg.*.dylib`.
3. Runs the packaging scripts (section F) to produce `FastCull.app` and the `.dmg`.
4. Uploads `FastCull-${VERSION}-macos.dmg` + `.sha256` as a release artifact.

**Publish job:** the existing `release` job additionally collects the macOS `.dmg` +
`.sha256` (via the shared `dist/` download) and attaches them to the same GitHub Release.
Its release-notes body gains a macOS install section (see section G).

### F. `.app` bundle + `.dmg` packaging (`culler/macos/`)

Hand-rolled with built-in macOS tools only (no `cargo-bundle`/Tauri dependency), matching
the project's minimalist ethos. New files:

- **`culler/macos/Info.plist.in`** — template with a `@VERSION@` placeholder. Keys:
  `CFBundleExecutable=fastcull`, `CFBundleIdentifier=com.shellkah.fastcull`,
  `CFBundleName`/`CFBundleDisplayName=FastCull`, `CFBundlePackageType=APPL`,
  `CFBundleShortVersionString`/`CFBundleVersion=@VERSION@`, `CFBundleIconFile=AppIcon`,
  `LSMinimumSystemVersion=11.0`, `NSHighResolutionCapable=true`,
  `LSApplicationCategoryType=public.app-category.photography`, `NSHumanReadableCopyright`
  (MIT, matching LICENSE).

- **`culler/macos/make-icns.sh`** — rasterize `culler/ui/logo-3a.svg` → a 1024×1024 PNG
  (`brew install librsvg`; `rsvg-convert`), build the `.iconset` at the required sizes with
  `sips`, then `iconutil -c icns` → `AppIcon.icns`. Runs on the macOS runner (all tools are
  macOS-side). Accepts that the logo may need later art tuning for the squircle grid; a
  straight full-bleed rasterization is acceptable for v1.

- **`culler/macos/bundle-app.sh`** — inputs: a universal `fastcull`, a universal
  `libturbojpeg.*.dylib`, `AppIcon.icns`, and `VERSION`. Steps:
  1. Build the tree: `FastCull.app/Contents/{MacOS,Frameworks,Resources}`.
  2. Copy the universal binary → `Contents/MacOS/fastcull`; the universal dylib →
     `Contents/Frameworks/`; the icns → `Contents/Resources/AppIcon.icns`; render
     `Info.plist.in` (`@VERSION@` → `VERSION`) → `Contents/Info.plist`.
  3. Relocate the dylib reference: `install_name_tool -change <discovered-old-path>
     @executable_path/../Frameworks/<dylib-basename> Contents/MacOS/fastcull` (old path
     discovered via `otool -L`). Optionally `install_name_tool -id
     @rpath/<basename>` on the bundled dylib.
  4. **Ad-hoc sign** the dylib first, then the app bundle:
     `codesign --force -s - Contents/Frameworks/<dylib>` then `codesign --force -s -
     FastCull.app`. (Signing the nested dylib before the outer bundle is required or the
     bundle signature is invalid.)
  5. Verify: `codesign --verify --deep FastCull.app`; `otool -L Contents/MacOS/fastcull`
     shows only the `@executable_path` turbojpeg reference plus system libs.

- **`culler/macos/make-dmg.sh`** — `hdiutil create` a compressed `.dmg` containing
  `FastCull.app` + an `/Applications` symlink (simple drag-install layout). Emit
  `FastCull-${VERSION}-macos.dmg`; compute its `.sha256`.

The combine job wires these together. Because the scripts run for the first time only in
CI, the `workflow_dispatch` dry-run (section E) is the pre-tag validation path.

### G. Documentation

- **`README.md`**:
  - Intro/tagline: "Linux **and macOS**".
  - Build-from-source: a macOS prerequisites block (`xcode-select --install`;
    `brew install jpeg-turbo`; `export PKG_CONFIG_PATH=…`) beside the existing Debian/apt
    block.
  - **Install (macOS)** section: download `FastCull-vX.Y.Z-macos.dmg`, open, drag to
    Applications. First-launch Gatekeeper note: right-click → **Open** once, or
    `xattr -dr com.apple.quarantine /Applications/FastCull.app`. Verify with the `.sha256`.
  - Note the `.dmg` is a universal binary (Intel + Apple Silicon) and self-contained
    (bundles libturbojpeg; no Homebrew needed to run).

- **`CLAUDE.md`**:
  - Retarget the "photo culling GUI for Linux" line to "for Linux and macOS".
  - Add a macOS build-environment block (Homebrew `jpeg-turbo`, `PKG_CONFIG_PATH`,
    deployment target, "no fontconfig on macOS — CoreText").
  - Update **Releasing** to describe the macOS universal `.dmg` output and the
    `workflow_dispatch` dry-run.
  - Keep the existing "package is `culler`, binary is `fastcull`" gotcha.

## File change map

**Created:**
- `docs/superpowers/specs/2026-07-18-macos-port-design.md` (this spec)
- `culler/macos/Info.plist.in`
- `culler/macos/make-icns.sh`
- `culler/macos/bundle-app.sh`
- `culler/macos/make-dmg.sh`

**Modified:**
- `culler/Cargo.toml` — split `rfd` into per-target stanzas (section A)
- `.github/workflows/ci.yml` — macOS matrix rows + per-OS dependency step + cache key
- `.github/workflows/release.yml` — `workflow_dispatch`; macOS build jobs; combine/package
  job; publish job attaches `.dmg`
- `README.md` — macOS build prereqs + install section
- `CLAUDE.md` — macOS build env + releasing
- (optional) `culler-core/src/fsops.rs`, `culler-core/src/decode.rs`,
  `culler-core/tests/turbojpeg_probe.rs` — doc-comment accuracy (section B)

`Cargo.lock` may gain macOS-only transitive deps for `rfd`'s AppKit backend when first
resolved on a macOS runner; that is expected and committed if it changes.

## Testing / validation strategy

- **Local (Linux):** after the `rfd` split, `cargo build/test/fmt/clippy --workspace`
  must stay green — the target-specific stanza means Linux resolves exactly the same `rfd`
  as before. This is the primary regression gate the agent can run.
- **macOS build + test:** exercised by the new CI matrix rows on every push/PR. Green here
  is the definition of "compiles and unit-tests pass on macOS". The `turbojpeg_probe`
  test confirms system libjpeg-turbo linkage.
- **Packaging:** exercised via `release.yml`'s `workflow_dispatch` dry-run on a branch —
  produces the `.dmg` artifact without publishing, so the `lipo`/bundle/`install_name_tool`/
  `codesign`/`hdiutil` chain is validated before a real tag.
- **End-user smoke (manual, post-release):** on a real Mac, mount the `.dmg`, drag to
  Applications, clear quarantine, launch → folder picker appears, cull a test folder,
  Apply. (Cannot be automated without a Mac; documented as the human acceptance step.)

## Risks & mitigations

1. **Homebrew `jpeg-turbo` doesn't ship `libturbojpeg.pc`, or the pkg-config version
   check fails.** Mitigation: the `turbojpeg_probe` test fails fast in CI; `PKG_CONFIG_PATH`
   is set explicitly. Fallback if needed: vendored build (turbojpeg `cmake` feature +
   `brew install cmake nasm`) — not expected to be necessary.
2. **Slint/Skia toolchain requires a macOS deployment target > 11.0.** Mitigation: CI
   surfaces the link/build error; bump `MACOSX_DEPLOYMENT_TARGET` + `LSMinimumSystemVersion`.
3. **Universal `lipo` of the dylib mismatches** (e.g. Homebrew ships a differently-named
   dylib per arch). Mitigation: discover the basename via `otool -L` on each arch; assert
   both basenames match before `lipo`.
4. **Ad-hoc-signed app still blocked by Gatekeeper.** Expected — documented workaround
   (right-click Open / `xattr`). Upgrade path: add Developer ID + `notarytool` later behind
   CI secrets (a self-contained follow-up; the bundle/dmg scripts are reused as-is).
5. **`renameatx_np` unsupported on the user's target volume.** Apply fails loudly and
   safely; documented limitation.

## Handoff

Proceed to the **writing-plans** skill to produce a task-by-task implementation plan at
`docs/superpowers/plans/2026-07-18-macos-port.md`.
