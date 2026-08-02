# Free aviation data sources

Surveyed 2026-08-02, for seeding [`tools/mock-stratux`](../tools/mock-stratux) so the display can
be exercised on a desk with no Pi, no radios and no sky view.

Two of these are used. The rest are recorded because they were evaluated and rejected for a
reason, and the reason is worth more than the list.

## What is actually used

### adsb.lol — live traffic

```
https://api.adsb.lol/v2/lat/<LAT>/lon/<LON>/dist/<NM>
```

No key, no account, no registration. Community-fed, unfiltered, radius capped at 250 nm.
Returns an `ac` array of aircraft with position, altitude, track, ground speed, vertical rate,
callsign, registration, squawk and emitter category — everything `TrafficInfo` needs.

**Licence: ODbL.** Attribution and share-alike. This matters practically: it is *not* compatible
with this repo's MIT/Apache licensing, which is why **no snapshot is committed here** and why
`.gitignore` covers `snapshot*.json`. The test fixture in `snapshot.rs` is hand-written to the
same shape rather than being an extract of real data.

### aviationweather.gov — METAR, TAF, PIREP, SIGMET

```
https://aviationweather.gov/api/data/metar?bbox=<lat0,lon0,lat1,lon1>&format=json
https://aviationweather.gov/api/data/taf?ids=KEWR&format=json
https://aviationweather.gov/api/data/pirep?bbox=...&format=json&age=3
```

No key, no account. NOAA/NWS, so **US Government work and public domain** — freely
redistributable, unlike the traffic feed.

Rate limited to **100 requests/minute**, and the service asks callers to set a custom user agent;
`fetch-snapshot.sh` does. Formats include raw text, JSON, GeoJSON, CSV, XML and IWXXM. The display
only ever shows `rawOb`/`rawTAF`, since it renders the raw report and decodes abbreviations
itself.

One gotcha: `pirep` rejects a bare `age` parameter with `Must specify bounding box or stations ID
and radial distance`. The bbox is required.

## Evaluated and not used

| Source | Why not |
| --- | --- |
| **OpenSky Network** | The obvious first choice, and it *was* usable anonymously. As of **March 2026 basic auth is retired** in favour of OAuth2 client credentials, so it now needs an account and a client secret. A daily credit budget applies, larger for people who feed a receiver. Rejected purely because adsb.lol needs no account at all — for a tool whose whole point is being easy to run once, an OAuth dance is friction with no payoff. |
| **ADSBexchange** | Unfiltered and well regarded, but free access is via feeding a receiver or a RapidAPI free tier with a key. Same friction, no advantage over adsb.lol here. |
| **adsb.fi, airplanes.live, ADSBiq** | All viable community feeds with similar shapes and licences. Any of them could be swapped in by changing one URL in `fetch-snapshot.sh`. adsb.lol was picked for having the simplest documented radius query. Terms lean non-commercial. |
| **AVWX, CheckWX, metar-taf.com** | Decoded weather in tidy JSON, free tiers available — but they need keys, and the display wants the *raw* report anyway because decoding is its own job (`glossary.rs`). A pre-decoded feed would bypass the very code under test. |
| **NOAA NEXRAD mosaics** | Free and public domain, but delivered as imagery or Level II/III radar products, not as the FIS-B block structure Stratux publishes. Converting one to the other means writing the geo-referencing this project deliberately does not have to write, because Stratux hands over `NEXRADBlock` already decoded. Left alone — see below. |
| **OurAirports, FAA NASR** | Free airport and airspace databases. Genuinely useful, and no use here: Phase 1 has no basemap, so there is nothing to put them on. |

## What the mock still cannot do

**NEXRAD.** The precipitation underlay is the one part of the display no free service can seed,
because what Stratux publishes is FIS-B `NEXRADBlock` — geo-referenced, RLE-expanded blocks from
the 978 MHz uplink — and nothing on the open internet serves that shape. Ground-based NEXRAD is
available in abundance, but as imagery or radar products, and converting it would mean writing the
FIS-B geo-referencing this project specifically avoids having to write.

So the underlay is still exercised only by `replay synth`, which generates real `NEXRADBlock`
structures. That is the right tool for it, and the split is worth stating plainly:

| Wanted | Use |
| --- | --- |
| Real traffic, real weather text, real density | `mock-stratux` with a snapshot |
| The live WebSocket path, reconnect, staleness, malformed frames | `mock-stratux` — nothing else exercises it |
| NEXRAD underlay, deterministic scenarios, a guaranteed conflict | `replay synth` |
| A specific recorded moment, byte for byte | `replay record` / `--replay` |

## Terms, briefly

Both services are free to query and neither needs an account, but they are somebody else's
infrastructure. `fetch-snapshot.sh` is built to be run **once per test scenario**, not in a loop:
one call per source, a 30-second timeout, an identifying user agent, and everything after it runs
from the file on disk. That also happens to be this project's existing house rule for anything
that can only be observed live — capture once, move the analysis offline.
