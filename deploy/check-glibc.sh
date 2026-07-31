#!/usr/bin/env bash
#
# Refuse to ship a binary that the Pi's glibc cannot load.
#
#   ./deploy/check-glibc.sh target/aarch64-unknown-linux-gnu/release/avionics
#   ./deploy/check-glibc.sh --max 2.36 path/to/binary
#
# Cross-linking against a sysroot is easy to get subtly wrong: the link succeeds, the ELF
# looks right, `file` reports the correct architecture, and the binary still dies on the
# target with "version `GLIBC_2.43' not found". The two ways it has actually happened here:
#
#   * the sysroot had no libc6-dev, so the toolchain's own (newer) glibc was used;
#   * the sysroot had libc6-dev, but its libm.so was an ABSOLUTE symlink that resolved
#     against the dev machine's root instead of the sysroot.
#
# Both are invisible until runtime. This is the check that makes them visible at build time.
#
# Debian Bookworm ships glibc 2.36, so that is the default ceiling. Confirm against the real
# image with:  ssh pi@stratux 'ldd --version | head -1'
#
set -euo pipefail

MAX="2.36"
BIN=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --max) MAX="${2:?--max needs a version}"; shift 2 ;;
    -h|--help) sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//; $d'; exit 0 ;;
    *) BIN="$1"; shift ;;
  esac
done

[[ -n "$BIN" ]] || { echo "usage: check-glibc.sh [--max VERSION] BINARY" >&2; exit 1; }
[[ -r "$BIN" ]] || { echo "!!! cannot read $BIN" >&2; exit 1; }

command -v readelf >/dev/null || { echo "!!! readelf not found (apt install binutils)" >&2; exit 1; }

mapfile -t versions < <(readelf -V "$BIN" 2>/dev/null | grep -o 'GLIBC_[0-9][0-9.]*' | sed 's/^GLIBC_//' | sort -uV)

if [[ ${#versions[@]} -eq 0 ]]; then
  echo "==> $BIN requires no versioned glibc symbols at all."
  exit 0
fi

highest="${versions[-1]}"
echo "==> $(basename "$BIN") requires glibc up to $highest (target ceiling $MAX)"

# dpkg's comparator understands version ordering properly; sort -V is a fallback.
if command -v dpkg >/dev/null; then
  too_new=$(dpkg --compare-versions "$highest" gt "$MAX" && echo yes || echo no)
else
  too_new=$([[ "$(printf '%s\n%s\n' "$MAX" "$highest" | sort -V | tail -1)" == "$highest" && "$highest" != "$MAX" ]] && echo yes || echo no)
fi

if [[ "$too_new" == "yes" ]]; then
  echo >&2
  echo "!!! This binary will NOT start on the target." >&2
  echo "    Symbols needing more than GLIBC_$MAX:" >&2
  readelf -sW --dyn-syms "$BIN" 2>/dev/null \
    | grep -oE '[A-Za-z_][A-Za-z0-9_]*@GLIBC_[0-9][0-9.]*' \
    | sort -u \
    | while IFS='@' read -r sym ver; do
        v="${ver#GLIBC_}"
        if dpkg --compare-versions "$v" gt "$MAX" 2>/dev/null; then echo "      $sym@$ver" >&2; fi
      done
  echo >&2
  echo "    Rebuild the sysroot, then rebuild:" >&2
  echo "        ./deploy/sync-sysroot.sh --offline" >&2
  echo "        cargo build --release --target aarch64-unknown-linux-gnu -p avionics --features kms" >&2
  exit 1
fi

echo "    OK."
