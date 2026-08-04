//! Turning a snapshot of free public data into Stratux wire structs.
//!
//! The snapshot is whatever `fetch-snapshot.sh` wrote: raw responses from adsb.lol (traffic) and
//! aviationweather.gov (text weather), in an envelope that records where and when they came from.
//! Nothing here touches the network — that is the point. The fetch happens once, with internet;
//! everything after it is offline, which is the same rule the rest of this project follows for
//! anything that can only be observed live.
//!
//! # The conversions that are easy to get wrong
//!
//! adsb.lol reports what a receiver decoded; Stratux reports its own normalised view. The gap
//! between them is small but full of sharp edges, and every one of these has a test:
//!
//! * **`alt_baro` is a number *or* the string `"ground"`.** A parser that assumes a number drops
//!   every aircraft on the airfield, which is exactly the traffic the threat tiers deliberately
//!   ignore — so the bug would look like the feature working.
//! * **`hex` may carry a `~` prefix**, meaning a non-ICAO address: TIS-B or ADS-R rebroadcast
//!   rather than a directly received aircraft. Parsing that as hex fails and the target vanishes.
//! * **`flight` is space-padded to eight characters.** Untrimmed it makes every tag look
//!   left-aligned in a field that is not there.
//! * **`category` is a GDL90 letter-digit pair, not a number.** `A3` is 3 but `B1` is 9, because
//!   the encoding packs A0-A7 into 0-7, B0-B7 into 8-15 and so on. Getting this wrong changes the
//!   symbol shape, which is a silent, plausible-looking error.
//! * **`squawk` is a string.** It reads as decimal digits that happen to be an octal code, which
//!   is also how Stratux stores it, so it is parsed as decimal and not converted.

// Field names below mirror the two services' JSON exactly, as `wire.rs` mirrors Stratux's. A
// rename here would need a serde attribute to undo it, which is a second place to get it wrong.
#![allow(non_snake_case)]

use anyhow::{Context, Result};
use serde::Deserialize;
use stratux_client::wire;

/// The envelope `fetch-snapshot.sh` writes.
#[derive(Debug, Deserialize)]
pub struct Snapshot {
    #[serde(default)]
    pub captured_utc: String,
    pub origin: Origin,
    /// Raw `ac` array from adsb.lol.
    #[serde(default)]
    pub traffic: Vec<Aircraft>,
    #[serde(default)]
    pub metar: Vec<AwcReport>,
    #[serde(default)]
    pub taf: Vec<AwcReport>,
    #[serde(default)]
    pub pirep: Vec<AwcReport>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Origin {
    pub lat: f64,
    pub lon: f64,
}

/// One aircraft as adsb.lol reports it. Every field optional: the feed omits what it has not
/// heard, and a target that has been heard on Mode S only carries little more than an address.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Aircraft {
    #[serde(default)]
    pub hex: String,
    #[serde(default)]
    pub flight: Option<String>,
    #[serde(default)]
    pub r: Option<String>,
    #[serde(default)]
    pub lat: Option<f64>,
    #[serde(default)]
    pub lon: Option<f64>,
    #[serde(default)]
    pub alt_baro: Option<serde_json::Value>,
    #[serde(default)]
    pub alt_geom: Option<f64>,
    #[serde(default)]
    pub gs: Option<f64>,
    #[serde(default)]
    pub track: Option<f64>,
    #[serde(default)]
    pub baro_rate: Option<f64>,
    #[serde(default)]
    pub geom_rate: Option<f64>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub squawk: Option<String>,
    #[serde(default)]
    pub rssi: Option<f64>,
    #[serde(default)]
    pub seen: Option<f64>,
    #[serde(default)]
    pub seen_pos: Option<f64>,
    #[serde(default)]
    pub mlat: Vec<serde_json::Value>,
    #[serde(default)]
    pub tisb: Vec<serde_json::Value>,
}

/// One report from aviationweather.gov. The raw text is the only field this display shows.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AwcReport {
    #[serde(default)]
    pub icaoId: Option<String>,
    #[serde(default)]
    pub rawOb: Option<String>,
    #[serde(default)]
    pub rawTAF: Option<String>,
}

/// Traffic source codes, confirmed against upstream Stratux.
const SOURCE_1090ES: u8 = 1;
/// Target types, mirroring Stratux's `TARGET_TYPE_*`.
const TARGET_ADSB: u8 = 1;
const TARGET_ADSR: u8 = 2;
const TARGET_TISB: u8 = 4;

impl Snapshot {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        serde_json::from_slice(bytes).context("snapshot is not the JSON this tool writes")
    }

    /// Every aircraft converted to the struct Stratux would have published.
    pub fn targets(&self) -> Vec<wire::TrafficInfo> {
        self.traffic.iter().map(to_traffic).collect()
    }

    /// Text products, in the order the display should first see them.
    pub fn weather(&self) -> Vec<wire::WeatherMessage> {
        let mut out = Vec::new();
        for r in &self.metar {
            if let Some(body) = r.rawOb.clone().filter(|s| !s.is_empty()) {
                out.push(weather_message("METAR", r.icaoId.clone(), body));
            }
        }
        for r in &self.taf {
            if let Some(body) = r.rawTAF.clone().filter(|s| !s.is_empty()) {
                out.push(weather_message("TAF", r.icaoId.clone(), body));
            }
        }
        for r in &self.pirep {
            if let Some(body) = r.rawOb.clone().filter(|s| !s.is_empty()) {
                out.push(weather_message("PIREP", r.icaoId.clone(), body));
            }
        }
        out
    }
}

fn weather_message(kind: &str, location: Option<String>, body: String) -> wire::WeatherMessage {
    wire::WeatherMessage {
        Type: kind.into(),
        Location: location.unwrap_or_default(),
        Time: String::new(),
        Data: body,
        LocaltimeReceived: String::new(),
    }
}

/// Parse an adsb.lol address. Returns the address and whether it is a real ICAO one.
///
/// A leading `~` means the address is not an ICAO aircraft address — the target reached the feed
/// as TIS-B or ADS-R rather than being heard directly. Stripping it keeps the target; treating the
/// whole string as hex loses it.
pub fn parse_hex_address(hex: &str) -> Option<(u32, bool)> {
    let icao = !hex.starts_with('~');
    let digits = hex.trim_start_matches('~').trim();
    u32::from_str_radix(digits, 16).ok().map(|a| (a, icao))
}

/// GDL90 emitter category from adsb.lol's letter-digit form.
///
/// `A0`-`A7` are 0-7, `B0`-`B7` are 8-15, `C0`-`C7` 16-23, `D0`-`D7` 24-31 — the packing the
/// wire struct documents with "A7 becomes 0x07, B0 becomes 0x08".
pub fn parse_category(category: &str) -> u8 {
    let mut chars = category.trim().chars();
    let (Some(letter), Some(digit)) = (chars.next(), chars.next()) else {
        return 0;
    };
    let base = match letter.to_ascii_uppercase() {
        'A' => 0,
        'B' => 8,
        'C' => 16,
        'D' => 24,
        _ => return 0,
    };
    let Some(n) = digit.to_digit(10).filter(|d| *d < 8) else {
        return 0;
    };
    base + n as u8
}

/// Barometric altitude, and whether the aircraft is on the ground.
///
/// `alt_baro` is a number in feet, or the literal string `"ground"`.
pub fn parse_altitude(value: Option<&serde_json::Value>) -> (Option<i32>, bool) {
    match value {
        Some(serde_json::Value::Number(n)) => (n.as_f64().map(|v| v as i32), false),
        Some(serde_json::Value::String(s)) if s.eq_ignore_ascii_case("ground") => (Some(0), true),
        _ => (None, false),
    }
}

fn to_traffic(a: &Aircraft) -> wire::TrafficInfo {
    let (icao, is_icao) = parse_hex_address(&a.hex).unwrap_or((0, true));
    let (alt_baro, on_ground) = parse_altitude(a.alt_baro.as_ref());
    // Prefer pressure altitude, because that is what the threat tiers compare against. Fall back
    // to the GNSS figure and say so, rather than silently presenting one as the other.
    let (alt, alt_is_gnss) = match (alt_baro, a.alt_geom) {
        (Some(ft), _) => (ft, false),
        (None, Some(ft)) => (ft as i32, true),
        (None, None) => (0, false),
    };

    // An MLAT position was computed from arrival times by the network, not transmitted by the
    // aircraft. Stratux has no separate code for it, and ADS-R — rebroadcast by a ground station
    // rather than heard directly — is the closest honest label. Calling it ADS-B would overstate
    // how the position was obtained.
    let target_type = if !a.tisb.is_empty() {
        TARGET_TISB
    } else if !a.mlat.is_empty() || !is_icao {
        TARGET_ADSR
    } else {
        TARGET_ADSB
    };

    wire::TrafficInfo {
        Icao_addr: icao,
        Reg: a.r.clone().unwrap_or_default(),
        // Space-padded to eight characters on the wire.
        Tail: a.flight.clone().unwrap_or_default().trim().to_string(),
        Emitter_category: a.category.as_deref().map(parse_category).unwrap_or(0),
        OnGround: on_ground,
        TargetType: target_type,
        Last_source: SOURCE_1090ES,
        SignalLevel: a.rssi.unwrap_or(-30.0),
        // Reads as decimal digits that happen to spell an octal code, which is how Stratux holds
        // it too, so this is a plain decimal parse and not a base conversion.
        Squawk: a
            .squawk
            .as_deref()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0),
        Position_valid: a.lat.is_some() && a.lon.is_some(),
        Lat: a.lat.unwrap_or(0.0) as f32,
        Lng: a.lon.unwrap_or(0.0) as f32,
        Alt: alt,
        AltIsGNSS: alt_is_gnss,
        Track: a.track.unwrap_or(0.0) as f32,
        Speed: a.gs.unwrap_or(0.0).max(0.0) as u16,
        Speed_valid: a.gs.is_some(),
        Vvel: a.baro_rate.or(a.geom_rate).unwrap_or(0.0) as i16,
        Age: a.seen.unwrap_or(0.0),
        AgeLastAlt: a.seen_pos.or(a.seen).unwrap_or(0.0),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_aircraft_on_the_ground_is_not_dropped() {
        // `alt_baro: "ground"` is a string where every other altitude is a number. A parser that
        // assumes a number loses every aircraft on the airfield — and since the threat tiers
        // deliberately ignore on-ground traffic, the bug would look like the feature working.
        let (alt, on_ground) = parse_altitude(Some(&serde_json::json!("ground")));
        assert_eq!(alt, Some(0));
        assert!(on_ground);

        let (alt, on_ground) = parse_altitude(Some(&serde_json::json!(12975)));
        assert_eq!(alt, Some(12975));
        assert!(!on_ground);

        assert_eq!(parse_altitude(None), (None, false));
        assert_eq!(
            parse_altitude(Some(&serde_json::json!(null))),
            (None, false)
        );
    }

    #[test]
    fn a_non_icao_address_keeps_its_target() {
        // The `~` prefix marks a TIS-B or ADS-R rebroadcast. Feeding the whole string to a hex
        // parser fails, and the target simply never appears.
        assert_eq!(parse_hex_address("a1f0b4"), Some((0xa1f0b4, true)));
        assert_eq!(parse_hex_address("~ad8d2e"), Some((0xad8d2e, false)));
        assert_eq!(parse_hex_address("not hex"), None);
    }

    #[test]
    fn emitter_categories_use_the_gdl90_packing() {
        // The encoding the wire struct documents: A7 is 0x07 and B0 is 0x08, so the letter is a
        // block of eight and not a separate axis. Getting this wrong changes the symbol shape,
        // which is silent and looks entirely plausible.
        assert_eq!(parse_category("A0"), 0);
        assert_eq!(parse_category("A3"), 3);
        assert_eq!(parse_category("A7"), 7);
        assert_eq!(parse_category("B0"), 8);
        assert_eq!(parse_category("B1"), 9);
        assert_eq!(parse_category("C0"), 16);
        assert_eq!(parse_category("D7"), 31);
        // Anything unrecognised falls back to "no information", never to a wrong shape.
        assert_eq!(parse_category(""), 0);
        assert_eq!(parse_category("Z9"), 0);
        assert_eq!(parse_category("A9"), 0);
    }

    #[test]
    fn a_callsign_is_trimmed_of_its_padding() {
        let a = Aircraft {
            hex: "a1f0b4".into(),
            flight: Some("RPA5623 ".into()),
            ..Default::default()
        };
        assert_eq!(to_traffic(&a).Tail, "RPA5623");
    }

    #[test]
    fn a_target_without_a_position_is_marked_as_such() {
        // Mode-S-only targets are counted in the status bar rather than plotted, so this flag is
        // what keeps them off the rings instead of putting them at the equator.
        let a = Aircraft {
            hex: "a1f0b4".into(),
            alt_baro: Some(serde_json::json!(3000)),
            ..Default::default()
        };
        let t = to_traffic(&a);
        assert!(!t.Position_valid);
        assert_eq!(t.Lat, 0.0);

        let positioned = Aircraft {
            lat: Some(40.7),
            lon: Some(-74.3),
            ..a
        };
        assert!(to_traffic(&positioned).Position_valid);
    }

    #[test]
    fn gnss_altitude_is_used_only_as_a_fallback_and_is_flagged() {
        // Threat tiers compare against own-ship *pressure* altitude, so a GNSS figure presented as
        // a baro one is a datum error of several hundred feet — a whole tier.
        let baro = Aircraft {
            alt_baro: Some(serde_json::json!(5000)),
            alt_geom: Some(5300.0),
            ..Default::default()
        };
        let t = to_traffic(&baro);
        assert_eq!((t.Alt, t.AltIsGNSS), (5000, false));

        let geom_only = Aircraft {
            alt_geom: Some(5300.0),
            ..Default::default()
        };
        let t = to_traffic(&geom_only);
        assert_eq!((t.Alt, t.AltIsGNSS), (5300, true));
    }

    #[test]
    fn a_real_response_from_the_feed_converts() {
        // Shape taken from a live adsb.lol response, hand-written rather than captured: the feed's
        // data is ODbL and this repo is MIT/Apache, so no real extract is committed here.
        let snap = Snapshot::parse(
            br#"{
              "captured_utc": "2026-08-02T17:10:00Z",
              "origin": {"lat": 40.7784, "lon": -74.3343},
              "traffic": [
                {"hex":"a1f0b4","flight":"RPA5623 ","r":"N224JQ","lat":40.71,"lon":-75.39,
                 "alt_baro":12975,"gs":366.7,"track":104.5,"baro_rate":-128,"category":"A3",
                 "squawk":"1471","rssi":-6.6,"seen":0.0},
                {"hex":"~ad8d2e","alt_baro":"ground","category":"A1","tisb":["lat"]}
              ],
              "metar": [{"icaoId":"KMMU","rawOb":"METAR KMMU 021656Z 15014G21KT 10SM VCSH BKN031 27/22 A2993"}],
              "taf": [{"icaoId":"KEWR","rawTAF":"TAF KEWR 021543Z 0216/0318 15010G18KT P6SM FEW070"}]
            }"#,
        )
        .expect("parses");

        let targets = snap.targets();
        assert_eq!(targets.len(), 2);

        let airliner = &targets[0];
        assert_eq!(airliner.Icao_addr, 0xa1f0b4);
        assert_eq!(airliner.Tail, "RPA5623");
        assert_eq!(airliner.Alt, 12975);
        assert_eq!(airliner.Speed, 366);
        assert_eq!(airliner.Squawk, 1471);
        assert_eq!(airliner.Emitter_category, 3);
        assert!(airliner.Position_valid);
        assert!(!airliner.OnGround);

        let rebroadcast = &targets[1];
        assert!(rebroadcast.OnGround);
        assert_eq!(rebroadcast.TargetType, TARGET_TISB);
        assert!(!rebroadcast.Position_valid);

        let weather = snap.weather();
        assert_eq!(weather.len(), 2);
        assert_eq!(weather[0].Type, "METAR");
        assert!(weather[0].Data.starts_with("METAR KMMU"));
        assert_eq!(weather[1].Type, "TAF");
    }

    #[test]
    fn a_snapshot_with_nothing_in_it_is_not_an_error() {
        // An empty sky at 3am is a legitimate snapshot, and the display's own "quiet sky" handling
        // is one of the things worth testing against it.
        let snap = Snapshot::parse(br#"{"origin":{"lat":0.0,"lon":0.0}}"#).expect("parses");
        assert!(snap.targets().is_empty());
        assert!(snap.weather().is_empty());
    }
}
