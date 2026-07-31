#!/usr/bin/env bash
#
# M0 hardware survey. Run this ON THE PI (or `ssh pi@host 'bash -s' < deploy/m0-survey.sh`)
# and keep the output — every later milestone depends on these facts, and several of them
# (architecture, panel native mode, touch device name) determine build and layout decisions
# that are annoying to change later.
#
# Read-only: it inspects and reports, it does not configure anything.
#
set -uo pipefail

hr() { printf '\n=== %s %s\n' "$1" "$(printf '=%.0s' $(seq 1 $((66 - ${#1}))))"; }
have() { command -v "$1" >/dev/null 2>&1; }

hr "Identity"
cat /proc/device-tree/model 2>/dev/null | tr -d '\0'; echo
echo "kernel        : $(uname -srm)"
echo "dpkg arch     : $(dpkg --print-architecture 2>/dev/null)"
echo "userland bits : $(getconf LONG_BIT)"
[ -r /etc/os-release ] && . /etc/os-release && echo "os            : $PRETTY_NAME"
echo
echo ">>> The dpkg architecture decides the Rust target:"
echo "      arm64 -> aarch64-unknown-linux-gnu    armhf -> armv7-unknown-linux-gnueabihf"

hr "DRM / display"
echo "--- /dev/dri ---"
ls -l /dev/dri/ 2>/dev/null || echo "(no /dev/dri: is vc4-kms-v3d loaded?)"
echo
echo "--- connectors, status, and modes ---"
for c in /sys/class/drm/card*-*; do
  [ -e "$c/status" ] || continue
  printf '%-24s %-12s' "$(basename "$c")" "$(cat "$c/status")"
  if [ "$(cat "$c/status")" = connected ]; then
    printf 'preferred=%s' "$(head -1 "$c/modes" 2>/dev/null)"
    n=$(wc -l < "$c/modes" 2>/dev/null)
    printf '  (%s modes total)' "${n:-0}"
  fi
  echo
done
echo
echo ">>> The first mode listed is the panel's preferred mode. Do not hardcode 800x480;"
echo "    7\" DSI panels in this class also ship as 1024x600."
echo
echo "--- loaded graphics drivers ---"
lsmod 2>/dev/null | grep -E '^(vc4|v3d|drm|drm_kms_helper)' || echo "(none matched)"
echo
echo "--- CMA (scanout memory) ---"
grep -iE 'cma' /proc/meminfo 2>/dev/null || echo "(no CMA lines in /proc/meminfo)"
dmesg 2>/dev/null | grep -iE 'cma:|cma reserved' | tail -3

hr "GLES capability"
if have eglinfo; then
  eglinfo 2>/dev/null | grep -iE 'EGL version|EGL vendor|OpenGL ES profile version' | head -8
else
  echo "(eglinfo not installed: sudo apt install mesa-utils-bin mesa-utils)"
fi
echo
echo ">>> Expect 'OpenGL ES 2.x' — the Pi 3's vc4 is GLES 2.0 / OpenGL 2.1 only. If a Mesa"
echo "    build reports something higher here, trust the driver, not the string."

hr "Touch input"
echo "--- devices mentioning touch/ft5x06/edt ---"
grep -iE -B2 -A6 'ft5|edt|touch' /proc/bus/input/devices 2>/dev/null || echo "(none found)"
echo
echo "--- /dev/input ---"
ls -l /dev/input/ 2>/dev/null | grep -E 'event|by-path' | head -20
echo
echo ">>> Record the exact device Name= string; avionics-input matches on it rather than on a"
echo "    fragile eventN number, which reorders across boots."

hr "SDR radios"
if have rtl_test; then
  timeout 5 rtl_test -t 2>&1 | head -20
else
  echo "(rtl_test not installed)"
fi
echo
echo "--- USB devices ---"
lsusb 2>/dev/null | grep -iE 'rtl|realtek|dvb|nooelec' || lsusb 2>/dev/null
echo
echo ">>> Both NESDR Nano 2 dongles must appear, with distinct serials. Stratux uses the"
echo "    serial (stx:1090 / stx:978) to decide which radio does which job."

hr "GPS"
ls -l /dev/ttyACM* /dev/ttyUSB* /dev/serial/by-id/* 2>/dev/null || echo "(no serial devices)"
echo
echo ">>> The GPYes 2.0 is a u-blox 8 over CDC-ACM, so expect /dev/ttyACM0."

hr "Stratux backend"
systemctl is-active stratux 2>/dev/null && systemctl status stratux --no-pager -n 5 2>/dev/null | head -12
echo
if have curl; then
  echo "--- GET /getStatus (truncated) ---"
  curl -s --max-time 5 http://localhost/getStatus | head -c 900; echo
  echo
  echo "--- GET /getSituation (truncated) ---"
  curl -s --max-time 5 http://localhost/getSituation | head -c 600; echo
else
  echo "(curl not installed)"
fi
echo
echo ">>> GPSFixQuality > 0 means a usable fix. Non-zero ES_messages_last_minute and"
echo "    UAT_messages_last_minute mean both radios are actually decoding."

hr "CPU / thermal headroom"
echo "cores      : $(nproc)"
echo "loadavg    : $(cut -d' ' -f1-3 /proc/loadavg)"
have vcgencmd && {
  echo "temp       : $(vcgencmd measure_temp)"
  echo "throttled  : $(vcgencmd get_throttled)   # 0x0 is clean; see docs for bit meanings"
  echo "arm clock  : $(vcgencmd measure_clock arm)"
}
echo
echo "--- top CPU consumers ---"
ps -eo pcpu,comm --sort=-pcpu 2>/dev/null | head -8
echo
echo ">>> This is the budget the renderer has to live inside. dump1090 and dump978 together"
echo "    will already be using a large share of two of the four cores; the display must not"
echo "    starve them, because a dropped ADS-B message is worse than a dropped frame."

hr "Survey complete"
echo "Save this output. M1 (the rendering spike) is the next step:"
echo "    ./deploy/sync-sysroot.sh <user>@<host>"
echo "    ./deploy/deploy.sh       <user>@<host>"
echo "    ssh -t <user>@<host> 'sudo /tmp/gfx-spike'"
