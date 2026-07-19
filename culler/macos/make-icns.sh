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
