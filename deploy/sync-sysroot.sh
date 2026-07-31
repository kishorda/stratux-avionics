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
# *build-time* artifact, needed only here, so there are two honest ways to get them and this
# script does both.
#
# (Do not confuse that with the RUNTIME libraries. The Stratux image ships no Mesa at all —
# no libgbm.so.1, no libEGL — so the Pi does need packages installed before the display can
# run. That is deploy/push-packages.sh, and it is a separate, deliberate step.)
#
#   default    Mirror the Pi's real libraries over rsync (read-only) and take headers from
#              the archive. The sysroot then reflects the real flight machine, which is the
#              thing you are actually linking for. NOTHING on the Pi is modified — to change
#              what is installed there, use deploy/push-packages.sh deliberately.
#
#   --offline  Download the closure here and unpack it straight into ./sysroot. The Pi is
#              never contacted. Reproducible from an archive snapshot. Add --rpi to pull
#              Raspberry Pi's builds (Mesa 24.2.8+rpt) instead of Debian's (22.3.6), which is
#              what the Stratux image actually runs; without it the sysroot is Debian's idea
#              of bookworm and can differ from the real machine.
#
set -euo pipefail

MODE="remote"
DO_INSTALL=1
RPI=0
HOST=""
TRIPLE=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --offline)          MODE="offline"; shift ;;
    --no-install)       DO_INSTALL=0; shift ;;
    --rpi)              RPI=1; shift ;;
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
  RPI_FLAG=()
  [[ "$RPI" -eq 1 ]] && RPI_FLAG=(--rpi)
  "$ROOT/deploy/fetch-target-debs.sh" --arch "$DEB_ARCH" --out "$DEBS" "${RPI_FLAG[@]}" "${PKGS[@]}"

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
# Remote: mirror the Pi READ-ONLY, and take headers from the archive.
#
# This branch used to dpkg -i the -dev packages on the Pi and rsync the result back. That is
# gone, for two reasons found on the real machine:
#
#   * Building a sysroot must not modify the flight computer. If you want to change what is
#     installed on the Pi, do it deliberately with deploy/push-packages.sh.
#   * It could not work anyway. The image is built on Raspberry Pi's archive (+rpt versions)
#     and is months behind it, so Debian's exact-version -dev dependencies dragged in a
#     partial dist-upgrade — libc6, and the kernel from 6.6.74 to 6.12.96.
#
# So: headers and linker symlinks come from the archive (self-consistent, unpacked locally),
# and the Pi's real runtime libraries are rsync'd over the top so DT_NEEDED resolves against
# what the machine actually has.
# ---------------------------------------------------------------------------------------
TMP="$(mktemp -d "${TMPDIR:-/tmp}/sync-sysroot.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

echo "==> Reading the target's package state"
scp -q "$HOST:/var/lib/dpkg/status" "$TMP/status"
installed_versions "$TMP/status" > "$TMP/installed.tsv"
echo "    $(wc -l < "$TMP/installed.tsv") packages installed on the target"

RPI_FLAG=()
if grep -q '+rpt' "$TMP/status"; then
  RPI_FLAG=(--rpi)
fi

# Warn early about runtime libraries the binary will need but the image does not have. The
# Stratux image ships no Mesa at all, so this is not hypothetical: without it the cross-built
# binary links here and then dies on the Pi with "libgbm.so.1: cannot open shared object file".
echo "==> Checking the target has the runtime libraries the binary needs"
missing=""
for lib in libgbm.so.1 libEGL.so.1; do
  ssh "$HOST" "ls /usr/lib/$TRIPLE/$lib >/dev/null 2>&1" || missing="$missing $lib"
done
if [[ -n "$missing" ]]; then
  echo "    !!! target is missing:$missing" >&2
  echo "        The display cannot run there until that is fixed:" >&2
  echo "            ./deploy/push-packages.sh $HOST libgbm1 libegl1 libgles2 libgl1-mesa-dri" >&2
  echo "        Continuing — the sysroot is still buildable, it just has nothing to run on yet." >&2
else
  echo "    present."
fi

if [[ "$DO_INSTALL" -eq 1 ]]; then
  rm -rf "$DEBS"
  echo "==> Fetching headers and linker symlinks from the archive (nothing is sent to the Pi)"
  # No --status here on purpose: we want a self-consistent set from the archive to unpack
  # locally, not a delta against the Pi. Nothing is installed on the target either way.
  "$ROOT/deploy/fetch-target-debs.sh" \
    --arch "$DEB_ARCH" --out "$DEBS" "${RPI_FLAG[@]}" "${PKGS[@]}"

  echo "==> Unpacking into $SYSROOT"
  mkdir -p "$SYSROOT"
  for d in "$DEBS"/*.deb; do
    dpkg-deb -x "$d" "$SYSROOT"
  done
else
  echo "==> --no-install: skipping the archive headers, mirroring only"
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

# The mirror lands on top of the unpacked archive files, so where both supply something the
# Pi's real copy wins — which is the point. Absolute symlinks from either source have to be
# confined afterwards or ld follows them out onto the dev machine's glibc.
echo "==> Confining absolute symlinks to the sysroot"
relativise_symlinks "$SYSROOT"

echo
echo "==> Done. Sanity check:"
ls -l "$SYSROOT/usr/lib/$TRIPLE/libgbm.so"* 2>/dev/null || {
  echo "!!! libgbm.so is missing from the sysroot; cross-linking will fail." >&2
  exit 1
}
echo
echo "Nothing on the target was modified. To change what is installed there, use:"
echo "    ./deploy/push-packages.sh $HOST <packages...>"
echo
echo "Verify the result before deploying:"
echo "    ./deploy/check-glibc.sh target/$([[ $DEB_ARCH == arm64 ]] && echo aarch64-unknown-linux-gnu || echo armv7-unknown-linux-gnueabihf)/release/avionics"
