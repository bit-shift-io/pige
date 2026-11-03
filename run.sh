#!/usr/bin/env bash
# Run pige on the desktop.
# Extra args go to cargo (e.g. ./run.sh --release).
# Adapted from qlaunch (bitshift, MIT).
set -euo pipefail
cd "$(dirname "$0")/rust"

# A desktop session normally exports these; fill them in for a bare shell.
if [[ -z "${XDG_RUNTIME_DIR:-}" ]]; then
  export XDG_RUNTIME_DIR="/run/user/$(id -u)"
fi
if [[ -z "${WAYLAND_DISPLAY:-}" && -S "$XDG_RUNTIME_DIR/wayland-1" ]]; then
  export WAYLAND_DISPLAY=wayland-1
fi

exec cargo run -p pige --bin pige "$@"
