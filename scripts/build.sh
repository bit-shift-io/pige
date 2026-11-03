#!/usr/bin/env bash
# arm64 Android build. Run with bash scripts/build.sh [debug|release].
# Adapted from qlaunch (bitshift, MIT).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
MODE="${1:-release}"
case "$MODE" in debug|release) ;; *) echo "Usage: $0 [debug|release]" >&2; exit 2;; esac
if [[ -f "$ROOT/android/gradlew" && -f "$ROOT/android/gradle/wrapper/gradle-wrapper.jar" ]]; then
    GRADLE=(bash "$ROOT/android/gradlew")
elif command -v gradle >/dev/null; then
    GRADLE=(gradle)
else
    echo "Install Gradle 9.8.1 or restore the Android Gradle wrapper." >&2
    exit 1
fi
export JAVA_HOME="${JAVA_HOME:-/opt/jbr-21}"
export ANDROID_HOME="${ANDROID_HOME:-$HOME/Android/Sdk}"
export ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-$ANDROID_HOME/ndk/28.2.13676358}"
TOOLCHAIN="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin"
if [[ ! -x "$TOOLCHAIN/clang" ]]; then
    echo "Incomplete NDK at $ANDROID_NDK_HOME (clang is missing)." >&2
    echo "Set ANDROID_NDK_HOME to a complete NDK; this build uses 28.2.13676358." >&2
    exit 1
fi
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$TOOLCHAIN/aarch64-linux-android34-clang"
export CC_aarch64_linux_android="$CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER"
export AR_aarch64_linux_android="$TOOLCHAIN/llvm-ar"
# Retain compatibility with Android devices using 16 KiB pages.
export CARGO_TARGET_AARCH64_LINUX_ANDROID_RUSTFLAGS="-C link-arg=-Wl,-z,max-page-size=16384"
FLAGS=()
if [[ "$MODE" == release ]]; then FLAGS+=(--release); TASK=assembleRelease; else TASK=assembleDebug; fi
cargo build --locked --manifest-path "$ROOT/rust/Cargo.toml" -p pige --lib \
    --target aarch64-linux-android "${FLAGS[@]}"
mkdir -p "$ROOT/android/app/src/main/jniLibs/arm64-v8a"
cp "$ROOT/rust/target/aarch64-linux-android/$MODE/libpige.so" \
    "$ROOT/android/app/src/main/jniLibs/arm64-v8a/"
JAVA_SRC="$(cargo metadata --locked --format-version 1 --filter-platform aarch64-linux-android --manifest-path "$ROOT/rust/Cargo.toml" | \
    python3 -c 'import json,sys,pathlib; p=next(p for p in json.load(sys.stdin)["packages"] if p["name"]=="iced_android"); print(pathlib.Path(p["manifest_path"]).parent / "java")')"
"${GRADLE[@]}" -p "$ROOT/android" "-PicedAndroidJava=$JAVA_SRC" "$TASK"
