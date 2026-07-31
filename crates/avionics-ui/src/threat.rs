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
