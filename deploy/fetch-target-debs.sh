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
RPI=0
FREEZE=1

# Raspberry Pi's archive, needed because the Stratux image is built on it rather than on
# stock Debian. Its packages carry a "+rpt" version suffix and Debian's -dev packages pin
# exact versions, so mixing the two archives is not optional: with Debian sources alone,
#
#     libc6-dev : Depends: libc6 (= 2.36-9+deb12u14) but 2.36-9+rpt2+deb12u9 is to be installed
#
# and nothing resolves.
#
# The fingerprint below is the trust anchor and is deliberately hardcoded here rather than
# read from a file: it is what makes the downloaded key verifiable. It was confirmed from two
# independent sources — fetched over HTTPS from archive.raspberrypi.com, and read off the
# flashed Stratux image — which produced byte-identical keyrings.
RPI_URI="http://archive.raspberrypi.com/debian"
RPI_KEY_URL="https://archive.raspberrypi.com/debian/raspberrypi.gpg.key"
RPI_KEY_FPR="CF8A1AF502A2AA2D763BAE7E82B129927FA3303E"

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
    --rpi)     RPI=1; shift ;;
    --allow-upgrades) FREEZE=0; shift ;;
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

# Fetch and verify Raspberry Pi's archive key.
#
# The key is fetched over HTTPS and then checked against the fingerprint pinned at the top of
# this script. The pin is what does the real work: TLS only proves we reached a host claiming
# to be archive.raspberrypi.com, whereas the fingerprint proves the key is the one we vetted.
# A mismatch is fatal and must never be "worked around" — it means either the archive rotated
# its key (verify the new one out-of-band, then update the pin) or something is wrong.
ensure_rpi_keyring() {
  local dir="$ROOT/deploy/keyrings"
  local keyring="$dir/raspberrypi-archive-keyring.gpg"

  if [[ -s "$keyring" ]] && key_fingerprint_matches "$keyring"; then
    printf '%s' "$keyring"
    return 0
  fi

  command -v gpg >/dev/null || { echo "!!! gpg is required to verify the Raspberry Pi key" >&2; exit 1; }
  mkdir -p "$dir"

  local tmp="$WORK/rpi.key"
  echo "==> Fetching the Raspberry Pi archive key" >&2
  curl -fsSL --proto '=https' --tlsv1.2 "$RPI_KEY_URL" -o "$tmp" \
    || { echo "!!! could not download $RPI_KEY_URL" >&2; exit 1; }

  # Accept either armoured or binary; apt wants binary.
  if ! gpg --dearmor < "$tmp" > "$keyring.new" 2>/dev/null; then
    cp "$tmp" "$keyring.new"
  fi

  if ! key_fingerprint_matches "$keyring.new"; then
    rm -f "$keyring.new"
    echo "!!! The downloaded Raspberry Pi key does NOT match the pinned fingerprint." >&2
    echo "    expected $RPI_KEY_FPR" >&2
    echo "    Refusing to use it. Do not bypass this." >&2
    exit 1
  fi

  mv "$keyring.new" "$keyring"
  echo "    verified against pinned fingerprint $RPI_KEY_FPR" >&2
  printf '%s' "$keyring"
}

key_fingerprint_matches() {
  gpg --show-keys --with-colons "$1" 2>/dev/null \
    | awk -F: '$1 == "fpr" { print $10 }' \
    | grep -qx "$RPI_KEY_FPR"
}

if [[ "$RPI" -eq 1 ]]; then
  RPI_KEYRING="$(ensure_rpi_keyring)"
  echo "deb [arch=$ARCH signed-by=$RPI_KEYRING] $RPI_URI $SUITE main" >> "$WORK/etc/apt/sources.list"

  # Raspberry Pi rebuilds some Debian packages with a "+rpt" suffix. Those suffixes sort
  # HIGHER than the Debian originals, so apt prefers them automatically wherever both exist —
  # which is what we want, since that is what the image is actually running.
  echo "==> Raspberry Pi archive enabled (image is built on it, not stock Debian)"
fi

if [[ -n "$STATUS" ]]; then
  [[ -r "$STATUS" ]] || { echo "!!! cannot read status file: $STATUS" >&2; exit 1; }
  cp "$STATUS" "$WORK/var/lib/dpkg/status"
  echo "==> Using target package state from $STATUS ($(grep -c '^Package: ' "$STATUS") packages)"

  # Freeze every installed package at its current version.
  #
  # Without this, asking for one small package quietly drags in a partial dist-upgrade. Asking
  # this Pi for libgbm-dev proposed upgrading libc6 AND jumping the kernel from 6.6.74 to
  # 6.12.96 — because the -dev packages pin exact versions and the image is months behind the
  # archive. On a machine that flies, an unrequested kernel swap is not an inconvenience, it
  # is a different aircraft.
  #
  # Pinning to the installed version at priority 1001 leaves apt free to ADD packages while
  # forbidding it to change anything already there. If that makes the request unsolvable, apt
  # says so and we stop — which is the correct outcome, not something to override blindly.
  if [[ "$FREEZE" -eq 1 ]]; then
    awk 'BEGIN { RS = ""; FS = "\n" }
         {
           p = ""; v = ""; s = ""
           for (i = 1; i <= NF; i++) {
             if      (substr($i, 1, 9) == "Package: ") p = substr($i, 10)
             else if (substr($i, 1, 9) == "Version: ") v = substr($i, 10)
             else if (substr($i, 1, 8) == "Status: ")  s = substr($i, 9)
           }
           if (p != "" && v != "" && s ~ /[[:space:]]installed$/)
             printf "Package: %s\nPin: version %s\nPin-Priority: 1001\n\n", p, v
         }' "$STATUS" > "$WORK/etc/apt/preferences.d/00-freeze-installed"
    echo "    frozen at current versions; only NEW packages may be added"
    echo "    (pass --allow-upgrades to permit changing what is already installed)"
  fi
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
