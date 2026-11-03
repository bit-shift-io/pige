#!/usr/bin/env bash
# Build, install, launch and tail the log for the iced Android app.
# Usage: run_adb.sh [debug|release]   (defaults to debug for fast iteration)
# Adapted from qlaunch (bitshift, MIT).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"
MODE="${1:-debug}"
case "$MODE" in debug|release) ;; *) echo "Usage: $0 [debug|release]" >&2; exit 2;; esac

if ! command -v adb >/dev/null 2>&1; then
  echo "adb not found in PATH." >&2
  exit 1
fi

# Fail clearly when nothing is attached, before the blocking wait below.
if ! adb get-state 1>/dev/null 2>&1; then
  echo "No Android device attached. Connect one and enable USB debugging." >&2
  exit 1
fi
# Wait for a device that is still coming up (offline -> device).
adb wait-for-device
SERIAL="$(adb get-serialno)"
MODEL="$(adb -s "$SERIAL" shell getprop ro.product.model | tr -d '\r')"
echo "Device: $SERIAL ($MODEL)"

export JAVA_HOME="${JAVA_HOME:-/opt/jbr-21}"
bash "$ROOT/scripts/build.sh" "$MODE"

case "$MODE" in
  release) APK="$ROOT/build/app/outputs/apk/release/app-release.apk" ;;
  debug)   APK="$ROOT/build/app/outputs/apk/debug/app-debug.apk" ;;
esac

adb install -r "$APK"
adb shell am start -W -n bitshift.pige/.MainActivity
exec adb logcat -s pige
