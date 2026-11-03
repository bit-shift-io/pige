#!/usr/bin/env bash
# Build, install and run the iced Android app on a connected device.
# Extra args go to the build script (e.g. ./run_adb.sh debug).
# Adapted from qlaunch (bitshift, MIT).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"
MODE="${1:-release}"

export JAVA_HOME="${JAVA_HOME:-/opt/jbr-21}"
bash "$ROOT/scripts/build.sh" "$MODE"

case "$MODE" in
  release) APK="$ROOT/build/app/outputs/apk/release/app-release.apk" ;;
  debug)   APK="$ROOT/build/app/outputs/apk/debug/app-debug.apk" ;;
esac

if ! command -v adb >/dev/null 2>&1; then
  echo "adb not found in PATH." >&2
  exit 1
fi

adb install -r "$APK"
adb shell am start -W -n bitshift.pige/.MainActivity
