//! Turn raw Stratux JSON frames into [`Event`]s.
//!
//! This is the only module that knows Stratux's field names, so an upstream rename is contained
//! here. Decoding is intentionally forgiving: a frame we cannot make sense of is dropped with a
//! log line, never propagated as a fatal error. A cockpit display that dies because one weather
//! product changed shape is worse than one that misses that product.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use serde_json::{Map, Value};

use crate::domain::{
    Ahrs, GpsFix, LatLon, NexradBlock, NexradKind, OwnShip, SystemStatus, Target, TargetType,
    TrafficSource, WeatherProduct, WeatherText,
};
use crate::wire;
use crate::Stream;

/// Something useful that arrived from the backend.
#[derive(Debug, Clone)]
pub enum Event {
    Traffic(Target),
    OwnShip(OwnShip),
    Weather(WeatherText),
    /// One or more NEXRAD blocks from a single uplink frame.
    Nexrad(Vec<NexradBlock>),
    Status(SystemStatus),
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("payload was not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("payload was not a JSON object")]
    NotAnObject,
}

/// What a `/jsonio` payload turned out to be.
///
/// `/jsonio` subscribes one socket to four different broadcasters and writes raw
/// `json.Marshal` output with **no envelope and no type discriminator**, so the only way to
/// tell the shapes apart is by which keys are present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonIoShape {
    /// A `uatparse.UATFrame` — the only reason we connect to this socket.
    UatFrame,
    /// `TrafficInfo`. Discarded: `/traffic` carries the same data with a known type.
    Traffic,
    /// `SituationData`. Discarded: `/situation` carries it at 10 Hz.
    Situation,
    /// `globalSettings`, pushed by `radarUpdate`. Discarded.
    Settings,
    Unrecognised,
}

/// Identify a `/jsonio` payload by key presence.
///
/// Probe keys are chosen to be unique to one shape: `Product_id` appears only on `UATFrame`,
/// `Icao_addr` only on `TrafficInfo`, `GPSFixQuality` only on `SituationData`.
pub fn classify(object: &Map<String, Value>) -> JsonIoShape {
    if object.contains_key("Product_id") {
        JsonIoShape::UatFrame
    } else if object.contains_key("Icao_addr") {
        JsonIoShape::Traffic
    } else if object.contains_key("GPSFixQuality") {
        JsonIoShape::Situation
    } else if object.contains_key("UAT_Enabled") || object.contains_key("ES_Enabled") {
        JsonIoShape::Settings
    } else {
        JsonIoShape::Unrecognised
    }
}

/// Log unrecognised `/jsonio` shapes once rather than on every frame.
static WARNED_UNRECOGNISED: AtomicBool = AtomicBool::new(false);

/// Decode one frame. `Ok(None)` means "understood but deliberately ignored".
pub fn decode(stream: Stream, payload: &[u8], now: Instant) -> Result<Option<Event>, DecodeError> {
    match stream {
        Stream::Traffic => {
            let raw: wire::TrafficInfo = serde_json::from_slice(payload)?;
            Ok(Some(Event::Traffic(target_from_wire(&raw, now))))
        }
        Stream::Situation => {
            let raw: wire::SituationData = serde_json::from_slice(payload)?;
            Ok(Some(Event::OwnShip(ownship_from_wire(&raw, now))))
        }
        Stream::Weather => {
            let raw: wire::WeatherMessage = serde_json::from_slice(payload)?;
            Ok(Some(Event::Weather(weather_from_wire(&raw, now))))
        }
        Stream::Status => {
            let raw: wire::Status = serde_json::from_slice(payload)?;
            Ok(Some(Event::Status(status_from_wire(&raw, now))))
        }
        Stream::JsonIo => {
            let value: Value = serde_json::from_slice(payload)?;
            let object = value.as_object().ok_or(DecodeError::NotAnObject)?;
            match classify(object) {
                JsonIoShape::UatFrame => {
                    let raw: wire::UATFrame = serde_json::from_value(value)?;
                    let blocks = nexrad_from_wire(&raw, now);
                    // Most uplink frames are text products with no NEXRAD payload.
                    if blocks.is_empty() {
                        Ok(None)
                    } else {
                        Ok(Some(Event::Nexrad(blocks)))
                    }
                }
                JsonIoShape::Unrecognised => {
                    if !WARNED_UNRECOGNISED.swap(true, Ordering::Relaxed) {
                        tracing::warn!(
                            keys = ?object.keys().take(12).collect::<Vec<_>>(),
                            "unrecognised /jsonio payload shape; NEXRAD may be unavailable. \
                             Has upstream renamed a field?"
                        );
                    }
                    Ok(None)
                }
                // Deliberately dropped: the dedicated sockets carry these with known types.
                _ => Ok(None),
            }
        }
    }
}

fn non_empty(s: &str) -> Option<String> {
    let trimmed = s.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn target_from_wire(raw: &wire::TrafficInfo, now: Instant) -> Target {
    let position = if raw.Position_valid {
        let pos = LatLon::new(raw.Lat as f64, raw.Lng as f64);
        pos.is_plausible().then_some(pos)
    } else {
        None
    };

    // Track is only meaningful alongside a velocity solution; Stratux has no separate
    // Track_valid flag, so Speed_valid stands in for both.
    let (track_deg, ground_speed_kt) = if raw.Speed_valid {
        (Some(raw.Track), Some(raw.Speed))
    } else {
        (None, None)
    };

    Target {
        icao: raw.Icao_addr,
        // Transmitted callsign is preferred; Stratux's derived registration is the fallback.
        identity: non_empty(&raw.Tail).or_else(|| non_empty(&raw.Reg)),
        position,
        altitude_ft: Some(raw.Alt),
        altitude_is_gnss: raw.AltIsGNSS,
        on_ground: raw.OnGround,
        track_deg,
        ground_speed_kt,
        vertical_speed_fpm: Some(raw.Vvel),
        emitter_category: raw.Emitter_category,
        target_type: TargetType::from_raw(raw.TargetType),
        source: TrafficSource::from_raw(raw.Last_source),
        signal_level_db: raw.SignalLevel,
        squawk: (raw.Squawk != 0).then_some(raw.Squawk),
        extrapolated: raw.ExtrapolatedPosition,
        age_s: raw.Age,
        age_last_alt_s: raw.AgeLastAlt,
        reported_bearing_deg: raw.BearingDist_valid.then_some(raw.Bearing),
        reported_distance_m: raw.BearingDist_valid.then_some(raw.Distance),
        received: now,
    }
}

/// Below this ground speed, GPS track is noise and would make a track-up display spin.
const TRACK_VALID_MIN_GROUND_SPEED_KT: f64 = 2.0;

fn ownship_from_wire(raw: &wire::SituationData, now: Instant) -> OwnShip {
    let fix = GpsFix::from_raw(raw.GPSFixQuality);
    let usable = fix.is_usable();

    let candidate = LatLon::new(raw.GPSLatitude as f64, raw.GPSLongitude as f64);
    let position = (usable && candidate.is_plausible()).then_some(candidate);

    // BaroSourceType 0 means no pressure sensor. This build HAS one (the target reports type 1),
    // so pressure altitude is normally present and traffic comparisons are like-for-like. The
    // gate stays because a build without the sensor must still fall back to GPS MSL.
    let has_baro = raw.BaroSourceType != 0;

    OwnShip {
        position,
        fix,
        satellites_locked: raw.GPSSatellites,
        satellites_seen: raw.GPSSatellitesSeen,
        altitude_msl_ft: usable.then_some(raw.GPSAltitudeMSL),
        pressure_altitude_ft: has_baro.then_some(raw.BaroPressureAltitude),
        track_deg: (usable && raw.GPSGroundSpeed >= TRACK_VALID_MIN_GROUND_SPEED_KT)
            .then_some(raw.GPSTrueCourse),
        ground_speed_kt: usable.then_some(raw.GPSGroundSpeed),
        // A pressure-derived vertical speed is smoother than a GPS one when available.
        vertical_speed_fpm: if has_baro {
            Some(raw.BaroVerticalSpeed)
        } else {
            usable.then_some(raw.GPSVerticalSpeed)
        },
        horizontal_accuracy_m: usable.then_some(raw.GPSHorizontalAccuracy),
        turn_rate_deg_s: usable.then_some(raw.GPSTurnRate),
        ahrs: ahrs_from_wire(raw, now),
        received: Some(now),
    }
}

/// Pull attitude out of the same `/situation` message.
///
/// Every field goes through [`Ahrs::value`], which maps Stratux's 3276.7 sentinel to `None`.
/// Doing it here rather than at the draw site means no drawing code can ever forget: the domain
/// type simply has no way to express "3276.7 degrees of roll".
fn ahrs_from_wire(raw: &wire::SituationData, now: Instant) -> Ahrs {
    // No module reporting: discard every field rather than gating only the ones the attitude
    // indicator happens to read.
    //
    // A Stratux without an AHRS leaves these at Go's zero value, so pitch, roll, slip and G-load
    // all arrive as a plausible-looking 0.0. Gating just `attitude()` produced a screen that
    // showed "AHRS UNAVAILABLE" across the horizon while the numeric readouts underneath it
    // confidently reported PITCH +0.0, ROLL +0.0, HDG 000 — the display contradicting itself,
    // with the wrong half being the more precise-looking one. Zeroing at the boundary means no
    // consumer, present or future, can read a value that was never measured.
    if raw.AHRSStatus == 0 {
        return Ahrs {
            status: 0,
            received: Some(now),
            ..Default::default()
        };
    }

    Ahrs {
        pitch_deg: Ahrs::value(raw.AHRSPitch),
        roll_deg: Ahrs::value(raw.AHRSRoll),
        slip_skid_deg: Ahrs::value(raw.AHRSSlipSkid),
        turn_rate_deg_s: Ahrs::value(raw.AHRSTurnRate),
        g_load: Ahrs::value(raw.AHRSGLoad),
        g_load_min: Ahrs::value(raw.AHRSGLoadMin),
        g_load_max: Ahrs::value(raw.AHRSGLoadMax),
        gyro_heading_deg: Ahrs::value(raw.AHRSGyroHeading),
        mag_heading_deg: Ahrs::value(raw.AHRSMagHeading),
        status: raw.AHRSStatus,
        // Timestamped on arrival, not from AHRSLastAttitudeTime: that field is a Go zero-time
        // ("0001-01-01T...") on this target, so it says nothing about freshness. Arrival time is
        // what actually answers "has the sensor stopped talking to us".
        received: Some(now),
    }
}

fn weather_from_wire(raw: &wire::WeatherMessage, now: Instant) -> WeatherText {
    WeatherText {
        product: WeatherProduct::from_type(&raw.Type),
        location: raw.Location.trim().to_string(),
        time: raw.Time.trim().to_string(),
        body: raw.Data.trim().to_string(),
        received: now,
    }
}

/// Log a malformed NEXRAD bin count once rather than per block.
static WARNED_BIN_COUNT: AtomicBool = AtomicBool::new(false);

fn nexrad_from_wire(raw: &wire::UATFrame, now: Instant) -> Vec<NexradBlock> {
    raw.NEXRAD
        .iter()
        .filter_map(|block| {
            // The block's own product id is authoritative; the enclosing frame's is a fallback.
            let kind = NexradKind::from_product_id(block.Radar_Type)
                .or_else(|| NexradKind::from_product_id(raw.Product_id))?;

            let mut bins: Vec<u8> = block
                .Intensity
                .iter()
                // Upstream stores 4-bit values in u16 "as a hack for the JSON encoding"; the
                // defined range is 0..=7.
                .map(|&v| v.min(7) as u8)
                .collect();

            if bins.len() != NexradBlock::BIN_COUNT {
                if !WARNED_BIN_COUNT.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        got = bins.len(),
                        want = NexradBlock::BIN_COUNT,
                        "NEXRAD block had an unexpected bin count; padding/truncating"
                    );
                }
                // Normalise rather than discard: a partially decoded block is still better
                // weather information than none, and the geometry is per-bin regardless.
                bins.resize(NexradBlock::BIN_COUNT, kind.empty_intensity());
            }

            Some(NexradBlock {
                kind,
                scale: block.Scale,
                lat_north: block.LatNorth,
                lon_west: block.LonWest,
                height_deg: block.Height,
                width_deg: block.Width,
                bins,
                received: now,
            })
        })
        .collect()
}

fn status_from_wire(raw: &wire::Status, now: Instant) -> SystemStatus {
    SystemStatus {
        version: raw.Version.clone(),
        uptime_s: raw.Uptime,
        cpu_temp_c: raw.CPUTemp,
        cpu_temp_max_c: raw.CPUTempMax,
        gps_connected: raw.GPS_connected,
        gps_solution: raw.GPS_solution.clone(),
        satellites_locked: raw.GPS_satellites_locked,
        satellites_seen: raw.GPS_satellites_seen,
        es_messages_last_minute: raw.ES_messages_last_minute,
        uat_messages_last_minute: raw.UAT_messages_last_minute,
        es_targets_tracking: raw.ES_traffic_targets_tracking,
        uat_targets_tracking: raw.UAT_traffic_targets_tracking,
        nexrad_products_total: raw.UAT_NEXRAD_total,
        metar_products_total: raw.UAT_METAR_total,
        errors: raw.Errors.clone(),
        received: Some(now),
    }
}
