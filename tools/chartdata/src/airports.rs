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
use crate::format::{Airport, Kind, Tier, FLAG_HARD_SURFACE, FLAG_LIGHTED};

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
}

/// Parse both files and produce the records to write.
pub fn parse(airports_csv: &str, runways_csv: &str) -> Result<(Vec<Airport>, Stats)> {
    let runways = longest_hard_runway(runways_csv)?;

    let reader = Reader::parse(airports_csv).context("parsing airports.csv")?;
    let c_ident = reader.column("ident")?;
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
        let runway = runways
            .iter()
            .find(|(id, _, _)| id == ident)
            .map(|(_, ft, lit)| (*ft, *lit))
            .unwrap_or((0, false));

        let mut flags = 0u8;
        if runway.0 > 0 {
            flags |= FLAG_HARD_SURFACE;
        }
        if runway.1 {
            flags |= FLAG_LIGHTED;
        }

        out.push(Airport {
            lat_e6: (lat * 1e6).round() as i32,
            lon_e6: (lon * 1e6).round() as i32,
            label,
            elevation_ft: field(row, c_elev).parse::<f64>().unwrap_or(0.0).round() as i16,
            runway_ft: runway.0,
            kind,
            tier: tier(kind, runway.0),
            flags,
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

/// Longest open hard-surface runway per airport, and whether it is lit.
///
/// Returned as a sorted list rather than a map so the build has no hash iteration in it anywhere —
/// `write` already sorts for reproducibility, and this keeps the whole pipeline deterministic
/// without depending on that.
fn longest_hard_runway(runways_csv: &str) -> Result<Vec<(String, u16, bool)>> {
    let reader = Reader::parse(runways_csv).context("parsing runways.csv")?;
    let c_airport = reader.column("airport_ident")?;
    let c_length = reader.column("length_ft")?;
    let c_surface = reader.column("surface")?;
    let c_closed = reader.column("closed")?;
    let c_lighted = reader.column("lighted")?;

    let mut out: Vec<(String, u16, bool)> = Vec::new();
    for row in reader.rows() {
        if field(row, c_closed) == "1" {
            continue;
        }
        if !is_hard(field(row, c_surface)) {
            continue;
        }
        let length = field(row, c_length).parse::<f64>().unwrap_or(0.0);
        if !(0.0..=65_535.0).contains(&length) {
            continue;
        }
        let length = length.round() as u16;
        let lighted = field(row, c_lighted) == "1";
        let ident = field(row, c_airport).to_string();

        match out.iter_mut().find(|(id, _, _)| *id == ident) {
            Some(entry) => {
                if length > entry.1 {
                    entry.1 = length;
                }
                entry.2 |= lighted;
            }
            None => out.push((ident, length, lighted)),
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
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
        let (out, stats) = parse(&a, &r).unwrap();
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
        let (out, stats) = parse(&a, &runways("")).unwrap();
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
        let (out, stats) = parse(&a, &runways("")).unwrap();
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
        let (out, _) = parse(&a, &runways("")).unwrap();
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
        let (out, stats) = parse(&a, &runways("")).unwrap();
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
        let (out, _) = parse(&a, &runways("")).unwrap();
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
        let (out, _) = parse(&a, &r).unwrap();
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
        let (out, _) = parse(&a, &r).unwrap();
        assert_eq!(out[0].runway_ft, 5200, "the 8000 ft runway is closed");
        assert_eq!(out[0].flags & FLAG_LIGHTED, FLAG_LIGHTED);
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
        let (out, stats) = parse(&a, &runways("")).unwrap();
        assert!(out.is_empty());
        assert_eq!(stats.dropped_bad_position, 1);
    }
}
