//! Decoder tests against realistic Stratux payloads.
//!
//! The JSON literals here are shaped like real Stratux output: Go field names, no JSON tags,
//! zero-initialised fields present rather than omitted. Several tests exist specifically to pin
//! down behaviour that would otherwise regress silently and be visible only as a wrong picture
//! in the cockpit.

use std::time::Instant;

use stratux_client::decode::{classify, decode, Event, JsonIoShape};
use stratux_client::domain::{GpsFix, LatLon, NexradBlock, NexradKind, TrafficSource};
use stratux_client::Stream;

fn now() -> Instant {
    Instant::now()
}

fn object(json: &str) -> serde_json::Map<String, serde_json::Value> {
    serde_json::from_str(json).expect("test fixture must be a JSON object")
}

/// Compare positions with a tolerance: Stratux sends coordinates as `float32`.
fn near(a: LatLon, b: LatLon, epsilon: f64) -> bool {
    (a.lat - b.lat).abs() < epsilon && (a.lon - b.lon).abs() < epsilon
}

// --- /jsonio structural dispatch -----------------------------------------------------------

#[test]
fn classifies_jsonio_shapes_by_key_presence() {
    // /jsonio has no envelope and no type discriminator, so this dispatch is the only thing
    // standing between us and feeding a settings blob to the NEXRAD decoder.
    assert_eq!(
        classify(&object(r#"{"Product_id":63,"NEXRAD":[]}"#)),
        JsonIoShape::UatFrame
    );
    assert_eq!(
        classify(&object(r#"{"Icao_addr":10682157,"Lat":39.9}"#)),
        JsonIoShape::Traffic
    );
    assert_eq!(
        classify(&object(r#"{"GPSFixQuality":2,"GPSLatitude":39.9}"#)),
        JsonIoShape::Situation
    );
    assert_eq!(
        classify(&object(r#"{"UAT_Enabled":true,"ES_Enabled":true}"#)),
        JsonIoShape::Settings
    );
    assert_eq!(
        classify(&object(r#"{"SomethingNew":1}"#)),
        JsonIoShape::Unrecognised
    );
}

#[test]
fn jsonio_discards_everything_that_is_not_a_uat_frame() {
    // Traffic and situation also arrive on /jsonio. Accepting them here would double-count
    // targets against the dedicated /traffic socket.
    for payload in [
        r#"{"Icao_addr":10682157,"Position_valid":true,"Lat":39.9,"Lng":-105.1}"#,
        r#"{"GPSFixQuality":2,"GPSLatitude":39.9,"GPSLongitude":-105.1}"#,
        r#"{"UAT_Enabled":true}"#,
        r#"{"CompletelyUnknown":42}"#,
    ] {
        let decoded = decode(Stream::JsonIo, payload.as_bytes(), now()).expect("should not error");
        assert!(
            decoded.is_none(),
            "expected {payload} to be discarded on /jsonio"
        );
    }
}

#[test]
fn jsonio_text_only_uat_frame_yields_nothing() {
    // Most uplink frames are text products with an empty NEXRAD array; they must not surface as
    // an empty Nexrad event that would churn the mosaic.
    let payload = r#"{"Product_id":413,"Frame_type":0,"Text_data":["METAR KDEN ..."],"NEXRAD":[]}"#;
    assert!(decode(Stream::JsonIo, payload.as_bytes(), now())
        .unwrap()
        .is_none());
}

// --- lenient deserialisation ---------------------------------------------------------------

#[test]
fn missing_fields_fall_back_to_defaults_rather_than_failing() {
    // Stratux structs carry no JSON tags, so an upstream rename silently removes a key. That
    // must degrade one value, not kill the stream.
    let payload = r#"{"Icao_addr":11259375}"#;
    let Some(Event::Traffic(target)) = decode(Stream::Traffic, payload.as_bytes(), now()).unwrap()
    else {
        panic!("expected a traffic event");
    };
    assert_eq!(target.icao, 11259375);
    assert!(target.position.is_none());
    assert!(target.identity.is_none());
}

#[test]
fn unknown_fields_are_ignored() {
    // New upstream fields must not break decoding.
    let payload = r#"{"Icao_addr":1,"SomeFieldAddedUpstreamLater":{"nested":true}}"#;
    assert!(decode(Stream::Traffic, payload.as_bytes(), now())
        .unwrap()
        .is_some());
}

#[test]
fn malformed_json_is_an_error_not_a_panic() {
    assert!(decode(Stream::Traffic, b"{not json", now()).is_err());
}

// --- traffic ------------------------------------------------------------------------------

#[test]
fn decodes_a_full_traffic_report() {
    let payload = r#"{
        "Icao_addr": 10682157, "Reg": "N823KL", "Tail": "DAL1422",
        "Emitter_category": 3, "OnGround": false, "TargetType": 1, "Last_source": 2,
        "SignalLevel": -22.5, "Squawk": 1200, "Position_valid": true,
        "Lat": 39.8617, "Lng": -104.6731, "Alt": 12500, "AltIsGNSS": false,
        "NIC": 8, "NACp": 9, "Track": 271.5, "Speed": 284, "Speed_valid": true,
        "Vvel": -640, "Age": 0.8, "AgeLastAlt": 0.8, "ExtrapolatedPosition": false,
        "BearingDist_valid": true, "Bearing": 96.2, "Distance": 38500.0
    }"#;

    let Some(Event::Traffic(t)) = decode(Stream::Traffic, payload.as_bytes(), now()).unwrap()
    else {
        panic!("expected a traffic event");
    };

    assert_eq!(t.icao, 10682157);
    // Transmitted callsign wins over the derived registration.
    assert_eq!(t.identity.as_deref(), Some("DAL1422"));
    // Positions arrive as Go float32, so widening to f64 leaves ~1e-6 degrees (~0.2 m) of
    // rounding. Compare with a tolerance rather than for equality — plenty for a plan view, but
    // it means positions must never be used as exact map keys.
    let position = t.position.expect("position should decode");
    assert!(near(position, LatLon::new(39.8617, -104.6731), 1e-5));
    assert_eq!(t.altitude_ft, Some(12500));
    assert_eq!(t.ground_speed_kt, Some(284));
    assert_eq!(t.vertical_speed_fpm, Some(-640));
    // Last_source 2 is the 978 MHz UAT receiver.
    assert_eq!(t.source, TrafficSource::Uat978);
    assert_eq!(t.reported_distance_m, Some(38500.0));
}

#[test]
fn traffic_falls_back_to_registration_when_no_callsign_is_transmitted() {
    let payload = r#"{"Icao_addr":1,"Reg":"N91TC","Tail":"   "}"#;
    let Some(Event::Traffic(t)) = decode(Stream::Traffic, payload.as_bytes(), now()).unwrap()
    else {
        panic!();
    };
    assert_eq!(t.identity.as_deref(), Some("N91TC"));
}

#[test]
fn null_island_positions_are_rejected() {
    // Stratux zero-initialises its structs, so a target with no position yet reports exactly
    // 0,0. Trusting that would draw a phantom target in the Gulf of Guinea.
    let payload = r#"{"Icao_addr":1,"Position_valid":true,"Lat":0.0,"Lng":0.0}"#;
    let Some(Event::Traffic(t)) = decode(Stream::Traffic, payload.as_bytes(), now()).unwrap()
    else {
        panic!();
    };
    assert!(
        t.position.is_none(),
        "0,0 must not be treated as a position"
    );
}

#[test]
fn position_is_ignored_unless_position_valid_is_set() {
    let payload = r#"{"Icao_addr":1,"Position_valid":false,"Lat":39.9,"Lng":-105.1}"#;
    let Some(Event::Traffic(t)) = decode(Stream::Traffic, payload.as_bytes(), now()).unwrap()
    else {
        panic!();
    };
    assert!(t.position.is_none());
    assert!(!t.is_positional());
}

#[test]
fn track_and_speed_are_withheld_without_a_velocity_solution() {
    // Stratux has no Track_valid flag; Speed_valid stands in for both. A stale track drawn as
    // a heading barb is worse than no barb.
    let payload = r#"{"Icao_addr":1,"Track":271.0,"Speed":284,"Speed_valid":false}"#;
    let Some(Event::Traffic(t)) = decode(Stream::Traffic, payload.as_bytes(), now()).unwrap()
    else {
        panic!();
    };
    assert!(t.track_deg.is_none());
    assert!(t.ground_speed_kt.is_none());
}

// --- own-ship -----------------------------------------------------------------------------

#[test]
fn decodes_own_ship_situation() {
    let payload = r#"{
        "GPSLatitude": 39.9088, "GPSLongitude": -105.1172, "GPSFixQuality": 2,
        "GPSSatellites": 11, "GPSSatellitesSeen": 17, "GPSHorizontalAccuracy": 3.1,
        "GPSAltitudeMSL": 8500.0, "GPSTrueCourse": 43.0, "GPSGroundSpeed": 118.0,
        "GPSVerticalSpeed": 120.0, "BaroSourceType": 0
    }"#;
    let Some(Event::OwnShip(o)) = decode(Stream::Situation, payload.as_bytes(), now()).unwrap()
    else {
        panic!("expected an own-ship event");
    };

    assert_eq!(o.fix, GpsFix::Differential);
    assert!(o.fix.is_usable());
    assert_eq!(o.altitude_msl_ft, Some(8500.0));
    assert_eq!(o.track_deg, Some(43.0));
    // No pressure sensor on this build, so relative altitude falls back to GPS MSL.
    assert!(o.pressure_altitude_ft.is_none());
    assert_eq!(o.comparison_altitude_ft(), Some(8500.0));
    assert!(o.usable_position().is_some());
}

#[test]
fn no_fix_suppresses_position_and_derived_values() {
    let payload = r#"{"GPSFixQuality":0,"GPSLatitude":39.9,"GPSLongitude":-105.1,
                      "GPSAltitudeMSL":8500.0,"GPSGroundSpeed":118.0}"#;
    let Some(Event::OwnShip(o)) = decode(Stream::Situation, payload.as_bytes(), now()).unwrap()
    else {
        panic!();
    };
    assert_eq!(o.fix, GpsFix::None);
    assert!(o.position.is_none());
    assert!(o.usable_position().is_none());
    assert!(o.altitude_msl_ft.is_none());
    assert!(o.ground_speed_kt.is_none());
}

#[test]
fn track_is_withheld_when_essentially_stationary() {
    // GPS course is noise at a standstill; a track-up display fed from it would spin on the ramp.
    let payload = r#"{"GPSFixQuality":2,"GPSLatitude":39.9,"GPSLongitude":-105.1,
                      "GPSTrueCourse":187.0,"GPSGroundSpeed":0.4}"#;
    let Some(Event::OwnShip(o)) = decode(Stream::Situation, payload.as_bytes(), now()).unwrap()
    else {
        panic!();
    };
    assert!(o.track_deg.is_none());
    // The position itself is still good.
    assert!(o.usable_position().is_some());
}

#[test]
fn pressure_altitude_is_preferred_for_comparison_when_a_sensor_exists() {
    // Traffic reports pressure altitude, so comparing against own pressure altitude is
    // like-for-like; GPS MSL would disagree by the local altimeter error.
    let payload = r#"{"GPSFixQuality":2,"GPSLatitude":39.9,"GPSLongitude":-105.1,
                      "GPSAltitudeMSL":8500.0,"BaroSourceType":1,"BaroPressureAltitude":8320.0}"#;
    let Some(Event::OwnShip(o)) = decode(Stream::Situation, payload.as_bytes(), now()).unwrap()
    else {
        panic!();
    };
    assert_eq!(o.comparison_altitude_ft(), Some(8320.0));
}

// --- weather ------------------------------------------------------------------------------

#[test]
fn decodes_a_text_weather_product() {
    let payload = r#"{"Type":"METAR","Location":"KDEN","Time":"291853Z",
                      "Data":"METAR KDEN 291853Z 04012KT 10SM FEW120 28/07 A3002"}"#;
    let Some(Event::Weather(w)) = decode(Stream::Weather, payload.as_bytes(), now()).unwrap()
    else {
        panic!("expected a weather event");
    };
    assert_eq!(w.product.label(), "METAR");
    assert_eq!(w.location, "KDEN");
    assert!(w.body.contains("FEW120"));
}

// --- NEXRAD -------------------------------------------------------------------------------

/// A block whose 128 bins ramp 0..7 across each row.
fn nexrad_payload(radar_type: u32, bins: &[u16]) -> String {
    let intensity = bins
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join(",");
    format!(
        r#"{{"Product_id":{radar_type},"NEXRAD":[{{"Radar_Type":{radar_type},"Scale":1,
           "LatNorth":40.0,"LonWest":-105.6,"Height":0.0666666,"Width":0.8,
           "Intensity":[{intensity}]}}]}}"#
    )
}

#[test]
fn decodes_nexrad_blocks_from_jsonio() {
    let bins: Vec<u16> = (0..128).map(|i| (i % 8) as u16).collect();
    let payload = nexrad_payload(63, &bins);
    let Some(Event::Nexrad(blocks)) = decode(Stream::JsonIo, payload.as_bytes(), now()).unwrap()
    else {
        panic!("expected a NEXRAD event");
    };
    assert_eq!(blocks.len(), 1);
    let block = &blocks[0];
    assert_eq!(block.kind, NexradKind::Regional);
    assert_eq!(block.bins.len(), NexradBlock::BIN_COUNT);
    assert!(block.has_precipitation());
}

#[test]
fn nexrad_bin_geometry_walks_west_to_east_then_north_to_south() {
    // The FIS-B global block representation fills 32 (longitude) x 4 (latitude) bins W->E then
    // N->S. Getting this transposed would render the mosaic rotated and geographically wrong.
    let bins: Vec<u16> = (0..128).map(|i| (i % 8) as u16).collect();
    let payload = nexrad_payload(63, &bins);
    let Some(Event::Nexrad(blocks)) = decode(Stream::JsonIo, payload.as_bytes(), now()).unwrap()
    else {
        panic!();
    };
    let block = &blocks[0];

    let (nw, se) = block.bin_bounds(0, 0).unwrap();
    // First bin is the block's own north-west corner.
    assert!((nw.lat - 40.0).abs() < 1e-9);
    assert!((nw.lon - -105.6).abs() < 1e-9);
    // and it spans width/32 by height/4.
    assert!((nw.lon + block.width_deg / 32.0 - se.lon).abs() < 1e-9);
    assert!((nw.lat - block.height_deg / 4.0 - se.lat).abs() < 1e-9);

    // Increasing x moves east, increasing y moves south.
    let (east, _) = block.bin_bounds(31, 0).unwrap();
    assert!(east.lon > nw.lon);
    let (south, _) = block.bin_bounds(0, 3).unwrap();
    assert!(south.lat < nw.lat);

    // Index order: bin (x, y) is at y * 32 + x.
    assert_eq!(block.intensity(5, 2), Some(bins[2 * 32 + 5] as u8));
    // Out of range is None, not a panic or a wrapped index.
    assert_eq!(block.intensity(32, 0), None);
    assert_eq!(block.intensity(0, 4), None);
    assert!(block.bin_bounds(32, 0).is_none());
}

#[test]
fn conus_and_regional_disagree_on_what_empty_means() {
    // Regional: 0 means "valid data, below 5 dBZ". CONUS: 1 means "valid, no precipitation".
    // Swapping these paints either phantom weather or holes in real coverage.
    assert_eq!(NexradKind::Regional.empty_intensity(), 0);
    assert_eq!(NexradKind::Conus.empty_intensity(), 1);

    let all_ones = vec![1u16; 128];
    let Some(Event::Nexrad(regional)) = decode(
        Stream::JsonIo,
        nexrad_payload(63, &all_ones).as_bytes(),
        now(),
    )
    .unwrap() else {
        panic!();
    };
    let Some(Event::Nexrad(conus)) = decode(
        Stream::JsonIo,
        nexrad_payload(64, &all_ones).as_bytes(),
        now(),
    )
    .unwrap() else {
        panic!();
    };

    // Identical bins, opposite meanings.
    assert!(
        regional[0].has_precipitation(),
        "intensity 1 is real returns on the regional product"
    );
    assert!(
        !conus[0].has_precipitation(),
        "intensity 1 means no precipitation on CONUS"
    );
}

#[test]
fn oversized_intensity_values_are_clamped_to_the_defined_range() {
    // Upstream stores 4-bit values in a u16 "as a hack for the JSON encoding"; only 0..=7 is
    // defined, and an out-of-range value must not index off the end of a colour LUT later.
    let bins: Vec<u16> = vec![9999; 128];
    let payload = nexrad_payload(63, &bins);
    let Some(Event::Nexrad(blocks)) = decode(Stream::JsonIo, payload.as_bytes(), now()).unwrap()
    else {
        panic!();
    };
    assert!(blocks[0].bins.iter().all(|&v| v <= 7));
}

#[test]
fn short_and_long_bin_arrays_are_normalised() {
    for count in [0usize, 40, 128, 300] {
        let bins: Vec<u16> = vec![4; count];
        let payload = nexrad_payload(63, &bins);
        let decoded = decode(Stream::JsonIo, payload.as_bytes(), now()).unwrap();
        if let Some(Event::Nexrad(blocks)) = decoded {
            assert_eq!(
                blocks[0].bins.len(),
                NexradBlock::BIN_COUNT,
                "bin count {count} should normalise to 128"
            );
        } else {
            // An all-empty block is legitimately dropped as having no precipitation only if
            // has_precipitation() is false; a 0-length array pads to the empty intensity.
            assert_eq!(count, 0);
        }
    }
}

#[test]
fn unknown_product_ids_are_not_treated_as_nexrad() {
    // Only 63 and 64 are NEXRAD. Product 413 is text.
    let bins: Vec<u16> = vec![5; 128];
    let payload = nexrad_payload(413, &bins);
    assert!(decode(Stream::JsonIo, payload.as_bytes(), now())
        .unwrap()
        .is_none());
}

#[test]
fn retransmitted_blocks_share_a_key_and_moved_blocks_do_not() {
    let bins: Vec<u16> = vec![3; 128];
    let payload = nexrad_payload(63, &bins);
    let Some(Event::Nexrad(first)) = decode(Stream::JsonIo, payload.as_bytes(), now()).unwrap()
    else {
        panic!();
    };
    let Some(Event::Nexrad(second)) = decode(Stream::JsonIo, payload.as_bytes(), now()).unwrap()
    else {
        panic!();
    };
    // Same coordinates -> same key, so a retransmission replaces rather than accumulates.
    assert_eq!(first[0].key(), second[0].key());

    let moved = payload.replace("\"LatNorth\":40.0", "\"LatNorth\":41.0");
    let Some(Event::Nexrad(other)) = decode(Stream::JsonIo, moved.as_bytes(), now()).unwrap()
    else {
        panic!();
    };
    assert_ne!(first[0].key(), other[0].key());
}

// --- status -------------------------------------------------------------------------------

#[test]
fn decodes_status() {
    let payload = r#"{"Version":"1.6r1","Devices":2,"UAT_messages_last_minute":420,
                      "ES_messages_last_minute":3100,"GPS_connected":true,
                      "GPS_solution":"3D GPS + SBAS","GPS_satellites_locked":11,
                      "CPUTemp":62.4,"CPUTempMax":68.5,"UAT_NEXRAD_total":96,
                      "Errors":["something happened"]}"#;
    let Some(Event::Status(s)) = decode(Stream::Status, payload.as_bytes(), now()).unwrap() else {
        panic!("expected a status event");
    };
    assert_eq!(s.version, "1.6r1");
    assert_eq!(s.es_messages_last_minute, 3100);
    assert_eq!(s.uat_messages_last_minute, 420);
    assert!((s.cpu_temp_c - 62.4).abs() < 1e-4);
    assert_eq!(s.errors.len(), 1);
}

// --- AHRS ------------------------------------------------------------------------------------

/// Build a `/situation` payload with the given AHRS fields.
fn situation_json(status: u8, pitch: f64, roll: f64) -> String {
    format!(
        r#"{{"GPSFixQuality":2,"GPSLatitude":40.0,"GPSLongitude":-105.0,
            "AHRSStatus":{status},"AHRSPitch":{pitch},"AHRSRoll":{roll},
            "AHRSSlipSkid":0.0,"AHRSGLoad":1.0,
            "AHRSGyroHeading":3276.7,"AHRSMagHeading":3276.7,"AHRSTurnRate":3276.7}}"#
    )
}

fn decode_ownship(json: &str) -> stratux_client::domain::OwnShip {
    let now = Instant::now();
    match decode(Stream::Situation, json.as_bytes(), now)
        .unwrap()
        .unwrap()
    {
        Event::OwnShip(o) => o,
        other => panic!("expected OwnShip, got {other:?}"),
    }
}

#[test]
fn the_ahrs_invalid_sentinel_becomes_none_not_a_3276_degree_heading() {
    let ownship = decode_ownship(&situation_json(6, 2.5, -4.0));
    assert_eq!(ownship.ahrs.pitch_deg, Some(2.5));
    assert_eq!(ownship.ahrs.roll_deg, Some(-4.0));
    // These three read 3276.7 on the real target.
    assert_eq!(ownship.ahrs.gyro_heading_deg, None);
    assert_eq!(ownship.ahrs.mag_heading_deg, None);
    assert_eq!(ownship.ahrs.turn_rate_deg_s, None);
}

#[test]
fn a_status_of_zero_blanks_every_field_not_just_the_attitude() {
    // A Stratux with no AHRS module leaves these at Go's zero value, so 0.0 pitch and 0.0 roll
    // look exactly like a genuine wings-level reading. Gating only `attitude()` once produced a
    // screen reading "AHRS UNAVAILABLE" over the horizon while the numbers below it reported
    // PITCH +0.0 / ROLL +0.0 / HDG 000 — the display contradicting itself.
    let ownship = decode_ownship(&situation_json(0, 0.0, 0.0));
    let a = &ownship.ahrs;
    assert_eq!(a.attitude(), None);
    assert_eq!(a.pitch_deg, None);
    assert_eq!(a.roll_deg, None);
    assert_eq!(
        a.slip_skid_deg, None,
        "the slip ball must not be drawn either"
    );
    assert_eq!(a.g_load, None);
    assert_eq!(a.status, 0);
}

#[test]
fn a_genuine_level_attitude_still_reads_as_a_measurement() {
    // The mirror of the test above: with a module reporting, 0.0 means level, not missing.
    let ownship = decode_ownship(&situation_json(6, 0.0, 0.0));
    assert_eq!(ownship.ahrs.attitude(), Some((0.0, 0.0)));
}

#[test]
fn pressure_altitude_is_used_when_a_baro_sensor_is_present() {
    // The target reports BaroSourceType 1, so this is the live path, not a hypothetical one.
    let with_baro = decode_ownship(
        r#"{"GPSFixQuality":2,"GPSLatitude":40.0,"GPSLongitude":-105.0,
            "GPSAltitudeMSL":5000.0,"BaroSourceType":1,"BaroPressureAltitude":4800.0}"#,
    );
    assert_eq!(with_baro.pressure_altitude_ft, Some(4800.0));
    assert_eq!(with_baro.comparison_altitude_ft(), Some(4800.0));

    let without = decode_ownship(
        r#"{"GPSFixQuality":2,"GPSLatitude":40.0,"GPSLongitude":-105.0,
            "GPSAltitudeMSL":5000.0,"BaroSourceType":0,"BaroPressureAltitude":4800.0}"#,
    );
    assert_eq!(
        without.pressure_altitude_ft, None,
        "no sensor means no reading"
    );
    assert_eq!(
        without.comparison_altitude_ft(),
        Some(5000.0),
        "falls back to GPS MSL"
    );
}
