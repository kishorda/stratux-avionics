//! Dead reckoning between position reports.
//!
//! ADS-B positions arrive at roughly 1 Hz but the panel refreshes at 30–60 Hz. Drawing the raw
//! reported position makes every target jump once a second, which reads as a stuttering,
//! untrustworthy display. Extrapolating along the last known track and speed makes motion
//! continuous.
//!
//! Two guards keep this honest rather than inventive:
//!
//! * Extrapolation is capped ([`ReckonConfig::max_extrapolation`]). Past that the last real
//!   position is drawn and the target is marked coasting, because a confident-looking symbol
//!   several miles from where the aircraft actually is would be worse than an obviously stale one.
//! * Targets whose *fix* is already old are not extrapolated at all. Stratux reports `Age` from
//!   the last real fix, independent of when it sent us the message.
//!
//! Note there is no double-extrapolation risk when Stratux is itself coasting a target: the
//! reported position is valid as of the moment we received it, and we extrapolate from receipt.

use std::time::{Duration, Instant};

use stratux_client::domain::{LatLon, Target};

use crate::projection::advance;

#[derive(Debug, Clone)]
pub struct ReckonConfig {
    /// Never extrapolate further ahead than this.
    ///
    /// A little over one update interval: enough to bridge a missed report, not enough to invent
    /// a position. At 300 kt, 3 s is a quarter of a nautical mile.
    pub max_extrapolation: Duration,
    /// Don't extrapolate a target whose last real fix is older than this.
    pub max_fix_age_s: f64,
}

impl Default for ReckonConfig {
    fn default() -> Self {
        Self {
            max_extrapolation: Duration::from_secs(3),
            max_fix_age_s: 10.0,
        }
    }
}

/// Where to draw a target, and how much to trust it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reckoned {
    pub position: LatLon,
    /// Seconds of extrapolation applied. Zero means the reported position is drawn as-is.
    pub extrapolated_s: f64,
    /// The position is a guess: either Stratux was already coasting, or the fix is stale, or we
    /// hit the extrapolation cap. The symbol should be drawn hollow/dimmed.
    pub coasting: bool,
}

/// Compute where a target should be drawn now.
///
/// Returns `None` for targets with no position at all (Mode-S only), which belong in a count
/// rather than on the plan view.
pub fn reckon(target: &Target, now: Instant, config: &ReckonConfig) -> Option<Reckoned> {
    let reported = target.position?;

    // Stratux's own extrapolation flag, or a fix too old to project forward from.
    let fix_is_stale = target.age_s > config.max_fix_age_s;
    let already_coasting = target.extrapolated || fix_is_stale;

    let elapsed = now.saturating_duration_since(target.received).as_secs_f64();

    // Without a velocity solution there is nothing to extrapolate along.
    let (Some(track), Some(speed)) = (target.track_deg, target.ground_speed_kt) else {
        return Some(Reckoned {
            position: reported,
            extrapolated_s: 0.0,
            coasting: already_coasting,
        });
    };

    if fix_is_stale {
        // Freeze rather than project from a position we already distrust.
        return Some(Reckoned {
            position: reported,
            extrapolated_s: 0.0,
            coasting: true,
        });
    }

    let cap = config.max_extrapolation.as_secs_f64();
    let applied = elapsed.min(cap);
    let hit_cap = elapsed > cap;

    Some(Reckoned {
        position: advance(reported, track as f64, speed as f64, applied),
        extrapolated_s: applied,
        coasting: already_coasting || hit_cap,
    })
}

/// Own-ship's drawn position, extrapolated the same way.
///
/// Own-ship arrives at 10 Hz so the visible benefit is small, but it matters for consistency: if
/// traffic is extrapolated and own-ship is not, every relative position is wrong by own-ship's
/// movement during the gap.
pub fn reckon_ownship(
    position: LatLon,
    track_deg: Option<f32>,
    ground_speed_kt: Option<f64>,
    received: Option<Instant>,
    now: Instant,
    config: &ReckonConfig,
) -> LatLon {
    let (Some(track), Some(speed), Some(received)) = (track_deg, ground_speed_kt, received) else {
        return position;
    };
    let elapsed = now
        .saturating_duration_since(received)
        .as_secs_f64()
        .min(config.max_extrapolation.as_secs_f64());
    advance(position, track as f64, speed, elapsed)
}
