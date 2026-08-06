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

### Airports and runways — FAA AIS

```
.../services/US_Airport/FeatureServer/0     19,559 points
.../services/Runways/FeatureServer/0        23,401 runways
```

The same server the airspace comes from, so the whole file carries one AIRAC cycle and one
authority. `Runways.AIRPORT_ID` is a GUID matching `US_Airport.GLOBAL_ID`; the airport geometry is
already decimal degrees, so nothing is parsed out of the DMS text columns.

**This replaced OurAirports**, which fed the layer originally. Four reasons, in order of weight:

* **The ICAO identifier is real.** OurAirports' `gps_code` is the local code repeated for most
  fields — `ID15`, `WN43`, `VA10` — which cannot match a METAR and never will. Of the 20,056
  "stations" the old file carried, only 3,799 were even K-prefixed. Tested against the stations
  actually reporting across CONUS: **FAA matched 362 of 400, OurAirports 334**, using 2,335 strings
  instead of 20,056. Coverage fell from 97% to 13% and the join got *better*.
* **Enums, not free text.** `COMP_CODE` has thirty values. OurAirports' surface column has **564**
  distinct spellings of things like "asphalt", which the old build prefix-matched.
* **Stated, not inferred.** `OPERSTATUS` and `PRIVATEUSE` say what the old code guessed from a
  `type` string, and `DESIGNATOR` gives `05/23` outright instead of it being reconstructed from one
  end's identifier.
* **Currency.** Same 28-day cycle as the airspace, rather than whenever someone last edited a wiki.

Frequencies still come from OurAirports: the FAA publishes them behind a `Frequencies` → `Services`
→ airport join covering 2,493 services, where OurAirports has a flat table covering 3,752 airports,
and the CTAF at a small field is exactly what the FAA table is thinnest on. Both are public domain,
so carrying two sources costs nothing but the fetch. The two disagree about identifiers — the
frequency file says `KMMU`, the FAA layer says `MMU` — so every airport is indexed under both.

### Three things about the frequency table

**Kilohertz, not megahertz.** 121.975 is a real 25 kHz channel, and stored as a float it formats as
121.97 or 121.98 depending on which way it lands — one click off.

**One radio is often listed twice.** At a non-towered field CTAF and UNICOM are usually the same
number; at a towered one, so are CTAF and TWR. They collapse, and **tower wins over CTAF** even
though CTAF sorts first for display: Rocky Mountain Metro publishes 118.6 under both names, and
labelling a live tower frequency "CTAF" invites self-announcing on it.

**Not every published frequency is one you can tune.** The source carries military UHF and VHF —
Washington National's tower on 257.600, Fort Rucker's ground on 357.150, Seymour Johnson's on
138.100 — all real, none reachable from a civil comm radio, and `TWR 257.600` on a card reads as a
number you could call the tower on. Only 118.000–136.975 is kept.

The exception runs the other way and is the more interesting half: **ATIS and AWOS below the comm
band are kept**, because 74 airports broadcast them on a co-located navaid's voice channel and a
pilot tunes those on the NAV radio. The first instinct was that anything under 118 MHz was a data
error; it was not, and a tidy band check would have discarded working information.

Anything the builder cannot name is carried but **not shown on the card**. A number with no label
on an avionics display invites tuning a radio to it without knowing who answers.

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
FAA US_Airport (20 pages)  19559 features -> 18108 kept
    dropped: 932 outside CONUS, 337 not operational, 0 no position, 182 no identifier
    2335 with an ICAO station (13%), 10847 frequencies at 3750 airports,
    14926 runway orientations at 12531
airspace (7 pages)  1486 features -> 1408 kept
    dropped: 0 not class B/C/D, 78 outside the keep box, 0 no usable geometry
    vertices: 287618 -> 128479 (44.7%) at 10 m tolerance
variation    18108 airports, -16 to 15 degrees (east-positive)

conus.chart (2351 KiB), format v4
    18108 airports, 1408 airspace polygons, 1410 rings, 128479 vertices
    14926 runway orientations, 10847 frequencies, 276 KiB of names
    grid 28x61 cells from 22, -125
    FAA data effective 2026-07-09 (day 20643)
```

The 44.7% is not comparable with the table above: the input to the build is already generalised at
2.2 m by the service, so most of the reduction has happened before Douglas–Peucker sees it. What is
comparable is the largest polygon — 502 vertices, the same New York Class B, reached by both
routes.

The build is byte-reproducible: the same source directory gives an identical file, so a rebuild
that shows in the diff is a *data* change and nothing else. That still holds with variation in the
file, because it is computed at the chart's own effective date rather than at the wall clock of
whoever ran the build — the same source directory gives the same declinations however long after
the cycle it is rebuilt.

### Magnetic variation, and why the file has to carry it

One signed byte per airport, east-positive, whole degrees. It exists because two numbers the
display has to compare are in different reference frames:

- **Runway headings are magnetic**, coming from the painted designator — which is the only source
  populated for every runway, the survey heading having been available for under a third of them.
- **METAR surface winds are true.** The wind a pilot hears on ATIS or from the tower is magnetic;
  the one in the body of the report is not, which is exactly why this catches people out.

Subtracting them directly is wrong by the local variation — about 12° at Morristown, up to 16°
either way inside the CONUS box, which is more than one runway number. The failure is invisible:
plausible numbers, consistently offset, on a card that gives the pilot no way to notice.

**It is modelled, not read.** The FAA layers this build uses do not carry it. Checked rather than
assumed: `US_Airport` exposes 26 fields and none of them is magnetic, and `Runways` exposes 18
with no bearing at all. So `tools/chartdata` computes declination from the World Magnetic Model
(the `world_magnetic_model` crate, WMM2025, valid to 2030) at each airport's position and the
file's effective date.

Three consequences worth stating:

- **The model stays in the build tool.** A spherical-harmonic expansion with a coefficient table
  and an expiry date has no business in the aircraft binary to answer a question whose only input
  — an airport's position — is fixed at build time. The display adds a constant.
- **The record did not grow.** Airport records already ended with two spare bytes; variation took
  one and left one. `AIRPORT_LEN` is still 48 and no offset moved.
- **The version still went up, to v4, and older files are refused.** This is the case where
  tolerating an old file would be dangerous rather than convenient. Variation 0 is a *plausible*
  value — it is real along the agonic line — so a v3 file read with the field defaulted would be
  indistinguishable from a place needing no correction, and every runway on the display would be
  quietly off.

The model's own uncertainty is a fraction of a degree, which is irrelevant here: the runway
heading it corrects is a 10° bucket, so the stored value is rounded to whole degrees and the
rounding disappears several places below the noise floor of what it is corrected against.

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
| 0 | aerodromes worth seeing anywhere: public or military, hard runway ≥ 5000 ft | 1,890 | every range |
| 1 | any other public or military aerodrome with a paved runway | 2,014 | 20 nm and in |
| 2 | everything else fixed-wing, including private strips | 8,637 | 5 nm and in |
| 3 | heliports and balloonports | 5,567 | never, by default |

**There is deliberately no minimum runway length for tier 1**, and getting there took two failures.
At 3,000 ft it hid Somerset's lit 2,739 ft runway — which is what prompted this whole review, since
the field was reported as missing from the data when it was merely decluttered. At 2,500 ft it hid
Palo Alto's 2,443 ft one. Each fix moved the number and left the next field just below it. A
public-use aerodrome with a paved runway is worth drawing at 20 nm however short the runway is, and
measured across CONUS that costs exactly **one** extra symbol in the busiest 20 nm view, 14 to 15.

**Military fields count as public here.** 255 of the 276 military aerodromes are flagged
`PRIVATEUSE`, which is accurate and irrelevant: reading it literally demoted Edwards,
Wright-Patterson, Oceana and Vance to the 5 nm band. A 15,000 ft military runway is the most
conspicuous thing for miles and usually has controlled airspace stacked on it. The reason to draw
it is not that you might land there.

## Tapping airspace: numbers, not a verdict

Tapping inside a boundary shows what is stacked over that point:

```
AIRSPACE                086°  10.7 nm
D  TEB     SFC - 2500 ft
B  JFK    1800 - 7000 ft
```

Lowest floor first, because that is the order you meet them climbing, and because the number a
pilot wants from a stack of shelves is the bottom of the one above them. The airport wins a
contested tap — it is the smaller, more specific target, and a symbol inside a Class D would
otherwise be unreachable.

### Why it does not say whether you are inside

Because it cannot, and the measurement says so plainly. From the outdoor capture, sitting still at
Morristown with a field elevation of 187 ft:

| | samples | range |
| --- | --- | --- |
| `GPSAltitudeMSL` | 10,348 | 300 – 656 ft — **356 ft of scatter while stationary** |
| `BaroPressureAltitude` | 18,000 | 310 – 319 ft, stable to ±5 ft |

GPS averaged about 215 ft high against known field elevation. The pressure altitude is precise and
is on the 29.92 datum, so it equals MSL only when the local altimeter setting happens to be 29.92 —
on a real day that is a ±500 ft offset. Airspace floors are MSL, and legal compliance is your
altimeter on local QNH, which this box does not have.

So the card prints the floor and the ceiling and lets the pilot cross-check against the instrument
that is certified for it. A green "you are clear" would be the most confidently wrong thing this
display could say.

### Why not dim the shelves that do not apply

That was the plan, and measuring killed it. Counting the volumes in view whose vertical band
contains own-ship, with a 500 ft buffer:

| Location | altitude | dimmed |
| --- | --- | --- |
| Morristown | 2,000 ft | **6%** |
| Morristown | 3,500 ft | 18% |
| Broomfield | 7,500 ft | 15% |

In the band a light aircraft actually flies, nearly everything nearby is vertically relevant —
Class D floors are at the surface and their ceilings are above you, and the Class B shelves stack
right through your operating range. The machinery would have removed almost no ink, using an
altitude that cannot support the claim.

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
