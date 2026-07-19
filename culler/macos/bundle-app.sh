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
sed "s|@VERSION@|$VERSION|g" "$HERE/Info.plist.in" > "$APP/Contents/Info.plist"

# Give the bundled dylib an executable-relative id, then repoint the binary at
# it in EVERY arch slice. The x86_64 slice (built on the Intel runner) links a
# /usr/local/... path; the arm64 slice links /opt/homebrew/... — different
# strings, so run -change once per arch (a no-op where the ref is absent).
install_name_tool -id "@executable_path/../Frameworks/$base" "$APP/Contents/Frameworks/$base"
for arch in x86_64 arm64; do
  ref="$(otool -arch "$arch" -L "$APP/Contents/MacOS/fastcull" 2>/dev/null | awk '/turbojpeg/ {print $1; exit}')" || ref=""
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
