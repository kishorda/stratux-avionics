#!/usr/bin/env bash
#
# Install the display as a service on the Pi. Run this ON THE PI.
#
#   sudo ./install.sh              # install / upgrade
#   sudo ./install.sh --dry-run    # print what would change, touch nothing
#
# Idempotent and reversible. Everything it changes is listed at the end, with how to undo it.
#
# It deliberately does NOT enable the read-only root filesystem — that is a separate, deliberate
# step with real consequences for whether settings persist. See deploy/overlay.sh.
#
set -euo pipefail

PREFIX=/opt/avionics
DRY_RUN=0
[[ "${1:-}" == "--dry-run" ]] && DRY_RUN=1

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHANGES=()

run() {
  if (( DRY_RUN )); then
    echo "    would run: $*"
  else
    "$@"
  fi
}

note() { CHANGES+=("$1"); }
step() { printf '\n==> %s\n' "$1"; }

if (( DRY_RUN )); then
  echo "DRY RUN — nothing will be modified."
elif [[ $EUID -ne 0 ]]; then
  echo "!!! Must run as root (try: sudo $0)" >&2
  exit 1
fi

# --- sanity ------------------------------------------------------------------------------
step "Checking the target"
if ! grep -qi raspberry /proc/device-tree/model 2>/dev/null; then
  echo "!!! This does not look like a Raspberry Pi. Refusing to modify the system." >&2
  echo "    (install.sh is meant to be run on the Pi, not on the dev machine.)" >&2
  exit 1
fi
echo "    $(tr -d '\0' < /proc/device-tree/model)"
echo "    architecture: $(dpkg --print-architecture)"

BINARY="${AVIONICS_BINARY:-$SCRIPT_DIR/../target/aarch64-unknown-linux-gnu/release/avionics}"
if [[ ! -x "$BINARY" ]]; then
  # Also accept a binary sitting next to the script, which is how deploy.sh pushes it.
  for candidate in "$SCRIPT_DIR/avionics" /tmp/avionics; do
    [[ -x "$candidate" ]] && BINARY="$candidate" && break
  done
fi
if [[ ! -x "$BINARY" ]]; then
  echo "!!! No avionics binary found. Build and push it first:" >&2
  echo "    ./deploy/deploy.sh <user>@<host>   (from the dev machine)" >&2
  echo "    or set AVIONICS_BINARY=/path/to/avionics" >&2
  exit 1
fi
echo "    binary: $BINARY"

# --- font --------------------------------------------------------------------------------
step "Installing the font outside apt's reach"
FONT_SOURCE=""
for candidate in \
  /usr/share/fonts/truetype/dejavu/DejaVuSans.ttf \
  /usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf \
  /usr/share/fonts/truetype/noto/NotoSans-Regular.ttf
do
  [[ -f "$candidate" ]] && FONT_SOURCE="$candidate" && break
done

if [[ -z "$FONT_SOURCE" ]]; then
  echo "!!! No font found. Install one first: sudo apt install fonts-dejavu-core" >&2
  exit 1
fi
echo "    copying $FONT_SOURCE"
# Copied rather than symlinked on purpose: a symlink into /usr/share still breaks when the package
# is removed, which is the failure this is guarding against.
run install -d -m 0755 "$PREFIX/bin" "$PREFIX/assets"
run install -m 0644 "$FONT_SOURCE" "$PREFIX/assets/font.ttf"
note "installed $PREFIX/assets/font.ttf (delete $PREFIX to remove)"

# --- binary ------------------------------------------------------------------------------
step "Installing the binary"
run install -m 0755 "$BINARY" "$PREFIX/bin/avionics"
note "installed $PREFIX/bin/avionics"

# --- verify before wiring anything up ----------------------------------------------------
step "Verifying the install"
if (( DRY_RUN )); then
  echo "    would run: $PREFIX/bin/avionics --check"
else
  if AVIONICS_FONT="$PREFIX/assets/font.ttf" "$PREFIX/bin/avionics" --check; then
    echo "    check passed"
  else
    echo "!!! --check failed. Fix the above before enabling the service." >&2
    echo "    The binary and font are installed; nothing else has been changed." >&2
    exit 1
  fi
fi

# --- console ----------------------------------------------------------------------------
step "Freeing the console"
# The display puts tty1 into graphics mode; a login prompt on the same VT fights it for the screen.
if systemctl is-enabled getty@tty1.service &>/dev/null; then
  run systemctl disable --now getty@tty1.service
  note "disabled getty@tty1 (re-enable: systemctl enable --now getty@tty1)"
else
  echo "    getty@tty1 already disabled"
fi

# --- watchdog ---------------------------------------------------------------------------
step "Enabling the hardware watchdog"
# dtparam=watchdog=on exposes /dev/watchdog; systemd then pets it and the board resets if the
# kernel wedges. Without this, a hung kernel leaves a frozen picture in front of the pilot, which
# is worse than a blank one because it looks live.
if [[ -e /dev/watchdog ]]; then
  echo "    /dev/watchdog present"
else
  echo "    /dev/watchdog MISSING — add 'dtparam=watchdog=on' to /boot/firmware/config.txt"
  echo "    (see deploy/config.txt.fragment) and reboot."
fi

WATCHDOG_DIR=/etc/systemd/system.conf.d
if [[ ! -f "$WATCHDOG_DIR/watchdog.conf" ]]; then
  run install -d -m 0755 "$WATCHDOG_DIR"
  if (( DRY_RUN )); then
    echo "    would write $WATCHDOG_DIR/watchdog.conf"
  else
    cat > "$WATCHDOG_DIR/watchdog.conf" <<'EOF'
# Installed by stratux-avionics deploy/install.sh
[Manager]
# systemd pets /dev/watchdog at half this interval; the board resets if it stops.
RuntimeWatchdogSec=15
# Also arm the watchdog across a reboot, so a hang during shutdown still recovers.
RebootWatchdogSec=120
EOF
  fi
  note "wrote $WATCHDOG_DIR/watchdog.conf (delete to remove)"
else
  echo "    watchdog drop-in already present"
fi

# --- bound the logs ---------------------------------------------------------------------
step "Bounding journal size"
# Matters most once the root filesystem is read-only: the journal then lives in the RAM overlay, and
# an unbounded journal on a 1 GB board will eventually exhaust it. Capping here means the limit
# applies whether or not the overlay is enabled.
JOURNAL_DIR=/etc/systemd/journald.conf.d
if [[ ! -f "$JOURNAL_DIR/avionics.conf" ]]; then
  run install -d -m 0755 "$JOURNAL_DIR"
  if (( DRY_RUN )); then
    echo "    would write $JOURNAL_DIR/avionics.conf"
  else
    cat > "$JOURNAL_DIR/avionics.conf" <<'EOF'
# Installed by stratux-avionics deploy/install.sh
[Journal]
# Volatile: never write the journal to the SD card. Logs survive until reboot, which is long enough
# to diagnose a flight, and the card lasts much longer for it.
Storage=volatile
RuntimeMaxUse=32M
RuntimeMaxFileSize=8M
EOF
  fi
  note "wrote $JOURNAL_DIR/avionics.conf (delete to restore persistent logging)"
else
  echo "    journald drop-in already present"
fi

# --- service ----------------------------------------------------------------------------
step "Installing the service"
run install -m 0644 "$SCRIPT_DIR/systemd/avionics.service" /etc/systemd/system/avionics.service
run install -m 0644 "$SCRIPT_DIR/../README.md" "$PREFIX/README.md" 2>/dev/null || true
run systemctl daemon-reload
run systemctl enable avionics.service
note "installed and enabled avionics.service (undo: systemctl disable --now avionics)"

# --- done -------------------------------------------------------------------------------
step "Summary"
for change in "${CHANGES[@]}"; do
  echo "    - $change"
done

cat <<EOF

Not done, on purpose:
  - The read-only root filesystem is NOT enabled. Configure Stratux the way you want it first,
    then run: sudo ./deploy/overlay.sh enable
  - Boot time has not been trimmed. Run: sudo ./deploy/boot-trim.sh
  - CPU pinning is commented out in avionics.service. Measure first:
        sudo ./deploy/soak.sh --compare

Start it now with:
    sudo systemctl start avionics
    journalctl -u avionics -f
EOF
