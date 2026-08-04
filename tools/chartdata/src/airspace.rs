//! Turning FAA Class Airspace GeoJSON into airspace records.
//!
//! # Two things in this data are not what they look like
//!
//! **The dataset is not confined to the United States.** It carries Canadian and Mexican control
//! zones along the borders — reasonable, and worth keeping, because Vancouver's airspace matters
//! if you are near Bellingham — and it also carries Indonesian TMAs at Biak, Jayapura and Merauke.
//! Nothing in the attributes distinguishes them; `CLASS` is populated for all of it. So the filter
//! is a generous bounding box, kept wide enough that border airspace survives and Indonesia does
//! not.
//!
//! **Thirty polygons give their upper limit as a flight level, not as feet.** `UPPER_UOM` is `FL`
//! and `UPPER_CODE` is `STD`, so Tijuana's TCA reads `up=195`. Taken as feet that is a control
//! area topping out at 195 ft, which is both wrong and entirely plausible-looking on a display.
//! [`altitude_ft`] converts them.
//!
//! # Simplification happens here
//!
//! Every ring goes through [`crate::simplify`] before it is quantised, and the closing point is
//! dropped on the way out: a renderer closes the path itself, so storing the repeat would cost a
//! vertex per ring for nothing.

use anyhow::{Context, Result};
use serde_json::Value;

use crate::format::{Airspace, Class, FLAG_LOWER_SURFACE};
use crate::simplify;

/// The box airspace has to touch to be kept, as (south, north, west, east).
///
/// Deliberately loose. It is not trying to trace a border — it is trying to tell the difference
/// between "next to the United States" and "on the other side of the Pacific", and a tight box
/// would start cutting the Canadian control zones that are the reason for keeping any of it.
pub const KEEP_BOX: (f64, f64, f64, f64) = (22.0, 52.0, -128.0, -64.0);

pub struct Stats {
    pub read: usize,
    pub kept: usize,
    pub dropped_outside_box: usize,
    pub dropped_class: usize,
    pub dropped_empty: usize,
    pub vertices_before: usize,
    pub vertices_after: usize,
}

impl Stats {
    fn new() -> Self {
        Self {
            read: 0,
            kept: 0,
            dropped_outside_box: 0,
            dropped_class: 0,
            dropped_empty: 0,
            vertices_before: 0,
            vertices_after: 0,
        }
    }
}

/// Parse one or more GeoJSON pages into airspace records.
pub fn parse(pages: &[String]) -> Result<(Vec<Airspace>, Stats)> {
    let mut out = Vec::new();
    let mut stats = Stats::new();

    for (index, page) in pages.iter().enumerate() {
        let value: Value =
            serde_json::from_str(page).with_context(|| format!("parsing airspace page {index}"))?;
        let features = value
            .get("features")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);

        for feature in features {
            stats.read += 1;
            let properties = feature.get("properties").unwrap_or(&Value::Null);

            let Some(class) = properties
                .get("CLASS")
                .and_then(Value::as_str)
                .and_then(Class::parse)
            else {
                // Class E, or anything else the query let through.
                stats.dropped_class += 1;
                continue;
            };

            let raw = rings(feature.get("geometry").unwrap_or(&Value::Null));
            if raw.is_empty() {
                stats.dropped_empty += 1;
                continue;
            }
            if !touches_keep_box(&raw) {
                stats.dropped_outside_box += 1;
                continue;
            }

            let mut simplified = Vec::with_capacity(raw.len());
            for ring in &raw {
                stats.vertices_before += ring.len();
                let reduced = simplify::ring(ring, simplify::TOLERANCE_M);
                let quantised = quantise(&reduced);
                if quantised.len() < 3 {
                    continue;
                }
                stats.vertices_after += quantised.len();
                simplified.push(quantised);
            }
            if simplified.is_empty() {
                stats.dropped_empty += 1;
                continue;
            }

            let lower = altitude_ft(properties, "LOWER_VAL", "LOWER_UOM");
            let upper = altitude_ft(properties, "UPPER_VAL", "UPPER_UOM");
            let surface = properties
                .get("LOWER_CODE")
                .and_then(Value::as_str)
                .is_some_and(|code| code.trim() == "SFC");

            out.push(Airspace {
                class,
                label: label(properties),
                lower_ft: lower,
                upper_ft: upper,
                flags: if surface { FLAG_LOWER_SURFACE } else { 0 },
                rings: simplified,
            });
            stats.kept += 1;
        }
    }

    // Sorted so the build is reproducible regardless of the order pages were fetched in. B first
    // so that a renderer walking the file in order draws the largest airspace underneath.
    out.sort_by(|a, b| {
        (a.class as u8)
            .cmp(&(b.class as u8))
            .then_with(|| a.label.cmp(&b.label))
            .then_with(|| a.bounds().cmp(&b.bounds()))
    });

    Ok((out, stats))
}

/// Altitude in feet, converting flight levels.
///
/// `UPPER_UOM = "FL"` means the value is hundreds of feet. Reading it as feet would put the top of
/// Tijuana's control area at 195 ft.
pub fn altitude_ft(properties: &Value, value_key: &str, uom_key: &str) -> i32 {
    let value = properties
        .get(value_key)
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let uom = properties
        .get(uom_key)
        .and_then(Value::as_str)
        .unwrap_or("FT")
        .trim();
    let feet = match uom {
        "FL" => value * 100.0,
        // "FT" and anything unexpected. Treating an unknown unit as feet is the conservative
        // reading: it cannot inflate a limit, only leave it as stated.
        _ => value,
    };
    feet.round() as i32
}

/// The identifier to show. Two polygons in the set have none; they keep an empty label rather than
/// borrowing the airport's, which would claim a relationship the data does not state.
fn label(properties: &Value) -> String {
    properties
        .get("IDENT")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_ascii_uppercase())
        .filter(|s| !s.is_empty() && s.len() <= crate::format::LABEL_LEN)
        .unwrap_or_default()
}

/// Every ring of a Polygon or MultiPolygon, as longitude/latitude pairs.
///
/// The current data is all `Polygon`, but `MultiPolygon` is handled because a class airspace split
/// by an exclusion is naturally one, and finding out the hard way would mean silently losing a
/// piece of a boundary.
fn rings(geometry: &Value) -> Vec<Vec<simplify::Point>> {
    let coordinates = geometry.get("coordinates");
    match geometry.get("type").and_then(Value::as_str) {
        Some("Polygon") => coordinates
            .and_then(Value::as_array)
            .map(|rs| rs.iter().filter_map(ring_points).collect())
            .unwrap_or_default(),
        Some("MultiPolygon") => coordinates
            .and_then(Value::as_array)
            .map(|ps| {
                ps.iter()
                    .filter_map(Value::as_array)
                    .flat_map(|rs| rs.iter().filter_map(ring_points))
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn ring_points(ring: &Value) -> Option<Vec<simplify::Point>> {
    let points: Vec<simplify::Point> = ring
        .as_array()?
        .iter()
        .filter_map(|p| {
            let pair = p.as_array()?;
            Some((pair.first()?.as_f64()?, pair.get(1)?.as_f64()?))
        })
        .collect();
    (points.len() >= 3).then_some(points)
}

fn touches_keep_box(rings: &[Vec<simplify::Point>]) -> bool {
    rings.iter().flatten().any(|(lon, lat)| {
        (KEEP_BOX.0..=KEEP_BOX.1).contains(lat) && (KEEP_BOX.2..=KEEP_BOX.3).contains(lon)
    })
}

/// To micro-degrees, dropping the repeated closing point.
fn quantise(points: &[simplify::Point]) -> Vec<(i32, i32)> {
    let mut out: Vec<(i32, i32)> = points
        .iter()
        .map(|(lon, lat)| ((lat * 1e6).round() as i32, (lon * 1e6).round() as i32))
        .collect();
    // GeoJSON repeats the first point last. Quantisation can also make two neighbours identical,
    // so this dedupes rather than blindly popping one.
    out.dedup();
    while out.len() > 1 && out.first() == out.last() {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feature(class: &str, ident: &str, ring: &[(f64, f64)], extra: Value) -> Value {
        let mut properties = serde_json::json!({
            "CLASS": class,
            "IDENT": ident,
            "NAME": "TEST",
            "LOWER_VAL": 0.0, "LOWER_UOM": "FT", "LOWER_CODE": "SFC",
            "UPPER_VAL": 7000.0, "UPPER_UOM": "FT", "UPPER_CODE": "MSL",
        });
        if let (Some(map), Some(more)) = (properties.as_object_mut(), extra.as_object()) {
            for (k, v) in more {
                map.insert(k.clone(), v.clone());
            }
        }
        let coords: Vec<Value> = ring
            .iter()
            .map(|(lon, lat)| serde_json::json!([lon, lat]))
            .collect();
        serde_json::json!({
            "type": "Feature",
            "properties": properties,
            "geometry": {"type": "Polygon", "coordinates": [coords]}
        })
    }

    fn page(features: Vec<Value>) -> String {
        serde_json::json!({"type": "FeatureCollection", "features": features}).to_string()
    }

    /// A closed square, as GeoJSON supplies one.
    fn square(lon: f64, lat: f64, size: f64) -> Vec<(f64, f64)> {
        vec![
            (lon, lat),
            (lon + size, lat),
            (lon + size, lat + size),
            (lon, lat + size),
            (lon, lat),
        ]
    }

    #[test]
    fn a_flight_level_upper_limit_is_converted_to_feet() {
        // Tijuana's TCA gives up=195 FL. Read as feet that is a control area 195 ft tall, which is
        // wrong in a way that looks entirely ordinary on a display.
        let f = feature(
            "D",
            "MMTJ",
            &square(-117.0, 32.5, 0.2),
            serde_json::json!({"UPPER_VAL": 195.0, "UPPER_UOM": "FL", "UPPER_CODE": "STD"}),
        );
        let (out, _) = parse(&[page(vec![f])]).unwrap();
        assert_eq!(out[0].upper_ft, 19_500);
    }

    #[test]
    fn feet_are_left_alone_and_an_unknown_unit_is_treated_as_feet() {
        let props = serde_json::json!({"UPPER_VAL": 7000.0, "UPPER_UOM": "FT"});
        assert_eq!(altitude_ft(&props, "UPPER_VAL", "UPPER_UOM"), 7000);
        let odd = serde_json::json!({"UPPER_VAL": 7000.0, "UPPER_UOM": "M"});
        assert_eq!(
            altitude_ft(&odd, "UPPER_VAL", "UPPER_UOM"),
            7000,
            "an unknown unit must not inflate a limit"
        );
        let missing = serde_json::json!({});
        assert_eq!(altitude_ft(&missing, "UPPER_VAL", "UPPER_UOM"), 0);
    }

    #[test]
    fn indonesian_airspace_is_dropped_and_canadian_border_airspace_is_not() {
        // Both are in the FAA's file with a populated CLASS, and nothing in the attributes tells
        // them apart. Vancouver matters near Bellingham; Biak does not matter anywhere.
        let pages = [page(vec![
            feature("B", "WABB", &square(136.0, -1.2, 0.3), Value::Null),
            feature("C", "CYVR", &square(-123.2, 49.1, 0.3), Value::Null),
            feature("B", "KEWR", &square(-74.3, 40.6, 0.3), Value::Null),
        ])];
        let (out, stats) = parse(&pages).unwrap();
        let labels: Vec<&str> = out.iter().map(|a| a.label.as_str()).collect();
        assert!(
            !labels.contains(&"WABB"),
            "Biak should not be in a CONUS file"
        );
        assert!(labels.contains(&"CYVR"), "Vancouver is next door");
        assert!(labels.contains(&"KEWR"));
        assert_eq!(stats.dropped_outside_box, 1);
    }

    #[test]
    fn class_e_never_reaches_the_file() {
        // 4343 of the 6061 polygons, almost all E5 transition area from 700 ft AGL. A boundary
        // around everything is a boundary around nothing.
        let pages = [page(vec![
            feature("E", "KXXX", &square(-74.0, 40.0, 0.3), Value::Null),
            feature("D", "KMMU", &square(-74.4, 40.7, 0.1), Value::Null),
        ])];
        let (out, stats) = parse(&pages).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].label, "KMMU");
        assert_eq!(stats.dropped_class, 1);
    }

    #[test]
    fn the_closing_point_is_dropped_but_the_ring_still_encloses_an_area() {
        let f = feature("B", "KEWR", &square(-74.3, 40.6, 0.3), Value::Null);
        let (out, _) = parse(&[page(vec![f])]).unwrap();
        let ring = &out[0].rings[0];
        assert_eq!(ring.len(), 4, "a square is four points once unclosed");
        assert_ne!(ring.first(), ring.last(), "the repeat must be gone");
    }

    #[test]
    fn a_surface_lower_limit_is_flagged() {
        let sfc = feature("D", "KAAA", &square(-74.0, 40.0, 0.1), Value::Null);
        let msl = feature(
            "D",
            "KBBB",
            &square(-75.0, 41.0, 0.1),
            serde_json::json!({"LOWER_VAL": 1800.0, "LOWER_CODE": "MSL"}),
        );
        let (out, _) = parse(&[page(vec![sfc, msl])]).unwrap();
        let a = out.iter().find(|a| a.label == "KAAA").unwrap();
        let b = out.iter().find(|a| a.label == "KBBB").unwrap();
        assert_eq!(a.flags & FLAG_LOWER_SURFACE, FLAG_LOWER_SURFACE);
        assert_eq!(b.flags & FLAG_LOWER_SURFACE, 0);
        assert_eq!(b.lower_ft, 1800);
    }

    #[test]
    fn multipolygon_geometry_keeps_every_piece() {
        // Not in the current data — it is all Polygon — but an airspace split by an exclusion is
        // naturally a MultiPolygon, and losing half a boundary would not look like a fault.
        let coords = serde_json::json!([
            [[
                [-74.0, 40.0],
                [-73.9, 40.0],
                [-73.9, 40.1],
                [-74.0, 40.1],
                [-74.0, 40.0]
            ]],
            [[
                [-73.5, 40.0],
                [-73.4, 40.0],
                [-73.4, 40.1],
                [-73.5, 40.1],
                [-73.5, 40.0]
            ]]
        ]);
        let f = serde_json::json!({
            "type": "Feature",
            "properties": {"CLASS": "B", "IDENT": "KJFK", "LOWER_VAL": 0.0, "LOWER_CODE": "SFC",
                           "UPPER_VAL": 7000.0, "UPPER_UOM": "FT"},
            "geometry": {"type": "MultiPolygon", "coordinates": coords}
        });
        let (out, _) = parse(&[page(vec![f])]).unwrap();
        assert_eq!(out[0].rings.len(), 2);
    }

    #[test]
    fn a_dense_boundary_is_simplified_on_the_way_in() {
        // What the FAA actually ships: a Class D circle as thousands of points.
        let mut ring: Vec<(f64, f64)> = (0..3000)
            .map(|i| {
                let a = (i as f64 / 3000.0) * std::f64::consts::TAU;
                let cos_lat = 40.8f64.to_radians().cos();
                (
                    -74.4 + 4.4 * a.sin() / (60.0 * cos_lat),
                    40.8 + 4.4 * a.cos() / 60.0,
                )
            })
            .collect();
        ring.push(ring[0]);

        let f = feature("D", "KMMU", &ring, Value::Null);
        let (out, stats) = parse(&[page(vec![f])]).unwrap();
        assert_eq!(stats.vertices_before, 3001);
        assert!(
            stats.vertices_after < 300,
            "expected heavy reduction, kept {}",
            stats.vertices_after
        );
        assert!(out[0].rings[0].len() >= 3);
    }

    #[test]
    fn a_feature_with_no_usable_geometry_is_dropped_not_written_empty() {
        let f = serde_json::json!({
            "type": "Feature",
            "properties": {"CLASS": "B", "IDENT": "KAAA"},
            "geometry": Value::Null
        });
        let (out, stats) = parse(&[page(vec![f])]).unwrap();
        assert!(out.is_empty());
        assert_eq!(stats.dropped_empty, 1);
    }

    #[test]
    fn the_result_is_ordered_the_same_way_whatever_order_the_pages_arrive_in() {
        // Pages are fetched by class and offset, and the build must be byte-reproducible.
        let a = feature("D", "KMMU", &square(-74.4, 40.7, 0.1), Value::Null);
        let b = feature("B", "KEWR", &square(-74.3, 40.6, 0.3), Value::Null);
        let c = feature("C", "KTEB", &square(-74.1, 40.8, 0.2), Value::Null);

        let forward = parse(&[page(vec![a.clone(), b.clone(), c.clone()])])
            .unwrap()
            .0;
        let backward = parse(&[page(vec![c]), page(vec![b]), page(vec![a])])
            .unwrap()
            .0;
        let labels = |v: &[Airspace]| v.iter().map(|s| s.label.clone()).collect::<Vec<_>>();
        assert_eq!(labels(&forward), labels(&backward));
        assert_eq!(
            labels(&forward),
            vec!["KEWR", "KTEB", "KMMU"],
            "B, then C, then D"
        );
    }
}
