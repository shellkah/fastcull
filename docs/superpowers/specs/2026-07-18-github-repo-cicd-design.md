# GitHub Publication + CI/CD — Design Spec

**Date:** 2026-07-18
**Status:** Approved (brainstorm) — ready to execute
**Author:** Yoann (with Claude)

---

## 1. Goal

Publish FastCull to GitHub and give it a proper home: an accurate description, a
good README, and GitHub Actions that **compile**, **test**, and **release the
binary** on tagged versions.

## 2. Decisions (confirmed with the user)

| Decision | Choice |
|---|---|
| Repo | `shellkah/fastcull` — **public** |
| License | **MIT** (`LICENSE`, author Yoann / shellkah, 2026) |
| Release targets | **Linux `x86_64` + `aarch64`** |
| Default branch | keep **`master`** (matches the local trunk + `phase-*` branches) |
| Branches pushed | **`master` only**; local `phase-*` / `feat/*` stay local (merged history) |
| First release | cut **`v0.1.0`** (matches `Cargo.toml`) to exercise the release path |

**Judgment calls (approved):** CI tests on **both** arches (free + parallel ARM
runners); `v0.1.0` is tagged at the end to validate the release pipeline
end-to-end.

## 3. Context that shapes the design

- Cargo **workspace**: `culler-core` (lib) + `culler` (the Slint GUI binary).
  Edition **2024** → Rust **1.85+**.
- **System dependency**: `culler-core` links the machine's **libjpeg-turbo** via
  `pkg-config` (module `libturbojpeg`, 2.1.5). Ubuntu 24.04 runners ship the
  exact same 2.1.5, so the deliberate turbojpeg-sys 0.2.x pin holds on CI.
- The release binary **statically links Skia**; at runtime it only needs
  `libturbojpeg.so.0` + `libfontconfig.so.1` (verified via `ldd`). That is the
  whole runtime-dependency story to document for release users.
- Native ARM64 build (not cross-compile): public repos get free
  `ubuntu-24.04-arm` runners, so both arches build against apt-installed
  libjpeg-turbo — sidestepping a cross sysroot for the pkg-config probe.
- Pre-flight state: `cargo test` green (292 tests), `cargo clippy` clean (0
  warnings → safe to enforce `-D warnings`), `cargo fmt --check` fails on ~5
  files (trivial rustfmt 1.9.0 line-wrap drift) → fixed by one `style:` commit.

## 4. Deliverables

### 4.1 `README.md`
Tagline + hero screenshot (`docs/design/screens/1b-main.png`); CI / release /
license badges; the "culling before editing" pitch (the Photo Mechanic /
FastRawViewer niche); features (5-tier classify, tags, RAW+JPEG pairing,
non-destructive resumable Apply, crash recovery, histogram HUD, Fuji RAF
embedded preview); a few more screenshots; **Install** (download a release **or**
build from source, each with the system-dep list + runtime note); keybinding /
usage summary; an explicit **Linux-only by design** note; MIT license.

### 4.2 `LICENSE`
MIT, `Copyright (c) 2026 Yoann (shellkah)`.

### 4.3 `.github/workflows/ci.yml` — on push to `master` + PRs
Single matrix job over `{x86_64: ubuntu-24.04, aarch64: ubuntu-24.04-arm}`:
install system deps, `dtolnay/rust-toolchain@stable` (+ rustfmt, clippy),
`Swatinem/rust-cache` (keyed per arch), then `cargo build --workspace
--all-targets --locked` + `cargo test --workspace --locked` on both; `cargo fmt
--all -- --check` + `cargo clippy --workspace --all-targets --locked -- -D
warnings` **on x86_64 only**. `concurrency` cancels superseded runs.

### 4.4 `.github/workflows/release.yml` — on tag `v*.*.*`
- **build** matrix (both arches): install deps, build `cargo build --release
  --locked -p culler`, `strip`, stage `culler` + `README` + `LICENSE` into
  `culler-<tag>-<arch>-linux/`, `tar czf` it, emit a `.sha256`, upload as a
  per-arch artifact.
- **release** job (`needs: build`): download both artifacts, publish a **GitHub
  Release** via `softprops/action-gh-release@v2` with `generate_release_notes`
  + a body documenting the `libturbojpeg` / `libfontconfig` runtime requirement.
  `permissions: contents: write`.

### 4.5 System-dependency line (both workflows)
```
pkg-config libturbojpeg0-dev libfontconfig1-dev clang libclang-dev \
libxkbcommon-dev libwayland-dev libx11-dev libxcb1-dev
```
Starting superset (turbojpeg probe, fontconfig for Skia text, libclang for
skia-safe's bindgen, winit's X11/Wayland/xkb build headers). **Finalized by the
first real CI run** — the dep list is the most likely thing to need a fix pass.

## 5. Rollout (execution order)

1. Local pre-flight (done): confirm build/test/clippy/fmt; pin dep names.
2. Write files on `chore/github-setup`: this spec → `style:` rustfmt →
   README + LICENSE → the two workflows. Commit in logical chunks.
3. **Pre-publish scan** of working tree + full history for secrets / sensitive
   data (the repo is going public).
4. Create `shellkah/fastcull` (public) via the GitHub MCP; set description.
5. Fast-forward `master` to the branch; add remote; push `master`.
6. Watch the first CI run; **iterate the apt dep list until green** (budget a
   few rounds — this is the real work).
7. Set repo topics; then tag & push **`v0.1.0`**; watch `release.yml`; verify
   the Release has both `.tar.gz` + `.sha256` assets.
8. Report back with links.

## 6. Done criteria

- `shellkah/fastcull` public, described, topic-tagged, README rendering with
  screenshots + green badges.
- CI green on `master` for **both** arches.
- Release `v0.1.0` published with `culler-v0.1.0-x86_64-linux.tar.gz` +
  `culler-v0.1.0-aarch64-linux.tar.gz` (+ checksums), each unpacking to a
  runnable `culler`.

## 7. Risks / mitigations

- **CI dep gaps** (most likely): Skia/winit build may want a lib not in the
  starting set → read the failing log, add the package, re-push. Iterated in
  step 6.
- **aarch64 Skia**: if skia-safe has no prebuilt aarch64 binary it builds from
  source (slow first run; clang + python3 present). Cached afterwards.
- **`ubuntu-24.04-arm` availability**: free for public repos; if the label ever
  fails to schedule, fall back to `cross`/`ubuntu-22.04-arm`.
- **Irreversible actions** (public repo, release tag): gated behind the
  pre-publish scan (step 3) and a green CI (step 6) before tagging.
