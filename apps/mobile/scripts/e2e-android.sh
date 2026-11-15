#!/usr/bin/env bash
# End-to-end mobile → desktop test on an Android emulator.
#
#   boot AVD (headless) → install APK → enable the accessibility service → launch app →
#   "Share this device" → accept the MediaProjection consent → redirect the engine's QUIC
#   UDP port to 127.0.0.1 → read the one-time password from the UI → run the desktop probe
#   (crates/aether-core/examples/connect_probe.rs) with a remote tap → verify the tap on the
#   phone → teardown (emulator shut down if this script booted it).
#
# Usage: apps/mobile/scripts/e2e-android.sh [--build] [--keep-emulator]
#   --build           rebuild the debug APK first (tauri android build --debug --apk --target aarch64)
#   --keep-emulator   leave the emulator running at the end
# Env: AVD (aetherlink_test) · EMU_PORT (5580) · HOST_PORT (31118, UDP on 127.0.0.1)
#      PROBE_SECS (10) · TAPS (2) · APK (auto-detected) · OUT (target/e2e-android/<timestamp>)
#
# If the AVD is already running (another emulator instance holds its lock) the script reuses
# that instance and does NOT shut it down. Results: $OUT/report.json (+ logcat, UI dumps).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
MOBILE="$ROOT/apps/mobile"
export ANDROID_HOME="${ANDROID_HOME:-$HOME/Library/Android/sdk}"
export NDK_HOME="${NDK_HOME:-$ANDROID_HOME/ndk/28.2.13676358}"
export JAVA_HOME="${JAVA_HOME:-/Applications/Android Studio.app/Contents/jbr/Contents/Home}"
export PATH="$HOME/.cargo/bin:$ANDROID_HOME/platform-tools:$ANDROID_HOME/emulator:$PATH"

AVD="${AVD:-aetherlink_test}"
EMU_PORT="${EMU_PORT:-5580}"
HOST_PORT="${HOST_PORT:-31118}"
PROBE_SECS="${PROBE_SECS:-10}"
# The tap target (Connect tab) is idempotent: repeat it so one tap swallowed by a janky
# WebView on an overloaded host (it can turn into a long press) does not fail the run.
TAPS="${TAPS:-2}"
PKG=dev.aetherlink.mobile
OUT="${OUT:-$ROOT/target/e2e-android/$(date +%Y%m%d-%H%M%S)}"
BUILD=0
KEEP=0
for a in "$@"; do
  case "$a" in
    --build) BUILD=1 ;;
    --keep-emulator) KEEP=1 ;;
    *) echo "unknown argument: $a" >&2; exit 2 ;;
  esac
done
mkdir -p "$OUT"

log() { printf '\033[1;34m[e2e %s]\033[0m %s\n' "$(date +%H:%M:%S)" "$*" >&2; }
die() { printf '\033[1;31m[e2e] FAIL: %s\033[0m\n' "$*" >&2; exit 1; }

SERIAL=""
EMU_PID=""
STARTED_EMU=0
GUEST_PORT=""
WIFI_WAS_ON=""
cleanup() {
  local rc=$?
  set +e
  if [[ -n "$SERIAL" ]]; then
    adb -s "$SERIAL" logcat -d >"$OUT/logcat-full.txt" 2>/dev/null
    [[ -n "$GUEST_PORT" ]] && adb -s "$SERIAL" emu redir del "udp:$HOST_PORT" >/dev/null 2>&1
    adb -s "$SERIAL" shell am force-stop "$PKG" >/dev/null 2>&1
    [[ "$WIFI_WAS_ON" == 1 ]] && adb -s "$SERIAL" shell svc wifi enable >/dev/null 2>&1
    if [[ "$STARTED_EMU" == 1 && "$KEEP" == 0 ]]; then
      log "shutting down $SERIAL"
      adb -s "$SERIAL" emu kill >/dev/null 2>&1
      for _ in $(seq 1 30); do kill -0 "$EMU_PID" 2>/dev/null || break; sleep 1; done
      kill "$EMU_PID" 2>/dev/null
    fi
  fi
  log "artifacts in $OUT (exit $rc)"
}
trap cleanup EXIT

# ------------------------------------------------------------------ helpers
# uiautomator dump → $OUT/ui-<name>.xml (retries: WebView/systemui may be mid-transition)
dump() {
  local name="$1"
  for _ in 1 2 3 4 5; do
    if adb -s "$SERIAL" exec-out uiautomator dump /dev/tty 2>/dev/null | sed 's/UI hierchary dumped to.*//' >"$OUT/ui-$name.xml" &&
      grep -q '<hierarchy' "$OUT/ui-$name.xml"; then
      return 0
    fi
    sleep 1
  done
  return 1
}

# find <xml> <attr-regex-pairs...> → prints "x y text" of the first matching node's center.
# Pairs: text=REGEX desc=REGEX id=REGEX pkg=REGEX (all must match; case-insensitive).
find_node() {
  python3 -I - "$@" <<'PY'
import re, sys, xml.etree.ElementTree as ET
path, conds = sys.argv[1], [a.split('=', 1) for a in sys.argv[2:]]
keys = {'text': 'text', 'desc': 'content-desc', 'id': 'resource-id', 'pkg': 'package', 'class': 'class'}
try:
    root = ET.parse(path).getroot()
except Exception:
    sys.exit(1)
for n in root.iter('node'):
    if all(re.search(rx, n.get(keys[k], ''), re.I) for k, rx in conds):
        x1, y1, x2, y2 = map(int, re.findall(r'\d+', n.get('bounds')))
        print((x1 + x2) // 2, (y1 + y2) // 2, n.get('text') or n.get('content-desc'))
        sys.exit(0)
sys.exit(1)
PY
}

tap_node() { # tap_node <dump-name> <conds...>
  local name="$1"; shift
  dump "$name" || return 1
  local hit
  hit="$(find_node "$OUT/ui-$name.xml" "$@")" || return 1
  read -r x y _ <<<"$hit"
  adb -s "$SERIAL" shell input tap "$x" "$y"
  log "tapped '${hit#* * }' at $x,$y"
}

wait_until() { # wait_until <secs> <what> <cmd...>
  local secs="$1" what="$2"; shift 2
  local end=$((SECONDS + secs))
  until "$@"; do
    ((SECONDS < end)) || die "timed out after ${secs}s waiting for $what"
    sleep 1
  done
}

# ------------------------------------------------------------------ 0. build
if [[ "$BUILD" == 1 ]]; then
  log "building debug APK (aarch64)"
  (cd "$MOBILE" && npm install --no-audit --no-fund >/dev/null &&
    CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target/agent-mobile}" npm run tauri -- android build --debug --apk --target aarch64) >"$OUT/build.log" 2>&1 ||
    die "APK build failed (see $OUT/build.log)"
fi
APK="${APK:-$(ls -t "$MOBILE"/src-tauri/gen/android/app/build/outputs/apk/*/debug/*.apk 2>/dev/null | head -1)}"
[[ -f "$APK" ]] || die "no debug APK found; run with --build"
log "APK: $APK"

log "building connect_probe (host)"
(cd "$ROOT" && cargo build -q -p aether-core --example connect_probe) >"$OUT/probe-build.log" 2>&1 || die "probe build failed (see $OUT/probe-build.log)"
PROBE_BIN="$(cd "$ROOT" && cargo metadata --format-version 1 --no-deps | python3 -I -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')/debug/examples/connect_probe"

# ------------------------------------------------------------------ 1. boot
for s in $(adb devices | awk '/^emulator-/{print $1}'); do
  if [[ "$(adb -s "$s" emu avd name 2>/dev/null | head -1 | tr -d '\r')" == "$AVD" ]]; then SERIAL="$s"; fi
done
if [[ -n "$SERIAL" ]]; then
  log "reusing running $AVD on $SERIAL (it will be left running)"
else
  SERIAL="emulator-$EMU_PORT"
  log "booting $AVD on $SERIAL"
  emulator -avd "$AVD" -port "$EMU_PORT" -no-window -no-audio -gpu swiftshader_indirect -no-snapshot-save -no-boot-anim >"$OUT/emulator.log" 2>&1 &
  EMU_PID=$!
  STARTED_EMU=1
fi
adb -s "$SERIAL" wait-for-device
wait_until 300 "boot completed" bash -c "[[ \"\$(adb -s $SERIAL shell getprop sys.boot_completed 2>/dev/null | tr -d '\r')\" == 1 ]]"
adb -s "$SERIAL" shell input keyevent KEYCODE_WAKEUP >/dev/null
adb -s "$SERIAL" shell wm dismiss-keyguard >/dev/null 2>&1 || true
# `adb emu redir` forwards to the slirp guest address 10.0.2.15 (eth0). When the emulated Wi-Fi
# (wlan0, 10.0.2.16, same /24) is the default network, the engine's replies are routed out of
# wlan0 and never reach the redirect: the QUIC handshake times out. Keep eth0 as the only path
# for the duration of the test (restored on exit).
WIFI_WAS_ON="$(adb -s "$SERIAL" shell settings get global wifi_on | tr -d '\r')"
if [[ "$WIFI_WAS_ON" == 1 ]]; then
  log "disabling emulated Wi-Fi (restored at the end)"
  adb -s "$SERIAL" shell svc wifi disable
  wait_until 20 "wlan0 to go down" bash -c "! adb -s $SERIAL shell ip -4 addr show wlan0 2>/dev/null | grep -q inet"
fi
read -r SCREEN_W SCREEN_H < <(adb -s "$SERIAL" shell wm size | awk -F'[ x]' '/Physical/{print $3, $4}' | tr -d '\r')
log "booted: $(adb -s "$SERIAL" shell getprop ro.build.version.release | tr -d '\r') (API $(adb -s "$SERIAL" shell getprop ro.build.version.sdk | tr -d '\r')), ${SCREEN_W}x${SCREEN_H}"

# ------------------------------------------------------------------ 2. install + permissions
adb -s "$SERIAL" shell am force-stop "$PKG" >/dev/null 2>&1 || true
log "installing APK"
adb -s "$SERIAL" install -r -g "$APK" >"$OUT/install.log" 2>&1 || die "install failed (see $OUT/install.log)"

# ------------------------------------------------------------------ 3. launch + share
adb -s "$SERIAL" logcat -c
adb -s "$SERIAL" shell am start -W -n "$PKG/.MainActivity" >/dev/null
# Remote-control backend: enable the accessibility service. Written right after an (re)install
# the setting can be dropped by AccessibilityManagerService while it is still rescanning the
# package, so re-apply until the service is actually bound.
A11Y="$PKG/$PKG.AetherAccessibilityService"
a11y_bound() {
  adb -s "$SERIAL" shell settings put secure enabled_accessibility_services "$A11Y"
  adb -s "$SERIAL" shell settings put secure accessibility_enabled 1
  sleep 1
  adb -s "$SERIAL" shell dumpsys accessibility | grep -q 'Bound services:{Service\[label=AetherLink'
}
wait_until 30 "the accessibility service to bind" a11y_bound
log "waiting for the UI"
wait_until 90 "the Share tab" tap_node home "text=^(Share|Condividi)$"
wait_until 30 "the share button" tap_node host "class=ToggleButton" "text=^(Start sharing|Avvia la condivisione)$"

log "waiting for the MediaProjection consent dialog"
consent() {
  dump consent || return 1
  local f="$OUT/ui-consent.xml" hit
  find_node "$f" "pkg=systemui" >/dev/null || return 1
  # Android 14+: pick "entire screen" when the dialog offers a single-app mode.
  if hit="$(find_node "$f" "pkg=systemui" "class=Spinner")" &&
    ! find_node "$f" "pkg=systemui" "text=(entire|intero)" >/dev/null; then
    read -r x y _ <<<"$hit"; adb -s "$SERIAL" shell input tap "$x" "$y"; sleep 1
    dump consent-spinner && hit="$(find_node "$OUT/ui-consent-spinner.xml" "text=(entire screen|schermo intero|intero schermo)")" &&
      { read -r x y _ <<<"$hit"; adb -s "$SERIAL" shell input tap "$x" "$y"; sleep 1; dump consent; }
  fi
  hit="$(find_node "$f" "pkg=systemui" "id=android:id/button1")" ||
    hit="$(find_node "$f" "pkg=systemui" "class=Button" "text=^(Share screen|Start now|Start|Next|Condividi schermo|Avvia ora|Avvia|Avanti)$")" || return 1
  read -r x y _ <<<"$hit"
  adb -s "$SERIAL" shell input tap "$x" "$y"
  log "consent: tapped '${hit#* * }'"
}
wait_until 30 "the consent dialog" consent

sharing_line() { adb -s "$SERIAL" logcat -d -s AetherLink:I | grep -m1 -E 'sharing [0-9]+x[0-9]+ as' ; }
wait_until 30 "the encoder to start" sharing_line >/dev/null
SHARING="$(sharing_line)"
log "phone: ${SHARING#*: }"
ENCODER="$(adb -s "$SERIAL" logcat -d -s AetherLink:I | grep -m1 -E ' encoder .* lowLatency=' | sed 's/.*: //')"

# ------------------------------------------------------------------ 4. port + redirect
GUEST_PORT="$(adb -s "$SERIAL" logcat -d -s AetherLink:I | grep -m1 'AetherLink engine started' | sed -nE 's/.*addr=Some\(\[?[^]]*\]?:([0-9]+)\).*/\1/p')"
[[ -n "$GUEST_PORT" ]] || die "engine port not found in logcat"
log "phone engine bound UDP $GUEST_PORT → redirect 127.0.0.1:$HOST_PORT"
adb -s "$SERIAL" emu redir del "udp:$HOST_PORT" >/dev/null 2>&1 || true
adb -s "$SERIAL" emu redir add "udp:$HOST_PORT:$GUEST_PORT" | grep -q OK || die "emulator UDP redirect failed"

# ------------------------------------------------------------------ 5. password (from the Share page)
# The Share page shows the password in clear text. While the encoder runs the UI never goes
# idle, so each uiautomator dump can take 10-30 s: dump once, read password + tap target.
PASSWORD=""
read_pw() {
  dump password || return 1
  local hit
  hit="$(find_node "$OUT/ui-password.xml" 'text=^[a-z2-9]{4}-[a-z2-9]{4}$')" || return 1
  PASSWORD="${hit##* }"
}
wait_until 60 "the password on screen" read_pw
log "password read from the UI: ${PASSWORD:0:2}**-****"

# Remote tap target: the "Connect" tab (normalized), verified afterwards via the UI.
TAP_HIT="$(find_node "$OUT/ui-password.xml" 'text=^(Connect|Connetti)$')" || die "Connect tab not found"
read -r TX TY _ <<<"$TAP_HIT"
TAP_NX="$(python3 -c "print(round($TX / $SCREEN_W, 4))")"
TAP_NY="$(python3 -c "print(round($TY / $SCREEN_H, 4))")"

# ------------------------------------------------------------------ 6. probe
log "probe: connecting to 127.0.0.1:$HOST_PORT for ${PROBE_SECS}s (tap at $TAP_NX,$TAP_NY)"
set +e
"$PROBE_BIN" --target "127.0.0.1:$HOST_PORT" --password "$PASSWORD" --secs "$PROBE_SECS" --tap "$TAP_NX,$TAP_NY" --taps "$TAPS" >"$OUT/probe.json" 2>"$OUT/probe.stderr"
PROBE_RC=$?
set -e
cat "$OUT/probe.json"

# ------------------------------------------------------------------ 7. phone-side checks
sleep 1
adb -s "$SERIAL" logcat -d -s AetherLink >"$OUT/logcat-aetherlink.txt"
TAP_OK=false
if dump after-tap && find_node "$OUT/ui-after-tap.xml" 'text=^(This device|Questo dispositivo)$' >/dev/null; then TAP_OK=true; fi
A11Y_ON="$(adb -s "$SERIAL" shell settings get secure enabled_accessibility_services | tr -d '\r')"
VIEWERS_SEEN="$(grep -c 'viewers: [1-9]' "$OUT/logcat-aetherlink.txt" || true)"
HOST_SESSION="$(grep -m1 'host session started' "$OUT/logcat-aetherlink.txt" | sed 's/.*AetherLink *: *//' || true)"
GESTURES="$(grep -c 'gesture completed' "$OUT/logcat-aetherlink.txt" || true)"
INPUT_ERR="$(grep -m1 -iE 'inputPermission|inject failed|rejected the event|backend ready=false|gesture cancelled|dispatchGesture refused' "$OUT/logcat-aetherlink.txt" | sed 's/.*AetherLink *: *//' || true)"

python3 -I - "$OUT/probe.json" "$OUT/report.json" <<PY
import json, sys
try:
    probe = json.load(open(sys.argv[1]))
except Exception as e:
    probe = {"ok": False, "error": f"probe output unreadable: {e}"}
report = {
    "device": {"serial": "$SERIAL", "avd": "$AVD", "screen": "${SCREEN_W}x${SCREEN_H}",
               "sharing": """$SHARING""".split(": ", 1)[-1], "encoder": """$ENCODER""",
               "guestPort": int("$GUEST_PORT"), "hostPort": int("$HOST_PORT")},
    "probe": probe,
    "phone": {"hostSessionLog": """$HOST_SESSION""", "viewerLogLines": int("${VIEWERS_SEEN:-0}"),
              "accessibilityService": """$A11Y_ON""", "remoteTapLanded": $( [[ $TAP_OK == true ]] && echo True || echo False ),
              "gesturesCompleted": int("${GESTURES:-0}"),
              "inputError": """$INPUT_ERR""" or None},
}
report["ok"] = bool(probe.get("ok")) and report["phone"]["remoteTapLanded"]
json.dump(report, open(sys.argv[2], "w"), indent=2)
print(json.dumps({"ok": report["ok"], "connectMs": probe.get("connectMs"), "accessUnits": probe.get("video", {}).get("accessUnits"),
                  "keyframes": probe.get("video", {}).get("keyframes"), "gestures": report["phone"]["gesturesCompleted"], "remoteTapLanded": report["phone"]["remoteTapLanded"]}))
PY
[[ "$PROBE_RC" == 0 && "$TAP_OK" == true ]] || die "end-to-end checks failed (see $OUT/report.json)"
log "PASS"
