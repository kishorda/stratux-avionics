#!/usr/bin/env bash
#
# Trim boot time. Run this ON THE PI.
#
#   sudo ./boot-trim.sh --dry-run
#   sudo ./boot-trim.sh
#   sudo ./boot-trim.sh --measure     # just measure, change nothing
#
# Conservative by design. The unit keeps its WiFi access point and web UI, so anything the AP needs
# stays. Only services with no role on a single-purpose display are touched, and every one is listed
# with how to put it back.
set -euo pipefail

DRY_RUN=0
MEASURE_ONLY=0
case "${1:-}" in
  --dry-run) DRY_RUN=1 ;;
  --measure) MEASURE_ONLY=1 ;;
  "") ;;
  *) echo "usage: $0 [--dry-run|--measure]" >&2; exit 1 ;;
esac

measure() {
  echo "=== boot time ==="
  systemd-analyze 2>/dev/null || echo "  (systemd-analyze unavailable)"
  echo
  echo "--- slowest units ---"
  systemd-analyze blame 2>/dev/null | head -15 || true
  echo
  echo "--- critical chain ---"
  systemd-analyze critical-chain 2>/dev/null | head -20 || true
  echo
  echo "Note: what matters for the pilot is time to *first frame*, not to multi-user.target."
  echo "Measure that directly:"
  echo "    journalctl -b -u avionics --output=short-monotonic | head -5"
  echo "and compare the timestamp of the first 'rendering to the panel' line against boot."
}

if (( MEASURE_ONLY )); then
  measure
  exit 0
fi

echo "=== before ==="
measure

# Services with nothing to do on this build. Each entry is "unit:why".
#
# Deliberately NOT in this list, because the AP and web UI were kept:
#   hostapd, dnsmasq, dhcpcd/NetworkManager, wpa_supplicant, stratux, ssh
CANDIDATES=(
  "triggerhappy:global hotkey daemon; there is no keyboard"
  "bluetooth:no Bluetooth peripherals, and the Pi 3 shares the antenna path with WiFi"
  "hciuart:attaches the Bluetooth controller to the UART; not needed with Bluetooth off"
  "ModemManager:no cellular modem"
  "avahi-daemon:mDNS discovery; the AP hands out addresses directly"
  "apt-daily.timer:unattended apt work has no place on a flying appliance"
  "apt-daily-upgrade.timer:as above"
  "man-db.timer:rebuilds man pages; pure SD card wear"
  "e2scrub_all.timer:LVM filesystem scrub; not applicable"
  "raspi-config.service:first-boot configuration helper"
  "keyboard-setup.service:no keyboard"
)

echo
echo "=== disabling services with no role on a display appliance ==="
DISABLED=()
for entry in "${CANDIDATES[@]}"; do
  unit="${entry%%:*}"
  why="${entry#*:}"

  # Only touch units that actually exist and are actually enabled.
  if ! systemctl list-unit-files "$unit" &>/dev/null || \
     ! systemctl list-unit-files "$unit" 2>/dev/null | grep -q "$unit"; then
    continue
  fi
  state=$(systemctl is-enabled "$unit" 2>/dev/null || echo "missing")
  case "$state" in
    enabled|enabled-runtime)
      echo "  $unit — $why"
      if (( DRY_RUN )); then
        echo "      would run: systemctl disable --now $unit"
      else
        systemctl disable --now "$unit" 2>/dev/null || echo "      (failed; skipping)"
      fi
      DISABLED+=("$unit")
      ;;
    *)
      echo "  $unit — already $state, skipping"
      ;;
  esac
done

echo
echo "=== kernel command line ==="
CMDLINE=/boot/firmware/cmdline.txt
[[ -f "$CMDLINE" ]] || CMDLINE=/boot/cmdline.txt
if [[ -f "$CMDLINE" ]]; then
  echo "  current: $(cat "$CMDLINE")"
  MISSING=()
  # consoleblank=0            never blank the console; belt and braces alongside KD_GRAPHICS
  # vt.global_cursor_default=0 no blinking cursor before our first frame
  # logo.nologo               no raspberry logo
  # quiet                     less console spew, so less to render before we take over
  for opt in consoleblank=0 vt.global_cursor_default=0 logo.nologo quiet; do
    grep -q -- "$opt" "$CMDLINE" || MISSING+=("$opt")
  done
  if [[ ${#MISSING[@]} -eq 0 ]]; then
    echo "  all recommended options already present"
  else
    echo "  missing: ${MISSING[*]}"
    if (( DRY_RUN )); then
      echo "      would append them to $CMDLINE (single line, space separated)"
    else
      cp "$CMDLINE" "$CMDLINE.avionics-backup"
      # cmdline.txt must remain a single line.
      printf '%s %s\n' "$(tr -d '\n' < "$CMDLINE.avionics-backup")" "${MISSING[*]}" > "$CMDLINE"
      echo "      appended (backup at $CMDLINE.avionics-backup)"
    fi
  fi
else
  echo "  !!! cmdline.txt not found; skipping"
fi

echo
echo "=== summary ==="
if [[ ${#DISABLED[@]} -eq 0 ]]; then
  echo "  no services changed"
else
  echo "  disabled: ${DISABLED[*]}"
  echo "  to undo:  sudo systemctl enable --now ${DISABLED[*]}"
fi
cat <<EOF

Also check deploy/config.txt.fragment for the firmware-side settings that matter to boot time:
  disable_splash=1, boot_delay=0

Reboot, then measure again:
    sudo reboot
    sudo ./boot-trim.sh --measure
EOF
