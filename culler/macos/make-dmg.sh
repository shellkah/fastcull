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
