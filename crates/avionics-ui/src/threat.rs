//! Threat classification.
//!
//! Loosely modelled on TCAS traffic advisories, but this is **not** TCAS and must not be flown as
//! though it were. It has no closure-rate logic, no time-to-closest-approach, and depends on
//! whatever ADS-B and ADS-R happen to deliver. It exists to draw the eye, not to issue a
//! resolution advisory.
//!
//! The two design choices worth knowing about:
//!
//! * **Relative altitude is compared like-for-like where possible.** Traffic reports pressure
//!   altitude, so own-ship pressure altitude is preferred over GPS MSL; the two disagree by the
//!   local altimeter error, which at a non-standard setting is easily several hundred feet — a
//!   whole threat tier.
//! * **Without own-ship altitude, classification never escalates to Alert.** A range-only alert
//!   fires constantly in the circuit, and a display that cries wolf gets ignored precisely when
//!   it matters.
//!
//! This module also owns the **vertical filter** ([`AltitudeFilter`]), because the one property
//! that makes hiding traffic acceptable — that a filter can never hide a threat — is a statement
//! about the tiers above and the bands below, and the two have to be able to see each other.

use stratux_client::domain::Target;

/// How much attention a target deserves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ThreatLevel {
    /// Normal traffic.
    Normal,
    /// Close enough to watch.
    Advisory,
    /// Close enough to act on.
    Alert,
}

/// One tier's boundaries. A target must be inside *both* to qualify.
#[derive(Debug, Clone, Copy)]
pub struct Tier {
    pub range_nm: f32,
    pub altitude_ft: f32,
}

#[derive(Debug, Clone)]
pub struct ThreatConfig {
    pub advisory: Tier,
    pub alert: Tier,
    /// Ignore targets on the ground for threat purposes.
    ///
    /// Taxiing aircraft are legitimately within a few hundred feet and a few hundred metres while
    /// you sit at the hold-short line; alerting on them trains the pilot to ignore alerts.
    pub ignore_on_ground: bool,
}

impl Default for ThreatConfig {
    fn default() -> Self {
        Self {
            advisory: Tier {
                range_nm: 6.0,
                altitude_ft: 1200.0,
            },
            alert: Tier {
                range_nm: 3.0,
                altitude_ft: 600.0,
            },
            ignore_on_ground: true,
        }
    }
}

/// Everything the plan view needs to know about how to draw a target.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Assessment {
    pub level: ThreatLevel,
    /// Target altitude minus own-ship altitude, in feet. `None` if either is unknown.
    pub relative_altitude_ft: Option<f32>,
    pub range_nm: f32,
}

/// Classify a target.
///
/// `own_altitude_ft` should come from [`stratux_client::domain::OwnShip::comparison_altitude_ft`],
/// which already prefers pressure altitude over GPS MSL.
pub fn assess(
    target: &Target,
    range_nm: f32,
    own_altitude_ft: Option<f32>,
    config: &ThreatConfig,
) -> Assessment {
    let relative_altitude_ft = match (target.altitude_ft, own_altitude_ft) {
        // A GNSS-referenced target altitude is not comparable with a pressure altitude, but the
        // difference is a datum offset rather than noise, so it is still far better than nothing.
        (Some(target_alt), Some(own_alt)) => Some(target_alt as f32 - own_alt),
        _ => None,
    };

    let level = if config.ignore_on_ground && target.on_ground {
        ThreatLevel::Normal
    } else {
        match relative_altitude_ft {
            Some(delta) => {
                let separation = delta.abs();
                if range_nm <= config.alert.range_nm && separation <= config.alert.altitude_ft {
                    ThreatLevel::Alert
                } else if range_nm <= config.advisory.range_nm
                    && separation <= config.advisory.altitude_ft
                {
                    ThreatLevel::Advisory
                } else {
                    ThreatLevel::Normal
                }
            }
            // Range only: capped at Advisory. See the module note on crying wolf.
            None => {
                if range_nm <= config.alert.range_nm {
                    ThreatLevel::Advisory
                } else {
                    ThreatLevel::Normal
                }
            }
        }
    };

    Assessment {
        level,
        relative_altitude_ft,
        range_nm,
    }
}

/// Feet either side of own-ship in the narrow half of a band.
const NEAR_FT: f32 = 2700.0;

/// Feet in the wide half, on whichever side the pilot has asked to see more of.
const FAR_FT: f32 = 9000.0;

/// Which slice of the vertical world to draw.
///
/// The bands are Garmin's (GTS/GTX traffic pages) rather than anything invented here: a pilot who
/// has flown behind a GTN reads this display correctly with no learning, and the numbers already
/// have decades of use behind them.
///
/// # Why hiding traffic is safe here
///
/// It is safe because of exactly one rule, enforced in [`AltitudeFilter::admits`]: **only
/// [`ThreatLevel::Normal`] targets are ever filtered.** Anything the tiers above have flagged is
/// drawn whatever the band says.
///
/// That rule is structural, and deliberately not a claim about arithmetic. It happens to be true
/// today that the narrowest band ([`NEAR_FT`], 2700 ft) is more than twice the advisory tier
/// (1200 ft), so the numbers alone would keep a threat on screen. But "these two constants are in
/// the right relationship" is the kind of fact that quietly stops being true when somebody tunes
/// one of them, and the failure would be a target vanishing from a traffic display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AltitudeFilter {
    /// Equal slices above and below. The default, and the useful one in the circuit.
    #[default]
    Normal,
    /// Look up: the airway traffic overhead, at the cost of the same view below.
    Above,
    /// Look down: traffic in the pattern beneath you.
    Below,
    /// No vertical filtering at all.
    Unrestricted,
}

impl AltitudeFilter {
    /// Every band, in the order [`AltitudeFilter::cycle`] visits them.
    pub const ALL: [Self; 4] = [Self::Normal, Self::Above, Self::Below, Self::Unrestricted];

    /// Feet below and feet above own-ship, or `None` when nothing is excluded.
    pub fn band(self) -> Option<(f32, f32)> {
        match self {
            Self::Normal => Some((NEAR_FT, NEAR_FT)),
            Self::Above => Some((NEAR_FT, FAR_FT)),
            Self::Below => Some((FAR_FT, NEAR_FT)),
            Self::Unrestricted => None,
        }
    }

    pub fn cycle(self) -> Self {
        match self {
            Self::Normal => Self::Above,
            Self::Above => Self::Below,
            Self::Below => Self::Unrestricted,
            Self::Unrestricted => Self::Normal,
        }
    }

    /// Short form for the soft key and the footer. The two must never disagree, so they share this.
    pub fn label(self) -> &'static str {
        match self {
            Self::Normal => "ALT NRM",
            Self::Above => "ALT ABV",
            Self::Below => "ALT BLW",
            Self::Unrestricted => "ALT ALL",
        }
    }

    /// Whether this band narrows the picture at all. `false` only for [`Self::Unrestricted`].
    pub fn is_narrowing(self) -> bool {
        self.band().is_some()
    }

    /// Should this target be drawn?
    ///
    /// Three ways to be admitted, and only one way to be excluded:
    ///
    /// * **Anything that is not [`ThreatLevel::Normal`] is admitted.** See the type's note: the
    ///   filter removes clutter, and a target the tiers have flagged is not clutter.
    /// * **Unknown relative altitude is admitted.** You cannot exclude what you cannot measure,
    ///   and this is the ordinary case on the ground, where own-ship has no altitude reference and
    ///   every tag reads `---`. Silently emptying the screen there would reproduce precisely the
    ///   failure that `targets_unplotted` exists to prevent.
    /// * **[`Self::Unrestricted`] admits everything.**
    pub fn admits(self, assessment: &Assessment) -> bool {
        if assessment.level != ThreatLevel::Normal {
            return true;
        }
        let Some((below, above)) = self.band() else {
            return true;
        };
        let Some(delta) = assessment.relative_altitude_ft else {
            return true;
        };
        if delta >= 0.0 {
            delta <= above
        } else {
            -delta <= below
        }
    }
}

/// Format a relative altitude the way a traffic display conventionally does: hundreds of feet,
/// signed, or a placeholder when unknown.
///
/// A difference that rounds to zero is rendered unsigned as `00`. Signing it would produce either
/// `+00` or `-00` depending on which side of co-altitude the target happens to be by a few tens of
/// feet — a distinction the display cannot honestly make and that reads as a rendering bug.
pub fn format_relative_altitude(relative_ft: Option<f32>) -> String {
    match relative_ft {
        Some(delta) => {
            let hundreds = (delta / 100.0).round() as i32;
            if hundreds == 0 {
                "00".into()
            } else {
                format!("{hundreds:+03}")
            }
        }
        None => "---".into(),
    }
}
