# Airspace and airports

Design notes for the map layer: what the data is, why it is shaped the way it is, and the
measurements that decided each choice. Written 2026-08-02, before the layer was drawn on the panel.

The display has been a *relative* position display from the start — "no basemap: this is a relative
position display, which is what matters for seeing and avoiding"
([`avionics-ui/src/lib.rs`](../crates/avionics-ui/src/lib.rs)). This layer is the first deliberate
exception to that, and it is worth being explicit about why.

At 10 nm over New York the plan view draws ninety-three targets with tags fighting for space, and
there is nothing on screen to say which cluster is the Newark arrival stream. A pilot's mental
model of traffic is anchored to airports and airspace, not to a compass rose. That is what the
layer buys. The cost is that absolute geography can be *wrong* in a way relative traffic cannot:
traffic is cross-checked out of the window, and a Class B boundary is not.

## Two layers, two risk profiles

They are drawn together and they are not the same kind of thing.

**Airports** are points. If one is fifty metres off, nothing follows. The worst realistic failure
is clutter.

**Airspace** is a boundary a pilot might fly relative to. A shelf drawn half a mile wide, or one
cycle stale, invites exactly the violation it appears to prevent. So:

* the plan view gains a `NOT FOR NAVIGATION` banner, shown **only while airspace is drawn** — the
  airports layer alone does not raise it, because it makes no claim a pilot would fly against;
* the data carries the FAA's own last-edit date, and the display shows it;
* the **vertical** logic — highlighting the shelf you are in or about to enter — is deliberately
  deferred. It is the most valuable part and the part most able to be confidently wrong, so the
  lateral rings get flown and checked against a sectional first. The altitude fields ship in the
  file from day one so the format is decided once.

This is the same reasoning that put the permanent banner on the attitude page and made caging a
two-press state machine: the failure that matters is an instrument that lies convincingly.

## Sources

### Airports — OurAirports

<https://ourairports.com/data/> — `airports.csv` (12.7 MB), `runways.csv` (4.0 MB).

**Public domain**, stated plainly: *"All data is released to the Public Domain, and comes with no
guarantee of accuracy or fitness for use."* That is the difference that lets the built file be
committed to this repo, where [`snapshot*.json` cannot be](free-aviation-data.md) — adsb.lol's
traffic is ODbL, and share-alike does not match this repo's licensing.

85,824 rows worldwide. After keeping `iso_country=US`, dropping Alaska, Hawaii and the unassigned
region, and dropping `closed` and `balloonport`: **24,048 in CONUS**.

A further 3,312 are dropped for having no usable identifier — OurAirports assigns placeholders like
`US-10378` to fields with no official code, and a symbol labelled `US-10378` is noise on a 7"
panel. **20,736 airports** reach the file.

The label is `local_code` where there is one, else `gps_code`, else `ident`. That gives `MMU`,
`EWR`, `06N` — what a pilot says, and three or four characters, which is what fits beside a symbol.
Preferring `ident` would have given `KMMU` and `KEWR`, a third wider for no extra meaning.

### Airspace — FAA AIS

```
https://services6.arcgis.com/ssFJjBXIUyZDrSYZ/arcgis/rest/services/Class_Airspace/FeatureServer/0
```

US Government work, public domain. Published through
[the FAA's ArcGIS open data site](https://adds-faa.opendata.arcgis.com/datasets/class-airspace) and
updated on the 28-day AIRAC cycle.

6,061 polygons: **B 423, C 411, D 652, E 4,343**.

**Class E is excluded.** It is 72% of the dataset and almost all of it is E5 transition area, which
blankets the country from 700 ft AGL — drawing it puts a boundary around everything and means
nothing. B, C and D are the airspace a VFR pilot navigates relative to. That leaves **1,486
polygons**, of which **1,408** survive the geographic filter below.

### It is not only United States airspace

The dataset carries Canadian and Mexican control zones along the borders — worth keeping, because
Vancouver's airspace matters if you are near Bellingham — and it also carries Honolulu's Class B,
San Juan's Class C, Whitehorse in the Yukon, and TMAs at Biak, Jayapura and Merauke in Indonesia.
Nothing in the attributes distinguishes any of it; `CLASS` is populated throughout.

So airspace is filtered by a deliberately loose bounding box (22–52 N, 128–64 W). It is not trying
to trace a border, it is telling "next to the contiguous United States" from "on the other side of
the Pacific". The 78 polygons it drops are Hawaii, Puerto Rico and the USVI, remote Canada and
Indonesia — the same places the airports filter drops by region code, so the two agree without
being told to.

### One field is not in the units it looks like

Thirty polygons give their upper limit as a flight level: `UPPER_UOM` is `FL` and `UPPER_CODE` is
`STD`, so Tijuana's TCA reads `up=195`. Taken as feet that is a control area topping out at 195 ft
— wrong, and entirely plausible-looking on a display. They are converted on the way in.

Useful fields: `CLASS`, `IDENT`, `NAME`, `LOWER_VAL`/`LOWER_CODE`, `UPPER_VAL`/`UPPER_CODE`. The
lower limit is cleanly one of two things across the whole set — `MSL` (679) or `SFC` (407) — so the
vertical encoding needs one flag bit and one integer, not a units parser.

## The vertex problem, which is the whole engineering story

The first plan was "a few polygons of a few dozen vertices each, draw them as paths". The data says
otherwise. Measured on the raw service response:

| Class | Polygons | Median vertices | Max |
| --- | --- | --- | --- |
| B | 423 | 261 | 16,904 — Kansas City |
| C | 411 | 1,017 | 14,046 — South Bend |
| D | 652 | 3,256 | 11,336 — Worcester |

A Class D is a circle of about 4.4 nm radius. The FAA ships the median one as **3,256 vertices**.
Raw Class C alone is 45 MB of GeoJSON, and the three classes together are 2.34 million vertices.
Handing that to femtovg on a `vc4` at 30 Hz is not a tuning problem, it is a non-starter.

So the geometry is simplified at build time, with Douglas–Peucker. Measured over a 1,086-polygon
sample of the raw service response:

```
tolerance   5 m: 2,339,652 -> 143,138 vertices  (6.1%)
tolerance  10 m: 2,339,652 -> 104,562 vertices  (4.5%)   <- chosen
tolerance  25 m: 2,339,652 ->  68,019 vertices  (2.9%)
```

**10 m is chosen because it is invisible at every selectable range.** `Layout::for_size` gives an
outer ring radius of 187.5 px on the 800x480 panel, so at the tightest range — 2 nm — one pixel is
**19.8 m**. A 10 m tolerance is under half a pixel there, and under a fortieth of one at 40 nm.
Going to 25 m would save another 1.6% of a file that is already small, and would start to be
visible on the 2 nm ring.

After simplification the median polygon is 84 vertices and the largest in the country — New York
Class B — is 502.

### What the build actually produced

```
airports.csv        85824 rows -> 20736 kept
    dropped: 54448 outside CONUS, 7328 closed or balloonport,
             3312 no usable identifier, 0 no position
airspace (7 pages)  1486 features -> 1408 kept
    dropped: 0 not class B/C/D, 78 outside the keep box, 0 no usable geometry
    vertices: 287618 -> 128479 (44.7%) at 10 m tolerance

conus.chart (1567 KiB)
    20736 airports, 1408 airspace polygons, 1410 rings, 128479 vertices
    grid 26x58 cells from 24, -125
    FAA data effective 2026-07-09
    tiers: 821 major, 3151 paved, 10054 minor, 6710 heliport
    class B:  402 polygons,  33205 vertices, largest 502
    class C:  397 polygons,  41068 vertices, largest 257
    class D:  609 polygons,  54206 vertices, largest 467
```

The 44.7% is not comparable with the table above: the input to the build is already generalised at
2.2 m by the service, so most of the reduction has happened before Douglas–Peucker sees it. What is
comparable is the largest polygon — 502 vertices, the same New York Class B, reached by both
routes.

The build is byte-reproducible: the same source directory gives an identical file, so a rebuild
that shows in the diff is a *data* change and nothing else.

### Downloading it without simplifying it twice

The raw geometry is also too large to *fetch* comfortably: a 400-feature page of Class D is roughly
78 MB and times out. The service supports server-side generalisation, so `fetch-chartdata.sh` asks
for `maxAllowableOffset=0.00002` degrees — about **2.2 m**, deliberately far below the 10 m the
build actually wants. That takes the same page to 2.1 MB in 25 seconds.

This was checked rather than assumed, by fetching one page both ways and simplifying each to 10 m:

```
raw-sourced            17,059 vertices
generalized-sourced    17,482 vertices   (+2.5%)
max vertex deviation vs the raw geometry: 0.1 m
```

A tenth of a metre, against a 19.8 m pixel. The transfer saving is real and the geometric cost is
not measurable on the panel.

## What ends up on screen

The number that decides render cost is not the size of the dataset, it is how much of it is visible
at once. Measured against every polygon and every airport in CONUS, taking the worst position in
the country for each:

| | Worst case | Where |
| --- | --- | --- |
| Airspace @ 40 nm | 33 polygons / 3,855 vertices | Trenton |
| Airspace @ 20 nm | 29 polygons / 2,637 vertices | Vancouver BC |
| Airspace @ 10 nm | 19 polygons / 2,339 vertices | Vancouver BC |
| Airports, fixed-wing @ 10 nm | 35 | Denton TX |
| **Heliports @ 10 nm** | **287** | downtown Los Angeles |

(The FAA dataset includes some cross-border Canadian airspace, hence Vancouver.)

3,855 vertices in a frame is nothing — for comparison the compass rose is already batched into two
paths because *"femtovg emits a GL draw per `stroke_path`, and on the vc4's tile-based renderer
every one of those carries binning cost"*. Airspace batches by class for the same reason: three
paths, not thirty-three.

**Heliports are the entire clutter problem.** 287 within 10 nm of downtown Los Angeles, because LA
fire code mandated rooftop helipads for decades. Fixed-wing worst case over the same country is 35.
So heliports are written to the file — they are only 6,710 records — and never drawn by default.
The data being present and the layer being off is the right split: it costs nothing to carry, and
the day someone wants a helipad layer the file already has it.

### Declutter tiers

| Tier | What | Population | Drawn at |
| --- | --- | --- | --- |
| 0 | large + medium airports | 821 | every range |
| 1 | small airports, hard runway ≥ 3000 ft | 3,155 | 20 nm and in |
| 2 | remaining fixed-wing + seaplane bases | 11,452 | 5 nm and in |
| 3 | heliports | 6,710 | never, by default |

## File format

One file, `conus.chart`, built on the dev machine and read once at startup. The Pi parses nothing:
the format is fixed-layout little-endian so loading is a read into a `Vec`.

```
header        64 B    magic "AVCHART1", version, counts, section offsets,
                      grid origin and extent, FAA effective date
bucket grid   16 B    per 1°x1° cell: airport_first, airport_count,
                      airspace_first, airspace_count
airports      24 B    lat/lon (i32 micro-degrees), 8-byte label, elevation,
                      longest hard runway, kind, tier, flags
airspace      40 B    bounding box, ring range, class, flags, lower/upper ft, label
rings          8 B    vertex_first, vertex_count
vertices       8 B    lat/lon, i32 micro-degrees
```

Micro-degrees give about 0.11 m of latitude resolution, which is two orders of magnitude finer than
the 10 m simplification tolerance and therefore free of consequence.

**Airports are bucketed, airspace is not.** A 1°x1° grid over CONUS is 1,534 cells and makes the
airport query O(cells touched) instead of a scan of 20,736 records — at 30 Hz a full scan is
several hundred thousand bounding-box tests a second, which is the same order as the entire current
frame cost. Airspace is only 1,486 polygons with bounding boxes already in the record, so a linear
scan is a few microseconds and a second index would be machinery earning nothing.

## Currency

The header carries the FAA layer's own `dataLastEditDate`, not the date the file was fetched. Those
are different questions, and the one that matters is how old the *airspace* is.

Expired data is **dimmed, not hidden** — the same call `reckon` makes for coasting targets and
`nexrad.rs` makes for aging blocks. A layer that silently vanished at a cycle boundary would be
worse than one visibly out of date.

## Why the built file is committed

`.gitignore` excludes `deploy/debs` because they are "re-downloadable from the Debian archive at any
time", and excludes `snapshot*.json` for licensing. Neither applies here.

The FAA endpoint serves **only the current cycle**. Once a cycle rolls, the data that produced a
given file is gone — so unlike the debs, this is not re-downloadable, and committing it is the only
way to reproduce or audit what was on the panel on a given day. The data is public domain, so
nothing prevents it.

The cost is about 1.9 MB of history per refresh. B/C/D boundaries move slowly, so the intended
cadence is a few times a year or before a trip, not every 28 days.

## Building it

```sh
tools/chartdata/fetch-chartdata.sh                 # one download, needs internet
cargo run --release -p chartdata -- build \
    --source tools/chartdata/source \
    --out crates/avionics-ui/data/conus.chart
```

`chartdata` is a dev-only crate. `deploy.sh` builds `-p avionics`, so nothing here reaches the
aircraft — the same isolation `mock-stratux` has.

Fetching is a shell script and not part of the Rust binary, for the reason `fetch-snapshot.sh`
gives: an HTTP client with a TLS stack is a thing the workspace should not grow without cause, and
`curl` already exists.
