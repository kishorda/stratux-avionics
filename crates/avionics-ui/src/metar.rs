//! Partial METAR reading: flight category, and which tokens deserve attention.
//!
//! # This deliberately does not decode METARs
//!
//! [`crate::weatherpage`] shows raw text and will keep doing so. What happens here is narrower
//! and safer: recognise the few tokens that carry a *hazard* or drive the *flight category*, and
//! leave everything else exactly as transmitted.
//!
//! The distinction is the whole design. A full decoder turns the report into prose, which means
//! the display asserts a reading of it — and a decoder that is subtly wrong about weather is
//! worse than no decoder, because the raw text was ground truth and the prose is our opinion of
//! it. Here an unrecognised token is simply not highlighted. The failure mode is "we did not
//! draw attention to something", never "we said something false".
//!
//! That asymmetry is why this is worth doing and a full decode is not.
//!
//! # Token grammar, not substring search
//!
//! Weather phenomena are matched against the actual grammar — optional intensity, optional `VC`,
//! then two-letter groups from the published sets — rather than by searching for `"TS"` in the
//! line. A substring search finds `TS` inside the station identifier `KTSM` and paints a
//! thunderstorm warning on a clear day.

/// How much attention a token deserves. Ordered: `Warning` outranks `Caution`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Hazard {
    None,
    Caution,
    Warning,
}

/// Flight category, from ceiling and visibility. Ordered worst-last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FlightCategory {
    Vfr,
    Mvfr,
    Ifr,
    Lifr,
}

impl FlightCategory {
    pub fn label(self) -> &'static str {
        match self {
            Self::Vfr => "VFR",
            Self::Mvfr => "MVFR",
            Self::Ifr => "IFR",
            Self::Lifr => "LIFR",
        }
    }

    /// Colour for a badge or a readout.
    ///
    /// Green is deliberate for VFR: it is the one state that needs no action, and colouring it
    /// like everything else would waste the strongest signal on screen. Lives here rather than on
    /// a page because both the weather list and the airport card show it, and two places
    /// disagreeing about what VFR looks like is exactly the kind of drift worth designing out.
    pub fn colour(self, theme: &crate::Theme) -> avionics_gfx::femtovg::Color {
        match self {
            Self::Vfr => theme.good,
            Self::Mvfr => theme.caution,
            Self::Ifr | Self::Lifr => theme.warning,
        }
    }

    /// Ceiling alone, in feet AGL. FAA thresholds.
    fn from_ceiling(ft: u32) -> Self {
        match ft {
            f if f < 500 => Self::Lifr,
            f if f < 1000 => Self::Ifr,
            f if f <= 3000 => Self::Mvfr,
            _ => Self::Vfr,
        }
    }

    /// Visibility alone, in statute miles.
    fn from_visibility(sm: f32) -> Self {
        match sm {
            v if v < 1.0 => Self::Lifr,
            v if v < 3.0 => Self::Ifr,
            v if v <= 5.0 => Self::Mvfr,
            _ => Self::Vfr,
        }
    }
}

/// What could be read out of a report without decoding it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Summary {
    /// `None` when neither ceiling nor visibility could be read — never a guess.
    pub category: Option<FlightCategory>,
    /// Lowest broken/overcast layer, or vertical visibility, in feet AGL.
    pub ceiling_ft: Option<u32>,
    pub visibility_sm: Option<f32>,
}

/// Intensity and proximity qualifiers that may prefix a weather group.
const QUALIFIERS: [&str; 3] = ["-", "+", "VC"];

/// FMH-1 weather groups. Split by class because the hazard rating differs.
const DESCRIPTORS: [&str; 8] = ["MI", "BC", "PR", "DR", "BL", "SH", "TS", "FZ"];
const PRECIPITATION: [&str; 9] = ["DZ", "RA", "SN", "SG", "IC", "PL", "GR", "GS", "UP"];
const OBSCURATION: [&str; 8] = ["BR", "FG", "FU", "VA", "DU", "SA", "HZ", "PY"];
const OTHER: [&str; 5] = ["PO", "SQ", "FC", "SS", "DS"];

/// Split a weather token into its qualifier and two-letter groups, or `None` if it is not a
/// weather token at all.
///
/// This is the guard against false positives. `KTSM` and `TSNO` both contain "TS"; neither parses
/// as a weather group, so neither is highlighted.
pub fn parse_weather(token: &str) -> Option<(&str, Vec<&str>)> {
    let mut rest = token;
    let mut qualifier = "";
    for q in QUALIFIERS {
        if let Some(stripped) = rest.strip_prefix(q) {
            qualifier = q;
            rest = stripped;
            break;
        }
    }
    if rest.is_empty() || rest.len() % 2 != 0 || rest.len() > 8 {
        return None;
    }

    let mut groups = Vec::new();
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let pair = &rest[i..i + 2];
        if !(DESCRIPTORS.contains(&pair)
            || PRECIPITATION.contains(&pair)
            || OBSCURATION.contains(&pair)
            || OTHER.contains(&pair))
        {
            return None;
        }
        groups.push(pair);
        i += 2;
    }
    Some((qualifier, groups))
}

/// Hazard rating for one token, judged on its own.
///
/// Ceiling and visibility are rated separately by [`summarise`], because their severity depends
/// on a threshold rather than on the token's identity.
pub fn token_hazard(token: &str) -> Hazard {
    let Some((qualifier, groups)) = parse_weather(token) else {
        return Hazard::None;
    };

    // Convective and icing hazards, at any intensity and including "in the vicinity": a
    // thunderstorm eight miles away is still a reason to look up.
    let warning = groups.iter().any(|g| {
        matches!(
            *g,
            "TS" | "FZ" | "FC" | "SQ" | "DS" | "SS" | "VA" | "GR" | "GS" | "PL" | "IC"
        )
    });
    if warning {
        return Hazard::Warning;
    }

    // Heavy anything, and the obscurations that actually cost visibility. Mist (BR) and haze (HZ)
    // are deliberately absent: they appear in a large share of routine reports, and a highlight
    // that fires most of the time trains the eye to ignore it.
    if qualifier == "+" || groups.iter().any(|g| matches!(*g, "FG" | "FU" | "SA" | "DU")) {
        return Hazard::Caution;
    }

    Hazard::None
}

/// A sky-condition group: `BKN035`, `OVC004CB`, `VV002`.
///
/// Returns the height in feet and whether it counts as a ceiling. Only broken, overcast and
/// vertical visibility are ceilings — few and scattered are not, which is the single most common
/// mistake in home-grown METAR code.
fn sky_layer(token: &str) -> Option<(u32, bool)> {
    let (cover, rest) = if let Some(rest) = token.strip_prefix("VV") {
        ("VV", rest)
    } else if token.len() >= 6 {
        // Three letters of cover then at least three digits of height.
        token.split_at(3)
    } else {
        return None;
    };

    let is_ceiling = matches!(cover, "BKN" | "OVC" | "VV");
    if !matches!(cover, "FEW" | "SCT" | "BKN" | "OVC" | "VV") {
        return None;
    }

    let digits: String = rest.chars().take(3).collect();
    if digits.len() != 3 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    // Anything after the height must be a cloud-type suffix, not arbitrary text.
    let suffix = &rest[3..];
    if !matches!(suffix, "" | "CB" | "TCU") {
        return None;
    }
    Some((digits.parse::<u32>().ok()? * 100, is_ceiling))
}

/// Parse a visibility token in statute miles: `10SM`, `1/2SM`, `M1/4SM`, `P6SM`.
///
/// `whole` carries a preceding bare integer, so the two-token form `1 1/2SM` reads as 1.5 rather
/// than as 0.5 — getting that wrong turns marginal VFR into LIFR.
fn parse_visibility(token: &str, whole: Option<f32>) -> Option<f32> {
    let body = token.strip_suffix("SM")?;
    // M means "less than", P means "more than". Both are reported at the limit of what the
    // sensor can resolve, so the numeric value is the right thing to categorise on.
    let body = body.strip_prefix('M').or_else(|| body.strip_prefix('P')).unwrap_or(body);

    let value = if let Some((num, den)) = body.split_once('/') {
        num.parse::<f32>().ok()? / den.parse::<f32>().ok()?
    } else {
        body.parse::<f32>().ok()?
    };
    Some(value + whole.unwrap_or(0.0))
}

/// Read ceiling, visibility and flight category out of a report body.
pub fn summarise(body: &str) -> Summary {
    let tokens: Vec<&str> = body.split_whitespace().collect();
    let mut ceiling_ft: Option<u32> = None;
    let mut visibility_sm: Option<f32> = None;
    let mut cavok = false;
    // A bare integer immediately before a fractional visibility is its whole part.
    let mut pending_whole: Option<f32> = None;

    for (i, token) in tokens.iter().enumerate() {
        // Remarks are free-form and full of things that look like fields but are not.
        if *token == "RMK" {
            break;
        }
        if *token == "CAVOK" {
            cavok = true;
            continue;
        }

        if let Some((height, is_ceiling)) = sky_layer(token) {
            if is_ceiling {
                ceiling_ft = Some(ceiling_ft.map_or(height, |c: u32| c.min(height)));
            }
            pending_whole = None;
            continue;
        }

        if visibility_sm.is_none() {
            if let Some(v) = parse_visibility(token, pending_whole) {
                visibility_sm = Some(v);
                pending_whole = None;
                continue;
            }
        }

        // Remember a bare integer only if the next token is a fraction in statute miles.
        pending_whole = if token.len() <= 2
            && token.chars().all(|c| c.is_ascii_digit())
            && tokens
                .get(i + 1)
                .is_some_and(|n| n.ends_with("SM") && n.contains('/'))
        {
            token.parse().ok()
        } else {
            None
        };
    }

    // CLR/SKC/NSC mean no cloud below the reporting limit, so no ceiling — which is a positive
    // statement, not missing data. Without this a clear day has `ceiling_ft: None` and the
    // category would rest on visibility alone.
    let clear = tokens
        .iter()
        .take_while(|t| **t != "RMK")
        .any(|t| matches!(*t, "CLR" | "SKC" | "NSC" | "NCD"));

    if cavok {
        return Summary {
            category: Some(FlightCategory::Vfr),
            ceiling_ft: None,
            visibility_sm: None,
        };
    }

    let from_ceiling = ceiling_ft.map(FlightCategory::from_ceiling).or({
        // No ceiling layer reported AND an explicit clear indication: unlimited.
        if clear {
            Some(FlightCategory::Vfr)
        } else {
            None
        }
    });
    let from_vis = visibility_sm.map(FlightCategory::from_visibility);

    // The worse of the two governs, which is what "and/or" means in the FAA definitions.
    let category = match (from_ceiling, from_vis) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };

    Summary {
        category,
        ceiling_ft,
        visibility_sm,
    }
}

/// Hazard rating for a whole report: the worst of its tokens, and of its category.
pub fn body_hazard(body: &str) -> Hazard {
    let token_worst = body
        .split_whitespace()
        .take_while(|t| *t != "RMK")
        .map(token_hazard)
        .max()
        .unwrap_or(Hazard::None);

    let category_hazard = match summarise(body).category {
        Some(FlightCategory::Lifr) | Some(FlightCategory::Ifr) => Hazard::Warning,
        Some(FlightCategory::Mvfr) => Hazard::Caution,
        _ => Hazard::None,
    };

    token_worst.max(category_hazard)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real-shape reports. Written out in full rather than as fragments, because the parser has to
    // survive the fields it does NOT understand sitting next to the ones it does.
    const VFR: &str = "KBJC 291853Z 09012G20KT 10SM FEW120 24/07 A3002 RMK AO2 SLP123";
    const MVFR: &str = "KORD 291853Z 25012KT 5SM HZ BKN025 15/09 A3001";
    const IFR: &str = "KSEA 291853Z 18008KT 3SM BR OVC008 09/08 A2988";
    const LIFR: &str = "KDEN 291853Z 27015KT 1/2SM -SN OVC004 M02/M04 A2992";
    const TSTORM: &str = "KMIA 291853Z 09010KT 10SM TSRA SCT018 BKN035CB 28/24 A2998";

    #[test]
    fn flight_category_matches_the_faa_thresholds() {
        assert_eq!(summarise(VFR).category, Some(FlightCategory::Vfr));
        assert_eq!(summarise(MVFR).category, Some(FlightCategory::Mvfr));
        assert_eq!(summarise(IFR).category, Some(FlightCategory::Ifr));
        assert_eq!(summarise(LIFR).category, Some(FlightCategory::Lifr));
    }

    #[test]
    fn only_broken_and_overcast_count_as_a_ceiling() {
        // The commonest mistake in home-grown METAR code: SCT018 is not a ceiling, so this report
        // is VFR on its 10SM visibility and its BKN035 ceiling, not IFR on the scattered layer.
        let s = summarise(TSTORM);
        assert_eq!(s.ceiling_ft, Some(3500), "BKN035 is the ceiling, not SCT018");
        assert_eq!(s.category, Some(FlightCategory::Vfr));
    }

    #[test]
    fn the_lowest_ceiling_layer_wins() {
        let s = summarise("KXXX 291853Z 10SM BKN012 OVC030 10/05 A3000");
        assert_eq!(s.ceiling_ft, Some(1200));
    }

    #[test]
    fn vertical_visibility_is_a_ceiling() {
        let s = summarise("KXXX 291853Z 1/4SM FG VV002 05/05 A2990");
        assert_eq!(s.ceiling_ft, Some(200));
        assert_eq!(s.category, Some(FlightCategory::Lifr));
    }

    #[test]
    fn the_worse_of_ceiling_and_visibility_governs() {
        // Good ceiling, bad visibility.
        let s = summarise("KXXX 291853Z 1/2SM FG OVC050 05/05 A2990");
        assert_eq!(s.category, Some(FlightCategory::Lifr), "visibility should govern");
        // Bad ceiling, good visibility.
        let s = summarise("KXXX 291853Z 10SM OVC004 05/05 A2990");
        assert_eq!(s.category, Some(FlightCategory::Lifr), "ceiling should govern");
    }

    #[test]
    fn a_whole_plus_fraction_visibility_is_not_read_as_the_fraction() {
        // "1 1/2SM" is 1.5 miles. Reading it as 0.5 turns IFR into LIFR.
        let s = summarise("KXXX 291853Z 18008KT 1 1/2SM BR OVC020 09/08 A2988");
        assert_eq!(s.visibility_sm, Some(1.5));
        assert_eq!(s.category, Some(FlightCategory::Ifr));
    }

    #[test]
    fn less_than_and_more_than_visibility_parse() {
        assert_eq!(summarise("KXXX 291853Z M1/4SM FG").visibility_sm, Some(0.25));
        assert_eq!(summarise("KXXX 291853Z P6SM").visibility_sm, Some(6.0));
    }

    #[test]
    fn clear_skies_are_unlimited_not_unknown() {
        let s = summarise("KXXX 291853Z 09005KT 10SM CLR 20/10 A3000");
        assert_eq!(s.ceiling_ft, None);
        assert_eq!(s.category, Some(FlightCategory::Vfr));
    }

    #[test]
    fn cavok_is_vfr() {
        assert_eq!(
            summarise("EGLL 291853Z 09005KT CAVOK 20/10 Q1013").category,
            Some(FlightCategory::Vfr)
        );
    }

    #[test]
    fn a_report_with_neither_ceiling_nor_visibility_has_no_category() {
        // Never guess. A PIREP or a fragment must not be assigned a category.
        assert_eq!(summarise("UA /OV KDEN /TM 1840 /FL085 /TP C172").category, None);
        assert_eq!(summarise("").category, None);
    }

    #[test]
    fn remarks_are_not_parsed_as_fields() {
        // RMK sections carry things that look like sky groups and visibilities but are not.
        let s = summarise("KXXX 291853Z 10SM CLR 20/10 A3000 RMK AO2 SLP123 VIS 1/4 OVC002");
        assert_eq!(s.ceiling_ft, None, "an OVC in the remarks is not a ceiling");
        assert_eq!(s.category, Some(FlightCategory::Vfr));
    }

    // --- hazard tokens -----------------------------------------------------------------------

    #[test]
    fn thunderstorms_and_freezing_are_warnings() {
        for token in ["TSRA", "+TSRA", "VCTS", "TS", "FZRA", "FZFG", "FZDZ", "GR", "PL", "IC"] {
            assert_eq!(token_hazard(token), Hazard::Warning, "{token}");
        }
    }

    #[test]
    fn heavy_precipitation_and_real_obscurations_are_cautions() {
        for token in ["+RA", "+SN", "FG", "FU", "SA", "DU"] {
            assert_eq!(token_hazard(token), Hazard::Caution, "{token}");
        }
    }

    #[test]
    fn routine_tokens_are_not_highlighted() {
        // Mist and haze appear in a large share of reports. Highlighting them would train the eye
        // to ignore the highlight, which costs more than it gains.
        for token in ["BR", "HZ", "-RA", "RA", "SCT018", "10SM", "A3002", "24/07"] {
            assert_eq!(token_hazard(token), Hazard::None, "{token}");
        }
    }

    #[test]
    fn station_identifiers_are_not_mistaken_for_weather() {
        // The reason this parses the grammar instead of searching for substrings: all of these
        // contain a valid two-letter code and none of them are weather.
        for token in ["KTSM", "TSNO", "KFGZ", "KRAP", "KSNA", "AO2", "SLP123", "RMK"] {
            assert_eq!(
                token_hazard(token),
                Hazard::None,
                "{token} must not be read as weather"
            );
            assert!(parse_weather(token).is_none(), "{token} parsed as weather");
        }
    }

    #[test]
    fn a_taf_summarises_to_something_that_describes_no_single_moment() {
        // Why weatherpage gates the badge on WeatherProduct::Metar. This test records the reason
        // so nobody removes the gate on the grounds that "parsing a TAF works".
        //
        // It does parse — and produces a category that is true of no point in time. A TAF covers
        // many hours in FM/TEMPO/BECMG periods; `summarise` has no notion of periods, so it takes
        // the LOWEST ceiling anywhere in the forecast and the FIRST visibility, mixing conditions
        // eight hours apart into one badge.
        let taf = "TAF KDEN 291720Z 2918/3024 27012KT P6SM SCT120 FM300200 1/2SM FZRA OVC003";
        let s = summarise(taf);

        // Visibility comes from the first period (good), the ceiling from the last (bad) — so the
        // badge would read LIFR over a forecast whose current period is VFR.
        assert_eq!(s.visibility_sm, Some(6.0), "first period visibility");
        assert_eq!(s.ceiling_ft, Some(300), "last period ceiling");
        assert_eq!(s.category, Some(FlightCategory::Lifr));

        // Which is wrong in the other direction from the obvious guess, and just as misleading.
        // Observations only.
    }

    #[test]
    fn body_hazard_takes_the_worst_of_tokens_and_category() {
        assert_eq!(body_hazard(VFR), Hazard::None);
        assert_eq!(body_hazard(MVFR), Hazard::Caution, "MVFR alone is a caution");
        assert_eq!(body_hazard(IFR), Hazard::Warning, "IFR alone is a warning");
        assert_eq!(body_hazard(TSTORM), Hazard::Warning, "VFR, but a thunderstorm");
    }

    #[test]
    fn hazards_in_the_remarks_do_not_raise_the_rating() {
        // "TSNO" means the thunderstorm sensor is unavailable, not that there is a thunderstorm.
        let quiet = "KXXX 291853Z 09005KT 10SM CLR 20/10 A3000 RMK AO2 TSNO";
        assert_eq!(body_hazard(quiet), Hazard::None);
    }
}
