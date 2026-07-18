# FastCull — agent & contributor guide

FastCull is a keyboard-driven **photo culling** GUI for Linux (Rust + Slint): cull a folder of
shots into quality tiers, then **Apply** non-destructively reorganizes them. Spec:
`docs/specs/2026-07-08-fastcull-design.md`. UI contract: `docs/design/DESIGN.md`.

## Workspace

Cargo workspace, two crates:
- **`culler-core`** — pure-logic library: scan, EXIF/XMP, JPEG + RAW decode, plan + apply engine,
  persistence. No UI; keep it that way (it must stay testable without Slint).
- **`culler`** — the app: Slint GUI (`culler/ui/*.slint`, compiled by `culler/build.rs`) + clap CLI.

**Naming gotcha:** the package/crate is **`culler`**, but the binary it builds is **`fastcull`**
(set via `[[bin]] name`). So you select the package with `-p culler`, but the artifact is
`target/release/fastcull` and you launch it with `cargo run -p culler`.

## Build environment (required)

Recent stable Rust (built with 1.97; edition 2024 needs ≥1.85, Slint 1.17 wants a recent
toolchain). System packages (Debian/Ubuntu) — **the build fails without them**:

```sh
sudo apt install -y pkg-config libturbojpeg0-dev libfontconfig1-dev \
  clang libclang-dev libxkbcommon-dev libwayland-dev libx11-dev libxcb1-dev
```

`culler-core` links the **system** libjpeg-turbo through `pkg-config`. `turbojpeg` is pinned to
0.5.x on purpose (system lib is 2.1.5; turbojpeg-sys ≥1.0 requires 3.0) — full rationale is in the
comment in `culler-core/Cargo.toml`, read it before touching that dependency. `clang/libclang` is
for skia-safe's bindgen (Slint's Skia renderer).

## The loop — CI enforces every one of these

```sh
cargo build --workspace
cargo test  --workspace                                  # full suite must pass
cargo fmt --all -- --check                               # gate — run `cargo fmt --all` to fix
cargo clippy --workspace --all-targets -- -D warnings    # gate — a warning fails the build
cargo run -p culler -- <folder-of-shots>                 # launch the GUI
```

CI (`.github/workflows/ci.yml`) runs build + test on **x86_64 and aarch64**, plus fmt + clippy on
x86_64. "Green locally, red in CI" is almost always an unformatted file or a clippy warning — run
both gates before pushing.

**Testing `culler`:** it is a **binary crate with no lib target** — use `cargo test -p culler
<filter>`, never `cargo test -p culler --lib` (fails with `no library targets found`). `culler-core`
tests normally. GUI/Slint rendering can't be unit-tested — smoke-test visual changes by running the
app and driving the surface you touched.

## Design invariants — do not break

- **Non-destructive:** v1 performs **no deletions**. Every Apply is a verified *move*; Reject goes
  to `00_rejected`, never unlinked. Do not add a delete/unlink path in v1.
- **Nothing touches disk until Apply.** Decisions live in a resumable session sidecar, and a crash
  mid-Apply is recoverable from a journal. Preserve this when editing `apply`/`persist`/`plan`.
- **Display source is always a JPEG** (RAW shows its embedded preview; no demosaic).

## Releasing

1. Bump `version` in `culler/Cargo.toml` to match the tag.
2. Tag `vX.Y.Z` on `master` and push it → `.github/workflows/release.yml` builds both arches and
   publishes a GitHub Release with `fastcull-vX.Y.Z-{x86_64,aarch64}-linux.tar.gz` + `.sha256`.

The released binary dynamically needs only `libturbojpeg` + `libfontconfig` at runtime (Skia is
static). Re-pointing an existing public tag requires a force/delete — avoid once a release is out.

## Conventions

Conventional Commits with scopes: `feat(ui):`, `fix(core):`, `docs:`, `style:`, `ci:`, `build:`.
Match the surrounding code's idiom and comment density.
