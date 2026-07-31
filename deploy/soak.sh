#!/usr/bin/env bash
#
# Thermal and CPU headroom soak. Run this ON THE PI.
#
#   sudo ./soak.sh                    # 15 minutes with the display running
#   sudo ./soak.sh --minutes 30
#   sudo ./soak.sh --compare          # the experiment that actually matters (see below)
#
# ---------------------------------------------------------------------------------------------
# WHAT --compare MEASURES
#
# The concern is not that the display is slow. It is that the display steals CPU from dump1090 and
# dump978, and a dropped ADS-B message is worse than a dropped frame. That failure is invisible from
# inside the display: traffic just looks a bit thin.
#
# So --compare runs the same duration twice, once with the display running and once with it stopped,
# and reports the difference in the radios' own message counters. If the rates are materially lower
# with the display up, the display is costing you traffic, and CPU pinning is worth turning on.
# If they are the same, pinning would be pre-optimisation and should be left alone.
# ---------------------------------------------------------------------------------------------
set -euo pipefail

MINUTES=15
COMPARE=0
INTERVAL=5

while [[ $# -gt 0 ]]; do
  case "$1" in
    --minutes) MINUTES="$2"; shift 2 ;;
    --interval) INTERVAL="$2"; shift 2 ;;
    --compare) COMPARE=1; shift ;;
    -h|--help) sed -n '2,22p' "$0"; exit 0 ;;
    *) echo "unrecognised option $1" >&2; exit 1 ;;
  esac
done

have() { command -v "$1" >/dev/null 2>&1; }
have vcgencmd || echo "WARNING: vcgencmd missing; no temperature or throttling data" >&2

# Extract one numeric field from Stratux's /getStatus by name.
#
# By name, not by position: UAT_messages_last_minute appears BEFORE ES_messages_last_minute in the
# struct, so grepping for both and reading them in order silently swaps them. A soak report that
# blames the wrong radio is worse than no report.
# Always succeeds, emitting 0 when the field is absent. That matters: under `set -euo pipefail`
# an assignment from a failing pipeline aborts the script, so a Stratux that is simply not running
# would kill a 30-minute soak instead of being reported as "counters unavailable".
status_field() {
  local field="$1" json="$2" value
  value=$(printf '%s' "$json" | grep -oE "\"${field}\":[0-9]+" | grep -oE '[0-9]+$' | head -1) || true
  printf '%s' "${value:-0}"
}

stratux_status() {
  curl -s --max-time 3 http://localhost/getStatus 2>/dev/null || true
}

# Total CPU percentage across every process with this name. awk always succeeds, so this is safe
# under `set -e` even when the process is not running.
cpu_of() {
  ps -eo pcpu,comm --no-headers 2>/dev/null \
    | awk -v want="$1" '$2 == want {sum += $1} END {printf "%.1f", sum+0}'
}

# Run one sampling window. $1 is a label used in the output.
sample_window() {
  local label="$1"
  local samples=$(( MINUTES * 60 / INTERVAL ))
  local max_temp=0 sum_temp=0 count=0 throttle_events=0
  local es_total=0 uat_total=0 rate_samples=0
  local log="/tmp/soak-${label}.csv"

  echo "elapsed_s,temp_c,throttled,load1,cpu_avionics,cpu_dump1090,cpu_dump978,es_per_min,uat_per_min" > "$log"
  echo
  echo "--- $label: $MINUTES min, sampling every ${INTERVAL}s ---"
  printf '%8s %7s %10s %6s %7s %7s %7s %9s %9s\n' \
    ELAPSED TEMP THROTTLED LOAD AVIONICS DUMP1090 DUMP978 ES/MIN UAT/MIN

  local start=$SECONDS
  for (( i = 0; i < samples; i++ )); do
    local elapsed=$(( SECONDS - start ))

    local temp="0"
    if have vcgencmd; then
      temp=$(vcgencmd measure_temp | grep -oE '[0-9.]+' | head -1)
    fi
    local throttled="n/a"
    if have vcgencmd; then
      throttled=$(vcgencmd get_throttled | cut -d= -f2)
      # Any non-zero bit means the firmware has capped clocks or flagged undervoltage at some point.
      [[ "$throttled" != "0x0" ]] && throttle_events=$(( throttle_events + 1 ))
    fi
    local load1
    load1=$(cut -d' ' -f1 /proc/loadavg)

    local cpu_av cpu_1090 cpu_978
    cpu_av=$(cpu_of avionics)
    cpu_1090=$(cpu_of dump1090)
    cpu_978=$(cpu_of dump978)

    local status es uat
    status=$(stratux_status)
    es=$(status_field ES_messages_last_minute "$status")
    uat=$(status_field UAT_messages_last_minute "$status")
    es=${es:-0}; uat=${uat:-0}
    if (( es > 0 || uat > 0 )); then
      es_total=$(( es_total + es ))
      uat_total=$(( uat_total + uat ))
      rate_samples=$(( rate_samples + 1 ))
    fi

    printf '%8s %7s %10s %6s %7s %7s %7s %9s %9s\n' \
      "$elapsed" "$temp" "$throttled" "$load1" "$cpu_av" "$cpu_1090" "$cpu_978" "$es" "$uat"
    echo "$elapsed,$temp,$throttled,$load1,$cpu_av,$cpu_1090,$cpu_978,$es,$uat" >> "$log"

    # Track temperature in tenths using integer arithmetic, to avoid depending on bc.
    # Normalised to exactly one decimal place first: vcgencmd gives one, but a firmware that gave
    # two would silently be read as hundredths and every threshold would be off by 10x.
    local whole=${temp%%.*} frac=${temp#*.}
    [[ "$frac" == "$temp" ]] && frac=0
    frac=${frac:0:1}
    local tenths=$(( ${whole:-0} * 10 + ${frac:-0} ))
    (( tenths > max_temp )) && max_temp=$tenths
    sum_temp=$(( sum_temp + tenths ))
    count=$(( count + 1 ))

    sleep "$INTERVAL"
  done

  local mean_temp=$(( count > 0 ? sum_temp / count : 0 ))
  echo
  echo "  $label summary:"
  printf '    peak temp      : %d.%d C\n' $(( max_temp / 10 )) $(( max_temp % 10 ))
  printf '    mean temp      : %d.%d C\n' $(( mean_temp / 10 )) $(( mean_temp % 10 ))
  echo "    throttle flags : $throttle_events of $count samples non-zero"
  if (( rate_samples > 0 )); then
    echo "    mean ES/min    : $(( es_total / rate_samples ))"
    echo "    mean UAT/min   : $(( uat_total / rate_samples ))"
    # Exported for --compare.
    LAST_ES=$(( es_total / rate_samples ))
    LAST_UAT=$(( uat_total / rate_samples ))
  else
    echo "    radio counters : unavailable (is Stratux running?)"
    LAST_ES=0
    LAST_UAT=0
  fi
  echo "    raw samples    : $log"

  # The Pi 3 caps clocks at 80 C. Reaching it in a soak means it will certainly reach it in a
  # sun-facing cockpit, where the display and the radios both then slow down.
  if (( max_temp >= 800 )); then
    echo "    *** THERMAL LIMIT REACHED. Add airflow or a heatsink before flying this. ***"
  elif (( max_temp >= 750 )); then
    echo "    *** Within 5 C of the throttle point. Marginal for a warm cockpit. ***"
  fi
}

echo "=== avionics soak test ==="
echo "  model : $(tr -d '\0' < /proc/device-tree/model 2>/dev/null || echo unknown)"
echo "  cores : $(nproc)"

if (( COMPARE )); then
  if [[ $EUID -ne 0 ]]; then
    echo "!!! --compare needs root to stop and start the service (try: sudo $0 --compare)" >&2
    exit 1
  fi

  echo
  echo "Running the display, then stopping it, and comparing what the radios heard."
  systemctl start avionics 2>/dev/null || true
  sleep 10
  sample_window with-display
  with_es=$LAST_ES; with_uat=$LAST_UAT

  echo
  echo "--- stopping the display ---"
  systemctl stop avionics
  sleep 10
  sample_window without-display
  without_es=$LAST_ES; without_uat=$LAST_UAT

  echo
  echo "--- restarting the display ---"
  systemctl start avionics

  echo
  echo "=== verdict ==="
  printf '  ES/min  : %6d with display, %6d without\n' "$with_es" "$without_es"
  printf '  UAT/min : %6d with display, %6d without\n' "$with_uat" "$without_uat"
  echo
  verdict_ok=1
  for pair in "ES:$with_es:$without_es" "UAT:$with_uat:$without_uat"; do
    IFS=: read -r name with without <<<"$pair"
    if (( without > 0 )); then
      # Percentage drop, integer arithmetic.
      drop=$(( (without - with) * 100 / without ))
      if (( drop > 5 )); then
        echo "  $name is down ${drop}% with the display running."
        verdict_ok=0
      else
        echo "  $name is within noise (${drop}% difference)."
      fi
    fi
  done
  if (( verdict_ok )); then
    echo
    echo "  The display is not costing you traffic. Leave CPU pinning off — it would be"
    echo "  pre-optimisation, and hard-pinning can make things worse on a 4-core part."
  else
    echo
    echo "  The display IS costing you traffic. Turn on pinning:"
    echo "    1. Uncomment CPUAffinity=3 in /etc/systemd/system/avionics.service"
    echo "    2. sudo install -D -m0644 deploy/systemd/stratux-cpu-affinity.conf \\"
    echo "         /etc/systemd/system/stratux.service.d/cpu-affinity.conf"
    echo "    3. sudo systemctl daemon-reload && sudo systemctl restart stratux avionics"
    echo "    4. Re-run this test to confirm it actually helped."
  fi
else
  sample_window soak
fi
