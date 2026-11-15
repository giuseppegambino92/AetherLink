#!/usr/bin/env bash
# Runs inside aetherlink-linux: Ghost Mode (X11 curtain, logind backlight, capture exclusion) + OCR
# live tests, with Xvfb (two RandR monitors) + a D-Bus session. Usage: bash ci/linux-ghost-test.sh
set -uo pipefail
cd /work
RUSTUP_TOOLCHAIN=$(cd / && rustup default | awk '{print $1}')
export RUSTUP_TOOLCHAIN
STAGE=${STAGE:-all}

if [[ "${1:-}" != "--inner" ]]; then
  Xvfb :99 -screen 0 1280x800x24 -nolisten tcp >/tmp/xvfb.log 2>&1 &
  XVFB=$!
  trap 'kill $XVFB 2>/dev/null || true' EXIT
  for _ in $(seq 50); do [[ -e /tmp/.X11-unix/X99 ]] && break; sleep 0.1; done
  export DISPLAY=:99
  echo "== X extensions:"; xdpyinfo -queryExtensions | grep -E -i "^\s+(composite|shape|randr|mit-shm)" || true
  # Two RandR monitors side by side so the curtain must create one window per monitor.
  xrandr --setmonitor aether-left 640/170x800/210+0+0 none
  xrandr --setmonitor aether-right 640/170x800/210+640+0 none
  xrandr --listmonitors
  dbus-run-session -- bash "$0" --inner
  exit $?
fi

RC=0
step() { echo; echo "======== $*"; "$@" || { echo "######## FAILED: $*"; RC=1; }; }

if [[ $STAGE == all || $STAGE == clippy ]]; then
  step cargo clippy -p aether-display -p aether-capture -p aether-core --lib --tests -- -D warnings
  [[ -z "${SKIP_ALL_TARGETS:-}" ]] && step cargo clippy -p aether-display -p aether-capture -p aether-core --all-targets -- -D warnings
fi
if [[ $STAGE == all || $STAGE == test ]]; then
  step cargo test -p aether-display -p aether-core --no-fail-fast
  step cargo test -p aether-capture --lib --no-fail-fast -- linux::x11::
  # Live tests (ignored by default): X11 curtain on Xvfb, logind D-Bus mock, X11 capture exclusion, ocrs.
  step cargo test -p aether-display --lib -- --ignored --test-threads=1 --skip real_backlight --nocapture
  step cargo test -p aether-capture --lib -- --ignored --test-threads=1 linux::x11:: --nocapture
  if [[ -d /models ]]; then
    AETHERLINK_OCRS_MODEL_DIR=/models step cargo test -p aether-core --lib -- --ignored ocr::tests::ocrs_recognizes_rendered_text --nocapture
  fi
  if command -v tesseract >/dev/null; then
    step cargo test -p aether-core --lib -- --ignored ocr::tests::tesseract_recognizes_rendered_text --nocapture
  fi
fi
echo; [[ $RC == 0 ]] && echo "== ALL STEPS PASSED" || echo "== SOME STEPS FAILED"
exit $RC
