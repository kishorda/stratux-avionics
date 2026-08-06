//! Turning the FAA's own airport and runway layers into airport records.
//!
//! Replaces OurAirports for everything except communication frequencies. The two sources were
//! compared field by field before the switch; the reasoning is in `docs/free-aviation-data.md`
//! and the short version is:
//!
//! * **Currency.** These layers carry the same 28-day AIRAC cycle as the airspace, so the whole
//!   file has one effective date instead of "the airspace is from July and the airports are from
//!   whenever somebody last edited them".
//! * **The ICAO identifier is real.** OurAirports' `gps_code` is the local code repeated for most
//!   fields — `ID15`, `WN43` — which cannot match a METAR and never will. Against the stations
//!   actually reporting across CONUS, the FAA identifier matched 362 of 400 and OurAirports 334,
//!   using 2,847 strings instead of 20,056.
//! * **Enums instead of free text.** `COMP_CODE` has thirty values; OurAirports' surface column
//!   has **564** distinct spellings of things like "asphalt", which the old build prefix-matched.
//! * **Stated rather than inferred.** `OPERSTATUS` and `PRIVATEUSE` say what the old code guessed
//!   at from a `type` string, and `DESIGNATOR` gives `05/23` outright rather than being derived.
//!
//! # The join
//!
//! `Runways.AIRPORT_ID` is a GUID matching `US_Airport.GLOBAL_ID`. Frequencies still come from
//! OurAirports and join on the *local* identifier, which both sources agree on.

use std::collections::HashMap;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::format::{Airport, Frequency, Kind, Runway, Tier, FLAG_HARD_SURFACE, FLAG_LIGHTED};

/// Airports are filtered by the same box the airspace uses, not by a list of state codes.
///
/// A state list looked cleaner and was wrong: the FAA layer carries Pago Pago, Diego Garcia,
/// Ascension, Palau, Kwajalein and 56 more whose codes were not on it, and the only symptom was
/// the bucket grid quietly ballooning from 26x58 cells to 92x350. A box cannot miss a territory
/// nobody thought of, and sharing it with [`crate::airspace::KEEP_BOX`] means the two layers agree
/// on where the file stops by construction rather than by coincidence.
use crate::airspace::KEEP_BOX;

/// Runway surfaces that count as hard.
///
/// A closed enum, checked exactly — unlike OurAirports' 564 free-text spellings, which had to be
/// prefix-matched and which this replaces. Mixed surfaces are included when the hard component
/// leads: `ASP+GRS` is asphalt with a grass shoulder, not the other way round.
const HARD_SURFACES: [&str; 15] = [
    "ASPH",
    "CONC",
    "ASP+DIRT",
    "ASP+GRS",
    "ASP+GRVL",
    "ASP+TRTD",
    "CONC+ASPH",
    "CONC+GRS",
    "CONC+GRVL",
    "CONC+TRTD",
    "PSP",
    "BRICK",
    "COMP",
    "MATS",
    "METAL",
];

/// A public aerodrome needs this much hard runway to be drawn at every range.
///
/// 1,784 airports, and a worst case of 16 on screen in the busiest 40 nm view anywhere in the
/// country. Dropping to 4,000 ft would be 2,508 and 21, which starts to crowd the widest range
/// where the symbols are smallest.
const MAJOR_MIN_FT: u16 = 5000;

// There is deliberately no minimum length for the Paved tier.
//
// There was, and it was wrong twice. At 3,000 ft it hid Somerset's lit 2,739 ft runway — reported
// as "KSMQ is not in the dataset" — and at 2,500 it hid Palo Alto's 2,443 ft one. Each fix moved
// the number and left the next field just below it.
//
// The rule that does not have a next field is the one with no threshold at all: a public-use
// aerodrome with a paved runway is worth drawing at 20 nm, however short the runway is. Measured
// across CONUS that costs exactly one extra symbol in the busiest 20 nm view, 14 to 15.

#[derive(Default)]
pub struct Stats {
    pub read: usize,
    pub kept: usize,
    pub dropped_non_conus: usize,
    pub dropped_not_operational: usize,
    pub dropped_no_position: usize,
    pub dropped_no_ident: usize,
    pub with_station: usize,
    pub with_frequencies: usize,
    pub with_runway_headings: usize,
    pub frequencies: usize,
    pub runway_headings: usize,
}

struct RunwayRec {
    designator: String,
    length_ft: u16,
    hard: bool,
    lighted: bool,
}

/// Parse the FAA layers, joining frequencies supplied separately.
pub fn parse(
    airport_pages: &[String],
    runway_pages: &[String],
    frequencies: &[(String, Vec<Frequency>)],
) -> Result<(Vec<Airport>, Stats)> {
    let runways = runway_index(runway_pages)?;
    let mut out = Vec::new();
    let mut stats = Stats::default();

    for (index, page) in airport_pages.iter().enumerate() {
        let value: Value =
            serde_json::from_str(page).with_context(|| format!("parsing airport page {index}"))?;
        for feature in value
            .get("features")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[])
        {
            stats.read += 1;
            let a = feature.get("attributes").unwrap_or(&Value::Null);

            // Stated, not inferred. The old build worked this out from a `type` of "closed", which
            // says nothing about a field that is closed indefinitely but still in the database.
            if text(a, "OPERSTATUS") != "OPERATIONAL" {
                stats.dropped_not_operational += 1;
                continue;
            }

            let geometry = feature.get("geometry").unwrap_or(&Value::Null);
            let (Some(lon), Some(lat)) = (
                geometry.get("x").and_then(Value::as_f64),
                geometry.get("y").and_then(Value::as_f64),
            ) else {
                stats.dropped_no_position += 1;
                continue;
            };
            if !(KEEP_BOX.0..=KEEP_BOX.1).contains(&lat)
                || !(KEEP_BOX.2..=KEEP_BOX.3).contains(&lon)
            {
                stats.dropped_non_conus += 1;
                continue;
            }

            let label = text(a, "IDENT");
            if label.is_empty() || label.len() > crate::format::LABEL_LEN {
                stats.dropped_no_ident += 1;
                continue;
            }

            let global_id = text(a, "GLOBAL_ID");
            let runway_facts = runways.get(&global_id);
            let longest = runway_facts.map_or(0, |r| r.longest_hard);
            let lighted = runway_facts.is_some_and(|r| r.lighted);
            let headings = runway_facts.map(|r| r.headings.clone()).unwrap_or_default();

            let type_code = text(a, "TYPE_CODE");
            let private = a.get("PRIVATEUSE").and_then(Value::as_i64).unwrap_or(1) != 0;
            let military = !matches!(text(a, "MIL_CODE").as_str(), "CIVIL" | "");
            let kind = kind_for(&type_code, longest);
            let tier = tier_for(kind, &type_code, private, military, longest);

            // The real ICAO identifier, empty where the field has none. Not derived, not guessed:
            // that is the whole reason this source replaced the last one.
            let station = text(a, "ICAO_ID");
            if !station.is_empty() {
                stats.with_station += 1;
            }

            let freqs = frequencies
                .binary_search_by(|(id, _)| id.as_str().cmp(&label))
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

            let mut flags = 0u8;
            if longest > 0 {
                flags |= FLAG_HARD_SURFACE;
            }
            if lighted {
                flags |= FLAG_LIGHTED;
            }

            out.push(Airport {
                lat_e6: (lat * 1e6).round() as i32,
                lon_e6: (lon * 1e6).round() as i32,
                label,
                station,
                name: text(a, "NAME"),
                elevation_ft: a
                    .get("ELEVATION")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0)
                    .round() as i16,
                runway_ft: longest,
                kind,
                tier,
                flags,
                // Filled by a later pass, which is where the effective date lives. The FAA layer
                // does not carry variation — checked: `US_Airport` has 26 fields and none of them
                // is magnetic, and `Runways` carries no bearing at all — so it is modelled rather
                // than read. See `crate::variation`.
                mag_var_deg: 0,
                runways: headings,
                frequencies: freqs,
            });
            stats.kept += 1;
        }
    }

    // Sorted so the build is reproducible regardless of the order pages were fetched in.
    out.sort_by(|a, b| {
        a.label
            .cmp(&b.label)
            .then_with(|| a.lat_e6.cmp(&b.lat_e6))
            .then_with(|| a.lon_e6.cmp(&b.lon_e6))
    });
    Ok((out, stats))
}

/// Size band, kept only because the format carries it. Nothing is drawn from it — [`Tier`] decides
/// what appears — so it is a plain proxy for how big the field is rather than a claim about class.
fn kind_for(type_code: &str, longest_hard_ft: u16) -> Kind {
    match type_code {
        "HP" => Kind::Heliport,
        "SP" => Kind::Seaplane,
        _ if longest_hard_ft >= 8000 => Kind::Large,
        _ if longest_hard_ft >= MAJOR_MIN_FT => Kind::Medium,
        _ => Kind::Small,
    }
}

/// Which range band the airport first appears in.
///
/// The rule is "aerodrome you would want to see, with enough hard runway", which is what the old
/// large/medium/small tiering was reaching for through a proxy. Private fields stay in the closest
/// band: they are not somewhere to plan a diversion, but they are still a runway.
///
/// # Military fields count as public here
///
/// 255 of the 276 military aerodromes in this data are flagged `PRIVATEUSE`, which is accurate —
/// you may not land at Edwards — and reading it literally demoted Edwards, Wright-Patterson,
/// Oceana and Vance to the 5 nm band. That is exactly backwards. A 15,000 ft military runway is
/// the most conspicuous thing for miles, it almost always has controlled airspace stacked on it,
/// and the reason to draw it is not that you might land there.
fn tier_for(
    kind: Kind,
    type_code: &str,
    private: bool,
    military: bool,
    longest_hard_ft: u16,
) -> Tier {
    if matches!(kind, Kind::Heliport) || type_code == "BP" {
        return Tier::Heliport;
    }
    if (private && !military) || type_code != "AD" {
        return Tier::Minor;
    }
    match longest_hard_ft {
        ft if ft >= MAJOR_MIN_FT => Tier::Major,
        ft if ft > 0 => Tier::Paved,
        _ => Tier::Minor,
    }
}

struct RunwayFacts {
    longest_hard: u16,
    lighted: bool,
    headings: Vec<Runway>,
}

fn runway_index(pages: &[String]) -> Result<HashMap<String, RunwayFacts>> {
    let mut raw: HashMap<String, Vec<RunwayRec>> = HashMap::new();
    for (index, page) in pages.iter().enumerate() {
        let value: Value =
            serde_json::from_str(page).with_context(|| format!("parsing runway page {index}"))?;
        for feature in value
            .get("features")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[])
        {
            let a = feature.get("attributes").unwrap_or(&Value::Null);
            let airport = text(a, "AIRPORT_ID");
            if airport.is_empty() {
                continue;
            }
            // Every row in this layer is in feet, but the column exists, so it is checked. A
            // length silently read in the wrong unit is the same class of bug as the flight
            // levels in the airspace data.
            if !matches!(text(a, "DIM_UOM").as_str(), "FT" | "") {
                continue;
            }
            let length = a.get("LENGTH").and_then(Value::as_f64).unwrap_or(0.0);
            raw.entry(airport).or_default().push(RunwayRec {
                designator: text(a, "DESIGNATOR"),
                length_ft: if (0.0..=65_535.0).contains(&length) {
                    length.round() as u16
                } else {
                    0
                },
                hard: HARD_SURFACES.contains(&text(a, "COMP_CODE").as_str()),
                lighted: a.get("LIGHTACTV").and_then(Value::as_i64).unwrap_or(0) > 0,
            });
        }
    }

    let mut out = HashMap::with_capacity(raw.len());
    for (airport, recs) in raw {
        let mut facts = RunwayFacts {
            longest_hard: 0,
            lighted: false,
            headings: Vec::new(),
        };
        for rec in &recs {
            if rec.hard {
                facts.longest_hard = facts.longest_hard.max(rec.length_ft);
                facts.lighted |= rec.lighted;
            }
            let Some(heading) = heading_from_designator(&rec.designator) else {
                continue;
            };
            // Parallel and reciprocal runways are one line drawn twice.
            match facts
                .headings
                .iter_mut()
                .find(|r| r.heading_deg % 180 == heading % 180)
            {
                Some(existing) => existing.length_ft = existing.length_ft.max(rec.length_ft),
                None => facts.headings.push(Runway {
                    heading_deg: heading,
                    length_ft: rec.length_ft,
                }),
            }
        }
        facts.headings.sort_by(|a, b| {
            b.length_ft
                .cmp(&a.length_ft)
                .then(a.heading_deg.cmp(&b.heading_deg))
        });
        out.insert(airport, facts);
    }
    Ok(out)
}

/// Heading in degrees from a runway designator such as `05/23`, `9L/27R` or `NW/SE`.
///
/// The FAA gives the designator whole, so unlike the old build there is nothing to reconstruct
/// from one end's identifier — and the compass-named strips arrive already paired.
pub fn heading_from_designator(designator: &str) -> Option<u16> {
    let first = designator.split('/').next()?.trim().to_ascii_uppercase();
    if first.is_empty() {
        return None;
    }

    let digits: String = first.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() {
        let n: u16 = digits.parse().ok()?;
        return (1..=36).contains(&n).then_some(n * 10 % 360);
    }
    match first.as_str() {
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

fn text(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn airport_page(features: Vec<Value>) -> String {
        serde_json::json!({ "features": features }).to_string()
    }

    fn airport(ident: &str, icao: &str, state: &str, type_code: &str, private: i64) -> Value {
        serde_json::json!({
            "attributes": {
                "GLOBAL_ID": format!("GID-{ident}"),
                "IDENT": ident, "ICAO_ID": icao, "NAME": format!("{ident} Field"),
                "ELEVATION": 187.0, "TYPE_CODE": type_code, "STATE": state,
                "OPERSTATUS": "OPERATIONAL", "PRIVATEUSE": private, "IAPEXISTS": 1,
            },
            "geometry": { "x": -74.415, "y": 40.799 }
        })
    }

    fn runway(ident: &str, designator: &str, length: i64, comp: &str, lit: i64) -> Value {
        serde_json::json!({
            "attributes": {
                "AIRPORT_ID": format!("GID-{ident}"), "DESIGNATOR": designator,
                "LENGTH": length, "WIDTH": 100, "DIM_UOM": "FT",
                "COMP_CODE": comp, "LIGHTACTV": lit,
            }
        })
    }

    #[test]
    fn the_icao_identifier_is_taken_as_given_and_never_derived() {
        // The whole reason for this source. OurAirports' gps_code is the local code repeated for
        // most fields, which cannot match a METAR; here the column is either a real ICAO code or
        // empty, and empty is the honest answer.
        let pages = [airport_page(vec![
            airport("MMU", "KMMU", "NJ", "AD", 0),
            airport("7N7", "", "NJ", "AD", 0),
        ])];
        let (out, stats) = parse(&pages, &[], &[]).unwrap();
        let by = |l: &str| out.iter().find(|a| a.label == l).unwrap();
        assert_eq!(by("MMU").station, "KMMU");
        assert_eq!(by("7N7").station, "", "no invented K prefix");
        assert_eq!(stats.with_station, 1);
    }

    #[test]
    fn territories_outside_the_box_are_dropped_however_they_are_labelled() {
        // A state-code list missed Pago Pago, Diego Garcia, Palau and 58 others, and the only
        // symptom was the bucket grid growing from 26x58 to 92x350. The box cannot miss one.
        let mut samoa = airport("PPG", "NSTU", "AQ", "AD", 0);
        samoa["geometry"] = serde_json::json!({ "x": -170.71, "y": -14.33 });
        let mut diego = airport("FJDG", "FJDG", "", "AD", 0);
        diego["geometry"] = serde_json::json!({ "x": 72.41, "y": -7.31 });

        let pages = [airport_page(vec![
            airport("MMU", "KMMU", "NJ", "AD", 0),
            samoa,
            diego,
        ])];
        let (out, stats) = parse(&pages, &[], &[]).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].label, "MMU");
        assert_eq!(stats.dropped_non_conus, 2);
    }

    #[test]
    fn non_operational_and_non_conus_fields_are_dropped() {
        let mut closed = airport("XXX", "KXXX", "NJ", "AD", 0);
        closed["attributes"]["OPERSTATUS"] = serde_json::json!("CLOSED");
        let mut indefinite = airport("YYY", "KYYY", "NJ", "AD", 0);
        indefinite["attributes"]["OPERSTATUS"] = serde_json::json!("INDEFINITE");

        let mut anchorage = airport("ANC", "PANC", "AK", "AD", 0);
        anchorage["geometry"] = serde_json::json!({ "x": -149.99, "y": 61.17 });
        let pages = [airport_page(vec![
            airport("MMU", "KMMU", "NJ", "AD", 0),
            anchorage,
            closed,
            indefinite,
        ])];
        let (out, stats) = parse(&pages, &[], &[]).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].label, "MMU");
        assert_eq!(stats.dropped_non_conus, 1);
        assert_eq!(
            stats.dropped_not_operational, 2,
            "CLOSED and INDEFINITE both go"
        );
    }

    #[test]
    fn tiering_uses_public_use_and_hard_runway_rather_than_a_size_label() {
        let pages = [airport_page(vec![
            airport("BIG", "KBIG", "NJ", "AD", 0),
            airport("MID", "KMID", "NJ", "AD", 0),
            airport("SML", "KSML", "NJ", "AD", 0),
            airport("PVT", "", "NJ", "AD", 1),
            airport("HEL", "", "NJ", "HP", 0),
        ])];
        let rw = [airport_page(vec![
            runway("BIG", "05/23", 7000, "ASPH", 2),
            runway("MID", "05/23", 3000, "ASPH", 0),
            runway("SML", "05/23", 1800, "ASPH", 0),
            // A long paved runway does not promote a private field.
            runway("PVT", "09/27", 9000, "CONC", 2),
        ])];
        let (out, _) = parse(&pages, &rw, &[]).unwrap();
        let t = |l: &str| out.iter().find(|a| a.label == l).unwrap().tier;
        assert_eq!(t("BIG"), Tier::Major);
        assert_eq!(t("MID"), Tier::Paved);
        // Any paved runway reaches the Paved tier. A length threshold here was wrong twice —
        // Somerset at 2,739 ft and Palo Alto at 2,443 — so there is no longer one.
        assert_eq!(
            t("SML"),
            Tier::Paved,
            "1800 ft of asphalt is still a paved public field"
        );
        assert_eq!(t("PVT"), Tier::Minor, "private stays close in");
        assert_eq!(t("HEL"), Tier::Heliport);
    }

    #[test]
    fn a_military_field_is_drawn_even_though_it_is_not_public_use() {
        // 255 of 276 military aerodromes are flagged PRIVATEUSE, which is true and irrelevant:
        // reading it literally pushed Edwards, Wright-Patterson and Oceana into the 5 nm band.
        let mut edwards = airport("EDW", "KEDW", "CA", "AD", 1);
        edwards["attributes"]["MIL_CODE"] = serde_json::json!("MIL");
        let mut ranch = airport("PVT", "", "CA", "AD", 1);
        ranch["attributes"]["MIL_CODE"] = serde_json::json!("CIVIL");

        let pages = [airport_page(vec![edwards, ranch])];
        let rw = [airport_page(vec![
            runway("EDW", "04L/22R", 15000, "CONC", 2),
            runway("PVT", "09/27", 9000, "CONC", 2),
        ])];
        let (out, _) = parse(&pages, &rw, &[]).unwrap();
        let t = |l: &str| out.iter().find(|a| a.label == l).unwrap().tier;
        assert_eq!(
            t("EDW"),
            Tier::Major,
            "a military airfield is a landmark whatever its access"
        );
        assert_eq!(
            t("PVT"),
            Tier::Minor,
            "a private civil strip still stays close in"
        );
    }

    #[test]
    fn a_turf_runway_contributes_no_hard_length() {
        let pages = [airport_page(vec![airport("GRS", "", "NJ", "AD", 0)])];
        let rw = [airport_page(vec![
            runway("GRS", "09/27", 4200, "TURF+DIRT", 0),
            runway("GRS", "18/36", 2000, "GRASS", 0),
        ])];
        let (out, _) = parse(&pages, &rw, &[]).unwrap();
        assert_eq!(out[0].runway_ft, 0);
        assert_eq!(out[0].flags & FLAG_HARD_SURFACE, 0);
        assert_eq!(out[0].tier, Tier::Minor);
        // The orientations still count: a grass strip has a direction worth drawing.
        assert_eq!(out[0].runways.len(), 2);
    }

    #[test]
    fn designators_give_the_heading_without_reconstruction() {
        for (d, want) in [
            ("05/23", Some(50)),
            ("9L/27R", Some(90)),
            ("18/36", Some(180)),
            ("36/18", Some(0)),
            ("NW/SE", Some(315)),
            ("E/W", Some(90)),
            ("H1", None),
            ("", None),
            ("ALL/WAY", None),
        ] {
            assert_eq!(heading_from_designator(d), want, "{d}");
        }
    }

    #[test]
    fn parallel_and_reciprocal_runways_collapse() {
        let pages = [airport_page(vec![airport("ORD", "KORD", "IL", "AD", 0)])];
        let rw = [airport_page(vec![
            runway("ORD", "09L/27R", 7500, "CONC", 2),
            runway("ORD", "09R/27L", 8000, "CONC", 2),
            runway("ORD", "04L/22R", 6000, "ASPH", 2),
        ])];
        let (out, _) = parse(&pages, &rw, &[]).unwrap();
        let h: Vec<u16> = out[0].runways.iter().map(|r| r.heading_deg).collect();
        assert_eq!(h.len(), 2, "got {h:?}");
        assert_eq!(
            out[0].runways[0].length_ft, 8000,
            "longest of the pair wins"
        );
    }

    #[test]
    fn a_runway_in_an_unexpected_unit_is_ignored_rather_than_believed() {
        let pages = [airport_page(vec![airport("MET", "KMET", "NJ", "AD", 0)])];
        let mut odd = runway("MET", "05/23", 2000, "ASPH", 0);
        odd["attributes"]["DIM_UOM"] = serde_json::json!("M");
        let (out, _) = parse(&pages, &[airport_page(vec![odd])], &[]).unwrap();
        assert_eq!(
            out[0].runway_ft, 0,
            "2000 metres must not be read as 2000 feet"
        );
    }

    #[test]
    fn the_build_is_ordered_independently_of_page_order() {
        let a = airport("AAA", "KAAA", "NJ", "AD", 0);
        let b = airport("BBB", "KBBB", "NJ", "AD", 0);
        let one = parse(&[airport_page(vec![a.clone(), b.clone()])], &[], &[])
            .unwrap()
            .0;
        let two = parse(&[airport_page(vec![b]), airport_page(vec![a])], &[], &[])
            .unwrap()
            .0;
        let labels = |v: &[Airport]| v.iter().map(|x| x.label.clone()).collect::<Vec<_>>();
        assert_eq!(labels(&one), labels(&two));
        assert_eq!(labels(&one), vec!["AAA", "BBB"]);
    }
}
