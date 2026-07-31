#!/usr/bin/env bash
#
# Read-only root filesystem, for surviving power loss.
#
#   sudo ./overlay.sh status
#   sudo ./overlay.sh enable
#   sudo ./overlay.sh disable
#
# ---------------------------------------------------------------------------------------------
# WHY, AND WHAT IT COSTS
#
# In an aircraft the power gets pulled, not shut down. An SD card written mid-power-loss can corrupt
# in ways ext4's journal does not cover, and a display that will not boot is the worst possible
# failure. An overlay filesystem makes the root partition read-only and sends all writes to a RAM
# layer that is discarded on reboot, which removes the failure mode entirely.
#
# The cost is that **nothing persists**, including Stratux settings changed through its web UI.
#
# The project plan called for bind-mounting a small writable partition over /etc/stratux.conf and
# /var/log/stratux to get persistence back. This script does NOT do that, for two reasons:
#
#   1. It requires repartitioning the card. Automating destructive partition edits on the user's
#      only boot medium is not a good trade against the problem it solves.
#   2. The alternatives that avoid repartitioning (a loopback image on the FAT boot partition) put
#      the one file we care about behind a filesystem with no journal — which is the very thing the
#      overlay exists to protect against. It would look like persistence while being less safe.
#
# So the supported posture is: **configure Stratux once with the overlay off, then enable it.**
# To change settings later, disable the overlay, change them, re-enable. That is a deliberate,
# infrequent, on-the-ground operation, which is what configuration should be.
#
# If you genuinely need live settings persistence in the air, the upgrade path is a real second
# partition — see "Persistent partition" at the end of this file for the manual procedure.
# ---------------------------------------------------------------------------------------------
set -euo pipefail

ACTION="${1:-status}"

if [[ "$ACTION" != "status" && $EUID -ne 0 ]]; then
  echo "!!! Must run as root (try: sudo $0 $ACTION)" >&2
  exit 1
fi

RASPI_CONFIG=/usr/bin/raspi-config

# raspi-config's non-interactive entry points have been renamed across releases, so discover what
# this image actually provides rather than guessing and silently doing nothing.
find_nonint() {
  local candidate
  for candidate in "$@"; do
    if [[ -f "$RASPI_CONFIG" ]] && grep -qE "^[[:space:]]*${candidate}\(\)" "$RASPI_CONFIG"; then
      echo "$candidate"
      return 0
    fi
  done
  return 1
}

overlay_active() {
  # The overlay presents the root filesystem with type "overlay".
  findmnt -no FSTYPE / | grep -q '^overlay$'
}

boot_writable() {
  findmnt -no OPTIONS /boot/firmware 2>/dev/null | grep -qv '\bro\b'
}

case "$ACTION" in
  status)
    echo "=== filesystem status ==="
    printf '  root        : %s (%s)\n' \
      "$(findmnt -no FSTYPE /)" \
      "$(overlay_active && echo 'READ-ONLY via overlay' || echo 'writable')"
    if [[ -d /boot/firmware ]]; then
      printf '  boot        : %s\n' "$(findmnt -no OPTIONS /boot/firmware 2>/dev/null | cut -d, -f1)"
    fi
    printf '  journal     : %s\n' \
      "$(grep -h '^Storage' /etc/systemd/journald.conf.d/*.conf 2>/dev/null | tail -1 || echo 'default (persistent)')"
    echo
    if overlay_active; then
      echo "  The root filesystem is protected. Changes made now — including Stratux settings"
      echo "  edited in its web UI — will be LOST on the next reboot."
    else
      echo "  The root filesystem is writable. A power cut can corrupt it."
      echo "  Configure Stratux the way you want it, then: sudo $0 enable"
    fi
    ;;

  enable)
    if overlay_active; then
      echo "Already enabled. Nothing to do."
      exit 0
    fi

    echo "=== enabling the read-only root filesystem ==="
    echo
    echo "Before continuing, confirm Stratux is configured the way you want it:"
    echo "  - region, radio assignment, and any settings changed in the web UI"
    echo "  - current settings: /etc/stratux.conf"
    echo
    echo "After this, those settings are frozen. Changing them means running"
    echo "'$0 disable', changing them, and re-enabling."
    echo
    read -r -p "Continue? [y/N] " reply
    [[ "$reply" == [yY] ]] || { echo "Aborted."; exit 1; }

    # Record what we froze, so powercut-check.sh can tell corruption from an expected change.
    if [[ -f /etc/stratux.conf ]]; then
      mount -o remount,rw /boot/firmware 2>/dev/null || true
      sha256sum /etc/stratux.conf > /boot/firmware/avionics-stratux-conf.sha256
      sync
      echo "    recorded a baseline checksum of /etc/stratux.conf"
    fi

    if fn=$(find_nonint enable_overlayfs do_overlayfs); then
      echo "    using raspi-config nonint $fn"
      if [[ "$fn" == "do_overlayfs" ]]; then
        raspi-config nonint "$fn" 0
      else
        raspi-config nonint "$fn"
      fi
    else
      cat >&2 <<'EOF'
!!! Could not find a raspi-config non-interactive entry point for the overlay.

    Do it interactively instead:
        sudo raspi-config
        -> Performance Options -> Overlay File System -> enable
        -> also set the boot partition to read-only when prompted

    Then re-run: sudo ./overlay.sh status
EOF
      exit 1
    fi

    echo
    echo "Enabled. Reboot for it to take effect:  sudo reboot"
    echo "After rebooting, verify with:           sudo $0 status"
    ;;

  disable)
    if ! overlay_active; then
      echo "Not enabled. Nothing to do."
      exit 0
    fi
    echo "=== disabling the read-only root filesystem ==="
    if fn=$(find_nonint disable_overlayfs do_overlayfs); then
      if [[ "$fn" == "do_overlayfs" ]]; then
        raspi-config nonint "$fn" 1
      else
        raspi-config nonint "$fn"
      fi
      echo "Disabled. Reboot, then make your changes, then re-enable."
      echo "  sudo reboot"
    else
      echo "!!! Use 'sudo raspi-config' -> Performance Options -> Overlay File System." >&2
      exit 1
    fi
    ;;

  *)
    echo "usage: $0 {status|enable|disable}" >&2
    exit 1
    ;;
esac

# ---------------------------------------------------------------------------------------------
# Persistent partition (manual, optional)
#
# Only worth doing if Stratux settings must survive a reboot while the overlay is active. Do this
# from another machine with the card in a reader, not on the running Pi, and take an image first.
#
#   1. Shrink the root partition and create a small ext4 partition (64 MB is ample) after it.
#   2. Label it:            sudo e2label /dev/sdX3 avionics-persist
#   3. On the Pi, add to /etc/fstab, before enabling the overlay:
#          LABEL=avionics-persist /mnt/persist ext4 defaults,noatime,sync 0 2
#   4. Move the config and bind it back:
#          sudo mkdir -p /mnt/persist
#          sudo mount /mnt/persist
#          sudo cp /etc/stratux.conf /mnt/persist/stratux.conf
#          echo '/mnt/persist/stratux.conf /etc/stratux.conf none bind 0 0' | sudo tee -a /etc/fstab
#   5. Reboot, confirm the bind mount, then enable the overlay.
#
# `sync` in the mount options is deliberate: it costs write throughput, which does not matter for a
# file written once in a while, and buys durability against exactly the power cut this is all for.
# ---------------------------------------------------------------------------------------------
