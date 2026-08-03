//! Turning OurAirports CSV into airport records.
//!
//! # What is thrown away, and why
//!
//! The worldwide file is 85,824 rows. Everything outside the contiguous United States goes, along
//! with `closed` fields and balloonports, leaving about 24,000. A further ~3,300 go for having no
//! usable identifier: OurAirports assigns placeholders like `US-10378` to fields with no official
//! code, and a symbol labelled `US-10378` is noise on a 7" panel.
//!
//! # The label is not the `ident` column
//!
//! `local_code` is preferred, then `gps_code`, then `ident`. That gives `MMU`, `EWR`, `06N` — what
//! a pilot says, and three or four characters, which is what fits beside a symbol. Taking `ident`
//! would give `KMMU` and `KEWR`: a third wider, for no extra meaning, on the axis where tags are
//! already fighting each other for room.

use anyhow::{Context, Result};

use crate::csvread::{field, Reader};
use crate::format::{
    Airport, FreqKind, Frequency, Kind, Runway, Tier, FLAG_HARD_SURFACE, FLAG_LIGHTED,
};

/// Regions that are US but not contiguous. Filtering by region rather than by a bounding box keeps
/// the edges honest — a box wide enough for the Florida Keys and Puget Sound also admits parts of
/// Canada and Mexico, which the FAA airspace layer already contributes enough of.
const NON_CONUS: [&str; 3] = ["US-AK", "US-HI", "US-U-A"];

/// A small airport needs at least this much hard runway to be drawn before the 5 nm ring. Roughly
/// the point below which a field is not an alternate for most of what this display is fitted to.
const PAVED_MIN_FT: u16 = 3000;

/// Surface codes that mean a hard runway. OurAirports surfaces are free text with many spellings —
/// `ASPH`, `ASPH-G`, `Asphalt`, `CON`, `CONC`, `PEM` — so this matches on the prefix after
/// upper-casing rather than trying to enumerate them.
const HARD_SURFACES: [&str; 6] = ["ASP", "CON", "PEM", "BIT", "TAR", "CEM"];

pub struct Stats {
    pub read: usize,
    pub kept: usize,
    pub dropped_non_conus: usize,
    pub dropped_closed: usize,
    pub dropped_unlabelled: usize,
    pub dropped_bad_position: usize,
    pub with_frequencies: usize,
    pub with_runway_headings: usize,
    pub frequencies: usize,
    pub runway_headings: usize,
}

/// Parse all three files and produce the records to write.
pub fn parse(
    airports_csv: &str,
    runways_csv: &str,
    frequencies_csv: &str,
) -> Result<(Vec<Airport>, Stats)> {
    let runways = runway_index(runways_csv)?;
    let frequencies = frequency_index(frequencies_csv)?;

    let reader = Reader::parse(airports_csv).context("parsing airports.csv")?;
    let c_ident = reader.column("ident")?;
    let c_name = reader.column("name")?;
    let c_type = reader.column("type")?;
    let c_lat = reader.column("latitude_deg")?;
    let c_lon = reader.column("longitude_deg")?;
    let c_elev = reader.column("elevation_ft")?;
    let c_country = reader.column("iso_country")?;
    let c_region = reader.column("iso_region")?;
    let c_gps = reader.column("gps_code")?;
    let c_local = reader.column("local_code")?;

    let mut out = Vec::new();
    let mut stats = Stats {
        read: 0,
        kept: 0,
        dropped_non_conus: 0,
        dropped_closed: 0,
        dropped_unlabelled: 0,
        dropped_bad_position: 0,
        with_frequencies: 0,
        with_runway_headings: 0,
        frequencies: 0,
        runway_headings: 0,
    };

    for row in reader.rows() {
        stats.read += 1;

        if field(row, c_country) != "US" || NON_CONUS.contains(&field(row, c_region)) {
            stats.dropped_non_conus += 1;
            continue;
        }

        let Some(kind) = Kind::parse(field(row, c_type)) else {
            // `closed` and `balloonport`. A closed field still has a runway on the ground, but
            // drawing it invites planning around something that is not there any more.
            stats.dropped_closed += 1;
            continue;
        };

        let (Ok(lat), Ok(lon)) = (
            field(row, c_lat).parse::<f64>(),
            field(row, c_lon).parse::<f64>(),
        ) else {
            stats.dropped_bad_position += 1;
            continue;
        };

        let Some(label) = label(
            field(row, c_local),
            field(row, c_gps),
            field(row, c_ident),
        ) else {
            stats.dropped_unlabelled += 1;
            continue;
        };

        let ident = field(row, c_ident);
        let entry = runways
            .binary_search_by(|(id, _)| id.as_str().cmp(ident))
            .ok()
            .map(|i| &runways[i].1);
        let (longest, lighted, headings) = match entry {
            Some(e) => (e.longest_hard_ft, e.lighted, e.headings.clone()),
            None => (0, false, Vec::new()),
        };

        let mut flags = 0u8;
        if longest > 0 {
            flags |= FLAG_HARD_SURFACE;
        }
        if lighted {
            flags |= FLAG_LIGHTED;
        }

        let freqs = frequencies
            .binary_search_by(|(id, _)| id.as_str().cmp(ident))
            .ok()
            .map(|i| frequencies[i].1.clone())
            .unwrap_or_default();

        if !freqs.is_empty() {
            stats.with_frequencies += 1;
            stats.frequencies += freqs.len();
        }
        if !headings.is_empty() {
            stats.with_runway_headings += 1;
            stats.runway_headings += headings.len();
        }

        out.push(Airport {
            lat_e6: (lat * 1e6).round() as i32,
            lon_e6: (lon * 1e6).round() as i32,
            label,
            name: field(row, c_name).trim().to_string(),
            elevation_ft: field(row, c_elev).parse::<f64>().unwrap_or(0.0).round() as i16,
            runway_ft: longest,
            kind,
            tier: tier(kind, longest),
            flags,
            runways: headings,
            frequencies: freqs,
        });
        stats.kept += 1;
    }

    Ok((out, stats))
}

impl Kind {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "large_airport" => Some(Self::Large),
            "medium_airport" => Some(Self::Medium),
            "small_airport" => Some(Self::Small),
            "heliport" => Some(Self::Heliport),
            "seaplane_base" => Some(Self::Seaplane),
            // "closed", "balloonport", and anything added later. Skipping the unknown is right
            // here: a new type appearing should not silently join tier 2 and clutter the display.
            _ => None,
        }
    }
}

/// Which range band an airport first appears in.
fn tier(kind: Kind, hard_runway_ft: u16) -> Tier {
    match kind {
        Kind::Large | Kind::Medium => Tier::Major,
        Kind::Small if hard_runway_ft >= PAVED_MIN_FT => Tier::Paved,
        Kind::Small | Kind::Seaplane => Tier::Minor,
        Kind::Heliport => Tier::Heliport,
    }
}

/// The shortest identifier a pilot would recognise, or `None` when there is not one.
fn label(local_code: &str, gps_code: &str, ident: &str) -> Option<String> {
    for candidate in [local_code, gps_code, ident] {
        let candidate = candidate.trim();
        // `US-10378` and friends are OurAirports' internal placeholders, not identifiers.
        if candidate.is_empty() || candidate.starts_with("US-") {
            continue;
        }
        if candidate.len() <= crate::format::LABEL_LEN {
            return Some(candidate.to_ascii_uppercase());
        }
    }
    None
}

/// What the runway file says about one airport.
#[derive(Debug, Default, Clone)]
struct RunwayFacts {
    longest_hard_ft: u16,
    lighted: bool,
    /// One entry per distinct orientation, longest first.
    headings: Vec<Runway>,
}

/// Runway facts per airport, as a list sorted by identifier for binary search.
///
/// A sorted list rather than a map so the build has no hash iteration in it anywhere — `write`
/// already sorts for reproducibility, and this keeps the whole pipeline deterministic without
/// depending on that.
fn runway_index(runways_csv: &str) -> Result<Vec<(String, RunwayFacts)>> {
    let reader = Reader::parse(runways_csv).context("parsing runways.csv")?;
    let c_airport = reader.column("airport_ident")?;
    let c_length = reader.column("length_ft")?;
    let c_surface = reader.column("surface")?;
    let c_closed = reader.column("closed")?;
    let c_lighted = reader.column("lighted")?;
    let c_le = reader.column("le_ident")?;

    let mut out: Vec<(String, RunwayFacts)> = Vec::new();
    for row in reader.rows() {
        if field(row, c_closed) == "1" {
            continue;
        }
        let length = field(row, c_length).parse::<f64>().unwrap_or(0.0);
        let length = if (0.0..=65_535.0).contains(&length) {
            length.round() as u16
        } else {
            0
        };
        let hard = is_hard(field(row, c_surface));
        let lighted = field(row, c_lighted) == "1";
        let ident = field(row, c_airport);

        let index = match out.binary_search_by(|(id, _)| id.as_str().cmp(ident)) {
            Ok(i) => i,
            Err(i) => {
                out.insert(i, (ident.to_string(), RunwayFacts::default()));
                i
            }
        };
        let facts = &mut out[index].1;

        if hard {
            facts.longest_hard_ft = facts.longest_hard_ft.max(length);
            facts.lighted |= lighted;
        }

        // Orientation comes from the identifier, not from `le_heading_degT`. The heading column is
        // populated for under a third of runways; the identifier is populated for all of them, and
        // carries the same answer to 10 degrees — which is finer than a tick a few pixels long can
        // show anyway.
        if let Some(heading) = heading_from_ident(field(row, c_le)) {
            // Parallel runways share an orientation, and 9L/9R drawn on top of each other is one
            // tick that cost twice. Reciprocals collapse too: 05 and 23 are the same line.
            match facts
                .headings
                .iter_mut()
                .find(|r| same_line(r.heading_deg, heading))
            {
                Some(existing) => existing.length_ft = existing.length_ft.max(length),
                None => facts.headings.push(Runway {
                    heading_deg: heading,
                    length_ft: length,
                }),
            }
        }
    }

    // Longest first, so a renderer that draws only the first few draws the ones that matter.
    for (_, facts) in out.iter_mut() {
        facts
            .headings
            .sort_by(|a, b| b.length_ft.cmp(&a.length_ft).then(a.heading_deg.cmp(&b.heading_deg)));
    }
    Ok(out)
}

/// Whether two headings describe the same strip of tarmac, i.e. are equal or reciprocal.
fn same_line(a: u16, b: u16) -> bool {
    a % 180 == b % 180
}

/// Runway heading in degrees from its identifier.
///
/// `"5"` is 050, `"19"` is 190, and the L/R/C/W/G suffixes on parallel runways are ignored. Turf
/// strips and seaplane lanes are often named by compass point instead — 1,041 of them in CONUS —
/// so those are handled too. Helipads (`H1`) have no orientation and yield `None`.
pub fn heading_from_ident(ident: &str) -> Option<u16> {
    let s = ident.trim().to_ascii_uppercase();
    if s.is_empty() {
        return None;
    }

    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() {
        let n: u16 = digits.parse().ok()?;
        // 01 through 36. Anything else is not a runway number, whatever else it might be.
        if (1..=36).contains(&n) {
            return Some(n * 10 % 360);
        }
        return None;
    }

    match s.as_str() {
        "N" => Some(0),
        "NE" => Some(45),
        "E" => Some(90),
        "SE" => Some(135),
        "S" => Some(180),
        "SW" => Some(225),
        "W" => Some(270),
        "NW" => Some(315),
        _ => None,
    }
}

/// Communication frequencies per airport, sorted by identifier for binary search.
///
/// Only about 18% of CONUS airports have any. That is not a gap to apologise for: the ones that do
/// are the ones with somebody to talk to.
fn frequency_index(frequencies_csv: &str) -> Result<Vec<(String, Vec<Frequency>)>> {
    let reader = Reader::parse(frequencies_csv).context("parsing airport-frequencies.csv")?;
    let c_airport = reader.column("airport_ident")?;
    let c_type = reader.column("type")?;
    let c_mhz = reader.column("frequency_mhz")?;

    let mut out: Vec<(String, Vec<Frequency>)> = Vec::new();
    for row in reader.rows() {
        // Stored in kHz as an integer. Megahertz as a float would make 121.975 — a real 25 kHz
        // channel — into something that formats as 121.97 or 121.98 depending on the wind.
        let Ok(mhz) = field(row, c_mhz).trim().parse::<f64>() else {
            continue;
        };
        if !(50.0..=400.0).contains(&mhz) {
            continue;
        }
        let khz = (mhz * 1000.0).round() as u32;
        let kind = FreqKind::parse(field(row, c_type));
        let ident = field(row, c_airport);

        let index = match out.binary_search_by(|(id, _)| id.as_str().cmp(ident)) {
            Ok(i) => i,
            Err(i) => {
                out.insert(i, (ident.to_string(), Vec::new()));
                i
            }
        };
        let list = &mut out[index].1;
        // The same frequency is often listed twice under different names — CTAF and UNICOM on a
        // non-towered field are usually one radio. Keep the more specific kind.
        if let Some(existing) = list.iter_mut().find(|f| f.khz == khz) {
            existing.kind = existing.kind.preferred(kind);
            continue;
        }
        list.push(Frequency { khz, kind });
    }

    // Most useful first, so a card with room for four shows the four that matter.
    for (_, list) in out.iter_mut() {
        list.sort_by(|a, b| a.kind.cmp(&b.kind).then(a.khz.cmp(&b.khz)));
    }
    Ok(out)
}

fn is_hard(surface: &str) -> bool {
    let upper = surface.trim().to_ascii_uppercase();
    HARD_SURFACES.iter().any(|prefix| upper.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    const AIRPORTS_HEADER: &str = "id,ident,type,name,latitude_deg,longitude_deg,elevation_ft,\
continent,iso_country,iso_region,municipality,scheduled_service,icao_code,iata_code,gps_code,\
local_code,home_link,wikipedia_link,keywords\n";

    const RUNWAYS_HEADER: &str = "id,airport_ref,airport_ident,length_ft,width_ft,surface,lighted,\
closed,le_ident,le_latitude_deg,le_longitude_deg,le_elevation_ft,le_heading_degT,\
le_displaced_threshold_ft,he_ident,he_latitude_deg,he_longitude_deg,he_elevation_ft,\
he_heading_degT,he_displaced_threshold_ft\n";

    fn airports(rows: &str) -> String {
        format!("{AIRPORTS_HEADER}{rows}")
    }

    fn runways(rows: &str) -> String {
        format!("{RUNWAYS_HEADER}{rows}")
    }

    const FREQ_HEADER: &str =
        "id,airport_ref,airport_ident,type,description,frequency_mhz\n";

    fn frequencies(rows: &str) -> String {
        format!("{FREQ_HEADER}{rows}")
    }

    #[test]
    fn a_towered_field_keeps_its_local_code_not_its_icao_ident() {
        // MMU, not KMMU. See the module note: the shorter label is what fits beside a symbol.
        let a = airports(
            "1,\"KMMU\",\"medium_airport\",\"Morristown Municipal\",40.799,-74.415,187,\"NA\",\
\"US\",\"US-NJ\",\"Morristown\",\"no\",,\"MMU\",\"KMMU\",\"MMU\",,,\n",
        );
        let r = runways(
            "9,1,\"KMMU\",5999,150,\"ASPH\",1,0,\"5\",,,,,,\"23\",,,,,\n",
        );
        let (out, stats) = parse(&a, &r, &frequencies("")).unwrap();
        assert_eq!(stats.kept, 1);
        assert_eq!(out[0].label, "MMU");
        assert_eq!(out[0].runway_ft, 5999);
        assert_eq!(out[0].tier, Tier::Major);
        assert_eq!(out[0].flags & FLAG_HARD_SURFACE, FLAG_HARD_SURFACE);
        assert_eq!(out[0].flags & FLAG_LIGHTED, FLAG_LIGHTED);
        assert_eq!(out[0].lat_e6, 40_799_000);
        assert_eq!(out[0].lon_e6, -74_415_000);
    }

    #[test]
    fn alaska_and_hawaii_are_not_conus() {
        let a = airports(
            "1,\"PANC\",\"large_airport\",\"Anchorage\",61.17,-149.99,152,\"NA\",\"US\",\"US-AK\",\
\"Anchorage\",\"yes\",,\"ANC\",\"PANC\",\"ANC\",,,\n\
2,\"PHNL\",\"large_airport\",\"Honolulu\",21.31,-157.92,13,\"OC\",\"US\",\"US-HI\",\"Honolulu\",\
\"yes\",,\"HNL\",\"PHNL\",\"HNL\",,,\n\
3,\"KEWR\",\"large_airport\",\"Newark\",40.692,-74.169,18,\"NA\",\"US\",\"US-NJ\",\"Newark\",\
\"yes\",,\"EWR\",\"KEWR\",\"EWR\",,,\n",
        );
        let (out, stats) = parse(&a, &runways(""), &frequencies("")).unwrap();
        assert_eq!(stats.kept, 1);
        assert_eq!(stats.dropped_non_conus, 2);
        assert_eq!(out[0].label, "EWR");
    }

    #[test]
    fn placeholder_identifiers_are_dropped_rather_than_drawn() {
        // A symbol labelled US-10378 tells a pilot nothing and takes the space of one that would.
        let a = airports(
            "1,\"US-10378\",\"small_airport\",\"Private strip\",40.1,-74.1,300,\"NA\",\"US\",\
\"US-NJ\",,\"no\",,,,,,,\n",
        );
        let (out, stats) = parse(&a, &runways(""), &frequencies("")).unwrap();
        assert!(out.is_empty());
        assert_eq!(stats.dropped_unlabelled, 1);
    }

    #[test]
    fn a_placeholder_ident_still_yields_a_label_when_a_real_code_exists() {
        // The fallback order matters: the placeholder is in `ident`, but this field does have a
        // local code, and dropping it would lose a real airport.
        let a = airports(
            "1,\"US-11086\",\"small_airport\",\"Somewhere\",40.1,-74.1,300,\"NA\",\"US\",\"US-NJ\",\
,\"no\",,,,\"7N7\",,,\n",
        );
        let (out, _) = parse(&a, &runways(""), &frequencies("")).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].label, "7N7");
    }

    #[test]
    fn closed_fields_and_balloonports_do_not_reach_the_file() {
        let a = airports(
            "1,\"XX1\",\"closed\",\"Gone\",40.1,-74.1,300,\"NA\",\"US\",\"US-NJ\",,\"no\",,,,\
\"XX1\",,,\n\
2,\"XX2\",\"balloonport\",\"Balloons\",40.2,-74.2,300,\"NA\",\"US\",\"US-NJ\",,\"no\",,,,\
\"XX2\",,,\n",
        );
        let (out, stats) = parse(&a, &runways(""), &frequencies("")).unwrap();
        assert!(out.is_empty());
        assert_eq!(stats.dropped_closed, 2);
    }

    #[test]
    fn heliports_are_carried_but_in_their_own_tier() {
        // 287 of them fall within 10 nm of downtown Los Angeles, against a fixed-wing worst case
        // of 35 anywhere in the country. They ship in the file and are not drawn by default, so
        // the tier has to be distinguishable rather than merely last.
        let a = airports(
            "1,\"00A\",\"heliport\",\"Total RF\",40.07,-74.93,11,\"NA\",\"US\",\"US-PA\",\
\"Bensalem\",\"no\",,,\"K00A\",\"00A\",,,\n",
        );
        let (out, _) = parse(&a, &runways(""), &frequencies("")).unwrap();
        assert_eq!(out[0].tier, Tier::Heliport);
        assert_eq!(out[0].kind, Kind::Heliport);
    }

    #[test]
    fn a_small_field_is_promoted_only_by_a_long_hard_runway() {
        let a = airports(
            "1,\"AAA\",\"small_airport\",\"Paved\",40.1,-74.1,300,\"NA\",\"US\",\"US-NJ\",,\"no\",\
,,,\"AAA\",,,\n\
2,\"BBB\",\"small_airport\",\"Short\",40.2,-74.2,300,\"NA\",\"US\",\"US-NJ\",,\"no\",,,,\"BBB\",,,\n\
3,\"CCC\",\"small_airport\",\"Grass\",40.3,-74.3,300,\"NA\",\"US\",\"US-NJ\",,\"no\",,,,\"CCC\",,,\n",
        );
        let r = runways(
            "1,1,\"AAA\",4200,75,\"ASPH\",1,0,\"9\",,,,,,\"27\",,,,,\n\
2,2,\"BBB\",1800,50,\"CONC\",0,0,\"9\",,,,,,\"27\",,,,,\n\
3,3,\"CCC\",4500,100,\"TURF\",0,0,\"9\",,,,,,\"27\",,,,,\n",
        );
        let (out, _) = parse(&a, &r, &frequencies("")).unwrap();
        let by = |l: &str| out.iter().find(|a| a.label == l).unwrap();
        assert_eq!(by("AAA").tier, Tier::Paved, "4200 ft of asphalt");
        assert_eq!(by("BBB").tier, Tier::Minor, "1800 ft is below the threshold");
        assert_eq!(by("CCC").tier, Tier::Minor, "turf is not a hard surface");
        assert_eq!(by("CCC").runway_ft, 0, "a soft runway contributes no length");
    }

    #[test]
    fn the_longest_runway_wins_and_a_closed_one_does_not_count() {
        let a = airports(
            "1,\"AAA\",\"small_airport\",\"Two runways\",40.1,-74.1,300,\"NA\",\"US\",\"US-NJ\",,\
\"no\",,,,\"AAA\",,,\n",
        );
        let r = runways(
            "1,1,\"AAA\",3100,75,\"ASPH\",0,0,\"9\",,,,,,\"27\",,,,,\n\
2,1,\"AAA\",5200,75,\"ASPH\",1,0,\"18\",,,,,,\"36\",,,,,\n\
3,1,\"AAA\",8000,75,\"ASPH\",1,1,\"4\",,,,,,\"22\",,,,,\n",
        );
        let (out, _) = parse(&a, &r, &frequencies("")).unwrap();
        assert_eq!(out[0].runway_ft, 5200, "the 8000 ft runway is closed");
        assert_eq!(out[0].flags & FLAG_LIGHTED, FLAG_LIGHTED);
    }

    #[test]
    fn runway_orientation_comes_from_the_identifier() {
        // `le_heading_degT` is populated for under a third of runways; `le_ident` for all of them.
        // The identifier carries the same answer to 10 degrees, which is finer than a tick a few
        // pixels long can show.
        for (ident, want) in [
            ("5", Some(50)),
            ("05", Some(50)),
            ("19", Some(190)),
            ("36", Some(0)),
            ("9L", Some(90)),
            ("27R", Some(270)),
            ("13C", Some(130)),
            ("18W", Some(180)),
        ] {
            assert_eq!(heading_from_ident(ident), want, "{ident}");
        }
    }

    #[test]
    fn compass_named_strips_and_helipads_are_handled() {
        // 1,041 CONUS runways are named by compass point rather than by number — turf strips and
        // seaplane lanes. Helipads have no orientation at all and must not invent one.
        for (ident, want) in [
            ("N", Some(0)),
            ("NE", Some(45)),
            ("E", Some(90)),
            ("SW", Some(225)),
            ("NW", Some(315)),
        ] {
            assert_eq!(heading_from_ident(ident), want, "{ident}");
        }
        for ident in ["H1", "H", "", "  ", "ALL", "0", "37", "99"] {
            assert_eq!(heading_from_ident(ident), None, "{ident:?} is not a runway heading");
        }
    }

    #[test]
    fn parallel_and_reciprocal_runways_collapse_to_one_orientation() {
        // 9L and 9R are the same line drawn twice; so are 05 and 23. KORD has 11 runways and
        // roughly four orientations, and drawing eleven ticks would be a starburst.
        let a = airports(
            "1,\"KAAA\",\"medium_airport\",\"Parallel\",40.1,-74.1,300,\"NA\",\"US\",\"US-NJ\",,\
\"no\",,,,\"AAA\",,,\n",
        );
        let r = runways(
            "1,1,\"KAAA\",9000,150,\"ASPH\",1,0,\"9L\",,,,,,\"27R\",,,,,\n\
2,1,\"KAAA\",8000,150,\"ASPH\",1,0,\"9R\",,,,,,\"27L\",,,,,\n\
3,1,\"KAAA\",5000,100,\"ASPH\",1,0,\"23\",,,,,,\"5\",,,,,\n\
4,1,\"KAAA\",4000,100,\"ASPH\",1,0,\"5\",,,,,,\"23\",,,,,\n",
        );
        let (out, stats) = parse(&a, &r, &frequencies("")).unwrap();
        let headings: Vec<u16> = out[0].runways.iter().map(|r| r.heading_deg).collect();
        assert_eq!(headings.len(), 2, "got {headings:?}");
        assert!(headings.contains(&90));
        assert!(headings.iter().any(|h| *h == 50 || *h == 230));
        // Longest first, and the longest of a collapsed pair wins.
        assert_eq!(out[0].runways[0].heading_deg, 90);
        assert_eq!(out[0].runways[0].length_ft, 9000);
        assert_eq!(stats.runway_headings, 2);
    }

    #[test]
    fn frequencies_are_stored_in_kilohertz_and_ordered_by_usefulness() {
        // kHz as an integer, not MHz as a float: 121.975 is a real 25 kHz channel and would
        // format as 121.97 or 121.98 depending on which way the float landed.
        let a = airports(
            "1,\"KMMU\",\"medium_airport\",\"Morristown Municipal\",40.799,-74.415,187,\"NA\",\
\"US\",\"US-NJ\",\"Morristown\",\"no\",,\"MMU\",\"KMMU\",\"MMU\",,,\n",
        );
        let f = frequencies(
            "1,1,\"KMMU\",\"APP\",\"NEW YORK APP\",127.6\n\
2,1,\"KMMU\",\"ATIS\",\"ATIS\",124.25\n\
3,1,\"KMMU\",\"CTAF\",\"CTAF\",118.1\n\
4,1,\"KMMU\",\"GND\",\"GND\",121.975\n",
        );
        let (out, stats) = parse(&a, &runways(""), &f).unwrap();
        let got: Vec<(u32, FreqKind)> =
            out[0].frequencies.iter().map(|f| (f.khz, f.kind)).collect();
        assert_eq!(
            got,
            vec![
                (118_100, FreqKind::Ctaf),
                (121_975, FreqKind::Ground),
                (124_250, FreqKind::Atis),
                (127_600, FreqKind::Approach),
            ]
        );
        assert_eq!(stats.with_frequencies, 1);
        assert_eq!(stats.frequencies, 4);
    }

    #[test]
    fn one_radio_listed_twice_is_stored_once_under_the_more_specific_name() {
        // At a non-towered field CTAF and UNICOM are usually the same radio, and both rows exist.
        // Two identical numbers on a card of four lines wastes a quarter of it.
        let a = airports(
            "1,\"KAAA\",\"small_airport\",\"Non-towered\",40.1,-74.1,300,\"NA\",\"US\",\"US-NJ\",,\
\"no\",,,,\"AAA\",,,\n",
        );
        let f = frequencies(
            "1,1,\"KAAA\",\"UNIC\",\"UNICOM\",122.8\n\
2,1,\"KAAA\",\"CTAF\",\"CTAF\",122.8\n",
        );
        let (out, _) = parse(&a, &runways(""), &f).unwrap();
        assert_eq!(out[0].frequencies.len(), 1);
        assert_eq!(out[0].frequencies[0].kind, FreqKind::Ctaf, "CTAF beats UNICOM");
    }

    #[test]
    fn a_towered_fields_shared_frequency_is_labelled_tower_not_ctaf() {
        // Rocky Mountain Metro publishes 118.6 twice, as TWR and as CTAF — they are one radio.
        // Labelling a live tower frequency "CTAF" invites self-announcing on it, so the more
        // specific name wins even though CTAF sorts first for display.
        let a = airports(
            "1,\"KBJC\",\"medium_airport\",\"Rocky Mountain Metro\",39.9,-105.1,5673,\"NA\",\
\"US\",\"US-CO\",\"Denver\",\"no\",,,\"KBJC\",\"BJC\",,,\n",
        );
        let f = frequencies(
            "1,1,\"KBJC\",\"CTAF\",\"CTAF\",118.6\n\
2,1,\"KBJC\",\"TWR\",\"TWR\",118.6\n",
        );
        let (out, _) = parse(&a, &runways(""), &f).unwrap();
        assert_eq!(out[0].frequencies.len(), 1);
        assert_eq!(out[0].frequencies[0].kind, FreqKind::Tower);
        // Order of the two rows must not change the answer.
        let g = frequencies(
            "1,1,\"KBJC\",\"TWR\",\"TWR\",118.6\n\
2,1,\"KBJC\",\"CTAF\",\"CTAF\",118.6\n",
        );
        let (out, _) = parse(&a, &runways(""), &g).unwrap();
        assert_eq!(out[0].frequencies[0].kind, FreqKind::Tower);
    }

    #[test]
    fn airport_advisory_and_centre_are_named_rather_than_lumped_into_other() {
        // A/D is the second most common type in the file — 1,544 CONUS rows — and at BJC it is
        // "DENVER APP/DEP" on 126.1. Left as Other it would reach the card as a bare number with
        // no label, which is worse than not showing it.
        let a = airports(
            "1,\"KBJC\",\"medium_airport\",\"Rocky Mountain Metro\",39.9,-105.1,5673,\"NA\",\
\"US\",\"US-CO\",\"Denver\",\"no\",,,\"KBJC\",\"BJC\",,,\n",
        );
        let f = frequencies(
            "1,1,\"KBJC\",\"A/D\",\"DENVER APP/DEP\",126.1\n\
2,1,\"KBJC\",\"CNTR\",\"DENVER CENTER\",127.05\n",
        );
        let (out, _) = parse(&a, &runways(""), &f).unwrap();
        let kinds: Vec<FreqKind> = out[0].frequencies.iter().map(|f| f.kind).collect();
        assert_eq!(kinds, vec![FreqKind::Advisory, FreqKind::Center]);
        assert!(kinds.iter().all(|k| !k.label().is_empty()), "both must be labelled");
    }

    #[test]
    fn an_out_of_band_frequency_is_dropped_rather_than_shown() {
        let a = airports(
            "1,\"KAAA\",\"small_airport\",\"Odd\",40.1,-74.1,300,\"NA\",\"US\",\"US-NJ\",,\"no\",\
,,,\"AAA\",,,\n",
        );
        let f = frequencies(
            "1,1,\"KAAA\",\"CTAF\",\"CTAF\",0\n\
2,1,\"KAAA\",\"TWR\",\"TWR\",8118.1\n\
3,1,\"KAAA\",\"GND\",\"GND\",not-a-number\n\
4,1,\"KAAA\",\"ATIS\",\"ATIS\",124.25\n",
        );
        let (out, _) = parse(&a, &runways(""), &f).unwrap();
        assert_eq!(out[0].frequencies.len(), 1);
        assert_eq!(out[0].frequencies[0].khz, 124_250);
    }

    #[test]
    fn the_name_is_carried_for_the_inspect_card() {
        let a = airports(
            "1,\"KMMU\",\"medium_airport\",\"Morristown Municipal Airport\",40.799,-74.415,187,\
\"NA\",\"US\",\"US-NJ\",\"Morristown\",\"no\",,\"MMU\",\"KMMU\",\"MMU\",,,\n",
        );
        let (out, _) = parse(&a, &runways(""), &frequencies("")).unwrap();
        assert_eq!(out[0].name, "Morristown Municipal Airport");
        assert_eq!(out[0].label, "MMU", "the label is still the short one");
    }

    #[test]
    fn an_airport_with_no_frequencies_gets_an_empty_list_not_its_neighbours() {
        let a = airports(
            "1,\"KAAA\",\"small_airport\",\"Has one\",40.1,-74.1,300,\"NA\",\"US\",\"US-NJ\",,\
\"no\",,,,\"AAA\",,,\n\
2,\"KBBB\",\"small_airport\",\"Has none\",40.2,-74.2,300,\"NA\",\"US\",\"US-NJ\",,\"no\",,,,\
\"BBB\",,,\n",
        );
        let f = frequencies("1,1,\"KAAA\",\"CTAF\",\"CTAF\",122.8\n");
        let (out, stats) = parse(&a, &runways(""), &f).unwrap();
        let by = |l: &str| out.iter().find(|a| a.label == l).unwrap();
        assert_eq!(by("AAA").frequencies.len(), 1);
        assert!(by("BBB").frequencies.is_empty());
        assert_eq!(stats.with_frequencies, 1);
    }

    #[test]
    fn surface_spellings_are_matched_by_prefix() {
        for surface in ["ASPH", "ASPH-G", "Asphalt", "CON", "CONC", "PEM", "BIT", "TAR"] {
            assert!(is_hard(surface), "{surface} should be hard");
        }
        for surface in ["TURF", "GRVL", "GRASS", "WATER", "DIRT", "SAND", ""] {
            assert!(!is_hard(surface), "{surface} should not be hard");
        }
    }

    #[test]
    fn a_row_with_no_position_is_dropped_not_placed_at_null_island() {
        // The failure this prevents is visible and absurd — a cluster of airports off West Africa —
        // but only if someone happens to pan there, and this display cannot pan.
        let a = airports(
            "1,\"AAA\",\"small_airport\",\"No position\",,,300,\"NA\",\"US\",\"US-NJ\",,\"no\",,,,\
\"AAA\",,,\n",
        );
        let (out, stats) = parse(&a, &runways(""), &frequencies("")).unwrap();
        assert!(out.is_empty());
        assert_eq!(stats.dropped_bad_position, 1);
    }
}
