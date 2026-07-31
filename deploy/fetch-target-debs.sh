#!/usr/bin/env bash
#
# Download Debian packages for the *target* architecture on the *dev* machine.
#
# The Pi has no route to the internet, so `apt-get install` on the target cannot work. This
# fetches the packages here — where there is a network — so they can be carried over by scp,
# or unpacked straight into the cross-link sysroot without touching the Pi at all.
#
#   ./deploy/fetch-target-debs.sh libgbm-dev libdrm-dev
#   ./deploy/fetch-target-debs.sh --status /tmp/pi-dpkg-status libgbm-dev   # minimal delta
#   ./deploy/fetch-target-debs.sh --arch armhf --out /tmp/debs libgbm-dev
#
# Packages come from deb.debian.org over HTTPS and are verified against
# /usr/share/keyrings/debian-archive-keyring.gpg — the same signature check apt does on a
# Debian box. Nothing here trusts the transport alone.
#
# # Why --status matters
#
# apt decides what to download by diffing the requested packages against what it believes is
# already installed. With no status file it believes *nothing* is installed and pulls the
# entire dependency closure — for libgbm-dev that is 80 packages / 51 MB, nearly all of which
# are already on the Pi. Pass the target's own /var/lib/dpkg/status and the same request
# collapses to 3 packages / 285 KB. sync-sysroot.sh always passes it.
#
set -euo pipefail

ARCH="arm64"
SUITE="bookworm"
STATUS=""
OUT=""
COMPONENTS="main"

usage() {
  sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//; $d'
  exit "${1:-0}"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --arch)    ARCH="${2:?--arch needs a value}"; shift 2 ;;
    --suite)   SUITE="${2:?--suite needs a value}"; shift 2 ;;
    --status)  STATUS="${2:?--status needs a value}"; shift 2 ;;
    --out)     OUT="${2:?--out needs a value}"; shift 2 ;;
    -h|--help) usage 0 ;;
    --)        shift; break ;;
    -*)        echo "unknown option: $1" >&2; usage 1 >&2 ;;
    *)         break ;;
  esac
done

[[ $# -gt 0 ]] || { echo "!!! no packages named" >&2; usage 1 >&2; }
PACKAGES=("$@")

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="${OUT:-$ROOT/deploy/debs/$ARCH}"

KEYRING=/usr/share/keyrings/debian-archive-keyring.gpg
if [[ ! -r "$KEYRING" ]]; then
  echo "!!! $KEYRING is missing." >&2
  echo "    This is what verifies the packages are genuinely Debian's. Install it with:" >&2
  echo "        sudo apt-get install debian-archive-keyring" >&2
  echo "    Do not work around this by disabling signature checks." >&2
  exit 1
fi

# A private apt tree. Nothing here reads or writes the host's /etc/apt or /var/lib/apt, so
# this needs no root and cannot disturb the dev machine's own package state.
WORK="$(mktemp -d "${TMPDIR:-/tmp}/target-debs.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK"/etc/apt/preferences.d "$WORK"/etc/apt/sources.list.d \
         "$WORK"/var/lib/apt/lists/partial "$WORK"/var/lib/dpkg \
         "$WORK"/var/cache/apt/archives/partial

cat > "$WORK/etc/apt/sources.list" <<EOF
deb [arch=$ARCH signed-by=$KEYRING] https://deb.debian.org/debian $SUITE $COMPONENTS
deb [arch=$ARCH signed-by=$KEYRING] https://deb.debian.org/debian $SUITE-updates $COMPONENTS
deb [arch=$ARCH signed-by=$KEYRING] https://deb.debian.org/debian-security $SUITE-security $COMPONENTS
EOF

if [[ -n "$STATUS" ]]; then
  [[ -r "$STATUS" ]] || { echo "!!! cannot read status file: $STATUS" >&2; exit 1; }
  cp "$STATUS" "$WORK/var/lib/dpkg/status"
  echo "==> Using target package state from $STATUS ($(grep -c '^Package: ' "$STATUS") packages)"
else
  : > "$WORK/var/lib/dpkg/status"
  echo "==> No --status given: downloading the full dependency closure."
  echo "    This is correct for building a sysroot from scratch, but is much larger than a"
  echo "    delta and must NOT be dpkg -i'd wholesale onto a configured Pi."
fi

apt=(apt-get
  -o Dir::Etc::sourcelist="$WORK/etc/apt/sources.list"
  -o Dir::Etc::sourceparts="$WORK/etc/apt/sources.list.d"
  -o Dir::Etc::preferences="$WORK/etc/apt/preferences"
  -o Dir::Etc::preferencesparts="$WORK/etc/apt/preferences.d"
  -o Dir::State="$WORK/var/lib/apt"
  -o Dir::State::status="$WORK/var/lib/dpkg/status"
  -o Dir::Cache="$WORK/var/cache/apt"
  -o APT::Architecture="$ARCH"
  -o APT::Architectures::="$ARCH"
  -o APT::Get::AllowUnauthenticated=false
  -o Debug::NoLocking=1)

echo "==> Refreshing $SUITE/$ARCH package lists"
"${apt[@]}" update -qq

echo "==> Resolving and downloading: ${PACKAGES[*]}"
"${apt[@]}" install --yes --download-only --no-install-recommends "${PACKAGES[@]}"

shopt -s nullglob
debs=("$WORK"/var/cache/apt/archives/*.deb)
shopt -u nullglob
if [[ ${#debs[@]} -eq 0 ]]; then
  echo "==> Nothing to download; the target already has everything requested."
  # Still create the directory so callers can test for emptiness rather than existence.
  mkdir -p "$OUT"
  exit 0
fi

mkdir -p "$OUT"
cp "${debs[@]}" "$OUT/"

# A manifest makes the transfer auditable and lets sync-sysroot.sh compare versions against
# what the Pi actually has before it installs anything.
: > "$OUT/manifest.txt"
for d in "${debs[@]}"; do
  printf '%s\t%s\t%s\n' \
    "$(dpkg-deb -f "$d" Package)" \
    "$(dpkg-deb -f "$d" Version)" \
    "$(basename "$d")" >> "$OUT/manifest.txt"
done
sort -o "$OUT/manifest.txt" "$OUT/manifest.txt"

echo
echo "==> ${#debs[@]} package(s) in $OUT ($(du -sh "$OUT" | cut -f1))"
cut -f1,2 "$OUT/manifest.txt" | sed 's/^/    /'
