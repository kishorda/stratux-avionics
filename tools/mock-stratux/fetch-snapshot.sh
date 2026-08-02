#!/usr/bin/env bash
#
# Capture one snapshot of free public aviation data, for mock-stratux to serve offline.
#
#   ./fetch-snapshot.sh                          # 50 nm around Morristown NJ (the capture site)
#   ./fetch-snapshot.sh --lat 39.86 --lon -104.67 --radius 60 --out /tmp/kden.json
#
# Run this once, with internet. Everything afterwards is offline — same rule as the rest of this
# project: when something can only be observed live, capture it once and move the work to a desk.
#
# Deliberately a shell script and not part of the Rust binary. Fetching would mean an HTTP client
# with a TLS stack in the workspace, and the one thing this tool must not do is make the dependency
# graph of the aircraft binary harder to reason about. curl already exists.
#
# --- Sources, and what their terms allow -------------------------------------------------------
#
#   adsb.lol            traffic       no key, no account.  Data is ODbL: attribution and
#                                     share-alike. That is why no snapshot is committed to this
#                                     repo — see docs/free-aviation-data.md.
#   aviationweather.gov METAR/TAF     no key, no account.  US Government work, public domain.
#                                     Rate limited to 100 requests/minute; sets a user agent as
#                                     the service asks.
#
set -euo pipefail

LAT=40.7784
LON=-74.3343
RADIUS=50
OUT="snapshot.json"
# The service asks for a custom user agent so it can identify traffic. Saying who we are is both
# the polite and the self-interested choice: anonymous bulk callers are the ones that get blocked.
UA="stratux-avionics-mock/0.1 (offline display testing)"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --lat) LAT="$2"; shift 2 ;;
    --lon) LON="$2"; shift 2 ;;
    --radius) RADIUS="$2"; shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
    *) echo "!!! unrecognised argument $1" >&2; exit 1 ;;
  esac
done

for tool in curl jq; do
  command -v "$tool" >/dev/null || { echo "!!! $tool is required" >&2; exit 1; }
done

# adsb.lol caps the radius at 250 nm; anything beyond is silently clamped, which would make the
# snapshot quietly not cover what was asked for.
if (( RADIUS > 250 )); then
  echo "!!! --radius is capped at 250 nm by the feed" >&2
  exit 1
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

fetch() {
  local url="$1" dest="$2" label="$3"
  local code
  # --fail-with-body so an HTTP error is an error here rather than a JSON parse failure later, and
  # so the body is still available to say why.
  code="$(curl -sS --fail-with-body -A "$UA" --max-time 30 -o "$dest" -w '%{http_code}' "$url")" || {
    echo "!!! $label failed (HTTP $code)" >&2
    head -c 300 "$dest" >&2 || true
    echo >&2
    return 1
  }
  printf '    %-28s HTTP %s  %s bytes\n' "$label" "$code" "$(wc -c < "$dest")"
}

echo "==> Fetching around ${LAT},${LON} within ${RADIUS} nm"

fetch "https://api.adsb.lol/v2/lat/${LAT}/lon/${LON}/dist/${RADIUS}" "$TMP/traffic.json" "adsb.lol traffic"

# Weather is fetched by bounding box rather than by station list, so the snapshot covers whatever
# is actually near the position instead of a hardcoded set of airports that may be nowhere near it.
# One degree of latitude is 60 nm; longitude degrees are shorter, hence the cosine.
BBOX="$(python3 - "$LAT" "$LON" "$RADIUS" <<'PY'
import math, sys
lat, lon, radius = float(sys.argv[1]), float(sys.argv[2]), float(sys.argv[3])
dlat = radius / 60.0
dlon = radius / (60.0 * max(math.cos(math.radians(lat)), 1e-6))
print(f"{lat-dlat:.4f},{lon-dlon:.4f},{lat+dlat:.4f},{lon+dlon:.4f}")
PY
)"
echo "    bbox ${BBOX}"

fetch "https://aviationweather.gov/api/data/metar?bbox=${BBOX}&format=json"    "$TMP/metar.json" "aviationweather METAR"
fetch "https://aviationweather.gov/api/data/taf?bbox=${BBOX}&format=json"      "$TMP/taf.json"   "aviationweather TAF"
# PIREPs are frequently empty over a small area, and that is not a failure. Keep going either way.
fetch "https://aviationweather.gov/api/data/pirep?bbox=${BBOX}&format=json&age=3" "$TMP/pirep.json" "aviationweather PIREP" || echo '[]' > "$TMP/pirep.json"

# Normalise into the envelope mock-stratux reads. `// empty` guards each source independently, so
# one service having nothing to say does not discard the others.
jq -n \
  --arg captured "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  --argjson lat "$LAT" --argjson lon "$LON" \
  --slurpfile traffic "$TMP/traffic.json" \
  --slurpfile metar "$TMP/metar.json" \
  --slurpfile taf "$TMP/taf.json" \
  --slurpfile pirep "$TMP/pirep.json" \
  '{
     captured_utc: $captured,
     origin: {lat: $lat, lon: $lon},
     sources: {
       traffic: "adsb.lol (ODbL)",
       weather: "aviationweather.gov (US Govt, public domain)"
     },
     traffic: ($traffic[0].ac // []),
     metar:   (if ($metar[0]|type) == "array" then $metar[0] else [] end),
     taf:     (if ($taf[0]|type)   == "array" then $taf[0]   else [] end),
     pirep:   (if ($pirep[0]|type) == "array" then $pirep[0] else [] end)
   }' > "$OUT"

echo
echo "==> Wrote $OUT"
jq -r '"    traffic : \(.traffic|length) aircraft
    metar   : \(.metar|length)
    taf     : \(.taf|length)
    pirep   : \(.pirep|length)"' "$OUT"

if [[ "$(jq '.traffic|length' "$OUT")" == "0" ]]; then
  echo
  echo "    Note: no aircraft. That is a legitimate snapshot — an empty sky is a state the"
  echo "    display has to handle — but if it was not what you wanted, try a busier area or a"
  echo "    larger radius."
fi

cat <<EOF

Serve it:
    cargo run --release -p mock-stratux -- --snapshot $OUT

Then point the display at it:
    cargo run --release --features desktop -p avionics -- --window --host 127.0.0.1 --port 8080

This snapshot contains ODbL-licensed data from adsb.lol. Keep it out of version control (the
repo's .gitignore already covers snapshot*.json) and attribute it if you redistribute it.
EOF
