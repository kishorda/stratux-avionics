#!/usr/bin/env bash
#
# Install packages on a target that has no internet, by fetching them here and carrying them
# over. This is the only script in deploy/ that modifies the target's package state, and it
# is deliberately separate from sync-sysroot.sh so that building a sysroot can never mutate
# the flight machine as a side effect.
#
#   ./deploy/push-packages.sh pi@10.0.0.240 libgbm1 libegl1 libgles2 libgl1-mesa-dri
#   ./deploy/push-packages.sh --dry-run pi@10.0.0.240 libgbm1
#
# # What it will and will not do
#
# Everything already installed on the target is pinned to its current version, so apt may ADD
# packages but never upgrade, downgrade or remove one. That restriction is not paranoia: asked
# for libgbm-dev with the pin off, apt proposed upgrading libc6 and moving the kernel from
# 6.6.74 to 6.12.96. Frozen, the same request for the Mesa runtime resolves to 31 new packages
# and touches nothing that was already there.
#
# If a request cannot be satisfied without changing an installed package, this fails and says
# so. That is the right answer — reach for --allow-upgrades only deliberately, and never on a
# machine you are about to fly.
#
set -euo pipefail

DRY_RUN=0
ALLOW_UPGRADES=0
NO_RPI=0
HOST=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run)        DRY_RUN=1; shift ;;
    --allow-upgrades) ALLOW_UPGRADES=1; shift ;;
    --no-rpi)         NO_RPI=1; shift ;;
    -h|--help)        sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//; $d'; exit 0 ;;
    -*)               echo "unknown option: $1" >&2; exit 1 ;;
    *)                HOST="$1"; shift; break ;;
  esac
done

[[ -n "$HOST" ]] || { echo "usage: push-packages.sh [--dry-run] user@host PACKAGE..." >&2; exit 1; }
[[ $# -gt 0 ]] || { echo "!!! no packages named" >&2; exit 1; }
PACKAGES=("$@")

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

TMP="$(mktemp -d "${TMPDIR:-/tmp}/push-packages.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

echo "==> Reading the target's state"
ARCH="$(ssh "$HOST" 'dpkg --print-architecture')"
scp -q "$HOST:/var/lib/dpkg/status" "$TMP/status"
echo "    arch=$ARCH, $(grep -c '^Package: ' "$TMP/status") packages installed"

# The Stratux image is built on Raspberry Pi's archive, whose packages carry a "+rpt" suffix.
# Resolving against Debian alone cannot work: Debian's -dev packages pin exact versions that
# the rebuilt libc6/libdrm2 do not match.
RPI_FLAG=()
if [[ "$NO_RPI" -eq 0 ]] && grep -q '+rpt' "$TMP/status"; then
  RPI_FLAG=(--rpi)
  echo "    target uses Raspberry Pi's archive; enabling it as a source"
fi

UPGRADE_FLAG=()
[[ "$ALLOW_UPGRADES" -eq 1 ]] && UPGRADE_FLAG=(--allow-upgrades)

DEBS="$TMP/debs"
"$ROOT/deploy/fetch-target-debs.sh" \
  --arch "$ARCH" --status "$TMP/status" --out "$DEBS" \
  "${RPI_FLAG[@]}" "${UPGRADE_FLAG[@]}" "${PACKAGES[@]}"

if [[ ! -s "$DEBS/manifest.txt" ]]; then
  echo "==> Nothing to do; the target already has everything requested."
  exit 0
fi

# Re-check against the target rather than trusting the pin alone. The pin constrains apt's
# solver; this constrains what we actually hand to dpkg, which is the thing that changes the
# machine. Belt and braces, because the failure mode is a Pi that no longer boots.
echo "==> Verifying nothing already installed would change"
awk 'BEGIN { RS = ""; FS = "\n" }
     {
       p = ""; v = ""; s = ""
       for (i = 1; i <= NF; i++) {
         if      (substr($i, 1, 9) == "Package: ") p = substr($i, 10)
         else if (substr($i, 1, 9) == "Version: ") v = substr($i, 10)
         else if (substr($i, 1, 8) == "Status: ")  s = substr($i, 9)
       }
       if (p != "" && s ~ /[[:space:]]installed$/) print p "\t" v
     }' "$TMP/status" > "$TMP/installed.tsv"

changes=0
while IFS=$'\t' read -r pkg newver _; do
  cur="$(awk -F'\t' -v p="$pkg" '$1 == p { print $2; exit }' "$TMP/installed.tsv")"
  [[ -n "$cur" ]] || continue
  if [[ "$cur" != "$newver" ]]; then
    echo "    !!! $pkg: installed $cur -> would become $newver" >&2
    changes=1
  fi
done < "$DEBS/manifest.txt"

if [[ "$changes" -eq 1 && "$ALLOW_UPGRADES" -eq 0 ]]; then
  echo >&2
  echo "!!! Refusing: this would change packages already installed on the target." >&2
  echo "    Re-run with --allow-upgrades only if you genuinely intend that." >&2
  exit 1
fi

echo
echo "==> Plan: $(wc -l < "$DEBS/manifest.txt") package(s), $(du -sh "$DEBS" | cut -f1), all additions"
cut -f1,2 "$DEBS/manifest.txt" | sed 's/^/    /'

if [[ "$DRY_RUN" -eq 1 ]]; then
  echo
  echo "==> --dry-run: nothing was sent or installed."
  exit 0
fi

echo
echo "==> Copying to the target"
ssh "$HOST" 'rm -rf /tmp/avionics-debs && mkdir -p /tmp/avionics-debs'
scp -q "$DEBS"/*.deb "$HOST:/tmp/avionics-debs/"

# dpkg, not apt: the target has no working sources and needs none. Every dependency is in the
# directory we just copied, so one dpkg -i over the whole set satisfies them together.
echo "==> Installing (dpkg only; the target's network is never used)"
ssh -t "$HOST" 'sudo dpkg -i /tmp/avionics-debs/*.deb && sudo rm -rf /tmp/avionics-debs'

echo
echo "==> Done. Verifying:"
ssh "$HOST" "dpkg -l ${PACKAGES[*]} 2>/dev/null | tail -n +6 | awk '{print \"    \" \$1, \$2, \$3}'"
