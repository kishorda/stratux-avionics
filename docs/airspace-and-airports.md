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
    11199 frequencies at 3780 airports (18%),
    15573 runway orientations at 13104 (63%)
airspace (7 pages)  1486 features -> 1408 kept
    dropped: 0 not class B/C/D, 78 outside the keep box, 0 no usable geometry
    vertices: 287618 -> 128479 (44.7%) at 10 m tolerance

conus.chart (2676 KiB), format v3
    20736 airports, 1408 airspace polygons, 1410 rings, 128479 vertices
    15573 runway orientations, 11199 frequencies, 474 KiB of names
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

**Heliports are the clutter problem** — 291 within 10 nm of downtown Los Angeles against 5
fixed-wing fields, because LA fire code mandated rooftop helipads for decades. They are written to
the file and never drawn by default. Carrying them costs 6,710 records, and the day someone wants a
helipad layer the file already has it.

One correction to that figure, found when the runtime query was first tested against the built
file: **208 of those 291 are OurAirports placeholders** with no real identifier, and the builder has
already dropped them. So the identifier filter, which exists for legibility rather than for
decluttering, turns out to do most of the decluttering as a side effect. The tier still matters —
it is the difference between 83 symbols and 5 — but it is not carrying the load the first
measurement credited it with.

### Declutter tiers

| Tier | What | Population | Drawn at |
| --- | --- | --- | --- |
| 0 | large + medium airports | 821 | every range |
| 1 | small airports, hard runway ≥ 3000 ft | 3,155 | 20 nm and in |
| 2 | remaining fixed-wing + seaplane bases | 11,452 | 5 nm and in |
| 3 | heliports | 6,710 | never, by default |

## What each airport carries

Beyond position and tier, the file holds three things the card and the symbols need. Coverage is
what decides whether a feature is buildable, so it is measured rather than assumed:

| | Coverage | Notes |
| --- | --- | --- |
| Name | 20,736 (100%) | 474 KiB of the file; truncated at 40 bytes on a character boundary |
| **ICAO station** | **20,056 (97%)** | the join key for weather — see below |
| Elevation | 96% | from `elevation_ft` |
| Longest hard runway | — | already used for the tier |
| **Runway orientations** | **13,104 airports (63%), 15,573 entries** | see below |
| **Frequencies** | **3,780 airports (18%), 11,199 entries** | `UNI 2646, A/D 1529, AWOS 1320, CTR 993, CTAF 874, TWR 661, GND 602, CLR 489, ATIS 469, APP 215, DEP 130` |

### Orientation comes from the identifier, not the heading column

`runways.csv` has `le_heading_degT`, and it is populated for **under a third** of runways —
endpoint coordinates likewise, at 32%. `le_ident` is populated for all of them and carries the same
answer to 10 degrees, which is finer than a tick a few pixels long can show. `"5"` is 050, `"19"` is
190, and the L/R/C suffixes are ignored. A further 1,041 CONUS runways are named by compass point
instead (`N`, `NE`, `NW`) — turf strips and seaplane lanes — so those are handled too. Helipads
(`H1`) have no orientation and get none.

Parallel and reciprocal runways collapse: 9L and 9R are one line drawn twice, and so are 05 and 23.
That takes KORD's eleven runways down to the handful of distinct angles worth drawing. The maximum
anywhere is six; 10,217 airports have exactly one.

### Weather on the card is a join, not a new source

METARs are already on board. They arrive over the Stratux `/weather` socket and sit in `AppState`
keyed by station, which is exactly what the card needs — so tapping an airport costs one lookup and
no fetch, and it works with no internet on the aircraft.

The join key is the problem. METARs are keyed `KMMU`; the symbol says `MMU`, because the label is
deliberately the short one a pilot says. Deriving one from the other by prepending `K` is right most
of the time and **silently wrong sometimes**, and wrong here means showing another airport's
weather. So the ICAO identifier is carried in the file as its own field — the reason for format v3.

Coverage, which decides whether the feature ever shows anything:

| | |
| --- | --- |
| `gps_code` | 82% of CONUS fields |
| Major airports (large + medium) | **821 of 821** |
| Fields with an AWOS, ASOS or ATIS | **99%** |

Those last two are the numbers that matter — a field with no weather station has no METAR to join
to, so an empty identifier there costs nothing.

Two of the 821 majors needed a guarded last resort. Bakersfield (`KL45`) and Miami Homestead
(`KX51`) carry their ICAO code only in `ident`, with both dedicated columns empty. `ident` is
accepted **only when it looks like a US ICAO identifier** — four characters beginning with `K` —
because taking it unconditionally would give `7N7` a station of `7N7`, which is not an identifier
and could only ever match the wrong thing.

The card shows the flight category badge, **wind**, ceiling, visibility, how long ago the report
arrived, and whether a TAF is also on board. `metar::summarise` never guesses: when neither ceiling
nor visibility can be read the card names the product instead of showing a badge, because implying
VFR from a report that could not be parsed is the failure worth designing out.

Wind comes first on the line, ahead of ceiling and visibility. The card already names the runways
two lines above, and wind against runway is the pairing a pilot is reading for.

Three things about the wind group are worth stating, because each is a way to be quietly wrong:

* **`36010KT` is not `00010KT`.** 360 is not folded to 0 — a pilot reads 360 as from the north, and
  `000` is how the calm group is spelled. Collapsing them makes a 10 kt northerly look like no wind.
* **Calm carries no direction.** `00000KT` renders as `CALM`, not as "from 000 degrees", which is a
  direction the observation does not claim.
* **The unit is read, not assumed.** US reports are in knots, but the group may legitimately arrive
  as `MPS` or `KMH`. Reading 8 metres per second as 8 knots halves it — the same shape of error as
  taking a flight level for feet, and just as invisible on a display.

The parser matches the group's *shape* rather than searching for one, which is the same discipline
the weather-phenomena matcher already uses. `R04L/2000FT`, `M08`, `10SM` and `A2993` all look
wind-adjacent and none of them parse.

### Two traps in the frequency file

**Kilohertz, not megahertz.** 121.975 is a real 25 kHz channel, and stored as a float it formats as
121.97 or 121.98 depending on which way it lands — one click off.

**One radio is often listed twice.** At a non-towered field CTAF and UNICOM are usually the same
number; at a towered one, so are CTAF and TWR. They are collapsed, and **tower wins over CTAF**
even though CTAF sorts first for display: Rocky Mountain Metro publishes 118.6 under both names,
and labelling a live tower frequency "CTAF" invites self-announcing on it.

Anything the builder cannot name is carried but **not shown on the card**. A number with no label
on an avionics display invites tuning a radio to it without knowing who answers. That is why `A/D`
(airport advisory, 1,529 entries) and `CNTR` (ARTCC, 993) were given names of their own rather than
left in the catch-all — they were its two biggest members, and both reach the card.

## File format

One file, `conus.chart`, built on the dev machine and read once at startup. The Pi parses nothing:
the format is fixed-layout little-endian so loading is a read into a `Vec`, and records are decoded
on demand — a query allocates only the handful of results it returns.

```
header         96 B   magic "AVCHART1", version, counts, section offsets,
                      grid origin and extent, FAA effective date
bucket grid     8 B   per 1°x1° cell: airport_first, airport_count
airports       48 B   lat/lon (i32 micro-degrees), 8-byte label, 8-byte ICAO
                      station, elevation, longest hard runway, kind, tier,
                      flags, and ranges into the runway, frequency and string
                      tables
airspace       40 B   bounding box, ring range, class, flags, lower/upper ft, label
rings           8 B   vertex_first, vertex_count
vertices        8 B   lat/lon, i32 micro-degrees
runways         4 B   heading, length — one per distinct orientation
frequencies     8 B   kHz, kind
strings          -    airport names, UTF-8, addressed by offset and length
```

**Version 2** added the runway, frequency and string sections and the name; **version 3** added the
ICAO station. The reader refuses any other version outright rather than guessing at a layout — a v2
file read as v3 would take the elevation out of the middle of the station identifier.

Micro-degrees give about 0.11 m of latitude resolution, which is two orders of magnitude finer than
the 10 m simplification tolerance and therefore free of consequence.

**Airports are bucketed, airspace is not.** A 1°x1° grid over CONUS is 1,534 cells and makes the
airport query O(cells touched) instead of a scan of 20,736 records — at 30 Hz a full scan is
several hundred thousand bounding-box tests a second, which is the same order as the entire current
frame cost. Airspace is only 1,486 polygons with bounding boxes already in the record, so a linear
scan is a few microseconds and a second index would be machinery earning nothing.

## Tapping an airport, and the rule it bends

The plan-view body was **deliberately inert**. Before the soft keys existed, a tap anywhere in it
cycled the range, which meant a hand steadying itself against the panel in turbulence silently
changed the range scale. That was removed, and the reasoning was written down: *a hand steadying
itself against the panel should not change what is on it.*

Tap-to-inspect relaxes the letter of that rule and keeps its substance. The rule was about
**selections** — a brush against the glass changing the range or the heading reference is a real
way to be misled about the traffic picture. A card is not a selection:

* Only a tap **on an airport symbol** opens it, within 18 px. A tap on empty sky still does nothing,
  which is where an accidental touch almost always lands.
* It changes no selection, hides no traffic, and cannot move the pilot off the page.
* Any next tap dismisses it, and it lapses on its own after 20 seconds — so it cannot be left
  covering the picture by someone who has forgotten it is there.
* It is drawn in the **lower-left** of the content area. Own-ship is at the centre and the nearest
  threat is most often ahead of it, so that corner is the least costly to spend.

What it costs is honest: the card covers about 290x76 px of a 608 px-wide content area while it is
up. That is why it goes away by itself.

The hit test uses the same tier the drawing uses, so a symbol decluttered away at 40 nm cannot be
found by tapping where it would have been at 5 nm — and with the layer off, nothing is tappable at
all. Overlapping symbols resolve to the **nearest**, not the first, so the answer does not depend
on file order.

## What it costs to draw

Measured on the dev machine with the offscreen presenter, 400 frames of the synthetic Broomfield
session — which starts under the Denver Class B, so the airspace query is doing real work:

| | mean draw | worst steady |
| --- | --- | --- |
| `--map off`, 10 nm | 0.65 ms | 1.73 ms |
| `--map apt`, 10 nm | 0.83 ms | 3.79 ms |
| `--map all`, 10 nm | 1.19 ms | 8.04 ms |
| `--map off`, 40 nm | 0.67 ms | 6.73 ms |
| `--map apt`, 40 nm | 0.76 ms | 7.58 ms |
| `--map all`, 40 nm | 1.47 ms | 10.81 ms |

So airports cost about 0.15 ms and airspace about 0.6 ms per frame here.

### And on the Pi

Measured on the target on 2026-08-03, replaying the 30-minute outdoor capture — real own-ship,
real traffic, New York Class B overhead — at 20 nm, 900 frames each. The renderer reports
`VC4 V3D 2.1, OpenGL ES 2.0, gles2=true`, which is the thing the desktop harness cannot check:

| | mean draw | worst steady |
| --- | --- | --- |
| `--map off` | 2.05 ms | 12.70 ms |
| `--map apt` | 2.40 ms | 13.44 ms |
| `--map all` | 5.39 ms | 16.33 ms |

**Airports 0.35 ms, airspace 3.0 ms**, against a 33 ms budget at 30 fps, holding 30.2 fps
throughout. The airspace half is roughly five times its desktop cost while the airport half is
about twice — consistent with the dash re-tessellation being CPU work on a much slower CPU, and
the symbol drawing being GPU work that was never the bottleneck.

That the layer renders *at all* under real ES 2.0 is worth stating separately. The dashed Class D
boundaries go through femtovg's `dashed_with_tolerance`, which re-tessellates the path, and the
desktop harness warns in as many words that it "will NOT catch ES2-incompatible rendering" because
it gets ES 3.2. This is the first run where that warning did not apply.

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
