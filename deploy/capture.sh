#!/usr/bin/env bash
#
# Record a Stratux session unattended, with a board-health log beside it. Runs ON THE PI.
#
#   sudo ./capture.sh                       # record CAPTURE_DURATION seconds into CAPTURE_DIR
#   CAPTURE_DURATION=600 sudo ./capture.sh  # ten minutes
#
# Exists because the things still blocking this project — a GPS 3D fix, live UAT, a real NEXRAD
# mosaic, and what the renderer costs the radios — all need a sky view, and none of them need a
# network. Carry the Pi outside, let this run, bring it back, and pull the directory off over
# ethernet. What comes back is a recording that replays on the dev machine for ever, which is
# worth more than a live look you cannot repeat.
#
# # Why the health log
#
# A session recorded on a marginal battery is worthless, and nothing in the recording itself says
# so. `throttled` bits are sticky, so a brown-out thirty seconds in taints everything after it
# while the data still looks perfectly plausible. Sampling alongside means the summary can say
# "this run is not trustworthy" instead of you finding out months later.
#
set -euo pipefail

DURATION="${CAPTURE_DURATION:-1800}"
OUT_DIR="${CAPTURE_DIR:-/var/log/avionics-capture}"
KEEP="${CAPTURE_KEEP:-10}"
HOST="${CAPTURE_HOST:-127.0.0.1}"
PORT="${CAPTURE_PORT:-80}"
INTERVAL="${CAPTURE_HEALTH_INTERVAL:-10}"
REPLAY="${REPLAY_BINARY:-/opt/avionics/bin/replay}"

if [[ ! -x "$REPLAY" ]]; then
  for candidate in /opt/avionics/bin/replay /tmp/replay "$(dirname "${BASH_SOURCE[0]}")/replay"; do
    [[ -x "$candidate" ]] && REPLAY="$candidate" && break
  done
fi
if [[ ! -x "$REPLAY" ]]; then
  echo "!!! No replay binary found (looked for $REPLAY)." >&2
  echo "    Push one with: ./deploy/deploy.sh <user>@<host>" >&2
  exit 1
fi

# --- will the recording survive the walk back inside? -----------------------------------------
#
# The Stratux image boots through /sbin/init-overlay, which puts the root filesystem behind a RAM
# overlay unless /overlay/disable exists. That is good for surviving power cuts and fatal for this
# script: a capture written into the overlay looks entirely normal — right size, right frame count,
# readable — until the Pi is powered off, at which point it never existed.
#
# The state that matters is the state at capture time, not at install time, because the timer fires
# after a boot and the flag can be toggled from the Stratux web UI in between.
persistence() {
  local fstype
  fstype="$(findmnt -no FSTYPE / 2>/dev/null || echo unknown)"
  if [[ "$fstype" == overlay ]]; then
    echo VOLATILE
  elif grep -q 'init=/sbin/init-overlay' /proc/cmdline 2>/dev/null && [[ ! -e /overlay/disable ]]; then
    echo VOLATILE_NEXT_BOOT
  else
    echo PERSISTENT
  fi
}

PERSISTENCE="$(persistence)"
if [[ "$PERSISTENCE" == VOLATILE && "${CAPTURE_ALLOW_VOLATILE:-0}" != 1 ]]; then
  cat >&2 <<'EOF'
!!! REFUSING TO RECORD: the root filesystem is a RAM overlay.

    Anything written now is discarded when the Pi powers off. The recording would look
    completely normal — right size, right frame count, readable — right up until you got
    back and found nothing there. The entire point of this script is to bring data home,
    so it fails here rather than after you have carried the Pi outside for half an hour.

    Make the disk persistent:
        sudo touch /overlay/disable && sudo reboot

    Or, if you genuinely want a throwaway run:
        CAPTURE_ALLOW_VOLATILE=1 sudo ./capture.sh
EOF
  exit 1
fi

# The clock is wrong until Stratux gets a GPS fix and sets it, so a timestamp alone is not unique —
# two captures on the same stuck clock would collide and the second would overwrite the first.
# Seconds-since-boot disambiguates them and is monotonic regardless of what the clock thinks.
STAMP="$(date -u +%Y%m%dT%H%M%SZ)-up$(cut -d. -f1 /proc/uptime)s"
SESSION="$OUT_DIR/$STAMP"
mkdir -p "$SESSION"

HEALTH="$SESSION/health.csv"
RECORDING="$SESSION/session.jsonl"
SUMMARY="$SESSION/summary.txt"

# Read one vcgencmd value, or "?" if it is unavailable.
#
# Written without a pipeline on purpose: `vcgencmd ... | cut` succeeds even when vcgencmd does not
# exist, because `cut` is what sets the exit status, so a `|| echo "?"` fallback on the pipeline can
# never fire. That would have written empty fields into the health log on any board where vcgencmd
# is missing, and an empty field reads as "measured nothing" rather than "could not measure".
# `systemctl is-active` prints its answer AND exits non-zero for anything that is not active, so a
# plain `|| echo unknown` appends a second line to the one it already printed. Take the output and
# only substitute when there genuinely is none.
svc() {
  local out
  out="$(systemctl is-active "$1" 2>/dev/null)" || true
  printf '%s' "${out:-unknown}"
}

vc() {
  local out
  out="$(vcgencmd "$@" 2>/dev/null)" || { printf '?'; return 0; }
  [[ -n "$out" ]] || { printf '?'; return 0; }
  printf '%s' "${out#*=}"
}

# --- the things that made the last outdoor trip un-diagnosable ------------------------------
#
# A session came back with no GPS fix, no weather and a report that the access point was off, and
# none of it could be answered afterwards: the journal is volatile, so once the Pi is power-cycled
# the only evidence is what was written to disk while it ran. These sample the three subsystems
# whose absence is otherwise indistinguishable from "nothing was in range".

# Is the access point actually up, and is anyone on it?
ap_state() {
  [[ -e /sys/class/net/ap0 ]] || { printf 'absent'; return 0; }
  local s
  s="$(cat /sys/class/net/ap0/operstate 2>/dev/null)" || { printf '?'; return 0; }
  printf '%s' "${s:-?}"
}

ap_clients() {
  # `grep -c` exits non-zero when it counts zero, so the usual `|| fallback` would report "no iw"
  # every time the AP simply had no clients. Check for the tool up front instead.
  command -v iw >/dev/null 2>&1 || { printf '?'; return 0; }
  [[ -e /sys/class/net/ap0 ]] || { printf '?'; return 0; }
  local n
  n="$(iw dev ap0 station dump 2>/dev/null | grep -c '^Station' || true)"
  printf '%s' "${n:-0}"
}

# Satellites, fix state and message totals, straight from Stratux. Satellites *seen* is the field
# that matters: it separates "the antenna cannot see sky" from "the receiver is not talking", and
# those need completely different fixes.
stratux_sample() {
  local json
  json="$(curl -s --max-time 2 "http://$HOST:$PORT/getStatus" 2>/dev/null)" || { printf '?,?,?,?,?'; return 0; }
  [[ -n "$json" ]] || { printf '?,?,?,?,?'; return 0; }
  printf '%s' "$json" | python3 -c '
import json, sys
try:
    d = json.load(sys.stdin)
except Exception:
    print("?,?,?,?,?"); raise SystemExit
g = lambda k: d.get(k, "?")
print(",".join(str(x) for x in (
    g("GPS_satellites_seen"), g("GPS_satellites_locked"),
    str(g("GPS_solution")).replace(" ", "_").replace(",", ""),
    g("ES_messages_total"), g("UAT_messages_total"))))
' 2>/dev/null || printf '?,?,?,?,?'
}

# --- manifest: what this run even was ------------------------------------------------------
{
  echo "captured_by     : capture.sh"
  echo "clock_utc       : $(date -u --iso-8601=seconds)  (WRONG until Stratux gets a GPS fix)"
  echo "uptime_s        : $(cut -d. -f1 /proc/uptime)"
  echo "boot_id         : $(cat /proc/sys/kernel/random/boot_id)"
  echo "duration_s      : $DURATION"
  echo "source          : $HOST:$PORT"
  echo "model           : $(cat /proc/device-tree/model 2>/dev/null | tr -d '\0' || echo unknown)"
  echo "kernel          : $(uname -r)"
  echo "display_running : $(svc avionics)"
  echo "stratux_running : $(svc stratux)"
  echo "persistence     : $PERSISTENCE"
  echo "ap0             : $(ap_state)  $(ip -brief addr show ap0 2>/dev/null | awk '{print $3}')"
  echo "ap0_ssid        : $(iw dev ap0 info 2>/dev/null | awk '/ssid/{print $2}')"
  echo "throttled_start : $(vc get_throttled)"
  echo "temp_start      : $(vc measure_temp)"
} > "$SESSION/manifest.txt"

# --- health sampler -------------------------------------------------------------------------
echo "uptime_s,throttled,temp_c,arm_hz,load1,ap,ap_clients,sats_seen,sats_locked,gps_solution,es_total,uat_total" > "$HEALTH"
(
  while :; do
    printf '%s,%s,%s,%s,%s,%s,%s,%s\n' \
      "$(cut -d' ' -f1 /proc/uptime)" \
      "$(vc get_throttled)" \
      "$(t=$(vc measure_temp); case "$t" in *[0-9]*) printf '%s' "$t" | tr -dc '0-9.' ;; *) printf '?' ;; esac)" \
      "$(vc measure_clock arm)" \
      "$(cut -d' ' -f1 /proc/loadavg)" \
      "$(ap_state)" \
      "$(ap_clients)" \
      "$(stratux_sample)" >> "$HEALTH"
    sleep "$INTERVAL"
  done
) &
SAMPLER=$!
# Stop the sampler however this exits — a stray background loop would keep writing to the SD card
# for the rest of the flight.
trap 'kill "$SAMPLER" 2>/dev/null || true' EXIT INT TERM

echo "==> Recording ${DURATION}s from $HOST:$PORT"
echo "    into $SESSION"
RECORD_STATUS=0
"$REPLAY" record "$RECORDING" --host "$HOST" --port "$PORT" --duration "$DURATION" \
  > "$SESSION/record.log" 2>&1 || RECORD_STATUS=$?

kill "$SAMPLER" 2>/dev/null || true
wait "$SAMPLER" 2>/dev/null || true

# --- verdict ----------------------------------------------------------------------------------
# The throttled word is sticky: the low nibble is "right now", bits 16-19 are "has happened since
# boot". Either being set means the numbers in this session cannot be compared against a healthy
# board, so say so loudly rather than leaving it in a CSV nobody opens.
# Count the samples that were actually *measured*, not just taken. If vcgencmd is missing or
# broken every field is "?", and filtering those out leaves no throttle values — which looks
# exactly like a clean run. Reporting that as "clean" would be the script confidently vouching
# for a board it never read, which is worse than not checking at all.
SAMPLES="$(awk -F, 'NR>1' "$HEALTH" | wc -l)"
MEASURED="$(awk -F, 'NR>1 && $2 ~ /^0x/' "$HEALTH" | wc -l)"
WORST="$(awk -F, 'NR>1 && $2 ~ /^0x/ {print $2}' "$HEALTH" | sort -u | grep -v '^0x0$' | head -1 || true)"
TEMP_SAMPLES="$(awk -F, 'NR>1 && $3 ~ /^[0-9]/' "$HEALTH" | wc -l)"
if [[ "$TEMP_SAMPLES" -gt 0 ]]; then
  PEAK_TEMP="$(awk -F, 'NR>1 && $3 ~ /^[0-9]/ {if ($3+0 > m) m = $3+0} END {printf "%.1f C", m}' "$HEALTH")"
else
  PEAK_TEMP="unknown"
fi
FRAMES="$(wc -l < "$RECORDING" 2>/dev/null || echo 0)"
SIZE="$(du -h "$RECORDING" 2>/dev/null | cut -f1 || echo '?')"

{
  echo "session   : $STAMP"
  echo "recording : $FRAMES frames, $SIZE"
  # 143 is SIGTERM and 130 SIGINT: someone stopped the capture, or the Pi was shut down while it
  # ran — both entirely expected in the field, and neither is a failure. Calling them one would
  # send you hunting a problem that is not there while nine hundred good frames sit next to the
  # message. The final line of an interrupted recording is usually torn; the reader skips it and
  # keeps the rest, which is what record.rs is written for.
  case "$RECORD_STATUS" in
    0)        echo "record    : ok" ;;
    143|130)  echo "record    : stopped early (signal) — the recording is usable, its last line may be torn" ;;
    *)        echo "record    : FAILED (exit $RECORD_STATUS) — see record.log" ;;
  esac
  echo "peak temp : $PEAK_TEMP"
  if [[ "$PERSISTENCE" != PERSISTENT ]]; then
    echo "storage   : $PERSISTENCE  <-- THIS RECORDING MAY NOT SURVIVE A POWER CYCLE"
  fi
  if [[ "$MEASURED" -eq 0 ]]; then
    echo "throttled : UNKNOWN  <-- BOARD HEALTH WAS NOT MEASURED"
    echo
    echo "  vcgencmd returned nothing for all $SAMPLES health samples, so this run cannot be"
    echo "  vouched for either way. It is NOT evidence of a clean run. On the"
    echo "  Pi, check that vcgencmd exists and that the user running the capture is in the video"
    echo "  group (the capture service runs as root, which is why it normally works)."
  elif [[ -n "$WORST" ]]; then
    echo "throttled : $WORST  <-- NOT A CLEAN RUN"
    echo
    echo "  The board throttled during this capture, so any timing taken from it is not"
    echo "  comparable with a healthy board. The data itself is still fine to replay; the"
    echo "  performance numbers are not. Check the supply and capture again."
    echo "    bit 0  under-voltage now        bit 16  under-voltage has occurred"
    echo "    bit 1  arm frequency capped     bit 17  arm capping has occurred"
    echo "    bit 2  currently throttled      bit 18  throttling has occurred"
    echo "    bit 3  soft temperature limit   bit 19  soft limit has occurred"
  else
    echo "throttled : 0x0 across $MEASURED samples  (clean run)"
  fi
  # --- the three questions the last trip could not answer -------------------------------------
  AP_UP="$(awk -F, 'NR>1 && $6=="up"' "$HEALTH" | wc -l)"
  AP_PEAK="$(awk -F, 'NR>1 && $7 ~ /^[0-9]+$/ {if ($7+0>m) m=$7+0} END {print m+0}' "$HEALTH")"
  SATS_PEAK="$(awk -F, 'NR>1 && $8 ~ /^[0-9]+$/ {if ($8+0>m) m=$8+0} END {print m+0}' "$HEALTH")"
  LOCK_PEAK="$(awk -F, 'NR>1 && $9 ~ /^[0-9]+$/ {if ($9+0>m) m=$9+0} END {print m+0}' "$HEALTH")"
  ES_LAST="$(awk -F, 'NR>1 && $11 ~ /^[0-9]+$/ {v=$11} END {print v+0}' "$HEALTH")"
  UAT_LAST="$(awk -F, 'NR>1 && $12 ~ /^[0-9]+$/ {v=$12} END {print v+0}' "$HEALTH")"

  # Same trap as the throttle verdict: "no satellites found" and "never asked" look identical in
  # the CSV, and only one of them justifies telling someone their antenna cannot see sky. Count
  # what was actually measured before drawing any conclusion from it.
  AP_MEASURED="$(awk -F, 'NR>1 && $6!="?" && $6!="absent"' "$HEALTH" | wc -l)"
  GPS_MEASURED="$(awk -F, 'NR>1 && $8 ~ /^[0-9]+$/' "$HEALTH" | wc -l)"

  if [[ "$AP_MEASURED" -eq 0 ]]; then
    echo "access pt : no ap0 interface present — the AP was not running at all"
  else
    echo "access pt : up for $AP_UP of $AP_MEASURED samples, peak $AP_PEAK client(s)"
  fi

  if [[ "$GPS_MEASURED" -eq 0 ]]; then
    echo "gps       : NOT MEASURED — Stratux did not answer, so nothing here says anything"
    echo "            about the GPS. Do not read this as a bad antenna."
  else
    echo "gps       : peak $SATS_PEAK satellites seen, $LOCK_PEAK locked ($GPS_MEASURED samples)"
  fi
  if [[ "$GPS_MEASURED" -gt 0 && "$LOCK_PEAK" -eq 0 && "$SATS_PEAK" -le 4 ]]; then
    echo "            ^ a u-blox with a clear view of the sky sees 10-20 within a couple of"
    echo "              minutes. Under 5 means the antenna cannot see sky — check it is flat,"
    echo "              face up, and with nothing above it. This is not a receiver fault."
  fi
  if [[ "$GPS_MEASURED" -eq 0 ]]; then
    echo "radios    : not measured (Stratux did not answer)"
  else
    echo "radios    : $ES_LAST ES (1090) messages, $UAT_LAST UAT (978) messages"
  fi
  if [[ "$GPS_MEASURED" -gt 0 && "$UAT_LAST" -eq 0 && "$ES_LAST" -gt 0 ]]; then
    echo "            ^ 1090 is working. Zero UAT is normal on the ground: FIS-B ground stations"
    echo "              are line-of-sight and aimed at aircraft, so weather often needs altitude."
  fi

  if [[ "$FRAMES" -eq 0 ]]; then
    echo
    echo "  NOTHING WAS RECORDED. Stratux was probably not up, or not listening on $HOST:$PORT."
  fi
} | tee "$SUMMARY"

# --- prune ------------------------------------------------------------------------------------
# Keep the newest few. An unbounded capture directory on the SD card is a slow way to fill a
# filesystem that also holds the Stratux settings.
mapfile -t OLD < <(find "$OUT_DIR" -mindepth 1 -maxdepth 1 -type d -printf '%T@ %p\n' \
  | sort -rn | tail -n "+$((KEEP + 1))" | cut -d' ' -f2-)
for dir in "${OLD[@]:-}"; do
  [[ -n "$dir" && "$dir" == "$OUT_DIR"/* ]] || continue
  echo "==> Pruning old capture $(basename "$dir")"
  rm -rf -- "$dir"
done

echo
echo "==> Done. Collect it from the dev machine with:"
echo "    rsync -av pi@<pi>:$OUT_DIR/ ./captures/"
# Signals are a normal way for this to end, so do not propagate them as a unit failure.
case "$RECORD_STATUS" in
  143|130) exit 0 ;;
  *)       exit "$RECORD_STATUS" ;;
esac
