//! Mapping between geographic positions and screen pixels.
//!
//! A local tangent plane about own-ship, not a real map projection. At the ranges an ADS-B
//! receiver can see (tens of nautical miles), the error from treating the Earth as flat is far
//! below one pixel, and it avoids both a projection library and any question of which datum a
//! tile set was built in.

use stratux_client::domain::LatLon;

/// Nautical miles per degree of latitude. Exact by definition of the nautical mile.
pub const NM_PER_DEG_LAT: f64 = 60.0;

/// Which way is up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orientation {
    /// True north at the top of the screen.
    NorthUp,
    /// Own-ship's track at the top of the screen.
    TrackUp,
}

impl Orientation {
    pub fn toggled(self) -> Self {
        match self {
            Self::NorthUp => Self::TrackUp,
            Self::TrackUp => Self::NorthUp,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::NorthUp => "N-UP",
            Self::TrackUp => "TRK-UP",
        }
    }
}

/// Projects positions around an origin onto screen coordinates.
#[derive(Debug, Clone, Copy)]
pub struct Projection {
    origin: LatLon,
    center_px: (f32, f32),
    px_per_nm: f32,
    /// Degrees the world is rotated counter-clockwise: 0 for north-up, own track for track-up.
    rotation_deg: f32,
    /// cos(origin latitude), precomputed since it is used per target per frame.
    cos_origin_lat: f64,
    sin_rot: f32,
    cos_rot: f32,
}

impl Projection {
    pub fn new(
        origin: LatLon,
        center_px: (f32, f32),
        px_per_nm: f32,
        orientation: Orientation,
        own_track_deg: Option<f32>,
    ) -> Self {
        // Track-up needs a track to rotate by. With no velocity solution (stationary on the
        // ramp, or no fix) fall back to north-up rather than freezing at a stale heading, which
        // would silently mislabel every bearing on screen.
        let rotation_deg = match (orientation, own_track_deg) {
            (Orientation::TrackUp, Some(track)) => track,
            _ => 0.0,
        };
        let rot = rotation_deg.to_radians();
        Self {
            origin,
            center_px,
            px_per_nm,
            rotation_deg,
            // Latitude 90 would collapse the longitude scale; clamp rather than divide by zero.
            cos_origin_lat: origin.lat.to_radians().cos().max(1e-6),
            sin_rot: rot.sin(),
            cos_rot: rot.cos(),
        }
    }

    pub fn origin(&self) -> LatLon {
        self.origin
    }

    pub fn center_px(&self) -> (f32, f32) {
        self.center_px
    }

    pub fn px_per_nm(&self) -> f32 {
        self.px_per_nm
    }

    /// Rotation actually applied, which may be 0 even in track-up if no track was available.
    pub fn rotation_deg(&self) -> f32 {
        self.rotation_deg
    }

    /// Offset from the origin in nautical miles, as (east, north).
    pub fn offset_nm(&self, position: LatLon) -> (f32, f32) {
        let north = (position.lat - self.origin.lat) * NM_PER_DEG_LAT;
        let east = (position.lon - self.origin.lon) * NM_PER_DEG_LAT * self.cos_origin_lat;
        (east as f32, north as f32)
    }

    /// Range in nautical miles and true bearing in degrees from the origin.
    pub fn range_bearing(&self, position: LatLon) -> (f32, f32) {
        let (east, north) = self.offset_nm(position);
        let range = (east * east + north * north).sqrt();
        let bearing = east.atan2(north).to_degrees().rem_euclid(360.0);
        (range, bearing)
    }

    /// Project a position to screen pixels.
    pub fn project(&self, position: LatLon) -> (f32, f32) {
        let (east, north) = self.offset_nm(position);
        self.project_offset(east, north)
    }

    /// Project an east/north offset in nautical miles to screen pixels.
    ///
    /// Screen y grows downward, so north is negated. The rotation puts a target at true bearing
    /// `b` at screen angle `b - rotation` measured clockwise from straight up.
    pub fn project_offset(&self, east_nm: f32, north_nm: f32) -> (f32, f32) {
        let x = east_nm * self.cos_rot - north_nm * self.sin_rot;
        let y = north_nm * self.cos_rot + east_nm * self.sin_rot;
        (
            self.center_px.0 + x * self.px_per_nm,
            self.center_px.1 - y * self.px_per_nm,
        )
    }

    /// Screen angle in radians, clockwise from straight up, for a true bearing or heading.
    ///
    /// Use this to point heading barbs and to place ring labels, so they follow the same
    /// rotation as target positions.
    pub fn screen_angle_rad(&self, true_deg: f32) -> f32 {
        (true_deg - self.rotation_deg).to_radians()
    }

    pub fn nm_to_px(&self, nm: f32) -> f32 {
        nm * self.px_per_nm
    }

    /// Screen pixels back to a position — the inverse of [`Projection::project`].
    ///
    /// Needed because a tap arrives in pixels and the thing it might have landed on is a polygon
    /// in degrees. Inverting here rather than projecting every boundary vertex into screen space
    /// keeps the hit test in one coordinate system and costs one transform instead of thousands.
    pub fn unproject(&self, x: f32, y: f32) -> LatLon {
        let dx = (x - self.center_px.0) / self.px_per_nm;
        // Screen y grows downward; `project_offset` negates north, so undo that first.
        let dy = (self.center_px.1 - y) / self.px_per_nm;

        // Un-rotate. `project_offset` applies [cos, -sin; sin, cos] to (east, north), so the
        // inverse is the transpose.
        let east = dx * self.cos_rot + dy * self.sin_rot;
        let north = dy * self.cos_rot - dx * self.sin_rot;

        LatLon::new(
            self.origin.lat + north as f64 / NM_PER_DEG_LAT,
            self.origin.lon + east as f64 / (NM_PER_DEG_LAT * self.cos_origin_lat),
        )
    }
}

/// Advance a position along a track. Shared by dead reckoning and by tests.
pub fn advance(from: LatLon, track_deg: f64, speed_kt: f64, seconds: f64) -> LatLon {
    let distance_nm = speed_kt * seconds / 3600.0;
    let heading = track_deg.to_radians();
    let d_lat = distance_nm * heading.cos() / NM_PER_DEG_LAT;
    let d_lon =
        distance_nm * heading.sin() / (NM_PER_DEG_LAT * from.lat.to_radians().cos().max(1e-6));
    LatLon::new(from.lat + d_lat, from.lon + d_lon)
}

#[cfg(test)]
mod projection_tests {
    use super::*;

    fn probe(orientation: Orientation, track: Option<f32>) -> Projection {
        Projection::new(
            LatLon::new(40.7784, -74.3343),
            (400.0, 240.0),
            18.75,
            orientation,
            track,
        )
    }

    #[test]
    fn unproject_inverts_project_in_both_orientations() {
        // A tap arrives in pixels and the polygon it might have hit is in degrees, so this has to
        // be exact enough that a point near a boundary lands on the right side of it. Half a pixel
        // at the tightest range is about 10 m; the tolerance below is far inside that.
        for (orientation, track) in [
            (Orientation::NorthUp, None),
            (Orientation::NorthUp, Some(042.0)),
            (Orientation::TrackUp, Some(042.0)),
            (Orientation::TrackUp, Some(287.0)),
            (Orientation::TrackUp, None),
        ] {
            let p = probe(orientation, track);
            for (x, y) in [(400.0, 240.0), (120.0, 90.0), (700.0, 430.0), (401.5, 239.5)] {
                let back = p.project(p.unproject(x, y));
                assert!(
                    (back.0 - x).abs() < 0.01 && (back.1 - y).abs() < 0.01,
                    "{orientation:?} track {track:?}: ({x}, {y}) -> ({}, {})",
                    back.0,
                    back.1
                );
            }
        }
    }

    #[test]
    fn unprojecting_the_centre_gives_own_ship() {
        let p = probe(Orientation::TrackUp, Some(200.0));
        let at = p.unproject(400.0, 240.0);
        assert!((at.lat - p.origin().lat).abs() < 1e-9);
        assert!((at.lon - p.origin().lon).abs() < 1e-9);
    }

    #[test]
    fn a_point_above_the_centre_is_north_in_north_up() {
        let p = probe(Orientation::NorthUp, None);
        let above = p.unproject(400.0, 240.0 - 18.75); // one nautical mile up the screen
        assert!(above.lat > p.origin().lat, "up the screen should be north");
        assert!((above.lon - p.origin().lon).abs() < 1e-9, "and not east or west");
        assert!(((above.lat - p.origin().lat) * 60.0 - 1.0).abs() < 1e-6, "one nm");
    }
}
