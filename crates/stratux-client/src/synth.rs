//! Generate a synthetic Stratux session.
//!
//! Two jobs. First, it unblocks the plan view before the aircraft or even the Pi is available.
//! Second, it produces *known* inputs — a target at a chosen bearing and range, precipitation in
//! a chosen place — which real recordings cannot, so it is what threat-tier and projection tests
//! assert against.
//!
//! Frames are produced by serialising the same [`crate::wire`] structs the decoder reads, so a
//! synthetic session exercises the real parsing path rather than a shortcut around it.
//!
//! Generation is fully deterministic for a given [`SynthConfig`]: the "randomness" is a small
//! xorshift seeded from the config, so a failing test reproduces exactly.

use std::time::Duration;

use crate::domain::LatLon;
use crate::{wire, Frame, Stream};

#[derive(Debug, Clone)]
pub struct SynthConfig {
    pub duration: Duration,
    /// Own-ship starting position. Defaults to near Rocky Mountain Metro (KBJC).
    pub start: LatLon,
    pub start_altitude_ft: f32,
    pub start_track_deg: f32,
    pub ground_speed_kt: f64,
    pub target_count: usize,
    /// Include FIS-B text products and a NEXRAD mosaic.
    pub weather: bool,
    /// Add one target closing head-on at co-altitude, plus one Mode-S target with no position.
    ///
    /// Random targets almost never land inside the alert box, so without this the alert path and
    /// the no-position counter are never exercised. Both are deterministic.
    pub conflict: bool,
    pub seed: u64,
}

impl Default for SynthConfig {
    fn default() -> Self {
        Self {
            duration: Duration::from_secs(120),
            start: LatLon::new(39.9088, -105.1172),
            start_altitude_ft: 8500.0,
            start_track_deg: 43.0,
            ground_speed_kt: 118.0,
            target_count: 8,
            weather: true,
            conflict: false,
            seed: 0x5241_4441_5231,
        }
    }
}

/// Deterministic xorshift64*. A real PRNG dependency would be overkill for test fixtures.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // Guard against the all-zero state, which xorshift cannot escape.
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in [0, 1).
    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + self.unit() * (hi - lo)
    }
}

/// Nautical miles per degree of latitude.
const NM_PER_DEG_LAT: f64 = 60.0;

/// Move a position along a track. Flat-earth, which is accurate far beyond ADS-B range.
fn advance(from: LatLon, track_deg: f64, speed_kt: f64, dt_s: f64) -> LatLon {
    let distance_nm = speed_kt * dt_s / 3600.0;
    let heading = track_deg.to_radians();
    let d_lat = distance_nm * heading.cos() / NM_PER_DEG_LAT;
    let d_lon =
        distance_nm * heading.sin() / (NM_PER_DEG_LAT * from.lat.to_radians().cos().max(1e-6));
    LatLon::new(from.lat + d_lat, from.lon + d_lon)
}

/// Offset a position by a bearing and range, in nautical miles.
fn offset(from: LatLon, bearing_deg: f64, range_nm: f64) -> LatLon {
    advance(from, bearing_deg, range_nm * 3600.0, 1.0)
}

struct SynthTarget {
    icao: u32,
    callsign: String,
    position: LatLon,
    track_deg: f64,
    speed_kt: f64,
    altitude_ft: i32,
    vertical_speed_fpm: i16,
    source: u8,
    emitter_category: u8,
}

/// Build a full session. Frames come back sorted by offset, as a recording would be.
pub fn generate(config: &SynthConfig) -> Vec<Frame> {
    let mut rng = Rng::new(config.seed);
    let mut frames = Vec::new();

    // Targets are placed at a spread of bearings and ranges around own-ship, with a mix of
    // relative altitudes so threat tiers have something to bite on.
    let mut targets: Vec<SynthTarget> = (0..config.target_count)
        .map(|i| {
            let bearing = rng.range(0.0, 360.0);
            let range_nm = rng.range(1.5, 18.0);
            let relative_alt = rng.range(-4000.0, 4000.0);
            SynthTarget {
                icao: 0xA0_0000 + (i as u32) * 0x1D7 + 0x51,
                callsign: format!("N{:03}{}", 100 + i * 7, ["TC", "KL", "AB", "XR"][i % 4]),
                position: offset(config.start, bearing, range_nm),
                track_deg: rng.range(0.0, 360.0),
                speed_kt: rng.range(70.0, 320.0),
                altitude_ft: (config.start_altitude_ft as f64 + relative_alt) as i32,
                vertical_speed_fpm: (rng.range(-800.0, 800.0) / 100.0).round() as i16 * 100,
                // Mostly 1090ES with some UAT, matching a typical US traffic picture.
                source: if rng.unit() < 0.25 { 2 } else { 1 },
                emitter_category: if rng.unit() < 0.2 { 3 } else { 1 },
            }
        })
        .collect();

    if config.conflict {
        // Head-on at co-altitude, 4 nm ahead and closing. Starts outside the alert box and works
        // its way in, so a filmstrip captures the Normal -> Advisory -> Alert transitions.
        let ahead = config.start_track_deg as f64;
        targets.push(SynthTarget {
            icao: 0x00_BAD1,
            callsign: "CONFLICT".into(),
            position: offset(config.start, ahead, 4.0),
            track_deg: (ahead + 180.0).rem_euclid(360.0),
            speed_kt: 140.0,
            altitude_ft: config.start_altitude_ft as i32,
            vertical_speed_fpm: 0,
            source: 1,
            emitter_category: 1,
        });
    }

    let mut ownship_pos = config.start;
    let mut ownship_track = config.start_track_deg as f64;

    let total_ms = config.duration.as_millis() as u64;
    // 10 Hz own-ship matches Stratux's /situation ticker.
    const SITUATION_PERIOD_MS: u64 = 100;

    let mut next_traffic_ms = 0u64;
    let mut next_status_ms = 0u64;
    let mut next_weather_ms = 2_000u64;
    let mut next_nexrad_ms = 3_000u64;

    for t_ms in (0..total_ms).step_by(SITUATION_PERIOD_MS as usize) {
        let dt_s = SITUATION_PERIOD_MS as f64 / 1000.0;
        let offset_ms = t_ms;

        // A slow S-turn keeps track-up mode honest.
        ownship_track += (t_ms as f64 / 12_000.0).sin() * 0.35;
        ownship_pos = advance(ownship_pos, ownship_track, config.ground_speed_kt, dt_s);

        frames.push(frame(
            Stream::Situation,
            offset_ms,
            &situation(config, ownship_pos, ownship_track, t_ms),
        ));

        if t_ms >= next_traffic_ms {
            next_traffic_ms += 1_000;
            for target in targets.iter_mut() {
                target.position = advance(target.position, target.track_deg, target.speed_kt, 1.0);
                target.altitude_ft += target.vertical_speed_fpm as i32 / 60;
                frames.push(frame(
                    Stream::Traffic,
                    offset_ms,
                    &traffic(target, ownship_pos),
                ));
            }

            if config.conflict {
                // A Mode-S-only target: altitude and identity but no position, which is what
                // exercises the "+N nopos" counter rather than the plan view.
                frames.push(frame(
                    Stream::Traffic,
                    offset_ms,
                    &wire::TrafficInfo {
                        Icao_addr: 0x00_5EC0,
                        Tail: "MODES".into(),
                        TargetType: 0, // Mode-S
                        Last_source: 1,
                        Position_valid: false,
                        Alt: 11_500,
                        Age: 1.0,
                        AgeLastAlt: 1.0,
                        ..Default::default()
                    },
                ));
            }
        }

        if t_ms >= next_status_ms {
            next_status_ms += 1_000;
            frames.push(frame(Stream::Status, offset_ms, &status(t_ms)));
        }

        if config.weather {
            if t_ms >= next_weather_ms {
                next_weather_ms += 30_000;
                for message in weather_batch(&mut rng) {
                    frames.push(frame(Stream::Weather, offset_ms, &message));
                }
            }
            if t_ms >= next_nexrad_ms {
                next_nexrad_ms += 60_000;
                frames.push(frame(
                    Stream::JsonIo,
                    offset_ms,
                    &nexrad_frame(ownship_pos, &mut rng),
                ));
            }
        }
    }

    frames.sort_by_key(|f| f.offset);
    frames
}

fn frame<T: serde::Serialize>(stream: Stream, offset_ms: u64, value: &T) -> Frame {
    Frame {
        stream,
        offset: Duration::from_millis(offset_ms),
        // Serialising the real wire struct means synthetic sessions exercise the real decoder.
        payload: serde_json::to_vec(value).expect("wire structs are always serialisable"),
    }
}

fn situation(
    config: &SynthConfig,
    position: LatLon,
    track_deg: f64,
    t_ms: u64,
) -> wire::SituationData {
    let t = t_ms as f64 / 1000.0;
    // Slow, shallow oscillation: enough to prove the horizon moves and in which direction,
    // without pretending to model flight dynamics.
    let pitch = 2.5 * (t * 0.11).sin();
    let roll = 12.0 * (t * 0.07).sin();

    wire::SituationData {
        GPSLatitude: position.lat as f32,
        GPSLongitude: position.lon as f32,
        // 2 = DGPS/SBAS, what a GPYes 2.0 reports with WAAS in view.
        GPSFixQuality: 2,
        GPSSatellites: 11,
        GPSSatellitesTracked: 14,
        GPSSatellitesSeen: 17,
        GPSHorizontalAccuracy: 3.1,
        GPSVerticalAccuracy: 4.6,
        GPSNACp: 10,
        GPSAltitudeMSL: config.start_altitude_ft,
        GPSHeightAboveEllipsoid: config.start_altitude_ft - 55.0,
        GPSVerticalSpeed: 0.0,
        GPSTrueCourse: track_deg.rem_euclid(360.0) as f32,
        GPSGroundSpeed: config.ground_speed_kt,
        GPSPositionSampleRate: 10.0,
        // The target DOES have a pressure sensor (it reports BaroSourceType 1), so synthetic
        // sessions carry one too and exercise the pressure-altitude path the aircraft uses.
        BaroSourceType: 1,
        BaroPressureAltitude: config.start_altitude_ft - 40.0,
        BaroVerticalSpeed: 0.0,
        BaroTemperature: 12.0,

        // AHRS, mirroring what the target actually reports: live pitch/roll/slip/G-load, and the
        // 3276.7 "no reading" sentinel for gyro heading, mag heading and turn rate. Emitting the
        // sentinel rather than zeros matters — zeros would decode as a valid wings-level attitude
        // and hide exactly the bug the AHRS page exists to prevent.
        AHRSPitch: pitch,
        AHRSRoll: roll,
        AHRSSlipSkid: 1.5 * (t * 0.09).cos(),
        AHRSGLoad: 1.0 + 0.03 * (t * 0.13).sin(),
        AHRSGLoadMin: 0.94,
        AHRSGLoadMax: 1.07,
        AHRSGyroHeading: crate::domain::AHRS_INVALID,
        AHRSMagHeading: crate::domain::AHRS_INVALID,
        AHRSTurnRate: crate::domain::AHRS_INVALID,
        // Non-zero: a module is reporting. Zero would mean "no AHRS fitted".
        AHRSStatus: 6,
        ..Default::default()
    }
}

fn traffic(target: &SynthTarget, ownship: LatLon) -> wire::TrafficInfo {
    // Stratux computes these itself; include them so the cross-check in the plan view has
    // something to compare against.
    let (bearing, distance_m) = bearing_distance(ownship, target.position);

    wire::TrafficInfo {
        Icao_addr: target.icao,
        Reg: target.callsign.clone(),
        Tail: target.callsign.clone(),
        Emitter_category: target.emitter_category,
        OnGround: false,
        TargetType: 1, // ADS-B
        Last_source: target.source,
        SignalLevel: -18.0 - (distance_m / 3000.0),
        Squawk: 1200,
        Position_valid: true,
        Lat: target.position.lat as f32,
        Lng: target.position.lon as f32,
        Alt: target.altitude_ft,
        AltIsGNSS: false,
        NIC: 8,
        NACp: 9,
        Track: target.track_deg.rem_euclid(360.0) as f32,
        Speed: target.speed_kt as u16,
        Speed_valid: true,
        Vvel: target.vertical_speed_fpm,
        Age: 0.4,
        AgeLastAlt: 0.4,
        BearingDist_valid: true,
        Bearing: bearing,
        Distance: distance_m,
        ReceivedMsgs: 128,
        ..Default::default()
    }
}

/// Great-circle-ish bearing and distance. Flat-earth is fine at these ranges.
fn bearing_distance(from: LatLon, to: LatLon) -> (f64, f64) {
    let d_lat_nm = (to.lat - from.lat) * NM_PER_DEG_LAT;
    let d_lon_nm = (to.lon - from.lon) * NM_PER_DEG_LAT * from.lat.to_radians().cos();
    let distance_nm = (d_lat_nm * d_lat_nm + d_lon_nm * d_lon_nm).sqrt();
    let bearing = d_lon_nm.atan2(d_lat_nm).to_degrees().rem_euclid(360.0);
    (bearing, distance_nm * 1852.0)
}

fn status(t_ms: u64) -> wire::Status {
    wire::Status {
        Version: "synthetic".into(),
        Build: "synth".into(),
        Devices: 2,
        UAT_messages_last_minute: 420,
        ES_messages_last_minute: 3_100,
        UAT_traffic_targets_tracking: 3,
        ES_traffic_targets_tracking: 9,
        UATRadio_connected: true,
        GPS_satellites_locked: 11,
        GPS_satellites_seen: 17,
        GPS_satellites_tracked: 14,
        GPS_position_accuracy: 3.1,
        GPS_connected: true,
        GPS_solution: "3D GPS + SBAS".into(),
        Uptime: (t_ms / 1000) as i64,
        // A plausible Pi 3 temperature with two SDRs in an enclosure: warm, not throttling.
        CPUTemp: 62.0 + ((t_ms as f32 / 30_000.0).sin() * 3.0),
        CPUTempMax: 68.5,
        UAT_METAR_total: 24,
        UAT_NEXRAD_total: 96,
        ..Default::default()
    }
}

/// Conditions to synthesise, one per station.
///
/// A spread rather than four copies of the same fine day. The weather page highlights hazards and
/// derives a flight category, and neither can be exercised — or reviewed by eye — if every
/// synthetic report is `10SM FEW120`. These cover VFR through LIFR plus a convective and an
/// icing case, which is the range the display has to look right across.
const CONDITIONS: [(&str, &str); 4] = [
    // VFR: nothing to highlight, badge green.
    ("KBJC", "10SM FEW120 {temp:02}/07 A3002"),
    // MVFR on ceiling, with haze that must NOT be highlighted.
    ("KAPA", "5SM HZ BKN025 {temp:02}/09 A3001"),
    // IFR on both ceiling and visibility, with a thunderstorm.
    ("KDEN", "2SM TSRA BKN008 OVC015CB {temp:02}/12 A2992"),
    // LIFR, and freezing rain: the worst case the highlighter has to catch.
    ("KEIK", "1/2SM FZRA VV003 M02/M04 A2988"),
];

fn weather_batch(rng: &mut Rng) -> Vec<wire::WeatherMessage> {
    let index = (rng.unit() * CONDITIONS.len() as f64) as usize % CONDITIONS.len();
    let (station, conditions) = CONDITIONS[index];
    let wind_dir = (rng.range(0.0, 36.0) as u32) * 10;
    let wind_kt = rng.range(3.0, 18.0) as u32;
    let temp = rng.range(18.0, 31.0) as i32;
    let conditions = conditions.replace("{temp:02}", &format!("{temp:02}"));

    vec![
        wire::WeatherMessage {
            Type: "METAR".into(),
            Location: station.into(),
            Time: "291853Z".into(),
            Data: format!(
                "METAR {station} 291853Z {wind_dir:03}{wind_kt:02}KT {conditions} RMK AO2 SLP123"
            ),
            LocaltimeReceived: String::new(),
        },
        wire::WeatherMessage {
            Type: "TAF".into(),
            Location: station.into(),
            Time: "291720Z".into(),
            Data: format!(
                "TAF {station} 291720Z 2918/3024 {wind_dir:03}{wind_kt:02}KT P6SM SCT120"
            ),
            LocaltimeReceived: String::new(),
        },
        wire::WeatherMessage {
            Type: "PIREP".into(),
            Location: station.into(),
            Time: "291840Z".into(),
            Data: "UA /OV KDEN270015 /TM 1840 /FL085 /TP C172 /TB LGT OCNL MOD".into(),
            LocaltimeReceived: String::new(),
        },
    ]
}

/// A NEXRAD uplink frame carrying a small mosaic centred on own-ship.
///
/// Block geometry follows the FIS-B global block representation as implemented in upstream
/// `dump978/extract_nexrad.c`: below 60 degrees latitude a scale-1 block spans 48 arcminutes of
/// longitude by 4 arcminutes of latitude, subdivided into 32 x 4 bins.
fn nexrad_frame(centre: LatLon, rng: &mut Rng) -> wire::UATFrame {
    const BLOCK_WIDTH_DEG: f64 = 48.0 / 60.0;
    const BLOCK_HEIGHT_DEG: f64 = 4.0 / 60.0;
    const COLUMNS: i32 = 3;
    const ROWS: i32 = 12;

    // Drift the storm centre a little each cycle so the display has something changing.
    let storm = offset(centre, rng.range(0.0, 360.0), rng.range(2.0, 9.0));

    let mut blocks = Vec::new();
    for row in 0..ROWS {
        for col in 0..COLUMNS {
            let lat_north = centre.lat + (ROWS / 2 - row) as f64 * BLOCK_HEIGHT_DEG;
            let lon_west = centre.lon + (col - COLUMNS / 2) as f64 * BLOCK_WIDTH_DEG;

            let mut bins = Vec::with_capacity(128);
            for y in 0..4 {
                for x in 0..32 {
                    let bin_lat = lat_north - (y as f64 + 0.5) * (BLOCK_HEIGHT_DEG / 4.0);
                    let bin_lon = lon_west + (x as f64 + 0.5) * (BLOCK_WIDTH_DEG / 32.0);
                    let (_, distance_m) = bearing_distance(storm, LatLon::new(bin_lat, bin_lon));
                    let distance_nm = distance_m / 1852.0;
                    // A blob that fades from intensity 7 at the core to nothing by ~12 nm.
                    let intensity = (7.0 - distance_nm * 0.6).clamp(0.0, 7.0) as u16;
                    bins.push(intensity);
                }
            }

            blocks.push(wire::NEXRADBlock {
                Radar_Type: 63, // regional
                Scale: 1,
                LatNorth: lat_north,
                LonWest: lon_west,
                Height: BLOCK_HEIGHT_DEG,
                Width: BLOCK_WIDTH_DEG,
                Intensity: bins,
            });
        }
    }

    wire::UATFrame {
        Product_id: 63,
        Frame_type: 0,
        NEXRAD: blocks,
        ..Default::default()
    }
}
