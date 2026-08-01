//! METAR/TAF abbreviations, and how to find the ones in a report.
//!
//! Transcribed from the NWS/FAA *METAR/TAF List of Abbreviations and Acronyms*
//! (<https://www.weather.gov/media/wrh/mesowest/metar_decode_key.pdf>).
//!
//! # What this is, and what it is not
//!
//! The source is an **alphabetical glossary**, not a grammar. It can say that `BKN` means broken;
//! it cannot say that `BKN008` is a ceiling at 800 feet, because it carries no field order and no
//! syntax. So this module expands vocabulary and nothing more — it never claims to have
//! understood a report.
//!
//! That is exactly why it is safe to ship. [`explain`] returns the codes it recognises and stays
//! silent about everything else. Being silent is a correct answer here; guessing would not be.
//! Interpretation of ceiling, visibility and hazards lives in [`crate::metar`], which parses a
//! deliberately narrow slice of the real grammar.
//!
//! # Omissions
//!
//! Around fifteen purely organisational entries from the source list are left out — DOC, DOD,
//! DOT, FAA, ICAO, NCDC, NOS, NWS, OFCM, WMO, WG/SO, FMH-1, FMH2, cd, N/A. None can appear in a
//! report body, so including them would only add ways to match something that is not there.

use crate::metar;

/// Abbreviation to meaning, sorted by code so lookup can binary-search.
///
/// Sortedness is enforced by a test rather than by trust: an out-of-order entry would make its
/// code silently unfindable, which is the kind of fault that hides for years.
static ENTRIES: &[(&str, &str)] = &[
    ("$", "maintenance check indicator"),
    // ASCII order puts the symbols first: $ + - / all sort below the letters.
    ("+", "heavy intensity"),
    ("-", "light intensity"),
    ("/", "visual range follows; also separates temperature and dew point"),
    ("ACC", "altocumulus castellanus"),
    ("ACFT", "aircraft"),
    ("ACSL", "altocumulus standing lenticular cloud"),
    ("ALP", "airport location point"),
    ("AO1", "automated station without precipitation discriminator"),
    ("AO2", "automated station with precipitation discriminator"),
    ("APCH", "approach"),
    ("APRNT", "apparent"),
    ("APRX", "approximately"),
    ("ATCT", "airport traffic control tower"),
    ("AUTO", "fully automated report"),
    ("B", "began"),
    ("BC", "patches"),
    ("BKN", "broken"),
    ("BL", "blowing"),
    ("BR", "mist"),
    ("C", "center (runway designation)"),
    ("CA", "cloud-air lightning"),
    ("CB", "cumulonimbus cloud"),
    ("CBMAM", "cumulonimbus mammatus cloud"),
    ("CC", "cloud-cloud lightning"),
    ("CCSL", "cirrocumulus standing lenticular cloud"),
    ("CG", "cloud-ground lightning"),
    ("CHI", "cloud-height indicator"),
    ("CHINO", "sky condition at secondary location not available"),
    ("CIG", "ceiling"),
    ("CLR", "clear"),
    ("CONS", "continuous"),
    ("COR", "correction to a previously disseminated observation"),
    ("DR", "low drifting"),
    ("DS", "duststorm"),
    ("DSIPTG", "dissipating"),
    ("DSNT", "distant"),
    ("DU", "widespread dust"),
    ("DVR", "dispatch visual range"),
    ("DZ", "drizzle"),
    ("E", "east, ended, estimated ceiling"),
    ("FC", "funnel cloud"),
    ("FEW", "few clouds"),
    ("FG", "fog"),
    ("FIBI", "filed but impracticable to transmit"),
    ("FIRST", "first observation after a break in coverage"),
    ("FROPA", "frontal passage"),
    ("FRQ", "frequent"),
    ("FT", "feet"),
    ("FU", "smoke"),
    ("FZ", "freezing"),
    ("FZRANO", "freezing rain sensor not available"),
    ("G", "gust"),
    ("GR", "hail"),
    ("GS", "small hail and/or snow pellets"),
    ("HLSTO", "hailstone"),
    ("HZ", "haze"),
    ("IC", "ice crystals, in-cloud lightning"),
    ("INCRG", "increasing"),
    ("INTMT", "intermittent"),
    ("KT", "knots"),
    ("L", "left (runway designation)"),
    ("LAST", "last observation before a break in coverage"),
    ("LST", "Local Standard Time"),
    ("LTG", "lightning"),
    ("LWR", "lower"),
    ("M", "minus, less than"),
    ("METAR", "routine weather report at fixed intervals"),
    ("MI", "shallow"),
    ("MOV", "moved/moving/movement"),
    ("MT", "mountains"),
    ("N", "north"),
    ("NE", "northeast"),
    ("NOSPECI", "no SPECI reports are taken at the station"),
    ("NOTAM", "Notice to Airmen"),
    ("NW", "northwest"),
    ("OCNL", "occasional"),
    ("OHD", "overhead"),
    ("OVC", "overcast"),
    ("OVR", "over"),
    ("P", "greater than the highest reportable value"),
    ("PCPN", "precipitation"),
    ("PL", "ice pellets"),
    ("PNO", "precipitation amount not available"),
    ("PO", "dust/sand whirls (dust devils)"),
    ("PR", "partial"),
    ("PRES", "pressure"),
    ("PRESFR", "pressure falling rapidly"),
    ("PRESRR", "pressure rising rapidly"),
    ("PWINO", "precipitation identifier sensor not available"),
    ("PY", "spray"),
    ("R", "right (runway designation), runway"),
    ("RA", "rain"),
    ("RTD", "routine delayed (late) observation"),
    ("RV", "reportable value"),
    ("RVR", "runway visual range"),
    ("RVRNO", "RVR system values not available"),
    ("RY", "runway"),
    ("S", "snow, south"),
    ("SA", "sand"),
    ("SCSL", "stratocumulus standing lenticular cloud"),
    ("SCT", "scattered"),
    ("SE", "southeast"),
    ("SFC", "surface"),
    ("SG", "snow grains"),
    ("SH", "shower(s)"),
    ("SKC", "sky clear"),
    ("SLP", "sea-level pressure"),
    ("SLPNO", "sea-level pressure not available"),
    ("SM", "statute miles"),
    ("SN", "snow"),
    ("SNINCR", "snow increasing rapidly"),
    ("SP", "snow pellets"),
    ("SPECI", "unscheduled report taken when criteria are met"),
    ("SQ", "squalls"),
    ("SS", "sandstorm"),
    ("STN", "station"),
    ("SW", "snow shower, southwest"),
    ("TCU", "towering cumulus"),
    ("TS", "thunderstorm"),
    ("TSNO", "thunderstorm information not available"),
    ("TWR", "tower"),
    ("UNKN", "unknown"),
    ("UP", "unknown precipitation"),
    ("UTC", "Coordinated Universal Time"),
    ("V", "variable"),
    ("VA", "volcanic ash"),
    ("VC", "in the vicinity"),
    ("VIS", "visibility"),
    ("VISNO", "visibility at secondary location not available"),
    ("VR", "visual range"),
    ("VRB", "variable"),
    ("VV", "vertical visibility"),
    ("W", "west"),
    ("WND", "wind"),
    ("WSHFT", "wind shift"),
    ("Z", "zulu (Coordinated Universal Time)"),
];

/// Meaning of one abbreviation, exactly as listed.
pub fn lookup(code: &str) -> Option<&'static str> {
    ENTRIES
        .binary_search_by_key(&code, |(k, _)| k)
        .ok()
        .map(|i| ENTRIES[i].1)
}

/// Codes that make up one token, in the order they appear in it.
///
/// A token is not usually a glossary key. `BKN008` is a cover code plus a height; `-TSRA` is an
/// intensity plus two weather groups. This decomposes the shapes that have structure and falls
/// back to a whole-token match, so `AO2` and `RMK` resolve too.
///
/// Single-letter keys are **not** matched as whole tokens. `M`, `P`, `R`, `S`, `N`, `E`, `V` and
/// friends are real entries but almost never stand alone in a report, and matching them would
/// turn any stray letter into a confident definition.
pub fn codes_in(token: &str) -> Vec<&'static str> {
    // Weather groups first: they have the most structure, so a match here is the most certain.
    if let Some((qualifier, groups)) = metar::parse_weather(token) {
        let mut out = Vec::new();
        if !qualifier.is_empty() {
            if let Some((code, _)) = ENTRIES.iter().find(|(k, _)| *k == qualifier) {
                out.push(*code);
            }
        }
        for group in groups {
            if let Ok(i) = ENTRIES.binary_search_by_key(&group, |(k, _)| k) {
                out.push(ENTRIES[i].0);
            }
        }
        if !out.is_empty() {
            return out;
        }
    }

    // Sky-condition groups: a three-letter cover followed by a height, optionally CB or TCU.
    for cover in ["FEW", "SCT", "BKN", "OVC", "VV"] {
        if let Some(rest) = token.strip_prefix(cover) {
            if rest.len() >= 3 && rest[..3].chars().all(|c| c.is_ascii_digit()) {
                let mut out = vec![lookup(cover).map(|_| cover).unwrap_or(cover)];
                match &rest[3..] {
                    "CB" => out.push("CB"),
                    "TCU" => out.push("TCU"),
                    _ => {}
                }
                return out;
            }
        }
    }

    // Whole-token match, for the tokens that are simply an abbreviation.
    if token.len() > 1 {
        if let Ok(i) = ENTRIES.binary_search_by_key(&token, |(k, _)| k) {
            return vec![ENTRIES[i].0];
        }
    }

    Vec::new()
}

/// Every code appearing in a report, in order of first appearance, without repeats.
///
/// Order of appearance rather than alphabetical: the reader is looking at the report, so the list
/// should follow their eye across it rather than making them search.
pub fn explain(body: &str) -> Vec<(&'static str, &'static str)> {
    let mut out: Vec<(&'static str, &'static str)> = Vec::new();
    for token in body.split_whitespace() {
        for code in codes_in(token) {
            if !out.iter().any(|(c, _)| *c == code) {
                if let Some(meaning) = lookup(code) {
                    out.push((code, meaning));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_sorted_so_binary_search_finds_everything() {
        // An out-of-order entry makes its own code unfindable and nothing else fails, so this is
        // checked rather than assumed.
        for pair in ENTRIES.windows(2) {
            assert!(
                pair[0].0 < pair[1].0,
                "{:?} and {:?} are out of order",
                pair[0].0,
                pair[1].0
            );
        }
        // And every entry is reachable through the public path.
        for (code, meaning) in ENTRIES {
            assert_eq!(lookup(code), Some(*meaning), "{code} not found");
        }
    }

    #[test]
    fn weather_groups_decompose_into_their_parts() {
        assert_eq!(codes_in("TSRA"), vec!["TS", "RA"]);
        assert_eq!(codes_in("+TSRA"), vec!["+", "TS", "RA"]);
        assert_eq!(codes_in("VCTS"), vec!["VC", "TS"]);
        assert_eq!(codes_in("FZRA"), vec!["FZ", "RA"]);
        assert_eq!(codes_in("-SHSN"), vec!["-", "SH", "SN"]);
    }

    #[test]
    fn sky_groups_yield_the_cover_and_any_cloud_type() {
        assert_eq!(codes_in("BKN008"), vec!["BKN"]);
        assert_eq!(codes_in("OVC015CB"), vec!["OVC", "CB"]);
        assert_eq!(codes_in("SCT025TCU"), vec!["SCT", "TCU"]);
        assert_eq!(codes_in("VV003"), vec!["VV"]);
    }

    #[test]
    fn whole_token_abbreviations_resolve() {
        assert_eq!(codes_in("AUTO"), vec!["AUTO"]);
        assert_eq!(codes_in("AO2"), vec!["AO2"]);
        assert_eq!(codes_in("TSNO"), vec!["TSNO"]);
        assert_eq!(codes_in("SLPNO"), vec!["SLPNO"]);
    }

    #[test]
    fn tsno_is_a_sensor_notice_not_a_thunderstorm() {
        // The trap this whole module has to get right: TSNO contains "TS" but means the
        // thunderstorm sensor is unavailable. It must resolve to itself and nothing else.
        assert_eq!(codes_in("TSNO"), vec!["TSNO"]);
        assert_eq!(lookup("TSNO"), Some("thunderstorm information not available"));
    }

    #[test]
    fn single_letters_are_not_matched_as_whole_tokens() {
        // M, P, R, S, N, E and V are real entries but standing alone they are almost always part
        // of something else. Defining a stray letter would be confident noise.
        for token in ["M", "P", "R", "S", "N", "E", "V", "G", "B", "L", "W", "Z", "C"] {
            assert!(codes_in(token).is_empty(), "{token} should not resolve alone");
        }
    }

    #[test]
    fn unrecognised_tokens_stay_silent() {
        // Station identifiers, times, temperatures, altimeter settings and the rest are not
        // glossary entries. Silence is the correct answer.
        for token in ["KDEN", "291853Z", "27015KT", "M02/M04", "A2992", "2918/3024", "10SM"] {
            assert!(codes_in(token).is_empty(), "{token} should not resolve");
        }
    }

    #[test]
    fn a_report_explains_in_reading_order_without_repeats() {
        let body = "METAR KDEN 291853Z 27015KT 2SM TSRA BKN008 OVC015CB M02/M04 A2992 RMK AO2 TSNO";
        let codes: Vec<&str> = explain(body).into_iter().map(|(c, _)| c).collect();

        // Order follows the report, so the reader's eye and the list agree.
        assert_eq!(
            codes,
            vec!["METAR", "TS", "RA", "BKN", "OVC", "CB", "AO2", "TSNO"]
        );
    }

    #[test]
    fn a_code_appearing_twice_is_listed_once() {
        let codes: Vec<&str> = explain("SCT010 SCT020 BKN030").into_iter().map(|(c, _)| c).collect();
        assert_eq!(codes, vec!["SCT", "BKN"]);
    }

    #[test]
    fn a_report_with_nothing_to_explain_yields_nothing() {
        assert!(explain("KDEN 291853Z 27015KT").is_empty());
        assert!(explain("").is_empty());
    }
}
