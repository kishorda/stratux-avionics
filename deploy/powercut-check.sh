#!/usr/bin/env bash
#
# Power-loss integrity check. Run this ON THE PI, after every hard power cut.
#
#   sudo ./powercut-check.sh baseline   # once, when everything is known good
#   sudo ./powercut-check.sh            # after each power cut
#   sudo ./powercut-check.sh report     # cumulative summary
#
# The plan's exit criterion is 20 hard power cuts with no corruption and no lost Stratux settings.
# The cuts themselves are physical — pull the plug, do not shut down — but everything around them is
# automated here so the result is a number rather than an impression.
#
# State lives on the boot partition rather than the root filesystem, because the whole point is that
# the root filesystem may be read-only (or damaged) when this runs.
set -euo pipefail

STATE_DIR=/boot/firmware
[[ -d "$STATE_DIR" ]] || STATE_DIR=/boot
COUNTER="$STATE_DIR/avionics-powercut-count"
LOG="$STATE_DIR/avionics-powercut.log"
CONF_BASELINE="$STATE_DIR/avionics-stratux-conf.sha256"
ACTION="${1:-check}"

if [[ $EUID -ne 0 ]]; then
  echo "!!! Must run as root (try: sudo $0 $ACTION)" >&2
  exit 1
fi

writable_state() {
  # The boot partition may be mounted read-only alongside the overlay; remount just long enough.
  mount -o remount,rw "$STATE_DIR" 2>/dev/null || true
}
readonly_state() {
  sync
  mount -o remount,ro "$STATE_DIR" 2>/dev/null || true
}

case "$ACTION" in
  baseline)
    writable_state
    echo "0" > "$COUNTER"
    : > "$LOG"
    if [[ -f /etc/stratux.conf ]]; then
      sha256sum /etc/stratux.conf > "$CONF_BASELINE"
      echo "Recorded a baseline checksum of /etc/stratux.conf."
    else
      echo "WARNING: /etc/stratux.conf not found; settings persistence cannot be checked." >&2
    fi
    readonly_state
    echo "Baseline set. Now: pull the power, boot, and run '$0' after each cut."
    ;;

  report)
    count=$(cat "$COUNTER" 2>/dev/null || echo 0)
    fails=0
    echo "=== power-cut summary ==="
    echo "  cuts recorded : $count"
    if [[ -f "$LOG" ]]; then
      fails=$(grep -c FAIL "$LOG" 2>/dev/null) || fails=0
      echo "  failures      : $fails"
      echo
      tail -25 "$LOG"
    fi
    echo
    if [[ "${count:-0}" -ge 20 && "${fails:-0}" -eq 0 ]]; then
      echo "  PASS: 20+ cuts with no failures. The M6 power-loss criterion is met."
    else
      echo "  Not yet conclusive: need 20 clean cuts, have $count with ${fails:-0} failures."
    fi
    ;;

  check)
    failures=()
    warnings=()

    echo "=== power-cut integrity check ==="

    # --- filesystem ---
    root_src=$(findmnt -no SOURCE / 2>/dev/null | sed 's/\[.*\]//') || true
    root_fs=$(findmnt -no FSTYPE / 2>/dev/null) || true
    echo "  root filesystem : $root_fs on $root_src"
    if [[ "$root_fs" == overlay ]]; then
      echo "                    (read-only overlay active — this is the protected configuration)"
    else
      warnings+=("root filesystem is writable; a cut can still corrupt it. See deploy/overlay.sh")
    fi

    # ext4 records whether it was cleanly unmounted. After a hard cut that flag is the most direct
    # evidence of damage there is.
    #
    # Every extraction below is `|| true`-guarded: under `set -euo pipefail` an assignment from a
    # pipeline whose grep matches nothing aborts the script, and a check that dies halfway through
    # reports neither pass nor fail.
    for device in $(lsblk -lnpo NAME,FSTYPE 2>/dev/null | awk '$2=="ext4" {print $1}'); do
      local_state=$(dumpe2fs -h "$device" 2>/dev/null | grep -E '^Filesystem state:' || true)
      state=$(printf '%s' "$local_state" | awk '{print $3}')
      local_mounts=$(dumpe2fs -h "$device" 2>/dev/null | grep -E '^Mount count:' || true)
      mount_count=$(printf '%s' "$local_mounts" | awk '{print $3}')
      echo "  $device : state=${state:-unknown} mounts=${mount_count:-?}"
      if [[ -n "$state" && "$state" != "clean" ]]; then
        failures+=("$device is not clean (state=$state) — filesystem damage from the cut")
      fi
    done

    # --- kernel complaints ---
    # Errors here are the early warning that a card is starting to fail, well before it stops booting.
    errors=$(journalctl -b -p err --no-pager 2>/dev/null | grep -ciE 'ext4|mmcblk|I/O error|corrupt') || true
    echo "  kernel fs/IO errors this boot : ${errors:-0}"
    if [[ "${errors:-0}" -gt 0 ]]; then
      failures+=("$errors filesystem or I/O errors in this boot's journal")
      journalctl -b -p err --no-pager 2>/dev/null | grep -iE 'ext4|mmcblk|I/O error|corrupt' | tail -5 | sed 's/^/      /'
    fi

    # --- Stratux settings ---
    if [[ -f "$CONF_BASELINE" && -f /etc/stratux.conf ]]; then
      if sha256sum -c --status "$CONF_BASELINE" 2>/dev/null; then
        echo "  stratux.conf : unchanged from baseline"
      else
        # With the overlay enabled this means real loss. Without it, it may just be an edit you made.
        if [[ "$root_fs" == overlay ]]; then
          failures+=("/etc/stratux.conf differs from baseline — settings were lost or corrupted")
        else
          warnings+=("/etc/stratux.conf differs from baseline (expected if you edited settings; re-run 'baseline' to reset)")
        fi
      fi
    else
      warnings+=("no stratux.conf baseline; run '$0 baseline' first")
    fi

    # --- services ---
    for unit in avionics stratux; do
      if systemctl is-active --quiet "$unit"; then
        echo "  $unit : active"
      else
        failures+=("$unit is not running after the cut")
        systemctl status "$unit" --no-pager -n 5 2>/dev/null | sed 's/^/      /' || true
      fi
    done

    # --- display ---
    if [[ -x /opt/avionics/bin/avionics ]]; then
      if AVIONICS_FONT=/opt/avionics/assets/font.ttf /opt/avionics/bin/avionics --check >/tmp/pc-check.txt 2>&1; then
        echo "  avionics --check : passed"
      else
        failures+=("avionics --check failed after the cut")
        sed 's/^/      /' /tmp/pc-check.txt
      fi
    fi

    # --- record ---
    writable_state
    count=$(( $(cat "$COUNTER" 2>/dev/null || echo 0) + 1 ))
    echo "$count" > "$COUNTER"
    verdict=$([[ ${#failures[@]} -eq 0 ]] && echo PASS || echo FAIL)
    printf '%s cut=%d %s failures=%d warnings=%d\n' \
      "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "$count" "$verdict" "${#failures[@]}" "${#warnings[@]}" >> "$LOG"
    readonly_state

    echo
    for w in "${warnings[@]}"; do echo "  WARN: $w"; done
    for f in "${failures[@]}"; do echo "  FAIL: $f"; done
    echo
    echo "  cut #$count : $verdict"
    if [[ "$verdict" == PASS ]]; then
      echo "  Run '$0 report' for the running total. 20 clean cuts meets the M6 criterion."
      exit 0
    else
      exit 1
    fi
    ;;

  *)
    echo "usage: $0 {baseline|check|report}" >&2
    exit 1
    ;;
esac
