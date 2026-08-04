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
# Bold first, matching crates/avionics-ui/src/font.rs. At the sizes this display uses a
# regular-weight stem is one pixel on a 133 PPI panel; bold draws two, which is what survives
# antialiasing and daylight. The unit pins AVIONICS_FONT at the copy installed here, so this list
# is what actually decides the face on the aircraft.
FONT_SOURCE=""
for candidate in \
  /usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf \
  /usr/share/fonts/truetype/liberation/LiberationSans-Bold.ttf \
  /usr/share/fonts/truetype/noto/NotoSans-Bold.ttf \
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

# --- airport and airspace data -----------------------------------------------------------
#
# Beside the binary, because that is the first place the display looks. Staging it to /tmp is not
# enough: after installation the binary runs from $PREFIX/bin and would find nothing there, and a
# missing chart is silent by design — the map layer simply does not draw. Which means getting this
# wrong produces a display that looks finished and quietly has no airports on it.
step "Installing the airport and airspace data"
CHART="${AVIONICS_CHART:-}"
if [[ -z "$CHART" ]]; then
  for candidate in \
    "$SCRIPT_DIR/conus.chart" \
    /tmp/conus.chart \
    "$SCRIPT_DIR/../crates/avionics-ui/data/conus.chart"
  do
    [[ -f "$candidate" ]] && CHART="$candidate" && break
  done
fi
if [[ -n "$CHART" ]]; then
  echo "    copying $CHART ($(du -h "$CHART" | cut -f1))"
  run install -m 0644 "$CHART" "$PREFIX/bin/conus.chart"
  note "installed $PREFIX/bin/conus.chart"
else
  # Not fatal, for the same reason it is not fatal at runtime: traffic is why the panel exists.
  echo "    no conus.chart found — the map layer will not draw."
  echo "    Build it with tools/chartdata and redeploy; see docs/airspace-and-airports.md."
fi

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

# --- unattended capture -------------------------------------------------------------------
# Installed but NOT enabled. This is the tool for taking the Pi somewhere with a sky view and
# leaving it, and it writes to the SD card — neither of which should start happening because
# somebody installed the display.
step "Installing the unattended capture (not enabled)"
REPLAY_BIN=""
for candidate in \
  "${REPLAY_BINARY:-}" \
  "$SCRIPT_DIR/replay" \
  /tmp/replay \
  "$SCRIPT_DIR/../target/aarch64-unknown-linux-gnu/release/replay"
do
  [[ -n "$candidate" && -x "$candidate" ]] && REPLAY_BIN="$candidate" && break
done

# Say plainly whether anything written here will still be here after a power cycle. The Stratux
# image boots through /sbin/init-overlay, so the root filesystem is behind a RAM overlay unless
# /overlay/disable exists — and captures are the one thing on this box whose entire value is
# surviving a walk back indoors.
ROOT_FS="$(findmnt -no FSTYPE / 2>/dev/null || echo unknown)"
if [[ "$ROOT_FS" == overlay ]]; then
  echo "    !!! root filesystem is a RAM OVERLAY — captures will NOT survive a power cycle."
  echo "        Enable persistent disk with: sudo touch /overlay/disable && sudo reboot"
elif grep -q 'init=/sbin/init-overlay' /proc/cmdline 2>/dev/null && [[ ! -e /overlay/disable ]]; then
  echo "    !!! the overlay is off now but WILL engage on the next boot (/overlay/disable missing)."
  echo "        The capture timer runs after boot, so captures would land in RAM."
  echo "        Enable persistent disk with: sudo touch /overlay/disable"
else
  echo "    persistent disk: yes (root is $ROOT_FS, writes survive a power cycle)"
fi

if [[ -z "$REPLAY_BIN" ]]; then
  echo "    no replay binary found — skipping. Push one with deploy.sh and re-run."
else
  echo "    binary: $REPLAY_BIN"
  run install -m 0755 "$REPLAY_BIN" "$PREFIX/bin/replay"
  run install -m 0755 "$SCRIPT_DIR/capture.sh" "$PREFIX/bin/capture.sh"
  run install -d -m 0755 /var/log/avionics-capture
  run install -m 0644 "$SCRIPT_DIR/systemd/avionics-capture.service" \
    /etc/systemd/system/avionics-capture.service
  run install -m 0644 "$SCRIPT_DIR/systemd/avionics-capture.timer" \
    /etc/systemd/system/avionics-capture.timer
  run systemctl daemon-reload
  note "installed $PREFIX/bin/replay, $PREFIX/bin/capture.sh and avionics-capture.{service,timer} — NOT enabled"
fi

# --- done -------------------------------------------------------------------------------
step "Summary"
for change in "${CHANGES[@]}"; do
  echo "    - $change"
done

cat <<EOF

Not done, on purpose:
  - The read-only root filesystem is NOT enabled. Configure Stratux the way you want it first,
    then run: sudo ./deploy/overlay.sh enable
    NOTE: captures write to /var/log/avionics-capture. Give that a writable carve-out, or
    take your captures before enabling the overlay.
  - Boot time has not been trimmed. Run: sudo ./deploy/boot-trim.sh
  - CPU pinning is commented out in avionics.service. Measure first:
        sudo ./deploy/soak.sh --compare
  - The capture timer is installed but NOT enabled. It writes to the SD card and is only
    wanted when you are deliberately taking the Pi somewhere with a sky view.

Start the display now with:
    sudo systemctl start avionics
    journalctl -u avionics -f

Take a capture outside:
    sudo systemctl enable --now avionics-capture.timer   # records 1 min after each boot
    # ... carry the Pi out on a battery, power it up, leave it, bring it back ...
    sudo systemctl disable avionics-capture.timer
    cat /var/log/avionics-capture/*/summary.txt          # check it was a clean run

  Or record once, right now, without the timer:
    sudo CAPTURE_DURATION=600 /opt/avionics/bin/capture.sh

  Then from the dev machine:
    rsync -av pi@<pi>:/var/log/avionics-capture/ ./captures/
EOF
