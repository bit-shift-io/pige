#!/usr/bin/env bash
# Rasterize assets/app_icon.svg into the desktop window icon and the Android
# launcher mipmaps. The SVG is rendered 1:1 at each size; any margins or
# masking belong in the SVG itself.
# Usage: bash scripts/icons.sh
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SVG="$ROOT/assets/app_icon.svg"

if ! command -v inkscape >/dev/null 2>&1; then
    echo "inkscape is required to rasterize the icon." >&2
    exit 1
fi

render() { # size out_path
    inkscape "$SVG" --export-type=png \
        --export-width="$1" --export-height="$1" \
        --export-filename="$2" >/dev/null
}

# Desktop window icon (embedded via include_bytes! in rust/pige/src/app.rs).
render 256 "$ROOT/assets/app_icon_256.png"

# Android launcher mipmaps; the manifest points at @mipmap/ic_launcher.
render 48  "$ROOT/android/app/src/main/res/mipmap-mdpi/ic_launcher.png"
render 72  "$ROOT/android/app/src/main/res/mipmap-hdpi/ic_launcher.png"
render 96  "$ROOT/android/app/src/main/res/mipmap-xhdpi/ic_launcher.png"
render 144 "$ROOT/android/app/src/main/res/mipmap-xxhdpi/ic_launcher.png"
render 192 "$ROOT/android/app/src/main/res/mipmap-xxxhdpi/ic_launcher.png"

echo "Icons rendered from $(basename "$SVG"):"
git -C "$ROOT" status --short assets android/app/src/main/res
