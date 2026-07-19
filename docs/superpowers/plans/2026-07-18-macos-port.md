# FastCull macOS Port Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship FastCull as a self-contained, universal (`x86_64` + `arm64`) `FastCull.app` inside a `.dmg` on macOS, alongside the unchanged Linux release, built and tested entirely on GitHub Actions.

**Architecture:** The app is already ~portable — `rustix`, Slint (winit/Skia), and the folder-relative session/journal files all work on macOS. The port is: one target-specific dependency split (`rfd`), macOS CI/release jobs, hand-rolled `.app`/`.dmg` packaging scripts (bundling `libturbojpeg` so no Homebrew is needed at runtime), and docs. **Zero `#[cfg(target_os)]` Rust code.**

**Tech Stack:** Rust (edition 2024), Slint 1.17 (backend-winit + renderer-skia), `turbojpeg` 0.5.x (system-linked via pkg-config), GitHub Actions (`macos-13` Intel + `macos-14` Apple Silicon), built-in macOS tooling (`lipo`, `install_name_tool`, `codesign`, `hdiutil`, `sips`, `iconutil`), Homebrew `jpeg-turbo` + `librsvg` (build-time only).

Spec: `docs/superpowers/specs/2026-07-18-macos-port-design.md`.

## Global Constraints

Every task's requirements implicitly include these:

- Package/crate stays **`culler`**; binary stays **`fastcull`** (`-p culler` selects the package; artifact is `target/release/fastcull`).
- **`turbojpeg` stays pinned at `0.5` with `default-features = false, features = ["pkg-config"]`** on both platforms. Do NOT change its version — libjpeg-turbo 3.x (Homebrew) keeps the legacy `tj*` API it binds.
- **Linux is untouched in behavior**: its CI/release jobs, `apt` lists, artifact names (`fastcull-vX.Y.Z-{x86_64,aarch64}-linux.tar.gz`), and release notes text stay working. Additive only.
- **No local macOS exists** — the maintainer and agent are on Linux. Local verification covers Rust (Linux build stays green) + static checks (YAML/bash lint). **macOS build/test/packaging is verified only in GitHub Actions.** Tasks that need CI say so explicitly and require a push (get the user's go-ahead — the repo's rule is commit/push only when asked; branch off `master` first).
- macOS deployment floor **11.0** (`MACOSX_DEPLOYMENT_TARGET=11.0`, `LSMinimumSystemVersion=11.0`).
- Bundle identifier **`com.shellkah.fastcull`**. Runners: **`macos-13`** = Intel/`x86_64`, **`macos-14`** = Apple Silicon/`arm64`.
- Ad-hoc signing only (`codesign -s -`); no notarization, no Apple Developer secrets.
- The inner loop stays: `cargo build --workspace` · `cargo test --workspace` · `cargo fmt --all -- --check` · `cargo clippy --workspace --all-targets -- -D warnings`.

## File Structure

**Created:**
- `culler/macos/Info.plist.in` — `.app` metadata template (`@VERSION@` placeholder).
- `culler/macos/make-icns.sh` — `logo-3a.svg` → `AppIcon.icns` (macOS-side).
- `culler/macos/bundle-app.sh` — assemble + relocate-dylib + ad-hoc-sign `FastCull.app`.
- `culler/macos/make-dmg.sh` — `.app` → compressed `.dmg` + `.sha256`.

**Modified:**
- `culler/Cargo.toml` — split `rfd` into per-target stanzas (Task 1).
- `Cargo.lock` — regenerated to include macOS `rfd` backend crates (Task 1).
- `.github/workflows/ci.yml` — add macOS matrix rows (Task 2).
- `.github/workflows/release.yml` — `workflow_dispatch` + macOS build/bundle/publish jobs (Task 4).
- `README.md`, `CLAUDE.md` — macOS docs (Task 5).
- (optional) `culler-core/src/fsops.rs`, `culler-core/src/decode.rs`, `culler-core/tests/turbojpeg_probe.rs` — doc-comment accuracy (Task 6).

## Task dependency order

`Task 1` (rfd split, unblocks any macOS compile) → `Task 2` (CI: first macOS build/test proof) → `Task 3` (packaging scripts) → `Task 4` (release jobs; dry-run validates Task 3) → `Task 5` (docs) → `Task 6` (optional comments). Tasks 3 and 5 have no hard dependency on 2 and may be done in parallel by separate subagents once Task 1 lands.

---

### Task 1: Split `rfd` dependency by target + regenerate lockfile

**Files:**
- Modify: `culler/Cargo.toml` (the `[dependencies]` block, lines 10–16)
- Modify: `Cargo.lock` (regenerated)

**Interfaces:**
- Consumes: nothing.
- Produces: a workspace that resolves `rfd`'s native AppKit backend on macOS and the `xdg-portal` backend on Linux, with a `Cargo.lock` that satisfies `--locked` on **both** platforms. No Rust API change — `rfd::FileDialog::new().pick_folder()` (`culler/src/main.rs:842`) still compiles unchanged.

Why: `rfd`'s `xdg-portal` feature pulls in Linux-only D-Bus (`ashpd`) that fails to compile on macOS. macOS uses `rfd`'s AppKit `NSOpenPanel` backend (compiled unconditionally on Apple targets, genuinely synchronous — no async-runtime feature needed).

- [ ] **Step 1: Edit `culler/Cargo.toml`**

Replace the single `rfd` line inside `[dependencies]`. Change this block:

```toml
[dependencies]
culler-core = { path = "../culler-core" }
slint = { version = "1.17", features = ["backend-winit", "renderer-skia"] }
clap = { version = "4", features = ["derive"] }
serde_json = "1"
signal-hook = "0.3"
rfd = { version = "0.15", default-features = false, features = ["xdg-portal", "async-std"] }

[build-dependencies]
```

to:

```toml
[dependencies]
culler-core = { path = "../culler-core" }
slint = { version = "1.17", features = ["backend-winit", "renderer-skia"] }
clap = { version = "4", features = ["derive"] }
serde_json = "1"
signal-hook = "0.3"

# rfd's Linux backend (xdg-desktop-portal) pulls in D-Bus (ashpd) and does NOT
# compile on macOS; macOS uses rfd's native AppKit NSOpenPanel backend, which is
# compiled unconditionally on Apple targets and needs no async-runtime feature.
# The synchronous FileDialog::pick_folder() in main.rs compiles under both.
[target.'cfg(target_os = "linux")'.dependencies]
rfd = { version = "0.15", default-features = false, features = ["xdg-portal", "async-std"] }

[target.'cfg(target_os = "macos")'.dependencies]
rfd = { version = "0.15", default-features = false }

[build-dependencies]
```

- [ ] **Step 2: Regenerate the lockfile with macOS deps**

Run: `cargo build --workspace`
Expected: builds successfully (Linux unchanged); `Cargo.lock` is updated. Cargo's lockfile is cross-platform — this resolution adds `rfd`'s macOS-only backend crates (the `objc2` ecosystem: `objc2`, `objc2-app-kit`, `objc2-foundation`, `block2`, …) so a later `--locked` build on macOS finds them.

- [ ] **Step 3: Prove the lockfile now covers the macOS targets (no Mac needed)**

`cargo tree` resolves platform-cfg deps for an arbitrary target triple from the lockfile without compiling. Run both:

```sh
cargo tree --target aarch64-apple-darwin -p culler -i rfd
cargo tree --target x86_64-apple-darwin  -p culler -i rfd
```
Expected: each prints `rfd v0.15.*` and the crates that depend on it, and does NOT print `Updating`/`Adding` lockfile churn or an error. This confirms `--locked` will succeed on both macOS runners. As a negative control, confirm the Linux-only backend is excluded from the mac graph:

```sh
cargo tree --target aarch64-apple-darwin -p culler | grep -i ashpd || echo "OK: ashpd absent on macOS target"
```
Expected: prints `OK: ashpd absent on macOS target`.

- [ ] **Step 4: Confirm Linux is unaffected — the full gate**

Run each; all must pass exactly as before:
```sh
cargo build --workspace --all-targets --locked
cargo test  --workspace --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
```
Expected: PASS. `cargo tree --target x86_64-unknown-linux-gnu -p culler -i rfd` still shows `rfd` with the `xdg-portal`/`async-std` graph (Linux resolution identical to before the change).

- [ ] **Step 5: Commit**

```sh
git add culler/Cargo.toml Cargo.lock
git commit -m "build(macos): resolve rfd's AppKit backend on macOS

Split rfd into per-target dependency stanzas: Linux keeps the xdg-portal
backend (async-std), macOS gets the native AppKit NSOpenPanel backend
(default-features off). The xdg-portal feature pulls in Linux-only D-Bus
and cannot compile on macOS. No Rust code changes; Cargo.lock regenerated
to carry the macOS backend crates so --locked builds pass on both."
```

---

### Task 2: Add macOS build + test to CI

**Files:**
- Modify: `.github/workflows/ci.yml` (whole `check` job)

**Interfaces:**
- Consumes: Task 1's cross-platform `Cargo.lock`.
- Produces: CI that builds + tests all four (os, arch) combos on every push/PR — the project's early-warning system for macOS breakage. `fmt`/`clippy` remain a single x86_64-linux gate.

- [ ] **Step 1: Rewrite `.github/workflows/ci.yml`**

Replace the file's contents with:

```yaml
name: CI

on:
  push:
    branches: [master]
  pull_request:

# Cancel superseded runs on the same ref.
concurrency:
  group: ${{ github.workflow }}-${{ github.ref }}
  cancel-in-progress: true

env:
  CARGO_TERM_COLOR: always

jobs:
  check:
    name: build · test (${{ matrix.os }} ${{ matrix.arch }})
    strategy:
      fail-fast: false
      matrix:
        include:
          - { os: linux, arch: x86_64,  runner: ubuntu-24.04 }
          - { os: linux, arch: aarch64, runner: ubuntu-24.04-arm }
          - { os: macos, arch: x86_64,  runner: macos-13 }
          - { os: macos, arch: aarch64, runner: macos-14 }
    runs-on: ${{ matrix.runner }}
    steps:
      - uses: actions/checkout@v4

      - name: Install system dependencies (Linux)
        if: matrix.os == 'linux'
        run: |
          sudo apt-get update
          sudo apt-get install -y --no-install-recommends \
            pkg-config libturbojpeg0-dev libfontconfig1-dev \
            clang libclang-dev \
            libxkbcommon-dev libwayland-dev libx11-dev libxcb1-dev

      - name: Install system dependencies (macOS)
        if: matrix.os == 'macos'
        run: |
          HOMEBREW_NO_AUTO_UPDATE=1 brew install jpeg-turbo
          echo "PKG_CONFIG_PATH=$(brew --prefix jpeg-turbo)/lib/pkgconfig" >> "$GITHUB_ENV"
          echo "MACOSX_DEPLOYMENT_TARGET=11.0" >> "$GITHUB_ENV"

      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy

      - uses: Swatinem/rust-cache@v2
        with:
          key: ${{ matrix.os }}-${{ matrix.arch }}

      - name: Formatting
        if: matrix.os == 'linux' && matrix.arch == 'x86_64'
        run: cargo fmt --all -- --check

      - name: Clippy
        if: matrix.os == 'linux' && matrix.arch == 'x86_64'
        run: cargo clippy --workspace --all-targets --locked -- -D warnings

      - name: Build
        run: cargo build --workspace --all-targets --locked

      - name: Test
        run: cargo test --workspace --locked
```

Key changes from the current file: added an `os` key to every matrix row (so the two new macOS rows are distinguishable); the `Install system dependencies` step is split by OS; the `fmt`/`clippy` guards became `matrix.os == 'linux' && matrix.arch == 'x86_64'` (previously `matrix.arch == 'x86_64'`, which would now also match `macos-13`); the cache key gained the OS.

- [ ] **Step 2: Validate the YAML locally**

Run: `python3 -c "import yaml; yaml.safe_load(open('.github/workflows/ci.yml')); print('ci.yml OK')"`
Expected: `ci.yml OK` (no exception). If `actionlint` is installed, also run `actionlint .github/workflows/ci.yml` and expect no output.

- [ ] **Step 3: Commit**

```sh
git add .github/workflows/ci.yml
git commit -m "ci: build and test on macOS (Intel + Apple Silicon)

Add macos-13 (x86_64) and macos-14 (aarch64) rows to the check matrix with a
Homebrew jpeg-turbo + PKG_CONFIG_PATH setup step. fmt/clippy stay a single
x86_64-linux gate. Since macOS can't be built locally, CI is the macOS
early-warning system."
```

- [ ] **Step 4: Verify on GitHub Actions (requires push — get user go-ahead)**

This is the first real proof the port compiles + unit-tests on macOS; it cannot be checked locally. With the user's approval, push the working branch and watch the run:
```sh
git push -u origin <branch>
gh run watch --exit-status   # or: gh run list --workflow=ci.yml
```
Expected: all four `check` jobs are green. Specifically the two `macos` jobs build the workspace `--locked` (proving Task 1's lockfile covers macOS) and pass `cargo test` (including `culler-core/tests/turbojpeg_probe.rs`, which proves Homebrew's libjpeg-turbo links via pkg-config). If a macOS job fails on `brew install jpeg-turbo` (already-installed link conflict), change that line to `HOMEBREW_NO_AUTO_UPDATE=1 brew install jpeg-turbo || HOMEBREW_NO_AUTO_UPDATE=1 brew link --overwrite jpeg-turbo`.

---

### Task 3: macOS bundle assets + packaging scripts

**Files:**
- Create: `culler/macos/Info.plist.in`
- Create: `culler/macos/make-icns.sh`
- Create: `culler/macos/bundle-app.sh`
- Create: `culler/macos/make-dmg.sh`

**Interfaces:**
- Consumes: nothing (self-contained shell/plist).
- Produces (consumed by Task 4's release workflow — signatures are load-bearing):
  - `make-icns.sh <input.svg> <output.icns>`
  - `bundle-app.sh <universal-fastcull> <universal-libturbojpeg.dylib> <AppIcon.icns> <VERSION> <out-dir>` → writes `<out-dir>/FastCull.app`
  - `make-dmg.sh <FastCull.app> <volume-name> <output.dmg>` → writes the `.dmg` + a sibling `.dmg.sha256`

These run only on macOS runners. Local verification is `bash -n` (syntax) + `shellcheck`; real execution is Task 4's dry-run.

- [ ] **Step 1: Create `culler/macos/Info.plist.in`**

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleExecutable</key>
    <string>fastcull</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>CFBundleIdentifier</key>
    <string>com.shellkah.fastcull</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>FastCull</string>
    <key>CFBundleDisplayName</key>
    <string>FastCull</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>@VERSION@</string>
    <key>CFBundleVersion</key>
    <string>@VERSION@</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.photography</string>
    <key>LSMinimumSystemVersion</key>
    <string>11.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSHumanReadableCopyright</key>
    <string>© 2026 Yoann (shellkah). MIT License.</string>
</dict>
</plist>
```

- [ ] **Step 2: Create `culler/macos/make-icns.sh`**

```bash
#!/usr/bin/env bash
# Render an SVG into a macOS .icns app icon. macOS-only: needs librsvg
# (rsvg-convert), plus the built-in sips + iconutil.
set -euo pipefail

SVG="${1:?usage: make-icns.sh <input.svg> <output.icns>}"
OUT="${2:?usage: make-icns.sh <input.svg> <output.icns>}"

command -v rsvg-convert >/dev/null 2>&1 || HOMEBREW_NO_AUTO_UPDATE=1 brew install librsvg

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
iconset="$work/AppIcon.iconset"
mkdir -p "$iconset"

# One high-res master raster from the SVG.
rsvg-convert -w 1024 -h 1024 "$SVG" -o "$work/master.png"

# The full standard iconset (1x + 2x). "<px>:<icon_name>".
for spec in \
  "16:icon_16x16.png"      "32:icon_16x16@2x.png" \
  "32:icon_32x32.png"      "64:icon_32x32@2x.png" \
  "128:icon_128x128.png"   "256:icon_128x128@2x.png" \
  "256:icon_256x256.png"   "512:icon_256x256@2x.png" \
  "512:icon_512x512.png"   "1024:icon_512x512@2x.png"; do
  px="${spec%%:*}"
  name="${spec##*:}"
  sips -z "$px" "$px" "$work/master.png" --out "$iconset/$name" >/dev/null
done

iconutil -c icns "$iconset" -o "$OUT"
echo "wrote $OUT"
```

- [ ] **Step 3: Create `culler/macos/bundle-app.sh`**

```bash
#!/usr/bin/env bash
# Assemble a self-contained FastCull.app from a (universal) fastcull binary and a
# (universal) libturbojpeg dylib, relocate the dylib into the bundle, and ad-hoc
# sign. macOS-only: uses otool / install_name_tool / codesign (built-in).
set -euo pipefail

BIN="${1:?usage: bundle-app.sh <fastcull> <libturbojpeg.dylib> <AppIcon.icns> <VERSION> <out-dir>}"
DYLIB="${2:?}"
ICNS="${3:?}"
VERSION="${4:?}"
OUTDIR="${5:?}"

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
base="$(basename "$DYLIB")"
APP="$OUTDIR/FastCull.app"

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Frameworks" "$APP/Contents/Resources"

install -m 0755 "$BIN" "$APP/Contents/MacOS/fastcull"
install -m 0644 "$ICNS" "$APP/Contents/Resources/AppIcon.icns"
install -m 0644 "$DYLIB" "$APP/Contents/Frameworks/$base"
chmod u+w "$APP/Contents/Frameworks/$base"
sed "s/@VERSION@/$VERSION/g" "$HERE/Info.plist.in" > "$APP/Contents/Info.plist"

# Give the bundled dylib an executable-relative id, then repoint the binary at
# it in EVERY arch slice. The x86_64 slice (built on the Intel runner) links a
# /usr/local/... path; the arm64 slice links /opt/homebrew/... — different
# strings, so run -change once per arch (a no-op where the ref is absent).
install_name_tool -id "@executable_path/../Frameworks/$base" "$APP/Contents/Frameworks/$base"
for arch in x86_64 arm64; do
  ref="$(otool -arch "$arch" -L "$APP/Contents/MacOS/fastcull" 2>/dev/null | awk '/turbojpeg/ {print $1; exit}')"
  if [ -n "$ref" ]; then
    install_name_tool -change "$ref" "@executable_path/../Frameworks/$base" "$APP/Contents/MacOS/fastcull"
  fi
done

# Fail loudly if any slice still points at an absolute (Homebrew) turbojpeg path.
for arch in x86_64 arm64; do
  if otool -arch "$arch" -L "$APP/Contents/MacOS/fastcull" | awk '/turbojpeg/ {print $1}' | grep -q '^/'; then
    echo "error: $arch slice still has an absolute libturbojpeg reference" >&2
    otool -arch "$arch" -L "$APP/Contents/MacOS/fastcull" >&2
    exit 1
  fi
done

# Ad-hoc sign: nested dylib first, then the whole bundle (order is required).
codesign --force -s - "$APP/Contents/Frameworks/$base"
codesign --force -s - "$APP"
codesign --verify --deep --strict "$APP"
echo "assembled $APP"
```

- [ ] **Step 4: Create `culler/macos/make-dmg.sh`**

```bash
#!/usr/bin/env bash
# Package a FastCull.app into a compressed .dmg (app + /Applications symlink) and
# emit a .sha256 sidecar. macOS-only: uses the built-in hdiutil + shasum.
set -euo pipefail

APP="${1:?usage: make-dmg.sh <FastCull.app> <volume-name> <output.dmg>}"
VOLNAME="${2:?}"
DMG="${3:?}"

staging="$(mktemp -d)"
trap 'rm -rf "$staging"' EXIT
cp -R "$APP" "$staging/"
ln -s /Applications "$staging/Applications"

rm -f "$DMG"
hdiutil create -volname "$VOLNAME" -srcfolder "$staging" -ov -format UDZO "$DMG"

outdir="$(cd "$(dirname "$DMG")" && pwd)"
b="$(basename "$DMG")"
( cd "$outdir" && shasum -a 256 "$b" > "$b.sha256" )
echo "wrote $DMG"
```

- [ ] **Step 5: Make the scripts executable + syntax-check locally**

```sh
chmod +x culler/macos/make-icns.sh culler/macos/bundle-app.sh culler/macos/make-dmg.sh
for f in culler/macos/*.sh; do bash -n "$f" && echo "syntax OK: $f"; done
python3 -c "import xml.dom.minidom as m; m.parse('culler/macos/Info.plist.in'); print('Info.plist.in is well-formed XML')"
```
Expected: `syntax OK:` for all three scripts and `Info.plist.in is well-formed XML`. If `shellcheck` is installed, also run `shellcheck culler/macos/*.sh` and resolve any error-level findings (info/style may remain).

- [ ] **Step 6: Commit**

```sh
git add culler/macos/
git commit -m "build(macos): add .app/.dmg packaging scripts

Info.plist.in + three macOS-side scripts: make-icns.sh (SVG -> .icns via
librsvg/sips/iconutil), bundle-app.sh (assemble FastCull.app, relocate the
bundled libturbojpeg into Contents/Frameworks per-arch, ad-hoc sign), and
make-dmg.sh (hdiutil .dmg + .sha256). Built-in tooling only; no cargo-bundle."
```

---

### Task 4: macOS build + bundle + publish jobs in the release workflow

**Files:**
- Modify: `.github/workflows/release.yml` (add trigger + three job groups)

**Interfaces:**
- Consumes: Task 3's scripts (exact signatures above), Task 1's lockfile.
- Produces: on a `v*` tag, a universal `fastcull-vX.Y.Z-macos.dmg` (+ `.sha256`) attached to the same GitHub Release as the Linux tarballs; on `workflow_dispatch`, the same `.dmg` as a downloadable Actions artifact **without publishing** (the pre-tag validation path).

- [ ] **Step 1: Rewrite `.github/workflows/release.yml`**

Replace the file's contents with (the Linux `build` job is unchanged from today; everything else is new):

```yaml
name: Release

on:
  push:
    tags: ['v*.*.*']
  workflow_dispatch: {}

permissions:
  contents: write

env:
  CARGO_TERM_COLOR: always

jobs:
  build:
    name: build linux (${{ matrix.arch }})
    strategy:
      fail-fast: false
      matrix:
        include:
          - { arch: x86_64, runner: ubuntu-24.04 }
          - { arch: aarch64, runner: ubuntu-24.04-arm }
    runs-on: ${{ matrix.runner }}
    steps:
      - uses: actions/checkout@v4

      - name: Install system dependencies
        run: |
          sudo apt-get update
          sudo apt-get install -y --no-install-recommends \
            pkg-config libturbojpeg0-dev libfontconfig1-dev \
            clang libclang-dev \
            libxkbcommon-dev libwayland-dev libx11-dev libxcb1-dev

      - uses: dtolnay/rust-toolchain@stable

      - uses: Swatinem/rust-cache@v2
        with:
          key: release-${{ matrix.arch }}

      - name: Build release binary
        run: cargo build --release --locked -p culler

      - name: Package
        run: |
          STAGE="fastcull-${GITHUB_REF_NAME}-${{ matrix.arch }}-linux"
          mkdir "$STAGE"
          cp target/release/fastcull "$STAGE/"
          strip "$STAGE/fastcull"
          cp README.md LICENSE "$STAGE/"
          tar czf "${STAGE}.tar.gz" "$STAGE"
          sha256sum "${STAGE}.tar.gz" > "${STAGE}.tar.gz.sha256"

      - uses: actions/upload-artifact@v4
        with:
          name: dist-${{ matrix.arch }}
          path: fastcull-*-linux.tar.gz*
          if-no-files-found: error

  build-macos:
    name: build macos (${{ matrix.arch }})
    strategy:
      fail-fast: false
      matrix:
        include:
          - { arch: x86_64,  runner: macos-13 }
          - { arch: aarch64, runner: macos-14 }
    runs-on: ${{ matrix.runner }}
    steps:
      - uses: actions/checkout@v4

      - name: Install system dependencies
        run: |
          HOMEBREW_NO_AUTO_UPDATE=1 brew install jpeg-turbo
          echo "PKG_CONFIG_PATH=$(brew --prefix jpeg-turbo)/lib/pkgconfig" >> "$GITHUB_ENV"
          echo "MACOSX_DEPLOYMENT_TARGET=11.0" >> "$GITHUB_ENV"

      - uses: dtolnay/rust-toolchain@stable

      - uses: Swatinem/rust-cache@v2
        with:
          key: release-macos-${{ matrix.arch }}

      - name: Build release binary
        run: cargo build --release --locked -p culler

      - name: Stage binary + its libturbojpeg slice
        run: |
          mkdir -p stage
          cp target/release/fastcull "stage/fastcull-${{ matrix.arch }}"
          ref="$(otool -L target/release/fastcull | awk '/turbojpeg/ {print $1; exit}')"
          if [ -z "$ref" ]; then echo "fastcull does not link libturbojpeg" >&2; exit 1; fi
          cp "$ref" "stage/libturbojpeg-${{ matrix.arch }}.dylib"

      - uses: actions/upload-artifact@v4
        with:
          name: macos-${{ matrix.arch }}
          path: stage/*
          if-no-files-found: error

  bundle-macos:
    name: bundle macos universal dmg
    needs: build-macos
    runs-on: macos-14
    steps:
      - uses: actions/checkout@v4

      - uses: actions/download-artifact@v4
        with:
          pattern: macos-*
          path: slices
          merge-multiple: true

      - name: Compute version + tag
        id: ver
        run: |
          if [ "${{ github.event_name }}" = "push" ]; then
            tag="${GITHUB_REF_NAME}"; version="${tag#v}"
          else
            tag="dev"; version="0.0.0"
          fi
          echo "tag=$tag" >> "$GITHUB_OUTPUT"
          echo "version=$version" >> "$GITHUB_OUTPUT"

      - name: Assemble universal .app and .dmg
        run: |
          set -euo pipefail
          mkdir -p dist
          # Basename of the linked dylib (same for both arches, discovered off arm64).
          base="$(otool -arch arm64 -L slices/fastcull-aarch64 | awk '/turbojpeg/ {print $1; exit}' | xargs basename)"
          test -n "$base"
          lipo -create slices/fastcull-x86_64 slices/fastcull-aarch64 -output dist/fastcull
          lipo -create slices/libturbojpeg-x86_64.dylib slices/libturbojpeg-aarch64.dylib -output "dist/$base"
          file dist/fastcull   # expect: Mach-O universal binary with 2 architectures
          bash culler/macos/make-icns.sh culler/ui/logo-3a.svg dist/AppIcon.icns
          bash culler/macos/bundle-app.sh dist/fastcull "dist/$base" dist/AppIcon.icns \
            "${{ steps.ver.outputs.version }}" dist
          bash culler/macos/make-dmg.sh dist/FastCull.app \
            "FastCull ${{ steps.ver.outputs.version }}" \
            "dist/fastcull-${{ steps.ver.outputs.tag }}-macos.dmg"

      - uses: actions/upload-artifact@v4
        with:
          name: dist-macos
          path: |
            dist/fastcull-*-macos.dmg
            dist/fastcull-*-macos.dmg.sha256
          if-no-files-found: error

  release:
    name: publish release
    needs: [build, bundle-macos]
    if: github.event_name == 'push'
    runs-on: ubuntu-24.04
    steps:
      - uses: actions/download-artifact@v4
        with:
          pattern: dist-*
          path: dist
          merge-multiple: true

      - name: Publish GitHub Release
        uses: softprops/action-gh-release@v2
        with:
          files: dist/*
          generate_release_notes: true
          fail_on_unmatched_files: true
          body: |
            ## FastCull ${{ github.ref_name }}

            Native builds for **Linux** (`x86_64`, `aarch64`) and **macOS**
            (universal — Intel + Apple Silicon).

            ### Linux
            ```sh
            tar xzf fastcull-${{ github.ref_name }}-x86_64-linux.tar.gz   # or -aarch64-linux
            ./fastcull-${{ github.ref_name }}-x86_64-linux/fastcull --help
            ```
            The binary statically links Skia and needs only **libjpeg-turbo** and
            **fontconfig** at runtime:
            ```sh
            sudo apt install libturbojpeg0 libfontconfig1
            ```

            ### macOS
            Download `fastcull-${{ github.ref_name }}-macos.dmg`, open it, and drag
            **FastCull** to Applications. It is a self-contained universal binary —
            no Homebrew needed. On first launch Gatekeeper blocks unsigned apps:
            right-click the app and choose **Open**, or run:
            ```sh
            xattr -dr com.apple.quarantine /Applications/FastCull.app
            ```

            ### Verify a download
            ```sh
            sha256sum -c fastcull-${{ github.ref_name }}-x86_64-linux.tar.gz.sha256   # Linux
            shasum -a 256 -c fastcull-${{ github.ref_name }}-macos.dmg.sha256          # macOS
            ```
```

Notes: the publish job downloads only `dist-*` artifacts (`dist-x86_64`, `dist-aarch64`, `dist-macos`) — NOT the intermediate `macos-*` slices — so raw per-arch binaries never get published. The `release` job is guarded to `push` only, so `workflow_dispatch` runs everything except publishing.

- [ ] **Step 2: Validate the YAML locally**

```sh
python3 -c "import yaml; yaml.safe_load(open('.github/workflows/release.yml')); print('release.yml OK')"
```
Expected: `release.yml OK`. If `actionlint` is installed, run it too and expect no errors.

- [ ] **Step 3: Commit**

```sh
git add .github/workflows/release.yml
git commit -m "ci: release a universal macOS .dmg alongside the Linux tarballs

Add workflow_dispatch plus build-macos (per-arch native builds staging the
fastcull binary + its libturbojpeg slice), bundle-macos (lipo into a universal
binary + dylib, build the icon, assemble/sign FastCull.app, hdiutil the .dmg),
and extend the publish job to attach the .dmg (push/tag only). workflow_dispatch
runs the whole pipeline without publishing, for pre-tag dry-runs."
```

- [ ] **Step 4: Dry-run the whole pipeline on GitHub (requires push — get user go-ahead)**

The packaging scripts run for the first time here; this is their only real test before a tag. With the user's approval, from the pushed branch trigger the workflow manually:
```sh
gh workflow run release.yml --ref <branch>
gh run watch --exit-status
```
Expected: `build`, `build-macos` (×2), and `bundle-macos` are green; `release` is **skipped** (dispatch, not a tag). Download and inspect the artifact:
```sh
gh run download --name dist-macos --dir /tmp/dmgcheck
ls /tmp/dmgcheck    # fastcull-dev-macos.dmg + .sha256
```
Then, on a real Mac (the human acceptance step — see Task-level note): mount the `.dmg`, verify `file FastCull.app/Contents/MacOS/fastcull` reports a universal binary, `otool -L` shows the turbojpeg ref as `@executable_path/../Frameworks/...`, `codesign --verify --deep --strict FastCull.app` passes, and the app launches to the folder-picker after clearing quarantine. If the combine step fails on mismatched dylib basenames, add an assertion comparing the two slices' `otool -D` ids before `lipo`.

---

### Task 5: Documentation — README + CLAUDE.md

**Files:**
- Modify: `README.md`
- Modify: `CLAUDE.md`

**Interfaces:**
- Consumes: the artifact names/behavior settled in Tasks 2 & 4 (`fastcull-vX.Y.Z-macos.dmg`, Gatekeeper workaround).
- Produces: user + contributor docs that present Linux **and** macOS as first-class.

- [ ] **Step 1: README — tagline (line 3)**

Replace:
```markdown
**Fast, keyboard-driven photo culling for Linux.** Point it at a folder of
```
with:
```markdown
**Fast, keyboard-driven photo culling for Linux and macOS.** Point it at a folder of
```

- [ ] **Step 2: README — platform badge (line 11)**

Replace:
```markdown
[![Platform: Linux](https://img.shields.io/badge/platform-Linux-informational)](#platform)
```
with:
```markdown
[![Platform: Linux | macOS](https://img.shields.io/badge/platform-Linux%20%7C%20macOS-informational)](#platform)
```

- [ ] **Step 3: README — add a macOS download subsection**

After the Linux download block (immediately after the closing ```` ``` ```` of the `sudo apt install libturbojpeg0 libfontconfig1` snippet, line 67) and BEFORE `### Build from source`, insert:

```markdown

### macOS

Download `fastcull-vX.Y.Z-macos.dmg` from the
[latest release](https://github.com/shellkah/fastcull/releases/latest), open it,
and drag **FastCull** to Applications. It is a **universal** build (Intel +
Apple Silicon) and fully self-contained — libjpeg-turbo is bundled, so no
Homebrew is required to run it.

FastCull is ad-hoc signed, not notarized, so on first launch Gatekeeper will
refuse to open it. Clear that once:

```sh
xattr -dr com.apple.quarantine /Applications/FastCull.app
```

or right-click the app in Finder and choose **Open** the first time. Verify a
download with its sidecar: `shasum -a 256 -c fastcull-vX.Y.Z-macos.dmg.sha256`.
```

- [ ] **Step 4: README — add macOS build-from-source prerequisites**

In `### Build from source`, replace this block (lines 71–77):
```markdown
Requires **Rust 1.85+** (2024 edition) and these system packages:

```sh
sudo apt install -y pkg-config libturbojpeg0-dev libfontconfig1-dev \
  clang libclang-dev libxkbcommon-dev libwayland-dev libx11-dev libxcb1-dev
cargo build --release          # binary at target/release/fastcull
```
```
with:
```markdown
Requires **Rust 1.85+** (2024 edition).

**Linux** — system packages:

```sh
sudo apt install -y pkg-config libturbojpeg0-dev libfontconfig1-dev \
  clang libclang-dev libxkbcommon-dev libwayland-dev libx11-dev libxcb1-dev
cargo build --release          # binary at target/release/fastcull
```

**macOS** — Xcode Command Line Tools + Homebrew's libjpeg-turbo (no fontconfig;
Skia uses CoreText):

```sh
xcode-select --install         # if not already present
brew install jpeg-turbo
export PKG_CONFIG_PATH="$(brew --prefix jpeg-turbo)/lib/pkgconfig"
cargo build --release          # binary at target/release/fastcull
```
```

- [ ] **Step 5: README — rewrite the Platform section (lines 121–126)**

Replace:
```markdown
## Platform

**Linux-only, by design.** FastCull is a focused personal tool: it renders with
Slint/Skia, uses the XDG desktop portal for folder picking, and links the system
libjpeg-turbo. Cross-platform packaging is an explicit non-goal. Releases are
built natively for `x86_64` and `aarch64`.
```
with:
```markdown
## Platform

**Linux and macOS.** FastCull renders with Slint/Skia and links the system
libjpeg-turbo. On Linux it uses the XDG desktop portal for folder picking and is
released natively for `x86_64` and `aarch64`; on macOS it uses the native AppKit
picker and ships as a self-contained **universal** `.dmg` (Intel + Apple
Silicon). Windows is not supported.
```

- [ ] **Step 6: CLAUDE.md — retarget the intro line**

Replace:
```markdown
FastCull is a keyboard-driven **photo culling** GUI for Linux (Rust + Slint): cull a folder of
```
with:
```markdown
FastCull is a keyboard-driven **photo culling** GUI for Linux and macOS (Rust + Slint): cull a folder of
```

- [ ] **Step 7: CLAUDE.md — add a macOS build-environment block**

After the Linux "Build environment (required)" `sudo apt install …` fenced block and its following `culler-core` links / `clang/libclang` paragraph, insert a new subsection:

```markdown
### macOS build environment

macOS builds link Homebrew's **libjpeg-turbo 3.x** (which keeps the legacy
TurboJPEG `tj*` API that the pinned `turbojpeg` 0.5.x binds — so the crate is
unchanged across platforms). No fontconfig (Skia uses CoreText), no X11/Wayland
packages.

```sh
xcode-select --install                                    # clang/libclang for bindgen
brew install jpeg-turbo
export PKG_CONFIG_PATH="$(brew --prefix jpeg-turbo)/lib/pkgconfig"
export MACOSX_DEPLOYMENT_TARGET=11.0
```

The `rfd` dependency is target-split in `culler/Cargo.toml` (Linux xdg-portal vs
macOS AppKit); there is no `#[cfg(target_os)]` in the Rust source. macOS can't be
built locally on a Linux box — CI (`macos-13` Intel, `macos-14` Apple Silicon) is
the macOS build/test harness.
```

- [ ] **Step 8: CLAUDE.md — update the Releasing section**

Append to the Releasing section (after the existing paragraph about the released Linux binary's runtime deps) a macOS note. Add:

```markdown

macOS releases build natively on `macos-13` (x86_64) + `macos-14` (arm64), then
`lipo`-merge into a **universal** `FastCull.app` (bundling `libturbojpeg` so the
app is self-contained), packaged as `fastcull-vX.Y.Z-macos.dmg` and ad-hoc
signed (not notarized — first launch needs the Gatekeeper right-click/`xattr`
workaround, documented in the README). Use `workflow_dispatch` on
`release.yml` to dry-run the whole `.dmg` pipeline on a branch before tagging.
The packaging scripts live in `culler/macos/`.
```

- [ ] **Step 9: Verify the docs render + links resolve**

```sh
python3 - <<'PY'
import re, pathlib
for f in ("README.md", "CLAUDE.md"):
    t = pathlib.Path(f).read_text()
    assert "macOS" in t, f
    # fenced code blocks balance
    assert t.count("```") % 2 == 0, f"unbalanced fences in {f}"
    print(f, "OK")
PY
```
Expected: `README.md OK` and `CLAUDE.md OK`. Eyeball the rendered Markdown (the new macOS install/build sections and the rewritten Platform section) if a preview is available.

- [ ] **Step 10: Commit**

```sh
git add README.md CLAUDE.md
git commit -m "docs: document macOS support (install, build, releasing)

README gains a macOS .dmg install section, macOS build-from-source prereqs,
an updated platform badge/section, and the Gatekeeper workaround. CLAUDE.md
gains a macOS build-environment block and macOS release notes."
```

---

### Task 6 (optional): doc-comment accuracy for cross-platform

Purely cosmetic — no behavior change. Skip if minimizing churn; included for spec completeness (spec §B).

**Files:**
- Modify: `culler-core/src/fsops.rs`
- Modify: `culler-core/src/decode.rs`
- Modify: `culler-core/tests/turbojpeg_probe.rs`

**Interfaces:** none (comments only).

- [ ] **Step 1: `fsops.rs` — generalize the Linux-only wording**

Replace the `RealFs` doc line:
```rust
/// Production `FsOps` over the real Linux filesystem via rustix.
```
with:
```rust
/// Production `FsOps` over the real filesystem via rustix (Linux + macOS;
/// `renameat_with(NOREPLACE)` maps to `renameat2(RENAME_NOREPLACE)` on Linux and
/// `renameatx_np(RENAME_EXCL)` on macOS).
```
And in `fsync_dir`, replace:
```rust
        // Opening a directory read-only then fsync flushes its entries on Linux.
```
with:
```rust
        // Opening a directory read-only then fsync flushes its entries (Linux;
        // on macOS fsync is not F_FULLFSYNC, a weaker but acceptable guarantee —
        // the journal/reconcile model tolerates an unsynced tail).
```

- [ ] **Step 2: `decode.rs` — note the guard is platform-agnostic**

In the `MAX_DECODE_PIXELS` comment, replace:
```rust
/// `vm.overcommit_memory=2` the allocator's `handle_alloc_error` aborts the
/// whole process (SIGABRT), which is uncatchable and would crash the Phase 6
```
with:
```rust
/// `vm.overcommit_memory=2` the allocator's `handle_alloc_error` aborts the
/// whole process (SIGABRT) — uncatchable, and macOS aborts a huge allocation
/// too; the guard is platform-agnostic and would crash the Phase 6
```

- [ ] **Step 3: `turbojpeg_probe.rs` — generalize the linking rationale**

Replace the header comment:
```rust
//! Linking probe: proves the `turbojpeg` crate links the SYSTEM libjpeg-turbo
//! (pkg-config; this machine has no cmake/nasm so a from-source build cannot
//! have produced this binary). Kept as a permanent guard for the dep spelling.
```
with:
```rust
//! Linking probe: proves the `turbojpeg` crate links the SYSTEM libjpeg-turbo via
//! pkg-config (Linux 2.1.x, or Homebrew 3.x on macOS — both export the legacy
//! `tj*` API this crate binds). Kept as a permanent guard for the dep spelling
//! and the pkg-config wiring on every platform.
```

- [ ] **Step 4: Verify + commit**

```sh
cargo build --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
git add culler-core/src/fsops.rs culler-core/src/decode.rs culler-core/tests/turbojpeg_probe.rs
git commit -m "docs(core): note macOS parity in fsops/decode/turbojpeg-probe comments"
```
Expected: build + clippy + fmt all pass (comments only).

---

## Self-review

**Spec coverage** (against `2026-07-08`… no — `2026-07-18-macos-port-design.md`):
- §A rfd split → Task 1. §B doc comments → Task 6. §C build env → Tasks 2/4 steps + Task 5. §D CI → Task 2. §E release (workflow_dispatch, build-macos, combine, publish) → Task 4. §F bundle/scripts (Info.plist, icns, bundle, dmg) → Task 3. §G docs → Task 5. Testing strategy (local Linux green, CI macOS, dispatch dry-run) → embedded in Task 1 Step 4, Task 2 Step 4, Task 4 Step 4. All spec sections map to a task.

**Placeholder scan:** No TBD/TODO. `@VERSION@` is an intentional template token (rendered by `bundle-app.sh`), not a gap. The one unavoidable non-local check — a human launching the `.dmg` on a real Mac — is called out explicitly in Task 4 Step 4, not hidden.

**Type/interface consistency:** Script signatures declared in Task 3's Interfaces exactly match the invocations in Task 4's `bundle-macos` job: `make-icns.sh <svg> <icns>`, `bundle-app.sh <bin> <dylib> <icns> <VERSION> <out-dir>`, `make-dmg.sh <app> <volname> <dmg>`. Artifact names are consistent: per-arch slices `macos-{x86_64,aarch64}` (staged files `fastcull-<arch>`, `libturbojpeg-<arch>.dylib`); release artifacts `dist-{x86_64,aarch64,macos}`; publish downloads `pattern: dist-*` (excludes the `macos-*` slices). otool/lipo arch flags use `arm64`/`x86_64`; filenames use `aarch64`/`x86_64` — matched at each call site. CI matrix `os`/`arch` keys line up with the `fmt`/`clippy` `if:` guards.

**Execution note:** Tasks 1, 3, 5, 6 are fully verifiable on the Linux dev box. Tasks 2 and 4 have a local static-check step plus a GitHub-only verification step that needs a push — do those with the user's go-ahead, on a branch off `master`.
