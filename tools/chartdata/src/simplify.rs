//! Douglas–Peucker line simplification, in metres on a local tangent plane.
//!
//! This is the step that makes the layer possible at all. The FAA ships a Class D — geometrically a
//! circle of about 4.4 nm radius — as a median of **3,256 vertices**, and the three classes
//! together are 2.34 million. Drawing that on a `vc4` at 30 Hz is not a tuning problem.
//!
//! At [`TOLERANCE_M`] the whole set comes down to about 4.5% of its vertices, and the tolerance is
//! chosen so the loss cannot be seen: `Layout::for_size` gives an outer ring radius of 187.5 px on
//! the 800x480 panel, so at the tightest selectable range — 2 nm — one pixel is **19.8 m**. Ten
//! metres is under half a pixel there, and under a fortieth of one at 40 nm.
//!
//! # Closed rings
//!
//! GeoJSON rings repeat their first point as their last, which makes the initial Douglas–Peucker
//! baseline a zero-length segment. That is handled rather than special-cased: with a degenerate
//! baseline the perpendicular distance falls back to the distance from the start point, so the
//! first split is at the point furthest from the start and both halves then have real baselines.
//! That is the textbook way to simplify a closed ring, and it drops out of the arithmetic.

/// How far a simplified edge may stray from the original, in metres.
///
/// See the module note: half a pixel at the tightest range the display offers.
pub const TOLERANCE_M: f64 = 10.0;

/// A ring must keep at least this many points to still enclose an area.
const MIN_RING: usize = 3;

/// Metres per degree of latitude. The WGS84 meridian varies from about 110,574 m at the equator to
/// 111,694 m at the poles; the mean is close enough for a tolerance that is itself approximate.
const M_PER_DEG_LAT: f64 = 111_132.0;

/// Metres per degree of longitude at the equator.
const M_PER_DEG_LON: f64 = 111_320.0;

/// A position as it comes out of GeoJSON: longitude first.
pub type Point = (f64, f64);

/// Simplify a ring, keeping its first and last points.
///
/// Returns the original when it is already too short to simplify, or when simplification would
/// leave too few points to enclose an area — a polygon that vanished would be worse than one that
/// stayed slightly over-detailed, and at 10 m on real airspace it does not happen.
pub fn ring(points: &[Point], tolerance_m: f64) -> Vec<Point> {
    if points.len() <= MIN_RING {
        return points.to_vec();
    }

    // Project once, about the first point. Every distance in the algorithm is a comparison against
    // the tolerance over a few nautical miles, so a tangent plane is exact for this purpose and
    // avoids a trigonometric call per point per recursion.
    let lat0 = points[0].1;
    let cos_lat = lat0.to_radians().cos().abs().max(1e-6);
    let plane: Vec<(f64, f64)> = points
        .iter()
        .map(|(lon, lat)| {
            (
                (lon - points[0].0) * M_PER_DEG_LON * cos_lat,
                (lat - points[0].1) * M_PER_DEG_LAT,
            )
        })
        .collect();

    let mut keep = vec![false; points.len()];
    keep[0] = true;
    keep[points.len() - 1] = true;

    // An explicit stack rather than recursion: a 16,904-vertex polygon exists in this data set
    // (Kansas City Class B) and a pathological one could nest deeply enough to matter.
    let mut stack = vec![(0usize, points.len() - 1)];
    while let Some((a, b)) = stack.pop() {
        if b <= a + 1 {
            continue;
        }
        let mut worst = 0.0f64;
        let mut worst_index = 0usize;
        for (i, point) in plane.iter().enumerate().take(b).skip(a + 1) {
            let d = perpendicular_distance(*point, plane[a], plane[b]);
            if d > worst {
                worst = d;
                worst_index = i;
            }
        }
        if worst > tolerance_m {
            keep[worst_index] = true;
            stack.push((a, worst_index));
            stack.push((worst_index, b));
        }
    }

    let simplified: Vec<Point> = points
        .iter()
        .zip(&keep)
        .filter_map(|(p, k)| k.then_some(*p))
        .collect();

    if simplified.len() < MIN_RING {
        return points.to_vec();
    }
    simplified
}

/// Distance from `p` to the segment `a`–`b`, both already on the tangent plane.
///
/// When the segment is degenerate — which is exactly the case for the first call on a closed ring,
/// where the endpoints are the same point — this is the distance to that point. See the module
/// note: that is what makes closed rings work without a special case.
fn perpendicular_distance(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len_sq = dx * dx + dy * dy;
    if len_sq <= f64::EPSILON {
        return ((p.0 - a.0).powi(2) + (p.1 - a.1).powi(2)).sqrt();
    }
    let t = (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len_sq).clamp(0.0, 1.0);
    let (cx, cy) = (a.0 + t * dx, a.1 + t * dy);
    ((p.0 - cx).powi(2) + (p.1 - cy).powi(2)).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A circle of `n` points, like the ones the FAA ships for a Class D.
    fn circle(centre: Point, radius_nm: f64, n: usize) -> Vec<Point> {
        let cos_lat = centre.1.to_radians().cos();
        let mut out: Vec<Point> = (0..n)
            .map(|i| {
                let a = (i as f64 / n as f64) * std::f64::consts::TAU;
                let d_lat = radius_nm * a.cos() / 60.0;
                let d_lon = radius_nm * a.sin() / (60.0 * cos_lat);
                (centre.0 + d_lon, centre.1 + d_lat)
            })
            .collect();
        out.push(out[0]); // closed, as GeoJSON rings are
        out
    }

    /// Everything on a common tangent plane about `origin`, in metres.
    fn plane_of(points: &[Point], origin: Point) -> Vec<(f64, f64)> {
        let cos_lat = origin.1.to_radians().cos().abs();
        points
            .iter()
            .map(|(lon, lat)| {
                (
                    (lon - origin.0) * M_PER_DEG_LON * cos_lat,
                    (lat - origin.1) * M_PER_DEG_LAT,
                )
            })
            .collect()
    }

    /// True distance from a point to the closest edge of an outline, in metres.
    ///
    /// Exact rather than sampled. An earlier version of the circle test walked eleven points along
    /// each simplified edge and measured to the nearest of them, which reported a 40.7 m error on
    /// geometry that was within tolerance — the simplified edges are about 807 m long, so the
    /// sample spacing alone accounts for 40 m. The test was measuring its own sampling.
    fn distance_to_outline(p: (f64, f64), outline: &[(f64, f64)]) -> f64 {
        outline
            .windows(2)
            .map(|w| perpendicular_distance(p, w[0], w[1]))
            .fold(f64::MAX, f64::min)
    }

    #[test]
    fn collinear_points_along_an_edge_collapse_to_its_ends() {
        // A rectangle whose edges are described by fifty points each, which is what an arc-free
        // stretch of a real boundary looks like after the FAA's discretisation.
        let mut points: Vec<Point> = Vec::new();
        for i in 0..50 {
            points.push((-74.0 + i as f64 * 0.001, 40.0));
        }
        for i in 0..50 {
            points.push((-73.951, 40.0 + i as f64 * 0.001));
        }
        for i in 0..50 {
            points.push((-73.951 - i as f64 * 0.001, 40.049));
        }
        points.push(points[0]);

        let simple = ring(&points, TOLERANCE_M);
        assert!(
            simple.len() <= 5,
            "a three-sided outline should keep its corners and nothing else, kept {}",
            simple.len()
        );
        assert!(simple.len() >= MIN_RING);
    }

    #[test]
    fn a_dense_circle_keeps_its_shape_within_the_tolerance() {
        // The case the whole module exists for: a Class D arrives as thousands of points and has
        // to come out as tens without moving.
        let dense = circle((-74.4, 40.8), 4.4, 4000);
        let simple = ring(&dense, TOLERANCE_M);

        assert!(
            simple.len() < dense.len() / 20,
            "expected a large reduction, got {} from {}",
            simple.len(),
            dense.len()
        );
        assert!(simple.len() >= MIN_RING);

        // Every original point is still within the tolerance of the simplified outline. This is
        // the property that matters — a vertex count is not evidence the shape survived.
        let origin = dense[0];
        let dense_m = plane_of(&dense, origin);
        let simple_m = plane_of(&simple, origin);
        let worst = dense_m
            .iter()
            .map(|p| distance_to_outline(*p, &simple_m))
            .fold(0.0f64, f64::max);
        assert!(
            worst <= TOLERANCE_M,
            "shape moved by {worst:.1} m, tolerance {TOLERANCE_M}"
        );
    }

    #[test]
    fn the_first_and_last_points_are_never_dropped() {
        // The ring has to stay closed. If the closing point were simplified away the renderer
        // would draw a polygon with a notch cut out of it at an arbitrary place.
        let dense = circle((-118.4, 34.0), 4.0, 500);
        let simple = ring(&dense, TOLERANCE_M);
        assert_eq!(simple[0], dense[0]);
        assert_eq!(*simple.last().unwrap(), *dense.last().unwrap());
        assert_eq!(simple[0], *simple.last().unwrap(), "ring must stay closed");
    }

    #[test]
    fn a_ring_is_never_simplified_out_of_existence() {
        // A tolerance far larger than the shape would otherwise leave two coincident points and a
        // polygon with no area, which draws as nothing and looks like missing data.
        let small = circle((-74.0, 40.0), 0.05, 200);
        let simple = ring(&small, 100_000.0);
        assert!(simple.len() >= MIN_RING, "got {} points", simple.len());
    }

    #[test]
    fn already_short_rings_are_returned_untouched() {
        let triangle = vec![(-74.0, 40.0), (-74.1, 40.0), (-74.05, 40.1), (-74.0, 40.0)];
        assert_eq!(ring(&triangle, TOLERANCE_M).len(), triangle.len());
    }

    #[test]
    fn a_tighter_tolerance_never_keeps_fewer_points() {
        // Monotonicity. Not a formality: an off-by-one in the stack bounds can make the result
        // non-monotonic, and that is invisible in a spot check of one tolerance.
        let dense = circle((-87.9, 41.9), 5.0, 1500);
        let mut previous = usize::MAX;
        for tolerance in [1.0, 5.0, 10.0, 25.0, 100.0] {
            let n = ring(&dense, tolerance).len();
            assert!(
                n <= previous,
                "tolerance {tolerance} kept {n} points, more than the tighter one's {previous}"
            );
            previous = n;
        }
    }

    #[test]
    fn a_notch_survives_but_ripple_below_the_tolerance_does_not() {
        // The shape that distinguishes simplification from decimation. A Class B with a corridor
        // cut into it must keep the corridor; the surveyor's noise along a straight edge must go.
        let mut points: Vec<Point> = Vec::new();
        for i in 0..=100 {
            // A long edge with sub-metre ripple on it.
            let x = -74.0 + i as f64 * 0.0005;
            let wobble = if i % 2 == 0 { 0.000002 } else { -0.000002 };
            points.push((x, 40.0 + wobble));
        }
        // ... then a 500 m notch, which is fifty times the tolerance.
        points.push((-73.95, 40.0045));
        points.push((-73.945, 40.0));
        points.push((-74.0, 40.0));

        let simple = ring(&points, TOLERANCE_M);
        assert!(
            simple.len() < 10,
            "ripple survived: {} points",
            simple.len()
        );
        assert!(
            simple.iter().any(|p| p.1 > 40.004),
            "the notch was simplified away"
        );
    }
}
