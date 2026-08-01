//! METAR/TAF abbreviations, and how to find the ones in a report.
//!
//! # Sources
//!
//! 1. The NWS/FAA *METAR/TAF List of Abbreviations and Acronyms*
//!    (<https://www.weather.gov/media/wrh/mesowest/metar_decode_key.pdf>), pages 1–3: an
//!    alphabetical glossary of vocabulary.
//! 2. **The same PDF's page 4**, *Key to Decode an ASOS (METAR) Observation*. This is a different
//!    kind of document — it describes the *structure* of a report field by field, which is where
//!    `RMK`, the `A` altimeter prefix, the `Tsnnnnnnn` hourly temperature group and the shapes of
//!    the wind, visibility and temperature groups are defined. An earlier version of this module
//!    transcribed only pages 1–3, which is why a plain METAR expanded to two entries: every
//!    structured group in it fell straight through.
//! 3. TAF change groups (`TAF`, `FM`, `TEMPO`, `BECMG`, `PROB`, `NSW`, `WS`, `AMD`, `CNL`, `NIL`)
//!    come from FMH-1 chapter 5, **not** from the PDF. Despite its title, that document contains
//!    no TAF forecast vocabulary at all. These are marked in the table below.
//!
//! # What this is, and what it is not
//!
//! Source 1 is a glossary, not a grammar: it can say that `BKN` means broken, but not that
//! `BKN008` is a ceiling at 800 feet. Source 2 supplies group *shapes* — enough to know that the
//! `KT` in `27015G35KT` is the knots suffix and the `G` is a gust — but this module still does not
//! interpret values. It expands vocabulary and locates it; it never claims to have understood a
//! report.
//!
//! That is what makes it safe to ship. [`explain`] returns the codes it recognises and stays silent
//! about everything else. Being silent is a correct answer here; guessing would not be.
//! Interpretation of ceiling, visibility and hazards lives in [`crate::metar`], which parses a
//! deliberately narrow slice of the real grammar.
//!
//! # Deliberate silences
//!
//! Some tokens are left unexplained on purpose, because the same shape means different things in
//! different places and this module has no notion of place:
//!
//! * `P0003` in remarks is an hourly precipitation amount, but `P6SM` in the body means "greater
//!   than". `P` is only decomposed where a unit disambiguates it.
//! * The all-digit remark groups (`60009`, `10066`, `21012`, `58033`) are defined on page 4 but
//!   have no letter to key on, so matching them would mean matching bare numbers.
//! * A TAF validity period (`2918/3024`) has no code to attach a meaning to.
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
    ("A", "altimeter setting, inches of mercury"),  // page 4
    ("ACC", "altocumulus castellanus"),
    ("ACFT", "aircraft"),
    ("ACSL", "altocumulus standing lenticular cloud"),
    ("ALP", "airport location point"),
    ("AMD", "amended forecast"),  // TAF
    ("AO1", "automated station without precipitation discriminator"),
    ("AO2", "automated station with precipitation discriminator"),
    ("APCH", "approach"),
    ("APRNT", "apparent"),
    ("APRX", "approximately"),
    ("ATCT", "airport traffic control tower"),
    ("AUTO", "fully automated report"),
    ("B", "began"),
    ("BC", "patches"),
    ("BECMG", "becoming — gradual change over the period"),  // TAF
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
    ("CNL", "cancelled"),  // TAF
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
    ("FM", "from — rapid change beginning at this time"),  // TAF
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
    ("NIL", "none, or no report available"),  // TAF
    ("NOSPECI", "no SPECI reports are taken at the station"),
    ("NOTAM", "Notice to Airmen"),
    ("NSW", "no significant weather"),  // TAF
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
    ("PROB", "probability of occurrence, per cent"),  // TAF
    ("PWINO", "precipitation identifier sensor not available"),
    ("PY", "spray"),
    ("R", "right (runway designation), runway"),
    ("RA", "rain"),
    ("RMK", "remarks follow"),  // page 4
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
    ("T", "hourly temperature and dew point, tenths of a degree Celsius"),  // page 4
    ("TAF", "terminal aerodrome forecast"),  // TAF
    ("TCU", "towering cumulus"),
    ("TEMPO", "temporary fluctuations, under one hour each"),  // TAF
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
    ("WS", "wind shear"),  // TAF
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

/// Is `s` exactly `n` ASCII digits?
fn digits(s: &str, n: usize) -> bool {
    s.len() == n && s.bytes().all(|b| b.is_ascii_digit())
}

/// Is `s` one or more ASCII digits?
fn all_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// Strip a leading `M` or `P` value qualifier, recording it.
///
/// Page 4 uses both to bound a reportable value: `M1/4SM` is below the lowest reportable
/// visibility, `P6SM` above the highest. Only ever called where a unit follows, because a bare
/// `Pnnnn` in remarks is a precipitation amount instead — see the module note.
fn strip_bound<'a>(s: &'a str, out: &mut Vec<&'static str>) -> &'a str {
    if let Some(rest) = s.strip_prefix('M') {
        out.push("M");
        rest
    } else if let Some(rest) = s.strip_prefix('P') {
        out.push("P");
        rest
    } else {
        s
    }
}

/// The structured group shapes from page 4 of the source, tried in order.
///
/// Returns the codes in the order they appear *within the token*, so the expansion list reads in
/// the same direction as the report. Each arm insists on the full shape — a partial match returns
/// nothing and the token falls through to the next arm — because a rule that fires on a near-miss
/// is how a station identifier ends up defined as a runway.
fn structured_codes(token: &str) -> Vec<&'static str> {
    // Wind: dddffKT, dddffGfmfmKT, VRBffKT, 00000KT.
    if let Some(head) = token.strip_suffix("KT") {
        let mut out = Vec::new();
        let head = match head.strip_prefix("VRB") {
            Some(rest) => {
                out.push("VRB");
                rest
            }
            None => head,
        };
        let (speed, gust) = match head.split_once('G') {
            Some((a, b)) => (a, Some(b)),
            None => (head, None),
        };
        if all_digits(speed) && gust.is_none_or(all_digits) {
            if gust.is_some() {
                out.push("G");
            }
            out.push("KT");
            return out;
        }
    }

    // Variable wind direction, reported alongside the wind group: 180V240.
    if let Some((from, to)) = token.split_once('V') {
        if digits(from, 3) && digits(to, 3) {
            return vec!["V"];
        }
    }

    // Date/time: always appended with Z. Six digits in a METAR, four in a remark.
    if let Some(head) = token.strip_suffix('Z') {
        if digits(head, 6) || digits(head, 4) {
            return vec!["Z"];
        }
    }

    // Visibility: whole miles or a fraction, appended with SM, optionally bounded by M or P.
    //
    // The fraction slash is deliberately not reported as `/`. That entry means "visual range
    // follows, or separates temperature and dew point" — neither of which is what the slash in
    // `1/2SM` is doing, and a definition that does not fit is worse than no definition.
    if let Some(head) = token.strip_suffix("SM") {
        let mut out = Vec::new();
        let head = strip_bound(head, &mut out);
        let numeric = match head.split_once('/') {
            Some((whole, part)) => all_digits(whole) && all_digits(part),
            None => all_digits(head),
        };
        if numeric {
            out.push("SM");
            return out;
        }
    }

    // Runway visual range: R11/P6000FT, R28L/0600V1000FT.
    if let Some(head) = token.strip_suffix("FT") {
        if let Some(rest) = head.strip_prefix('R') {
            if let Some((runway, value)) = rest.split_once('/') {
                let runway_ok = !runway.is_empty()
                    && runway
                        .bytes()
                        .all(|b| b.is_ascii_digit() || matches!(b, b'L' | b'C' | b'R'));
                let mut out = vec!["R", "/"];
                let value = strip_bound(value, &mut out);
                let (value_ok, variable) = match value.split_once('V') {
                    Some((low, high)) => (all_digits(low) && all_digits(high), true),
                    None => (all_digits(value), false),
                };
                if runway_ok && value_ok {
                    if variable {
                        out.push("V");
                    }
                    out.push("FT");
                    return out;
                }
            }
        }
    }

    // Temperature and dew point, in whole degrees Celsius, separated by a solidus and prefixed
    // with M below zero: 26/07, M02/M04.
    if let Some((temp, dew)) = token.split_once('/') {
        fn bare(s: &str) -> &str {
            s.strip_prefix('M').unwrap_or(s)
        }
        if digits(bare(temp), 2) && digits(bare(dew), 2) {
            let mut out = Vec::new();
            if temp.starts_with('M') || dew.starts_with('M') {
                out.push("M");
            }
            out.push("/");
            return out;
        }
    }

    // Altimeter: A followed by four digits, inches of mercury.
    if let Some(rest) = token.strip_prefix('A') {
        if digits(rest, 4) {
            return vec!["A"];
        }
    }

    // Sea-level pressure in remarks: SLPppp. `SLPNO` is a whole-token entry and is not digits, so
    // it falls through to the whole-token match with its own, different meaning.
    if let Some(rest) = token.strip_prefix("SLP") {
        if digits(rest, 3) {
            return vec!["SLP"];
        }
    }

    // Hourly temperature and dew point in remarks: T followed by eight digits.
    if let Some(rest) = token.strip_prefix('T') {
        if digits(rest, 8) {
            return vec!["T"];
        }
    }

    // TAF change groups: FMddhhmm and PROBnn.
    if let Some(rest) = token.strip_prefix("FM") {
        if digits(rest, 6) || digits(rest, 4) {
            return vec!["FM"];
        }
    }
    if let Some(rest) = token.strip_prefix("PROB") {
        if digits(rest, 2) {
            return vec!["PROB"];
        }
    }

    // Low-level wind shear in a TAF: WS020/27045KT.
    if let Some(rest) = token.strip_prefix("WS") {
        if let Some((height, wind)) = rest.split_once('/') {
            if digits(height, 3) {
                let mut out = vec!["WS", "/"];
                out.extend(structured_codes(wind));
                return out;
            }
        }
    }

    Vec::new()
}

/// Codes that make up one token, in the order they appear in it.
///
/// A token is not usually a glossary key. `BKN008` is a cover code plus a height; `-TSRA` is an
/// intensity plus two weather groups; `27015G35KT` is a gust and a unit wrapped around numbers.
/// This decomposes the shapes that have structure and falls back to a whole-token match, so `AUTO`
/// and `AO2` resolve too.
///
/// Single-letter keys are **not** matched as whole tokens. `M`, `P`, `R`, `S`, `N`, `E`, `V` and
/// friends are real entries but almost never stand alone in a report, and matching them would
/// turn any stray letter into a confident definition. They are reachable only through a structured
/// shape, where the surrounding digits and units say what they are.
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

    // Structured groups from page 4 — wind, time, visibility, RVR, temperature, altimeter and the
    // remark groups. Before the whole-token match, because `SLP123` must resolve as the sea-level
    // pressure group rather than falling through unexplained.
    let structured = structured_codes(token);
    if !structured.is_empty() {
        return structured;
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
        // Silence is the correct answer for anything with no code to attach a meaning to: a
        // station identifier is arbitrary, and a TAF validity period is two bare timestamps.
        for token in ["KDEN", "KBJC", "2918/3024", "3002/3006"] {
            assert!(codes_in(token).is_empty(), "{token} should not resolve");
        }
    }

    #[test]
    fn ambiguous_remark_groups_stay_silent() {
        // The documented deliberate silences. `P0003` is an hourly precipitation amount here, but
        // `P` on its own in the table means "greater than the highest reportable value" — the
        // definition that would be printed is the wrong one, so nothing is printed. The all-digit
        // groups have no letter to key on at all, and matching them would mean matching numbers.
        for token in ["P0003", "60009", "10066", "21012", "58033", "70015"] {
            assert!(
                codes_in(token).is_empty(),
                "{token} should stay silent rather than be confidently mislabelled"
            );
        }
    }

    #[test]
    fn wind_groups_yield_the_gust_and_the_unit() {
        assert_eq!(codes_in("21014KT"), vec!["KT"]);
        assert_eq!(codes_in("27015G35KT"), vec!["G", "KT"]);
        assert_eq!(codes_in("VRB05KT"), vec!["VRB", "KT"]);
        assert_eq!(codes_in("00000KT"), vec!["KT"]);
        // The variable-direction group that follows a wind group.
        assert_eq!(codes_in("180V240"), vec!["V"]);
    }

    #[test]
    fn visibility_carries_its_bound_but_not_its_fraction_slash() {
        assert_eq!(codes_in("10SM"), vec!["SM"]);
        assert_eq!(codes_in("P6SM"), vec!["P", "SM"]);
        assert_eq!(codes_in("M1/4SM"), vec!["M", "SM"]);
        // The slash in a fraction is not the `/` of the glossary, which means "visual range
        // follows, or separates temperature and dew point". A definition that does not fit the
        // thing it is attached to is worse than none.
        assert_eq!(codes_in("1/2SM"), vec!["SM"]);
    }

    #[test]
    fn the_remaining_body_groups_decompose() {
        assert_eq!(codes_in("291853Z"), vec!["Z"]);
        assert_eq!(codes_in("26/07"), vec!["/"]);
        assert_eq!(codes_in("M02/M04"), vec!["M", "/"]);
        assert_eq!(codes_in("A2992"), vec!["A"]);
        assert_eq!(codes_in("R11/P6000FT"), vec!["R", "/", "P", "FT"]);
        assert_eq!(codes_in("R28L/0600V1000FT"), vec!["R", "/", "V", "FT"]);
    }

    #[test]
    fn remark_groups_decompose_without_swallowing_their_own_negations() {
        assert_eq!(codes_in("SLP123"), vec!["SLP"]);
        assert_eq!(codes_in("T10171028"), vec!["T"]);
        // SLPNO starts with SLP but means the opposite of a pressure reading. The digit check is
        // what keeps the prefix rule off it.
        assert_eq!(codes_in("SLPNO"), vec!["SLPNO"]);
        assert_eq!(lookup("SLPNO"), Some("sea-level pressure not available"));
    }

    #[test]
    fn taf_change_groups_resolve() {
        // The forecast vocabulary a TAF is mostly made of, and which the PDF does not carry.
        assert_eq!(codes_in("TAF"), vec!["TAF"]);
        assert_eq!(codes_in("FM292100"), vec!["FM"]);
        assert_eq!(codes_in("TEMPO"), vec!["TEMPO"]);
        assert_eq!(codes_in("BECMG"), vec!["BECMG"]);
        assert_eq!(codes_in("PROB30"), vec!["PROB"]);
        assert_eq!(codes_in("NSW"), vec!["NSW"]);
        assert_eq!(codes_in("WS020/27045KT"), vec!["WS", "/", "KT"]);
    }

    #[test]
    fn a_report_explains_in_reading_order_without_repeats() {
        let body = "METAR KDEN 291853Z 27015KT 2SM TSRA BKN008 OVC015CB M02/M04 A2992 RMK AO2 TSNO";
        let codes: Vec<&str> = explain(body).into_iter().map(|(c, _)| c).collect();

        // Order follows the report, so the reader's eye and the list agree.
        assert_eq!(
            codes,
            vec![
                "METAR", "Z", "KT", "SM", "TS", "RA", "BKN", "OVC", "CB", "M", "/", "A", "RMK",
                "AO2", "TSNO"
            ]
        );
    }

    #[test]
    fn an_ordinary_metar_explains_every_token_but_its_station() {
        // The regression this module was rewritten for: this report used to yield two entries,
        // because every structured group in it fell through to nothing.
        let body = "METAR KBJC 291853Z 21014KT 10SM FEW120 26/07 A3002";
        let unexplained: Vec<&str> = body
            .split_whitespace()
            .filter(|t| codes_in(t).is_empty())
            .collect();
        assert_eq!(
            unexplained,
            vec!["KBJC"],
            "only the station identifier should be left unexplained"
        );
        assert!(explain(body).len() >= 7, "{:?}", explain(body));
    }

    #[test]
    fn a_code_appearing_twice_is_listed_once() {
        let codes: Vec<&str> = explain("SCT010 SCT020 BKN030").into_iter().map(|(c, _)| c).collect();
        assert_eq!(codes, vec!["SCT", "BKN"]);
    }

    #[test]
    fn a_report_with_nothing_to_explain_yields_nothing() {
        // Station identifiers and validity periods only — nothing here carries a code.
        assert!(explain("KDEN KBJC 2918/3024").is_empty());
        assert!(explain("").is_empty());
    }
}
