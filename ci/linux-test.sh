#!/usr/bin/env bash
# Linux backend test harness (run INSIDE the aetherlink-linux image).
#
# Brings up a throwaway desktop-ish session and runs clippy + the unit tests + the live
# (#[ignore]d) tests of the Linux backends:
#   * Xvfb :99 (X11 SHM capture, RandR, XTest, GTK app for AT-SPI)
#   * a D-Bus session bus + the AT-SPI bus launcher + gtk3-demo with the ATK bridge
#   * PipeWire + WirePlumber + pipewire-pulse with a null sink as the default sink
#   * /dev/uinput and /dev/fuse tests run only when the devices are present
#
# Host invocation (from the repository root):
#   docker run --rm \
#     --device /dev/uinput --device-cgroup-rule='c 13:* rmw' \
#     --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor:unconfined \
#     -v "$PWD":/work -v "$HOME/.cargo/registry":/usr/local/cargo/registry \
#     -v aetherlink-target-linux:/work/target \
#     aetherlink-linux bash ci/linux-test.sh
#
# Options (env): SKIP_CLIPPY=1, CRATES="aether-capture aether-input" (subset).
set -euo pipefail
cd "$(dirname "$0")/.."

CRATES=${CRATES:-"aether-capture aether-codec aether-input aether-dom aether-vfs"}

# rust-toolchain.toml says "stable", which makes rustup download a fresh toolchain (without
# clippy) inside the throwaway container. Use the image's preinstalled default toolchain.
if [[ -z "${RUSTUP_TOOLCHAIN:-}" ]]; then
  RUSTUP_TOOLCHAIN=$(cd / && rustup default | awk '{print $1}')
  export RUSTUP_TOOLCHAIN
fi
PKGS=()
for c in $CRATES; do PKGS+=(-p "$c"); done

if [[ "${1:-}" != "--inner" ]]; then
  export XDG_RUNTIME_DIR
  XDG_RUNTIME_DIR=$(mktemp -d /tmp/aether-xdg.XXXXXX)
  chmod 700 "$XDG_RUNTIME_DIR"
  export XDG_STATE_HOME="$XDG_RUNTIME_DIR/state"
  Xvfb :99 -screen 0 1280x800x24 -nolisten tcp >/tmp/xvfb.log 2>&1 &
  XVFB=$!
  trap 'kill $XVFB 2>/dev/null || true' EXIT
  for _ in $(seq 50); do [[ -e /tmp/.X11-unix/X99 ]] && break; sleep 0.1; done
  export DISPLAY=:99
  # Run the rest inside a private session bus (AT-SPI, WirePlumber and the portals need it).
  dbus-run-session -- bash "$0" --inner
  exit $?
fi

# ---------------------------------------------------------------- session services
BG=()
cleanup() { for p in "${BG[@]}"; do kill "$p" 2>/dev/null || true; done; }
trap cleanup EXIT

pipewire >/tmp/pipewire.log 2>&1 & BG+=($!)
sleep 0.5
wireplumber >/tmp/wireplumber.log 2>&1 & BG+=($!)
pipewire-pulse >/tmp/pipewire-pulse.log 2>&1 & BG+=($!)
for _ in $(seq 50); do pactl info >/dev/null 2>&1 && break; sleep 0.2; done
# pipewire-pulse answers before WirePlumber is ready to create nodes: retry.
for i in $(seq 40); do
  pactl load-module module-null-sink sink_name=aether_null >/dev/null 2>&1 && break
  [[ $i == 40 ]] && { echo "cannot create the null sink" >&2; exit 1; }
  sleep 0.5
done
# The new sink becomes selectable a moment after it is created: retry as well.
for i in $(seq 40); do
  pactl set-default-sink aether_null >/dev/null 2>&1 && break
  [[ $i == 40 ]] && { echo "cannot select the null sink as default" >&2; exit 1; }
  sleep 0.25
done

AT_SPI_LAUNCHER=$(ls /usr/libexec/at-spi-bus-launcher /usr/lib/*/at-spi-bus-launcher /usr/lib/at-spi2-core/at-spi-bus-launcher 2>/dev/null | head -n1 || true)
if [[ -n "$AT_SPI_LAUNCHER" ]]; then
  "$AT_SPI_LAUNCHER" --launch-immediately >/tmp/at-spi.log 2>&1 & BG+=($!)
  sleep 1
fi
export GTK_MODULES=gail:atk-bridge NO_AT_BRIDGE=0
gtk3-demo --run=dialog >/tmp/gtk3-demo.log 2>&1 & BG+=($!)
sleep 2

echo "== session: DISPLAY=$DISPLAY, PipeWire $(pactl info | sed -n 's/^Server Name: //p'), default sink $(pactl get-default-sink)"
[[ -e /dev/uinput ]] && echo "== /dev/uinput present" || echo "== /dev/uinput absent: uinput test will fail if requested"
[[ -e /dev/fuse ]] && echo "== /dev/fuse present" || echo "== /dev/fuse absent: FUSE test will fail if requested"

# ---------------------------------------------------------------- gates
if [[ -z "${SKIP_CLIPPY:-}" ]]; then
  cargo clippy "${PKGS[@]}" --all-targets -- -D warnings
fi
RC=0
cargo test "${PKGS[@]}" --no-fail-fast || RC=1
# Live tests (#[ignore] by default because they need this harness). One thread: several
# tests share the X pointer / keyboard focus and the default sink.
SKIP_ARGS=()
[[ -e /dev/uinput ]] || SKIP_ARGS+=(--skip uinput_devices_emit_real_evdev_events)
[[ -e /dev/fuse ]] || SKIP_ARGS+=(--skip fuse)
# The NFS fallback test needs root + the kernel NFS client (works in Docker Desktop's VM).
[[ $(id -u) == 0 ]] || SKIP_ARGS+=(--skip real_nfs_mount_roundtrip)
cargo test "${PKGS[@]}" --no-fail-fast -- --ignored --test-threads=1 "${SKIP_ARGS[@]}" || RC=1
if [[ $RC == 0 ]]; then echo "== linux-test.sh: all gates passed"; else echo "== linux-test.sh: FAILURES (see above)"; fi
exit $RC
