//! The display's own model of the world.
//!
//! Deliberately separate from [`crate::wire`]: units are in the names, absent values are
//! `Option` rather than a sentinel zero, and Stratux's integer codes become enums. If upstream
//! renames a field, only [`crate::decode`] changes.

use std::time::Instant;

/// A geographic position in decimal degrees, north/east positive.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LatLon {
    pub lat: f64,
    pub lon: f64,
}

impl LatLon {
    pub fn new(lat: f64, lon: f64) -> Self {
        Self { lat, lon }
    }

    /// Reject the (0, 0) null-island position and anything out of range.
    ///
    /// Stratux zero-initialises its structs, so a target whose position has not arrived yet
    /// reports exactly 0,0 rather than absent. Drawing that would put a phantom target in the
    /// Gulf of Guinea.
    pub fn is_plausible(&self) -> bool {
        self.lat.abs() <= 90.0
            && self.lon.abs() <= 180.0
            && !(self.lat == 0.0 && self.lon == 0.0)
            && !self.lat.is_nan()
            && !self.lon.is_nan()
    }
}

/// GPS fix quality, from `GPSFixQuality`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GpsFix {
    #[default]
    None,
    /// 3D GPS, no augmentation.
    ThreeD,
    /// Differential / SBAS (WAAS in the US).
    Differential,
    Unknown(u8),
}

impl GpsFix {
    pub fn from_raw(raw: u8) -> Self {
        match raw {
            0 => Self::None,
            1 => Self::ThreeD,
            2 => Self::Differential,
            other => Self::Unknown(other),
        }
    }

    /// Whether the position may be used for navigation display.
    pub fn is_usable(&self) -> bool {
        matches!(self, Self::ThreeD | Self::Differential)
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::None => "NO GPS",
            Self::ThreeD => "3D",
            Self::Differential => "3D+SBAS",
            Self::Unknown(_) => "GPS?",
        }
    }
}

/// Which radio last heard a target, from `Last_source`.
///
/// Values from `TRAFFIC_SOURCE_*` in upstream `main/traffic.go`: 1090ES = 1, UAT = 2, OGN = 4,
/// AIS = 8. Knowing this matters operationally: a UAT-only target means the 978 MHz receiver is
/// working, and ADS-B rebroadcast traffic is only as good as its ground-station coverage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrafficSource {
    #[default]
    Unknown,
    /// 1090 MHz Extended Squitter.
    Es1090,
    /// 978 MHz Universal Access Transceiver.
    Uat978,
    /// Open Glider Network.
    Ogn,
    Ais,
}

impl TrafficSource {
    pub fn from_raw(raw: u8) -> Self {
        match raw {
            1 => Self::Es1090,
            2 => Self::Uat978,
            4 => Self::Ogn,
            8 => Self::Ais,
            _ => Self::Unknown,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Es1090 => "1090",
            Self::Uat978 => "978",
            Self::Ogn => "OGN",
            Self::Ais => "AIS",
            Self::Unknown => "?",
        }
    }
}

/// How a target's position was obtained, from `TargetType`.
///
/// `TARGET_TYPE_*` in upstream `main/traffic.go`. TIS-B and ADS-R are rebroadcasts and can be
/// stale or duplicated relative to direct ADS-B, which is worth showing the pilot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TargetType {
    /// Mode-S only: no position, altitude and identity only.
    #[default]
    ModeS,
    Adsb,
    /// ADS-B rebroadcast.
    AdsR,
    /// TIS-B derived from Mode-S.
    TisbS,
    Tisb,
    Ais,
    Unknown(u8),
}

impl TargetType {
    pub fn from_raw(raw: u8) -> Self {
        match raw {
            0 => Self::ModeS,
            1 => Self::Adsb,
            2 => Self::AdsR,
            3 => Self::TisbS,
            4 => Self::Tisb,
            5 => Self::Ais,
            other => Self::Unknown(other),
        }
    }

    /// Rebroadcast traffic, which may lag or duplicate direct reception.
    pub fn is_rebroadcast(&self) -> bool {
        matches!(self, Self::AdsR | Self::TisbS | Self::Tisb)
    }
}

/// Own-ship state.
#[derive(Debug, Clone, Default)]
pub struct OwnShip {
    pub position: Option<LatLon>,
    pub fix: GpsFix,
    pub satellites_locked: u16,
    pub satellites_seen: u16,
    /// Feet above mean sea level, from GPS.
    pub altitude_msl_ft: Option<f32>,
    /// Feet, from a pressure sensor if one is fitted. This build has none, so usually `None`.
    pub pressure_altitude_ft: Option<f32>,
    /// Degrees true.
    pub track_deg: Option<f32>,
    pub ground_speed_kt: Option<f64>,
    /// Feet per minute.
    pub vertical_speed_fpm: Option<f32>,
    pub horizontal_accuracy_m: Option<f32>,
    pub turn_rate_deg_s: Option<f64>,
    pub received: Option<Instant>,
}

impl OwnShip {
    /// A position good enough to centre the plan view on.
    pub fn usable_position(&self) -> Option<LatLon> {
        self.position.filter(|_| self.fix.is_usable())
    }

    /// Best available altitude for computing relative altitude to traffic.
    ///
    /// Traffic reports pressure altitude, so pressure altitude is preferred for a like-for-like
    /// comparison; GPS MSL is the fallback and will disagree by the local altimeter error.
    pub fn comparison_altitude_ft(&self) -> Option<f32> {
        self.pressure_altitude_ft.or(self.altitude_msl_ft)
    }
}

/// One tracked target.
#[derive(Debug, Clone)]
pub struct Target {
    pub icao: u32,
    /// Transmitted callsign, falling back to Stratux's derived registration.
    pub identity: Option<String>,
    pub position: Option<LatLon>,
    /// Feet. Pressure altitude unless [`Self::altitude_is_gnss`].
    pub altitude_ft: Option<i32>,
    pub altitude_is_gnss: bool,
    pub on_ground: bool,
    /// Degrees true.
    pub track_deg: Option<f32>,
    pub ground_speed_kt: Option<u16>,
    /// Feet per minute.
    pub vertical_speed_fpm: Option<i16>,
    pub emitter_category: u8,
    pub target_type: TargetType,
    pub source: TrafficSource,
    pub signal_level_db: f64,
    pub squawk: Option<i32>,
    /// Stratux is coasting this target rather than reporting a fresh fix.
    pub extrapolated: bool,
    /// Seconds since the last valid fix, as reported by Stratux.
    pub age_s: f64,
    /// Seconds since the last altitude message.
    ///
    /// [`Self::altitude_ft`] is passed through unconditionally because 0 ft is a legitimate
    /// pressure altitude and cannot be distinguished from Stratux's zero-initialised "never
    /// received". Use this to decide whether the altitude is trustworthy rather than
    /// second-guessing the value.
    pub age_last_alt_s: f64,
    /// Stratux's own bearing/distance solution, in degrees true and metres.
    ///
    /// The plan view computes its own from [`Self::position`] rather than using these. Keeping
    /// them gives an *independent* check on our projection maths, which is only useful as long
    /// as the two are computed separately — do not "simplify" by drawing from these.
    pub reported_bearing_deg: Option<f64>,
    pub reported_distance_m: Option<f64>,
    pub received: Instant,
}

impl Target {
    /// A label for the target, falling back to the hex ICAO address.
    pub fn label(&self) -> String {
        self.identity
            .clone()
            .unwrap_or_else(|| format!("{:06X}", self.icao))
    }

    /// Whether this target can be drawn on a plan view.
    pub fn is_positional(&self) -> bool {
        self.position.is_some()
    }
}

/// Category of FIS-B text product.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WeatherProduct {
    Metar,
    Taf,
    Pirep,
    Winds,
    Notam,
    Sigmet,
    Airmet,
    Other(String),
}

impl WeatherProduct {
    pub fn from_type(raw: &str) -> Self {
        match raw.trim().to_ascii_uppercase().as_str() {
            "METAR" | "SPECI" => Self::Metar,
            "TAF" | "TAF.AMD" => Self::Taf,
            "PIREP" => Self::Pirep,
            "WINDS" => Self::Winds,
            "NOTAM" | "NOTAM-TFR" | "NOTAM-D" => Self::Notam,
            "SIGMET" | "WST" => Self::Sigmet,
            "AIRMET" => Self::Airmet,
            other => Self::Other(other.to_string()),
        }
    }

    pub fn label(&self) -> &str {
        match self {
            Self::Metar => "METAR",
            Self::Taf => "TAF",
            Self::Pirep => "PIREP",
            Self::Winds => "WINDS",
            Self::Notam => "NOTAM",
            Self::Sigmet => "SIGMET",
            Self::Airmet => "AIRMET",
            Self::Other(s) => s,
        }
    }
}

/// A decoded FIS-B text product.
#[derive(Debug, Clone)]
pub struct WeatherText {
    pub product: WeatherProduct,
    pub location: String,
    /// Issue time as transmitted, e.g. "291853Z". Left as text: it is what a pilot reads.
    pub time: String,
    pub body: String,
    pub received: Instant,
}

/// Which NEXRAD mosaic a block belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NexradKind {
    /// Product 63: higher resolution, regional coverage.
    Regional,
    /// Product 64: coarser, continental coverage.
    Conus,
}

impl NexradKind {
    pub fn from_product_id(id: u32) -> Option<Self> {
        match id {
            63 => Some(Self::Regional),
            64 => Some(Self::Conus),
            _ => None,
        }
    }

    /// Intensity value meaning "the radar looked here and saw nothing".
    ///
    /// This differs between the two products and getting it backwards paints either phantom
    /// precipitation or holes in real coverage: for regional, an empty block means "valid data,
    /// below 5 dBZ"; for CONUS it means "valid data, no precipitation".
    pub fn empty_intensity(&self) -> u8 {
        match self {
            Self::Regional => 0,
            Self::Conus => 1,
        }
    }
}

/// One geo-referenced NEXRAD block.
///
/// The bin grid is 32 wide (longitude) by 4 tall (latitude), filled west-to-east then
/// north-to-south, per the FIS-B global block representation.
#[derive(Debug, Clone)]
pub struct NexradBlock {
    pub kind: NexradKind,
    pub scale: i32,
    /// North edge, decimal degrees.
    pub lat_north: f64,
    /// West edge, decimal degrees.
    pub lon_west: f64,
    /// Degrees of latitude spanned by the whole block.
    pub height_deg: f64,
    /// Degrees of longitude spanned by the whole block.
    pub width_deg: f64,
    /// Intensities 0..=7, `BINS_X * BINS_Y` of them.
    pub bins: Vec<u8>,
    pub received: Instant,
}

impl NexradBlock {
    pub const BINS_X: usize = 32;
    pub const BINS_Y: usize = 4;
    pub const BIN_COUNT: usize = Self::BINS_X * Self::BINS_Y;

    /// Identity for de-duplication: a re-transmitted block replaces the one it supersedes.
    ///
    /// Coordinates are quantised to 1/1000 degree before hashing because they arrive as
    /// floats derived from integer arcminutes, and comparing f64 for equality would let
    /// bit-identical blocks accumulate as duplicates.
    pub fn key(&self) -> BlockKey {
        BlockKey {
            kind: self.kind,
            scale: self.scale,
            lat_north_milli: (self.lat_north * 1000.0).round() as i64,
            lon_west_milli: (self.lon_west * 1000.0).round() as i64,
        }
    }

    /// Intensity at a bin, or `None` if out of range.
    pub fn intensity(&self, x: usize, y: usize) -> Option<u8> {
        if x >= Self::BINS_X || y >= Self::BINS_Y {
            return None;
        }
        self.bins.get(y * Self::BINS_X + x).copied()
    }

    /// Geographic bounds of one bin, as (north-west corner, south-east corner).
    pub fn bin_bounds(&self, x: usize, y: usize) -> Option<(LatLon, LatLon)> {
        if x >= Self::BINS_X || y >= Self::BINS_Y {
            return None;
        }
        let bin_w = self.width_deg / Self::BINS_X as f64;
        let bin_h = self.height_deg / Self::BINS_Y as f64;
        let nw = LatLon::new(
            self.lat_north - y as f64 * bin_h,
            self.lon_west + x as f64 * bin_w,
        );
        let se = LatLon::new(nw.lat - bin_h, nw.lon + bin_w);
        Some((nw, se))
    }

    /// Whether any bin in the block shows precipitation worth drawing.
    pub fn has_precipitation(&self) -> bool {
        let floor = self.kind.empty_intensity();
        self.bins.iter().any(|&v| v > floor)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockKey {
    pub kind: NexradKind,
    pub scale: i32,
    lat_north_milli: i64,
    lon_west_milli: i64,
}

/// Backend health, from the `/status` socket.
#[derive(Debug, Clone, Default)]
pub struct SystemStatus {
    pub version: String,
    pub uptime_s: i64,
    pub cpu_temp_c: f32,
    pub cpu_temp_max_c: f32,
    pub gps_connected: bool,
    pub gps_solution: String,
    pub satellites_locked: u16,
    pub satellites_seen: u16,
    /// 1090 MHz messages in the last minute. Zero while airborne means a dead ES receiver.
    pub es_messages_last_minute: u32,
    /// 978 MHz messages in the last minute.
    pub uat_messages_last_minute: u32,
    pub es_targets_tracking: u16,
    pub uat_targets_tracking: u16,
    pub nexrad_products_total: u32,
    pub metar_products_total: u32,
    pub errors: Vec<String>,
    pub received: Option<Instant>,
}
