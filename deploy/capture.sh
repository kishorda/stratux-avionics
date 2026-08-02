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
vc() {
  local out
  out="$(vcgencmd "$@" 2>/dev/null)" || { printf '?'; return 0; }
  [[ -n "$out" ]] || { printf '?'; return 0; }
  printf '%s' "${out#*=}"
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
  echo "display_running : $(systemctl is-active avionics 2>/dev/null || echo unknown)"
  echo "stratux_running : $(systemctl is-active stratux 2>/dev/null || echo unknown)"
  echo "throttled_start : $(vc get_throttled)"
  echo "temp_start      : $(vc measure_temp)"
} > "$SESSION/manifest.txt"

# --- health sampler -------------------------------------------------------------------------
echo "uptime_s,throttled,temp_c,arm_hz,load1" > "$HEALTH"
(
  while :; do
    printf '%s,%s,%s,%s,%s\n' \
      "$(cut -d' ' -f1 /proc/uptime)" \
      "$(vc get_throttled)" \
      "$(vc measure_temp | tr -dc '0-9.')" \
      "$(vc measure_clock arm)" \
      "$(cut -d' ' -f1 /proc/loadavg)" >> "$HEALTH"
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
  echo "record    : $([[ $RECORD_STATUS -eq 0 ]] && echo ok || echo "FAILED (exit $RECORD_STATUS) — see record.log")"
  echo "peak temp : $PEAK_TEMP"
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
exit "$RECORD_STATUS"
