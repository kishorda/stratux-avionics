//! Magnetic variation per airport, from the World Magnetic Model.
//!
//! # Why the file has to carry this
//!
//! Two numbers on the airport card come from different reference frames, and the display has to
//! subtract one from the other to say anything about wind against runway:
//!
//! * **Runway headings are magnetic.** They come from the painted designator — runway `05` is
//!   between 045 and 054 degrees *magnetic* — which is the only source populated for every runway.
//!   See `faa::heading_from_designator`.
//! * **METAR winds are true.** The surface wind group in the body of a METAR or SPECI is referenced
//!   to true north. (ATIS and tower winds are magnetic, which is why this catches people out: the
//!   number a pilot hears on the radio and the number in the report are not the same number.)
//!
//! Subtracting them directly is wrong by the local variation — about 13 degrees west around
//! Morristown, and up to about 20 either way at the edges of the CONUS box. That is more than one
//! runway number, and on a 20 kt wind straight down the runway it invents four and a half knots of
//! crosswind that is not there. The error is invisible, plausible, and consistent, which is the
//! worst combination.
//!
//! # Why it is computed here and not on the Pi
//!
//! The model is a spherical-harmonic expansion with a table of coefficients and an expiry date.
//! None of that belongs in the aircraft binary to answer a question whose input — an airport's
//! position — is already fixed at build time. One signed byte per airport costs 18 KB on a 2.3 MB
//! file and leaves the display adding a constant.
//!
//! # Why it is stored rounded to whole degrees
//!
//! Because the runway heading it will be applied to is only good to 10 degrees anyway: a
//! designator names a 10-degree bucket. Half a degree of rounding here is lost several places
//! below the noise floor of the thing it is corrected against. See [`Variations::round`].

use anyhow::{Context, Result};
use world_magnetic_model::time::Date;
use world_magnetic_model::uom::si::angle::degree;
use world_magnetic_model::uom::si::f32::{Angle, Length};
use world_magnetic_model::uom::si::length::foot;
use world_magnetic_model::GeomagneticField;

/// East-positive, the sign convention every published variation uses: `13W` is `-13`.
///
/// Held as a whole number of degrees so it fits the byte the record has spare, and because the
/// runway headings it corrects are 10-degree buckets.
pub type Degrees = i8;

/// What a build learned about variation, for the report at the end of a run.
#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub computed: usize,
    /// Positions the model refused. These keep variation 0 and are reported rather than hidden;
    /// see [`declination`] for what a zero means downstream.
    pub failed: usize,
    pub min: Degrees,
    pub max: Degrees,
}

/// Variation at a position on a date, east-positive, rounded to whole degrees.
///
/// `days` is days since the Unix epoch — the chart's own effective date, so the variation ages
/// with the cycle it was built from rather than with the clock of whoever ran the build.
///
/// Height is passed as the airport's elevation. It barely matters — declination changes by
/// thousandths of a degree over the height of any runway in CONUS — but the model wants a height
/// and the elevation is already to hand, so there is nothing to gain by inventing a sea-level one.
pub fn declination(lat_deg: f64, lon_deg: f64, elevation_ft: i16, days: u32) -> Result<Degrees> {
    let date = date_from_days(days)?;
    let field = GeomagneticField::new(
        Length::new::<foot>(elevation_ft as f32),
        Angle::new::<degree>(lat_deg as f32),
        Angle::new::<degree>(lon_deg as f32),
        date,
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
    .with_context(|| format!("declination at {lat_deg:.4},{lon_deg:.4}"))?;

    Ok(round(field.declination().get::<degree>()))
}

/// Round to the nearest whole degree, saturating into the byte.
///
/// Saturation cannot trigger anywhere in CONUS — the range is about -20 to +17 — but a builder
/// that silently wrapped -130 to +126 because someone widened the bounding box would produce a
/// display confidently pointing at the wrong runway, so the clamp is explicit.
pub fn round(degrees: f32) -> Degrees {
    degrees.round().clamp(i8::MIN as f32, i8::MAX as f32) as Degrees
}

/// Days since the Unix epoch to a calendar date.
fn date_from_days(days: u32) -> Result<Date> {
    Date::from_calendar_date(1970, world_magnetic_model::time::Month::January, 1)
        .expect("1970-01-01 is a date")
        .checked_add(world_magnetic_model::time::Duration::days(days as i64))
        .with_context(|| format!("{days} days after the epoch is not a representable date"))
}

impl Stats {
    pub fn observe(&mut self, value: Degrees) {
        if self.computed == 0 {
            self.min = value;
            self.max = value;
        } else {
            self.min = self.min.min(value);
            self.max = self.max.max(value);
        }
        self.computed += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-08-01, the cycle this was written against.
    const DAYS: u32 = 20_666;

    /// Published variation is east-positive, and the eastern United States is west, i.e. negative.
    ///
    /// The values are checked as ranges rather than exact numbers: the point is that the sign and
    /// magnitude are right, and pinning a decimal would break on the next model epoch for no gain.
    #[test]
    fn variation_matches_the_published_pattern_across_the_country() {
        for (name, lat, lon, low, high) in [
            // Morristown NJ — the field this project keeps testing against.
            ("KMMU", 40.799, -74.415, -15, -10),
            // Daytona Beach FL.
            ("KDAB", 29.180, -81.058, -10, -4),
            // Seattle — the other sign entirely.
            ("KSEA", 47.449, -122.309, 12, 18),
            // Denver, near the middle.
            ("KBJC", 39.909, -105.117, 5, 11),
        ] {
            let v = declination(lat, lon, 0, DAYS).expect("model covers CONUS");
            assert!(
                (low..=high).contains(&v),
                "{name}: {v} degrees is outside the expected {low}..={high}"
            );
        }
    }

    /// The sign convention, stated as a test because getting it backwards is the whole risk here
    /// and both halves look equally plausible in isolation.
    #[test]
    fn east_is_positive_and_west_is_negative() {
        let east = declination(47.449, -122.309, 0, DAYS).expect("Seattle");
        let west = declination(40.799, -74.415, 0, DAYS).expect("Morristown");
        assert!(east > 0, "Seattle is east variation, got {east}");
        assert!(west < 0, "Morristown is west variation, got {west}");
    }

    #[test]
    fn rounding_goes_to_the_nearest_degree_and_cannot_wrap_the_byte() {
        assert_eq!(round(-12.4), -12);
        assert_eq!(round(-12.6), -13);
        assert_eq!(round(0.49), 0);
        assert_eq!(round(400.0), i8::MAX);
        assert_eq!(round(-400.0), i8::MIN);
    }

    #[test]
    fn the_epoch_conversion_lands_on_the_date_the_rest_of_the_builder_prints() {
        // 20_666 is 2026-08-01 by `main::civil_date`, which is tested separately against the same
        // day numbers. Both agreeing is what makes the variation match the stated cycle.
        let date = date_from_days(20_666).expect("representable");
        assert_eq!(date.year(), 2026);
        assert_eq!(date.month() as u8, 8);
        assert_eq!(date.day(), 1);
    }

    #[test]
    fn stats_track_the_range_seen() {
        let mut stats = Stats::default();
        stats.observe(-13);
        stats.observe(15);
        stats.observe(-2);
        assert_eq!(stats.computed, 3);
        assert_eq!(stats.min, -13);
        assert_eq!(stats.max, 15);
    }
}
