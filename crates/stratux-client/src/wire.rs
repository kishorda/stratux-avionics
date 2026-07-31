//! Stratux's JSON on the wire, mirrored field-for-field.
//!
//! Stratux's Go structs carry **no JSON tags**, so the Go field names *are* the wire keys —
//! `Icao_addr`, `Position_valid`, `GPSFixQuality`, and so on. Those names are inconsistently
//! cased (`Icao_addr` next to `GPSLatitude` next to `OnGround`), so there is no `rename_all`
//! rule that covers them.
//!
//! Rather than write ~40 individual `#[serde(rename = "...")]` attributes per struct — every
//! one a chance to typo a key into silent `Default::default()` — the Rust fields are named
//! *exactly* as the Go fields are and `non_snake_case` is allowed for this module. That makes
//! these declarations diffable against upstream source by eye, which is the property that
//! matters most here. [`crate::domain`] is the clean, idiomatic API; this module is a boundary.
//!
//! Two invariants hold throughout:
//!
//! * **Every field is `#[serde(default)]`.** An upstream rename or removal must degrade one
//!   value, not fail the whole message. `serde` ignores unknown fields by default, so we only
//!   declare what we consume and new upstream fields are harmless.
//! * **`time.Time` fields are kept as `String`.** Go marshals them as RFC 3339, but Stratux
//!   stamps several of them from its own monotonic `stratuxClock`, which makes them useless as
//!   wall-clock values. The `Age*` float-seconds fields are the trustworthy ones, so we avoid a
//!   date-time dependency entirely.
//!
//! Verified against `stratux/stratux` @ master: `main/traffic.go`, `main/gps.go`,
//! `main/gen_gdl90.go`, `uatparse/uatparse.go`.

#![allow(non_snake_case)]

use serde::{Deserialize, Serialize};

/// One tracked target, from the `/traffic` socket. Mirrors `TrafficInfo` in `main/traffic.go`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct TrafficInfo {
    pub Icao_addr: u32,
    /// Registration, derived by Stratux from `Icao_addr` for US civil aircraft.
    pub Reg: String,
    /// Callsign as actually transmitted by the aircraft. Often blank.
    pub Tail: String,
    /// GDL90 emitter category (A7 becomes 0x07, B0 becomes 0x08, ...).
    pub Emitter_category: u8,
    pub OnGround: bool,
    pub TargetType: u8,
    /// Last frequency the target was heard on. See [`crate::domain::TrafficSource`].
    pub Last_source: u8,
    pub SignalLevel: f64,
    pub Squawk: i32,
    /// Set once a position report has been received. Targets without this are Mode-S only.
    pub Position_valid: bool,
    /// Decimal degrees. Note this is `float32` upstream, so widening to `f64` leaves roughly
    /// 1e-6 degrees (~0.2 m) of rounding. Fine for a plan view; never use a position as an
    /// exact map key or compare one for equality.
    pub Lat: f32,
    pub Lng: f32,
    /// Pressure altitude in feet, unless `AltIsGNSS`.
    pub Alt: i32,
    pub AltIsGNSS: bool,
    pub GnssDiffFromBaroAlt: i32,
    /// Navigation Integrity Category.
    pub NIC: i32,
    /// Navigation Accuracy Category for Position.
    pub NACp: i32,
    /// Degrees true.
    pub Track: f32,
    pub TurnRate: f32,
    /// Knots.
    pub Speed: u16,
    pub Speed_valid: bool,
    /// Feet per minute.
    pub Vvel: i16,
    /// Seconds since the last valid position fix or Mode-S transmission.
    pub Age: f64,
    pub AgeLastAlt: f64,
    /// True when Stratux is coasting the target from its last known position.
    pub ExtrapolatedPosition: bool,
    /// Set when `Bearing` and `Distance` below are meaningful.
    pub BearingDist_valid: bool,
    /// Degrees true from own-ship. Used only as an independent cross-check on our own
    /// projection maths — see [`crate::domain::Target`].
    pub Bearing: f64,
    /// Metres from own-ship.
    pub Distance: f64,
    pub ReceivedMsgs: u64,
    pub Timestamp: String,
}

/// Own-ship state, from the `/situation` socket (pushed at 10 Hz).
/// Mirrors `SituationData` in `main/gps.go`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct SituationData {
    pub GPSLatitude: f32,
    pub GPSLongitude: f32,
    /// 0 = no fix, 1 = 3D GPS, 2 = DGPS/SBAS. See [`crate::domain::GpsFix`].
    pub GPSFixQuality: u8,
    pub GPSSatellites: u16,
    pub GPSSatellitesTracked: u16,
    pub GPSSatellitesSeen: u16,
    pub GPSHorizontalAccuracy: f32,
    pub GPSVerticalAccuracy: f32,
    pub GPSNACp: u8,
    /// Feet.
    pub GPSAltitudeMSL: f32,
    pub GPSHeightAboveEllipsoid: f32,
    pub GPSGeoidSep: f32,
    /// Feet per minute.
    pub GPSVerticalSpeed: f32,
    /// Degrees true.
    pub GPSTrueCourse: f32,
    pub GPSTurnRate: f64,
    /// Knots.
    pub GPSGroundSpeed: f64,
    pub GPSPositionSampleRate: f64,
    pub GPSTime: String,
    pub GPSLastFixLocalTime: String,

    /// Feet. Only present if a pressure sensor is fitted.
    pub BaroPressureAltitude: f32,
    pub BaroVerticalSpeed: f32,
    pub BaroTemperature: f32,
    pub BaroSourceType: u8,

    // No IMU is fitted on this build, so these stay at their defaults. Declared anyway so a
    // future AHRS addition is a domain change rather than a wire change.
    pub AHRSPitch: f64,
    pub AHRSRoll: f64,
    pub AHRSGyroHeading: f64,
    pub AHRSMagHeading: f64,
    pub AHRSSlipSkid: f64,
    pub AHRSTurnRate: f64,
    pub AHRSGLoad: f64,
    pub AHRSStatus: u8,
}

/// A decoded FIS-B text product, from the `/weather` socket.
/// Mirrors `WeatherMessage` in `main/gen_gdl90.go`.
///
/// Note that `handleWeatherWS` only subscribes the socket to future broadcasts — despite what
/// the HTTP API docs claim, it does **not** replay the current weather buffer on connect. See
/// [`crate::state`], which therefore never clears weather on reconnect.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct WeatherMessage {
    /// "METAR", "TAF", "PIREP", "WINDS", "NOTAM", ...
    pub Type: String,
    pub Location: String,
    pub Time: String,
    /// The raw product text.
    pub Data: String,
    pub LocaltimeReceived: String,
}

/// One NEXRAD block, already geo-referenced and RLE-expanded by Stratux.
/// Mirrors `NEXRADBlock` in `uatparse/uatparse.go`.
///
/// This is why the display does not need its own FIS-B APDU / block-63-64 / run-length
/// decoder: `uatparse` has already done all of it.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct NEXRADBlock {
    /// FIS-B product id: 63 = regional, 64 = CONUS.
    pub Radar_Type: u32,
    /// Block scale factor (1, 5 or 9); scales both `Height` and `Width`.
    pub Scale: i32,
    /// Decimal degrees, north edge.
    pub LatNorth: f64,
    /// Decimal degrees, west edge.
    pub LonWest: f64,
    /// Degrees of latitude spanned.
    pub Height: f64,
    /// Degrees of longitude spanned.
    pub Width: f64,
    /// 128 intensity values, 0..=7. Upstream comment: "Really only 4-bit values, but using
    /// this as a hack for the JSON encoding."
    pub Intensity: Vec<u16>,
}

/// A decoded UAT uplink frame. Mirrors `UATFrame` in `uatparse/uatparse.go`.
///
/// Reachable only via `/jsonio`, which multiplexes four unrelated object types onto one socket
/// with no envelope or discriminator — see [`crate::decode::classify`].
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct UATFrame {
    /// FIS-B product id. Its presence is what identifies this shape on `/jsonio`.
    pub Product_id: u32,
    pub Frame_type: u32,
    pub Text_data: Vec<String>,
    pub NEXRAD: Vec<NEXRADBlock>,
    pub FISB_month: u32,
    pub FISB_day: u32,
    pub FISB_hours: u32,
    pub FISB_minutes: u32,
    pub FISB_seconds: u32,
}

/// System status, from the `/status` socket (pushed at 1 Hz).
/// Mirrors `status` in `main/gen_gdl90.go`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Status {
    pub Version: String,
    pub Build: String,
    pub HardwareBuild: String,
    /// Bitfield of attached SDRs.
    pub Devices: u32,
    pub Connected_Users: u32,
    pub DiskBytesFree: u64,

    pub UAT_messages_last_minute: u32,
    pub UAT_messages_max: u32,
    pub ES_messages_last_minute: u32,
    pub ES_messages_max: u32,
    pub UAT_traffic_targets_tracking: u16,
    pub ES_traffic_targets_tracking: u16,
    pub UATRadio_connected: bool,

    pub GPS_satellites_locked: u16,
    pub GPS_satellites_seen: u16,
    pub GPS_satellites_tracked: u16,
    pub GPS_position_accuracy: f32,
    pub GPS_connected: bool,
    /// Human-readable fix description, e.g. "3D GPS + SBAS".
    pub GPS_solution: String,

    pub Uptime: i64,
    /// Degrees Celsius. The Pi 3 throttles at 80 C and two SDRs in an enclosure run hot.
    pub CPUTemp: f32,
    pub CPUTempMin: f32,
    pub CPUTempMax: f32,

    pub UAT_METAR_total: u32,
    pub UAT_TAF_total: u32,
    pub UAT_NEXRAD_total: u32,
    pub UAT_SIGMET_total: u32,
    pub UAT_PIREP_total: u32,
    pub UAT_NOTAM_total: u32,
    pub UAT_OTHER_total: u32,

    pub Errors: Vec<String>,
    pub BMPConnected: bool,
    pub IMUConnected: bool,
    pub NightMode: bool,
}
