#!/usr/bin/env bash
#
# Fetch the source data for the airport and airspace layer, once.
#
#   ./fetch-chartdata.sh                 # into tools/chartdata/source
#   ./fetch-chartdata.sh --out /tmp/src
#
# Then build the file the display reads:
#
#   cargo run --release -p chartdata -- build --source tools/chartdata/source \
#       --out crates/avionics-ui/data/conus.chart
#
# Run this with internet; everything afterwards is offline. Same rule as fetch-snapshot.sh, and
# the same reason it is a shell script rather than part of the Rust binary: fetching would mean an
# HTTP client with a TLS stack in the workspace, and curl already exists.
#
# --- Sources, and what their terms allow -------------------------------------------------------
#
#   OurAirports   airports, runways   Public domain, stated outright. This is why the built file
#                                     CAN be committed, unlike the ODbL snapshots.
#   FAA AIS       Class B/C/D         US Government work, public domain. 28-day AIRAC cycle.
#
# See docs/airspace-and-airports.md.
#
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUT="$HERE/source"
UA="stratux-avionics-chartdata/0.1 (offline display data build)"

OURAIRPORTS="https://davidmegginson.github.io/ourairports-data"
FAA_BASE="https://services6.arcgis.com/ssFJjBXIUyZDrSYZ/arcgis/rest/services"
FAA="$FAA_BASE/Class_Airspace/FeatureServer/0"

# Airports and runways are attribute-heavy but geometrically trivial, so they page far larger than
# the airspace polygons do.
FAA_PAGE=2000

# About 2.2 m at these latitudes. Deliberately far below the 10 m the build simplifies to, so the
# server's generalisation cannot show through in the result — verified at 0.1 m max deviation
# against raw geometry, see docs/airspace-and-airports.md. Without it a single page of Class D is
# ~78 MB and times out; with it, 2.1 MB.
GENERALIZE="0.00002"

# The service caps a page at 2000 features, but the geometry is what costs, not the count. 300 is
# sized so no single request is large enough to be worth retrying by hand.
PAGE=300

while [[ $# -gt 0 ]]; do
  case "$1" in
    --out) OUT="$2"; shift 2 ;;
    -h|--help) sed -n '2,25p' "$0"; exit 0 ;;
    *) echo "!!! unrecognised argument $1" >&2; exit 1 ;;
  esac
done

for tool in curl python3; do
  command -v "$tool" >/dev/null || { echo "!!! $tool is required" >&2; exit 1; }
done

mkdir -p "$OUT"

get() {
  local url="$1" dest="$2" label="$3"
  local code
  code="$(curl -sS --fail-with-body -A "$UA" --max-time 300 -o "$dest" -w '%{http_code}' "$url")" || {
    echo "!!! $label failed (HTTP $code)" >&2
    head -c 300 "$dest" >&2 || true
    echo >&2
    return 1
  }
  printf '    %-34s HTTP %s  %8s bytes\n' "$label" "$code" "$(wc -c < "$dest")"
}

echo "==> OurAirports"
get "$OURAIRPORTS/airports.csv" "$OUT/airports.csv" "airports.csv"
get "$OURAIRPORTS/runways.csv"  "$OUT/runways.csv"  "runways.csv"
# Communication frequencies. Only about 18% of CONUS airports have any — but that 18% is
# essentially every field you would actually talk to, and it is what makes the inspect card
# worth tapping.
get "$OURAIRPORTS/airport-frequencies.csv" "$OUT/airport-frequencies.csv" "airport-frequencies.csv"

echo
echo "==> FAA airports and runways"
# The same server the airspace comes from, so the whole file shares one AIRAC cycle and one
# authority. US_Airport carries the position (as point geometry, already decimal degrees), the
# ICAO identifier, elevation, operational status and public/private; Runways carries the
# designator, length and a COMP_CODE *enum* rather than OurAirports' 564 spellings of "asphalt".
faa_pages() {
  local svc="$1" fields="$2" geom="$3" out="$4" page=0 offset=0 dest count url
  while :; do
    dest="$OUT/${out}-$(printf '%03d' "$page").json"
    url="$FAA_BASE/$svc/FeatureServer/0/query?where=1%3D1&outFields=${fields}"
    url+="&returnGeometry=${geom}&outSR=4326&resultOffset=${offset}&resultRecordCount=${FAA_PAGE}&f=json"
    get "$url" "$dest" "${svc} offset ${offset}"
    count="$(python3 -c "
import json
print(len(json.load(open('$dest')).get('features',[])))
")"
    if [[ "$count" == "0" ]]; then rm -f "$dest"; break; fi
    # Advance by what came back, NOT by what was asked for. This layer caps a page at 1000 even
    # when asked for 2000, so stepping by the request size fetched records 0-999, 2000-2999, ...
    # and silently skipped half the file — 10,000 valid-looking airports out of 19,559.
    offset=$((offset + count))
    page=$((page + 1))
    if (( offset > 60000 )); then
      echo "!!! $svc did not terminate; stopping at $offset" >&2
      break
    fi
  done
}

faa_pages US_Airport \
  "GLOBAL_ID,IDENT,ICAO_ID,NAME,ELEVATION,TYPE_CODE,SERVCITY,STATE,COUNTRY,OPERSTATUS,PRIVATEUSE,IAPEXISTS,MIL_CODE" \
  true airports-faa
faa_pages Runways \
  "AIRPORT_ID,DESIGNATOR,LENGTH,WIDTH,DIM_UOM,COMP_CODE,LIGHTACTV" \
  false runways-faa

echo
echo "==> FAA Class Airspace layer metadata"
# Fetched for one field: editingInfo.dataLastEditDate. That is the currency of the *airspace*,
# which is a different and more useful question than when this script happened to run.
get "$FAA?f=json" "$OUT/airspace-meta.json" "layer metadata"
python3 - "$OUT/airspace-meta.json" <<'PY'
import json, sys, datetime
meta = json.load(open(sys.argv[1]))
ms = meta.get("editingInfo", {}).get("dataLastEditDate")
if ms:
    when = datetime.datetime.fromtimestamp(ms / 1000, datetime.timezone.utc)
    print(f"    FAA data last edited: {when:%Y-%m-%d}")
else:
    print("    !!! no dataLastEditDate in the layer metadata; the build will fall back to today")
PY

echo
echo "==> FAA Class B, C and D geometry"
# Class E is not fetched at all. It is 4343 of the 6061 polygons and almost all E5 transition area,
# which covers the country from 700 ft AGL — a boundary around everything is a boundary around
# nothing. See docs/airspace-and-airports.md.
for class in B C D; do
  offset=0
  page=0
  while :; do
    dest="$OUT/airspace-${class}-$(printf '%03d' "$page").json"
    url="$FAA/query?where=$(python3 -c "import urllib.parse;print(urllib.parse.quote(\"CLASS='$class'\"))")"
    url+="&outFields=IDENT,NAME,CLASS,LOWER_VAL,LOWER_UOM,LOWER_CODE,UPPER_VAL,UPPER_UOM,UPPER_CODE"
    url+="&resultOffset=${offset}&resultRecordCount=${PAGE}"
    url+="&outSR=4326&maxAllowableOffset=${GENERALIZE}&f=geojson"
    get "$url" "$dest" "class ${class} offset ${offset}"

    count="$(python3 -c "
import json,sys
print(len(json.load(open('$dest')).get('features',[])))
")"
    if [[ "$count" == "0" ]]; then
      rm -f "$dest"
      break
    fi
    offset=$((offset + PAGE))
    page=$((page + 1))
    # The service stops returning features past the end, which is the loop's exit. This guard is
    # only so a change in that behaviour cannot spin forever.
    if (( offset > 5000 )); then
      echo "!!! class $class did not terminate; stopping at offset $offset" >&2
      break
    fi
  done
done

echo
echo "==> Wrote $OUT"
ls -la "$OUT" | tail -n +2 | awk '{printf "    %10s  %s\n", $5, $9}'

cat <<EOF

Build the file the display reads:
    cargo run --release -p chartdata -- build --source $OUT \\
        --out crates/avionics-ui/data/conus.chart

Both sources are public domain, so unlike snapshot*.json the built file is committed. The FAA
endpoint serves only the current AIRAC cycle, so a past cycle cannot be fetched again — that is
why the built file is version-controlled rather than treated as a cache.
EOF
