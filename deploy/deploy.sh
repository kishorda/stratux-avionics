#!/usr/bin/env bash
#
# Cross-build a binary and push it to the Pi, along with everything install.sh needs.
#
#   ./deploy/deploy.sh pi@stratux.local                 # builds avionics (the display)
#   BIN=gfx-spike ./deploy/deploy.sh pi@stratux.local   # builds the M1 rendering spike
#   TARGET=armv7-unknown-linux-gnueabihf ./deploy/deploy.sh pi@stratux.local
#
set -euo pipefail

HOST="${1:?usage: deploy.sh user@host}"
TARGET="${TARGET:-aarch64-unknown-linux-gnu}"
BIN="${BIN:-avionics}"
DEST="${DEST:-/tmp}"
# Where the deploy scripts land, so install.sh can find the systemd unit next to itself.
STAGE="${STAGE:-/tmp/avionics-deploy}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

case "$TARGET" in
  aarch64-*) TRIPLE=aarch64-linux-gnu ;;
  armv7-*)   TRIPLE=arm-linux-gnueabihf ;;
  *) echo "!!! Unsupported TARGET '$TARGET'." >&2; exit 1 ;;
esac

# Check for the versioned library, not just the linker symlink. A directory containing only a
# hand-made libgbm.so would satisfy the link step but record DT_NEEDED as "libgbm.so" instead
# of "libgbm.so.1", producing a binary that then needs libgbm-dev installed on the Pi.
if [[ ! -e "sysroot/usr/lib/$TRIPLE/libgbm.so.1" ]]; then
  echo "!!! sysroot/usr/lib/$TRIPLE/libgbm.so.1 is missing." >&2
  echo "    Run: ./deploy/sync-sysroot.sh --offline        (no Pi network needed)" >&2
  echo "     or: ./deploy/sync-sysroot.sh $HOST $TRIPLE" >&2
  exit 1
fi

echo "==> Building $BIN for $TARGET (kms only; the offscreen backend is a dev-machine thing)"
cargo build --release --target "$TARGET" -p "$BIN" --no-default-features --features kms

# A cross-link against the wrong glibc still produces a perfectly valid-looking ELF; it fails
# only when the Pi tries to exec it. Catch it here rather than over SSH.
"$ROOT/deploy/check-glibc.sh" --max "${GLIBC_MAX:-2.36}" "target/$TARGET/release/$BIN"

echo "==> Copying the binary to $HOST:$DEST/$BIN"
rsync -az --info=progress2 "target/$TARGET/release/$BIN" "$HOST:$DEST/$BIN"

# The recorder travels with the display. It is what capture.sh runs, and the whole reason for
# having it on the Pi is to take a session somewhere with a sky view and no network — which is
# exactly when you cannot go back and fetch a missing binary.
if [[ "$BIN" == "avionics" && "${WITH_REPLAY:-1}" == "1" ]]; then
  echo "==> Building replay for $TARGET"
  cargo build --release --target "$TARGET" -p replay
  "$ROOT/deploy/check-glibc.sh" --max "${GLIBC_MAX:-2.36}" "target/$TARGET/release/replay"
  echo "==> Copying replay to $HOST:$DEST/replay"
  rsync -az "target/$TARGET/release/replay" "$HOST:$DEST/replay"
fi

# The airport and airspace file travels beside the binary, which is the first place the display
# looks for it. Missing is not fatal — the map layer simply does not draw — so this is a warning
# rather than a failure, and WITH_CHART=0 skips it on a tight card.
CHART="$ROOT/crates/avionics-ui/data/conus.chart"
if [[ "$BIN" == "avionics" && "${WITH_CHART:-1}" == "1" ]]; then
  if [[ -f "$CHART" ]]; then
    echo "==> Copying the chart to $HOST:$DEST/conus.chart ($(du -h "$CHART" | cut -f1))"
    rsync -az --info=progress2 "$CHART" "$HOST:$DEST/conus.chart"
  else
    echo "!!! $CHART is missing; the panel will run without the airport and airspace layer" >&2
    echo "    Build it with tools/chartdata — see docs/airspace-and-airports.md" >&2
  fi
fi

# install.sh reads the systemd unit from alongside itself, so the whole directory has to travel,
# not just the binary. Without this the documented install flow fails at the last step.
echo "==> Copying the deploy scripts to $HOST:$STAGE"
# `debs/` and `keyrings/` are sysroot material for the *dev machine* — fetch-target-debs.sh writes
# them, install.sh never reads them, and they are 58 MB of the 60 this rsync would otherwise push
# over wifi to a Pi that has no use for them.
rsync -az --info=progress2 --delete \
  --exclude '__pycache__' \
  --exclude 'debs' \
  --exclude 'keyrings' \
  deploy/ "$HOST:$STAGE/"
rsync -az README.md "$HOST:$STAGE/../avionics-README.md" 2>/dev/null || true

if [[ "$BIN" == "gfx-spike" ]]; then
  cat <<EOF

==> Deployed. To run the M1 rendering spike on the panel:

    ssh -t $HOST 'sudo $DEST/gfx-spike'

  Stop it with Ctrl-C; it restores the console on the way out. If a panic ever leaves the
  console blank (the release profile aborts rather than unwinding, so Drop does not run),
  recover with:

    ssh $HOST 'sudo chvt 1'
EOF
else
  cat <<EOF

==> Deployed. Next, on the Pi:

    ssh -t $HOST
    sudo $DEST/$BIN --check              # verify before wiring anything up
    cd $STAGE
    sudo AVIONICS_BINARY=$DEST/$BIN ./install.sh --dry-run
    sudo AVIONICS_BINARY=$DEST/$BIN ./install.sh
    sudo systemctl start avionics
    journalctl -u avionics -f

  To try it without installing a service, just run it:

    ssh -t $HOST 'sudo $DEST/$BIN'
EOF
fi
