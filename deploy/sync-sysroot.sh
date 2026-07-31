#!/usr/bin/env bash
#
# Build a link sysroot so the dev machine can cross-link against the Pi's libraries.
#
# Only libgbm genuinely has to be linked (the `drm` crate reaches the kernel through
# rustix/linux-raw-sys, and khronos-egl dlopen's libEGL at runtime), but we mirror whole
# library directories anyway: it makes the linker's DT_NEEDED resolution just work, and it
# means adding a dependency later doesn't send you back here.
#
#   ./deploy/sync-sysroot.sh pi@stratux.local                  # mirror the Pi (recommended)
#   ./deploy/sync-sysroot.sh --offline                         # no Pi involved at all
#   ./deploy/sync-sysroot.sh pi@stratux.local arm-linux-gnueabihf   # 32-bit image
#
# # The Pi has no internet
#
# So the old `ssh pi 'apt-get install libgbm-dev'` step cannot work. The -dev packages are a
# *build-time* artifact — the display itself needs only the runtime Mesa that the image
# already ships — so there are two honest ways to get them, and this script does both:
#
#   default    Download the packages here (deploy/fetch-target-debs.sh), carry the handful
#              that are actually missing over scp, dpkg -i them on the Pi, then rsync the
#              result back. The sysroot then reflects the real flight machine, which is the
#              thing you are actually linking for.
#
#   --offline  Download the full closure here and unpack it straight into ./sysroot. The Pi
#              is never contacted and never modified. Reproducible from an archive snapshot,
#              and the right choice if you would rather not mutate a configured flight box.
#              The risk it carries is the whole reason the default is the other one: the
#              sysroot is then Debian's idea of bookworm, not your Pi's, and if the image
#              carries Raspberry Pi's Mesa builds instead of Debian's, they can differ.
#
set -euo pipefail

MODE="remote"
DO_INSTALL=1
ALLOW_DOWNGRADE=0
HOST=""
TRIPLE=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --offline)          MODE="offline"; shift ;;
    --no-install)       DO_INSTALL=0; shift ;;
    --allow-downgrade)  ALLOW_DOWNGRADE=1; shift ;;
    -h|--help)          sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//; $d'; exit 0 ;;
    -*)                 echo "unknown option: $1" >&2; exit 1 ;;
    *)                  if [[ -z "$HOST" ]]; then HOST="$1"; else TRIPLE="$1"; fi; shift ;;
  esac
done

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SYSROOT="$ROOT/sysroot"
# libc6-dev is not optional even though we link almost nothing. Without it the sysroot has no
# libc.so/libm.so/crt1.o, the toolchain's own glibc gets used instead, and on Ubuntu 26.04
# that is 2.43 — which the Pi's 2.36 cannot load. See cross-cc-aarch64-linux-gnu.sh.
PKGS=(libc6-dev libgbm-dev libdrm-dev libegl1-mesa-dev)

# Rewrite absolute symlinks so they stay inside the sysroot.
#
# Debian's -dev packages ship e.g. usr/lib/<triple>/libm.so -> /lib/<triple>/libm.so.6. That
# leading slash means the host resolves it against the REAL root, not the sysroot, so ld
# silently links against the dev machine's glibc. It is invisible in the build log and shows
# up only as a GLIBC_2.43 requirement in the finished ELF. rsync's --copy-unsafe-links covers
# the mirror path; this covers the unpack path.
relativise_symlinks() {
  local root="$1" link target
  find "$root" -type l -print | while IFS= read -r link; do
    target="$(readlink "$link")"
    case "$target" in
      /*) ln -sfn "$(realpath -m --relative-to="$(dirname "$link")" "$root$target")" "$link" ;;
    esac
  done
}

# Read installed "package<TAB>version" pairs out of a dpkg status file.
installed_versions() {
  awk 'BEGIN { RS = ""; FS = "\n" }
       {
         p = ""; v = ""; s = ""
         for (i = 1; i <= NF; i++) {
           if      (substr($i, 1, 9)  == "Package: ") p = substr($i, 10)
           else if (substr($i, 1, 9)  == "Version: ") v = substr($i, 10)
           else if (substr($i, 1, 8)  == "Status: ")  s = substr($i, 9)
         }
         if (p != "" && s ~ /[[:space:]]installed$/) print p "\t" v
       }' "$1"
}

# ---------------------------------------------------------------------------------------
# Work out the architecture.
# ---------------------------------------------------------------------------------------
if [[ "$MODE" == "remote" ]]; then
  [[ -n "$HOST" ]] || { echo "usage: sync-sysroot.sh user@host [multiarch-triple]  |  sync-sysroot.sh --offline" >&2; exit 1; }
  TRIPLE="${TRIPLE:-aarch64-linux-gnu}"

  echo "==> Checking the target's architecture"
  REMOTE_ARCH="$(ssh "$HOST" 'dpkg --print-architecture')"
  echo "    remote dpkg architecture: $REMOTE_ARCH"
  case "$REMOTE_ARCH:$TRIPLE" in
    arm64:aarch64-linux-gnu|armhf:arm-linux-gnueabihf) ;;
    *)
      echo "!!! Mismatch: remote is '$REMOTE_ARCH' but you asked for '$TRIPLE'." >&2
      echo "    arm64 -> aarch64-linux-gnu (Rust target aarch64-unknown-linux-gnu)" >&2
      echo "    armhf -> arm-linux-gnueabihf (Rust target armv7-unknown-linux-gnueabihf)" >&2
      exit 1
      ;;
  esac
  DEB_ARCH="$REMOTE_ARCH"
else
  TRIPLE="${TRIPLE:-${HOST:-aarch64-linux-gnu}}"
  case "$TRIPLE" in
    aarch64-linux-gnu)   DEB_ARCH=arm64 ;;
    arm-linux-gnueabihf) DEB_ARCH=armhf ;;
    *) echo "!!! --offline needs a known triple, got '$TRIPLE'" >&2; exit 1 ;;
  esac
  echo "==> Offline mode: building a $DEB_ARCH sysroot from the Debian archive, no Pi contact"
fi

DEBS="$ROOT/deploy/debs/$DEB_ARCH"

# ---------------------------------------------------------------------------------------
# Offline: unpack the full closure into the sysroot and stop.
# ---------------------------------------------------------------------------------------
if [[ "$MODE" == "offline" ]]; then
  rm -rf "$DEBS"
  "$ROOT/deploy/fetch-target-debs.sh" --arch "$DEB_ARCH" --out "$DEBS" "${PKGS[@]}"

  echo "==> Unpacking into $SYSROOT"
  mkdir -p "$SYSROOT"
  for d in "$DEBS"/*.deb; do
    dpkg-deb -x "$d" "$SYSROOT"
  done

  echo "==> Confining absolute symlinks to the sysroot"
  relativise_symlinks "$SYSROOT"

  echo
  echo "==> Done (offline). Sanity check:"
  ls -l "$SYSROOT/usr/lib/$TRIPLE/libgbm.so" 2>/dev/null || {
    echo "!!! libgbm.so is missing from the sysroot; cross-linking will fail." >&2
    exit 1
  }
  echo
  echo "Reminder: this sysroot is Debian's bookworm, not your Pi's actual filesystem."
  echo "If linking succeeds here but the binary fails to start on the Pi with a symbol or"
  echo "version error, re-run without --offline to mirror the real machine."
  exit 0
fi

# ---------------------------------------------------------------------------------------
# Remote: fetch only what the Pi is missing, carry it over, install, then mirror.
# ---------------------------------------------------------------------------------------
TMP="$(mktemp -d "${TMPDIR:-/tmp}/sync-sysroot.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

echo "==> Reading the target's package state"
scp -q "$HOST:/var/lib/dpkg/status" "$TMP/status"
installed_versions "$TMP/status" > "$TMP/installed.tsv"
echo "    $(wc -l < "$TMP/installed.tsv") packages installed on the target"

if [[ "$DO_INSTALL" -eq 1 ]]; then
  rm -rf "$DEBS"
  echo "==> Downloading whatever the target is missing (on this machine — the Pi has no network)"
  "$ROOT/deploy/fetch-target-debs.sh" \
    --arch "$DEB_ARCH" --status "$TMP/status" --out "$DEBS" "${PKGS[@]}"

  if [[ -s "$DEBS/manifest.txt" ]]; then
    # Guard against silently downgrading the flight machine. `dpkg -i` will happily install
    # an older version over a newer one, and the Stratux images sometimes carry Raspberry
    # Pi's Mesa rather than Debian's, which is exactly where that would bite.
    echo "==> Checking none of these would downgrade the target"
    downgrades=0
    while IFS=$'\t' read -r pkg newver _; do
      cur="$(awk -F'\t' -v p="$pkg" '$1 == p { print $2; exit }' "$TMP/installed.tsv")"
      [[ -n "$cur" ]] || continue
      if dpkg --compare-versions "$newver" lt "$cur"; then
        echo "    !!! $pkg: target has $cur, archive offers $newver (DOWNGRADE)" >&2
        downgrades=1
      fi
    done < "$DEBS/manifest.txt"
    if [[ "$downgrades" -eq 1 ]]; then
      if [[ "$ALLOW_DOWNGRADE" -eq 1 ]]; then
        echo "    proceeding anyway because --allow-downgrade was given"
      else
        echo >&2
        echo "!!! Refusing to downgrade packages on the target." >&2
        echo "    The image is probably not running stock Debian Mesa. Either pass" >&2
        echo "    --allow-downgrade if you are sure, or use --offline to build the sysroot" >&2
        echo "    without touching the Pi." >&2
        exit 1
      fi
    fi

    echo "==> Copying $(wc -l < "$DEBS/manifest.txt") package(s) to the target"
    ssh "$HOST" 'rm -rf /tmp/avionics-debs && mkdir -p /tmp/avionics-debs'
    scp -q "$DEBS"/*.deb "$HOST:/tmp/avionics-debs/"

    echo "==> Installing them there (dpkg only — no network is touched)"
    ssh -t "$HOST" 'sudo dpkg -i /tmp/avionics-debs/*.deb && rm -rf /tmp/avionics-debs'
  fi
else
  echo "==> --no-install: assuming the headers are already on the target"
fi

echo "==> Mirroring into $SYSROOT"
mkdir -p "$SYSROOT/usr/lib/$TRIPLE" "$SYSROOT/lib/$TRIPLE" "$SYSROOT/usr/include"

# --copy-unsafe-links turns symlinks that point outside the tree into real files, so the
# sysroot is self-contained and the linker doesn't chase absolute paths back onto the host.
rsync -a --copy-unsafe-links --info=progress2 \
  "$HOST:/usr/lib/$TRIPLE/" "$SYSROOT/usr/lib/$TRIPLE/"
rsync -a --copy-unsafe-links --info=progress2 \
  "$HOST:/lib/$TRIPLE/" "$SYSROOT/lib/$TRIPLE/"
rsync -a --copy-unsafe-links --info=progress2 \
  "$HOST:/usr/include/" "$SYSROOT/usr/include/"

echo
echo "==> Done. Sanity check:"
ls -l "$SYSROOT/usr/lib/$TRIPLE/libgbm.so"* 2>/dev/null || {
  echo "!!! libgbm.so is missing from the sysroot; cross-linking will fail." >&2
  exit 1
}
echo
echo "Note: .cargo/config.toml hardcodes the sysroot path. If you moved the checkout,"
echo "update the --sysroot link-args there to match $SYSROOT."
