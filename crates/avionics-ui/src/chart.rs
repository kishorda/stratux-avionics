//! Reading the airport and airspace file built by `tools/chartdata`.
//!
//! The file is fixed-layout little-endian, so loading is a read and a header check — no parsing,
//! no allocation per record, and nothing on the aircraft that has to understand CSV or GeoJSON.
//! Records are decoded on demand out of the byte buffer, which is why a query allocates only the
//! handful of results it returns.
//!
//! ```text
//!   header     64 B   magic, version, counts, section offsets, grid, effective date
//!   buckets     8 B   per 1x1 degree cell: first airport, airport count
//!   airports   24 B   position, label, elevation, runway, kind, tier, flags
//!   airspace   40 B   bounding box, ring range, class, flags, lower/upper, label
//!   rings       8 B   first vertex, vertex count
//!   vertices    8 B   latitude and longitude, i32 micro-degrees
//! ```
//!
//! # Missing is not broken
//!
//! Every entry point here is fallible and the display treats a missing or unreadable file as "no
//! map layer", never as a startup failure. The file is a convenience; traffic is the reason the
//! panel exists, and a corrupt chart must not be able to stop it being drawn.
//!
//! See `docs/airspace-and-airports.md` for how the file is produced and why each threshold is
//! where it is.

use std::path::Path;

use anyhow::{bail, ensure, Result};
use stratux_client::domain::LatLon;

const MAGIC: [u8; 8] = *b"AVCHART1";
const VERSION: u16 = 1;

const HEADER_LEN: usize = 64;
const BUCKET_LEN: usize = 8;
const AIRPORT_LEN: usize = 24;
const AIRSPACE_LEN: usize = 40;
const RING_LEN: usize = 8;
const VERTEX_LEN: usize = 8;
const LABEL_LEN: usize = 8;

/// Grid cell size in degrees, matching the builder.
const CELL_DEG: i32 = 1;

/// What kind of place an airport record describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Large,
    Medium,
    Small,
    Heliport,
    Seaplane,
}

/// Which range band an airport first appears in.
///
/// Ordered, so a query is "everything at or below this tier". [`Tier::Heliport`] is last and is
/// its own tier rather than merely the least important: 287 heliports fall within 10 nm of
/// downtown Los Angeles, against a fixed-wing worst case of 35 anywhere in the country, so they
/// are carried in the file and never drawn by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    Major = 0,
    Paved = 1,
    Minor = 2,
    Heliport = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    B,
    C,
    D,
}

impl Class {
    pub fn label(self) -> &'static str {
        match self {
            Self::B => "B",
            Self::C => "C",
            Self::D => "D",
        }
    }
}

pub const FLAG_HARD_SURFACE: u8 = 1 << 0;
pub const FLAG_LIGHTED: u8 = 1 << 1;
pub const FLAG_LOWER_SURFACE: u8 = 1 << 0;

#[derive(Debug, Clone, Copy)]
pub struct Airport {
    pub position: LatLon,
    label: [u8; LABEL_LEN],
    pub elevation_ft: i16,
    /// Longest hard-surface runway in feet, 0 when there is none.
    pub runway_ft: u16,
    pub kind: Kind,
    pub tier: Tier,
    pub flags: u8,
}

impl Airport {
    pub fn label(&self) -> &str {
        let end = self
            .label
            .iter()
            .position(|b| *b == 0)
            .unwrap_or(LABEL_LEN);
        std::str::from_utf8(&self.label[..end]).unwrap_or("")
    }

    pub fn hard_surface(&self) -> bool {
        self.flags & FLAG_HARD_SURFACE != 0
    }
}

/// One airspace volume. The geometry stays in the file; [`Chart::ring`] walks it.
#[derive(Debug, Clone, Copy)]
pub struct Airspace {
    pub class: Class,
    label: [u8; LABEL_LEN],
    /// Lower limit in feet MSL. Meaningless when [`Airspace::lower_is_surface`] is set.
    pub lower_ft: i32,
    pub upper_ft: i32,
    pub flags: u8,
    ring_first: u32,
    ring_count: u16,
    bounds: Bounds,
}

impl Airspace {
    pub fn label(&self) -> &str {
        let end = self
            .label
            .iter()
            .position(|b| *b == 0)
            .unwrap_or(LABEL_LEN);
        std::str::from_utf8(&self.label[..end]).unwrap_or("")
    }

    pub fn lower_is_surface(&self) -> bool {
        self.flags & FLAG_LOWER_SURFACE != 0
    }

    pub fn ring_count(&self) -> usize {
        self.ring_count as usize
    }
}

/// A latitude/longitude box in micro-degrees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    pub lat_min: i32,
    pub lat_max: i32,
    pub lon_min: i32,
    pub lon_max: i32,
}

impl Bounds {
    pub fn overlaps(&self, other: &Bounds) -> bool {
        self.lat_min <= other.lat_max
            && other.lat_min <= self.lat_max
            && self.lon_min <= other.lon_max
            && other.lon_min <= self.lon_max
    }
}

#[derive(Debug, Clone, Copy)]
struct Grid {
    lat0: i16,
    lon0: i16,
    rows: u16,
    cols: u16,
}

impl Grid {
    fn cells(&self) -> usize {
        self.rows as usize * self.cols as usize
    }

    /// Row and column of a position, unclamped, so a caller can tell "off the grid" from "edge".
    fn row_col(&self, lat_e6: i32, lon_e6: i32) -> (i32, i32) {
        (
            div_floor(lat_e6, 1_000_000 * CELL_DEG) - self.lat0 as i32,
            div_floor(lon_e6, 1_000_000 * CELL_DEG) - self.lon0 as i32,
        )
    }
}

/// Floor division, which `/` is not for negative numbers — and every CONUS longitude is negative.
///
/// The builder has the same function for the same reason. If these two ever disagree the index
/// silently addresses the wrong cells, so the property is pinned by a test on both sides.
fn div_floor(a: i32, b: i32) -> i32 {
    let q = a / b;
    if a % b != 0 && ((a < 0) != (b < 0)) {
        q - 1
    } else {
        q
    }
}

/// The fixed 64-byte header, validated before any record is touched.
struct Header {
    effective_days: u32,
    airport_count: u32,
    airspace_count: u32,
    grid: Grid,
    bucket_off: usize,
    airport_off: usize,
    airspace_off: usize,
    ring_off: usize,
    vertex_off: usize,
}

impl Header {
    fn parse(bytes: &[u8]) -> Result<Self> {
        ensure!(bytes.len() >= HEADER_LEN, "shorter than its header");
        if bytes[..8] != MAGIC {
            bail!("not a chart file");
        }
        let u16at = |o: usize| u16::from_le_bytes([bytes[o], bytes[o + 1]]);
        let i16at = |o: usize| i16::from_le_bytes([bytes[o], bytes[o + 1]]);
        let u32at =
            |o: usize| u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);

        let version = u16at(8);
        ensure!(version == VERSION, "unsupported chart version {version}");

        let airport_count = u32at(16);
        let airspace_count = u32at(20);
        let ring_count = u32at(24);
        let vertex_count = u32at(28);
        let grid = Grid {
            lat0: i16at(32),
            lon0: i16at(34),
            rows: u16at(36),
            cols: u16at(38),
        };
        ensure!(grid.rows > 0 && grid.cols > 0, "empty grid");

        let bucket_off = u32at(40) as usize;
        let airport_off = u32at(44) as usize;
        let airspace_off = u32at(48) as usize;
        let ring_off = u32at(52) as usize;
        let vertex_off = u32at(56) as usize;

        ensure!(bucket_off == HEADER_LEN, "buckets misplaced");
        ensure!(
            airport_off == bucket_off + grid.cells() * BUCKET_LEN,
            "airport section misplaced"
        );
        ensure!(
            airspace_off == airport_off + airport_count as usize * AIRPORT_LEN,
            "airspace section misplaced"
        );
        ensure!(
            ring_off == airspace_off + airspace_count as usize * AIRSPACE_LEN,
            "ring section misplaced"
        );
        ensure!(
            vertex_off == ring_off + ring_count as usize * RING_LEN,
            "vertex section misplaced"
        );
        ensure!(
            bytes.len() == vertex_off + vertex_count as usize * VERTEX_LEN,
            "file length does not match its sections"
        );

        Ok(Self {
            effective_days: u32at(12),
            airport_count,
            airspace_count,
            grid,
            bucket_off,
            airport_off,
            airspace_off,
            ring_off,
            vertex_off,
        })
    }
}

pub struct Chart {
    bytes: Vec<u8>,
    grid: Grid,
    effective_days: u32,
    airport_count: u32,
    airspace_count: u32,
    bucket_off: usize,
    airport_off: usize,
    airspace_off: usize,
    ring_off: usize,
    vertex_off: usize,
}

impl Chart {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)?;
        Self::from_bytes(bytes)
    }

    /// Validate the header and every section boundary, then take ownership of the bytes.
    ///
    /// A file whose counts and offsets disagree would otherwise read one record into the next, and
    /// the first sign of that is an airport drawn in the sea — which is a long way from the cause.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        let header = Header::parse(&bytes)?;
        Ok(Self {
            bytes,
            grid: header.grid,
            effective_days: header.effective_days,
            airport_count: header.airport_count,
            airspace_count: header.airspace_count,
            bucket_off: header.bucket_off,
            airport_off: header.airport_off,
            airspace_off: header.airspace_off,
            ring_off: header.ring_off,
            vertex_off: header.vertex_off,
        })
    }

    /// Days since the Unix epoch of the FAA layer's own last edit — the currency of the airspace,
    /// not of the download.
    pub fn effective_days(&self) -> u32 {
        self.effective_days
    }

    pub fn airport_count(&self) -> usize {
        self.airport_count as usize
    }

    pub fn airspace_count(&self) -> usize {
        self.airspace_count as usize
    }

    fn u32at(&self, o: usize) -> u32 {
        u32::from_le_bytes([
            self.bytes[o],
            self.bytes[o + 1],
            self.bytes[o + 2],
            self.bytes[o + 3],
        ])
    }

    fn i32at(&self, o: usize) -> i32 {
        self.u32at(o) as i32
    }

    fn airport_at(&self, index: usize) -> Airport {
        let o = self.airport_off + index * AIRPORT_LEN;
        let mut label = [0u8; LABEL_LEN];
        label.copy_from_slice(&self.bytes[o + 8..o + 16]);
        Airport {
            position: LatLon::new(
                self.i32at(o) as f64 / 1e6,
                self.i32at(o + 4) as f64 / 1e6,
            ),
            label,
            elevation_ft: i16::from_le_bytes([self.bytes[o + 16], self.bytes[o + 17]]),
            runway_ft: u16::from_le_bytes([self.bytes[o + 18], self.bytes[o + 19]]),
            kind: match self.bytes[o + 20] {
                0 => Kind::Large,
                1 => Kind::Medium,
                2 => Kind::Small,
                3 => Kind::Heliport,
                _ => Kind::Seaplane,
            },
            tier: match self.bytes[o + 21] {
                0 => Tier::Major,
                1 => Tier::Paved,
                2 => Tier::Minor,
                _ => Tier::Heliport,
            },
            flags: self.bytes[o + 22],
        }
    }

    fn airspace_at(&self, index: usize) -> Airspace {
        let o = self.airspace_off + index * AIRSPACE_LEN;
        let mut label = [0u8; LABEL_LEN];
        label.copy_from_slice(&self.bytes[o + 32..o + 40]);
        Airspace {
            bounds: Bounds {
                lat_min: self.i32at(o),
                lat_max: self.i32at(o + 4),
                lon_min: self.i32at(o + 8),
                lon_max: self.i32at(o + 12),
            },
            ring_first: self.u32at(o + 16),
            ring_count: u16::from_le_bytes([self.bytes[o + 20], self.bytes[o + 21]]),
            class: match self.bytes[o + 22] {
                0 => Class::B,
                1 => Class::C,
                _ => Class::D,
            },
            flags: self.bytes[o + 23],
            lower_ft: self.i32at(o + 24),
            upper_ft: self.i32at(o + 28),
            label,
        }
    }

    /// Airports inside `bounds` at or below `max_tier`.
    ///
    /// Walks only the grid cells the box touches. The alternative — scanning all 20,736 records —
    /// is several hundred thousand bounding-box tests a second at 30 Hz, which is the same order
    /// as the whole current frame cost.
    pub fn airports_in(&self, bounds: &Bounds, max_tier: Tier) -> Vec<Airport> {
        let (row0, col0) = self.grid.row_col(bounds.lat_min, bounds.lon_min);
        let (row1, col1) = self.grid.row_col(bounds.lat_max, bounds.lon_max);

        let mut out = Vec::new();
        for row in row0.max(0)..=row1.min(self.grid.rows as i32 - 1) {
            for col in col0.max(0)..=col1.min(self.grid.cols as i32 - 1) {
                let cell = row as usize * self.grid.cols as usize + col as usize;
                let o = self.bucket_off + cell * BUCKET_LEN;
                let first = self.u32at(o) as usize;
                let count = self.u32at(o + 4) as usize;
                for index in first..first + count {
                    let airport = self.airport_at(index);
                    if airport.tier > max_tier {
                        continue;
                    }
                    // The cell is a whole degree, so most of what it holds is outside a 10 nm box.
                    let lat_e6 = (airport.position.lat * 1e6) as i32;
                    let lon_e6 = (airport.position.lon * 1e6) as i32;
                    if lat_e6 < bounds.lat_min
                        || lat_e6 > bounds.lat_max
                        || lon_e6 < bounds.lon_min
                        || lon_e6 > bounds.lon_max
                    {
                        continue;
                    }
                    out.push(airport);
                }
            }
        }
        out
    }

    /// Airspace whose bounding box overlaps `bounds`, in file order — Class B first, so a caller
    /// drawing in order puts the largest volumes underneath.
    ///
    /// Deliberately a linear scan. There are 1,408 records with their boxes already in them, which
    /// is a few microseconds a frame; a second index would be machinery earning nothing, and
    /// polygons span cells, which makes indexing them the harder of the two jobs as well.
    pub fn airspace_in(&self, bounds: &Bounds) -> Vec<Airspace> {
        (0..self.airspace_count as usize)
            .map(|i| self.airspace_at(i))
            .filter(|a| a.bounds.overlaps(bounds))
            .collect()
    }

    /// Vertices of one ring of an airspace volume, as latitude/longitude degrees.
    ///
    /// The ring is **open**: the closing point was dropped when the file was built, because a
    /// renderer closes the path itself. Iterating allocates nothing.
    pub fn ring(&self, space: &Airspace, ring: usize) -> RingIter<'_> {
        if ring >= space.ring_count as usize {
            return RingIter { chart: self, next: 0, end: 0 };
        }
        let o = self.ring_off + (space.ring_first as usize + ring) * RING_LEN;
        let first = self.u32at(o) as usize;
        let count = self.u32at(o + 4) as usize;
        RingIter {
            chart: self,
            next: first,
            end: first + count,
        }
    }
}

pub struct RingIter<'a> {
    chart: &'a Chart,
    next: usize,
    end: usize,
}

impl Iterator for RingIter<'_> {
    type Item = LatLon;

    fn next(&mut self) -> Option<LatLon> {
        if self.next >= self.end {
            return None;
        }
        let o = self.chart.vertex_off + self.next * VERTEX_LEN;
        self.next += 1;
        Some(LatLon::new(
            self.chart.i32at(o) as f64 / 1e6,
            self.chart.i32at(o + 4) as f64 / 1e6,
        ))
    }
}

impl ExactSizeIterator for RingIter<'_> {
    fn len(&self) -> usize {
        self.end - self.next
    }
}

/// A box around a position, big enough to cover a circular range.
///
/// Longitude degrees shrink with latitude, so the box would be too narrow east-west without the
/// cosine — at 40 degrees north by about a quarter, which would quietly clip airports off the
/// left and right of the screen while keeping the ones above and below.
pub fn bounds_around(centre: LatLon, radius_nm: f32) -> Bounds {
    let d_lat = radius_nm as f64 / 60.0;
    let d_lon = radius_nm as f64 / (60.0 * centre.lat.to_radians().cos().abs().max(1e-6));
    Bounds {
        lat_min: ((centre.lat - d_lat) * 1e6) as i32,
        lat_max: ((centre.lat + d_lat) * 1e6) as i32,
        lon_min: ((centre.lon - d_lon) * 1e6) as i32,
        lon_max: ((centre.lon + d_lon) * 1e6) as i32,
    }
}

/// The most detailed tier worth drawing at a given range.
///
/// See `docs/airspace-and-airports.md`: tier populations are 821 / 3,155 / 11,452, and the worst
/// on-screen counts anywhere in the country are 13 at 40 nm and 35 at 10 nm once heliports are
/// excluded. Without the tiering, 5 nm over Los Angeles draws 172 symbols.
pub fn max_tier_for_range(range_nm: f32) -> Tier {
    if range_nm <= 5.0 {
        Tier::Minor
    } else if range_nm <= 20.0 {
        Tier::Paved
    } else {
        Tier::Major
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The file the repo ships. Read once per test; it is 1.6 MB and this is a desktop test run.
    fn conus() -> Option<Chart> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("data/conus.chart");
        Chart::load(&path).ok()
    }

    #[test]
    fn the_shipped_file_loads_and_matches_what_the_builder_reported() {
        let Some(chart) = conus() else {
            panic!("crates/avionics-ui/data/conus.chart is missing or unreadable");
        };
        assert_eq!(chart.airport_count(), 20_736);
        assert_eq!(chart.airspace_count(), 1_408);
        // 2026-07-09, as the FAA layer reported it when the file was built.
        assert_eq!(chart.effective_days(), 20_643);
    }

    #[test]
    fn morristown_is_where_it_should_be() {
        // A real airport this project already uses as its reference position: the outdoor capture
        // was made at 40.7784, -74.3343, and KMMU is the field next to it.
        let Some(chart) = conus() else { return };
        let bounds = bounds_around(LatLon::new(40.7784, -74.3343), 10.0);
        let found = chart.airports_in(&bounds, Tier::Minor);

        let mmu = found
            .iter()
            .find(|a| a.label() == "MMU")
            .expect("MMU within 10 nm of the capture site");
        assert!((mmu.position.lat - 40.799).abs() < 0.01, "{}", mmu.position.lat);
        assert!((mmu.position.lon + 74.415).abs() < 0.01, "{}", mmu.position.lon);
        assert_eq!(mmu.kind, Kind::Medium);
        assert_eq!(mmu.tier, Tier::Major);
        assert!(mmu.hard_surface());
        assert!(mmu.runway_ft > 5000, "MMU's runway is about 6000 ft");
    }

    #[test]
    fn a_query_returns_only_what_is_inside_the_box() {
        // The grid cell is a whole degree — 60 nm of latitude — so most of what a cell holds is
        // outside a 10 nm box. Without the second test this would return a square of countryside.
        let Some(chart) = conus() else { return };
        let centre = LatLon::new(40.7784, -74.3343);
        let bounds = bounds_around(centre, 10.0);
        for airport in chart.airports_in(&bounds, Tier::Minor) {
            let lat_e6 = (airport.position.lat * 1e6) as i32;
            let lon_e6 = (airport.position.lon * 1e6) as i32;
            assert!(
                lat_e6 >= bounds.lat_min && lat_e6 <= bounds.lat_max,
                "{} is outside the box in latitude",
                airport.label()
            );
            assert!(
                lon_e6 >= bounds.lon_min && lon_e6 <= bounds.lon_max,
                "{} is outside the box in longitude",
                airport.label()
            );
        }
    }

    #[test]
    fn the_tier_filter_only_ever_removes() {
        let Some(chart) = conus() else { return };
        let bounds = bounds_around(LatLon::new(40.7784, -74.3343), 20.0);
        let major = chart.airports_in(&bounds, Tier::Major).len();
        let paved = chart.airports_in(&bounds, Tier::Paved).len();
        let minor = chart.airports_in(&bounds, Tier::Minor).len();
        assert!(major <= paved && paved <= minor, "{major} {paved} {minor}");
        assert!(major > 0, "the New York area has major airports");
    }

    #[test]
    fn heliports_are_in_the_file_and_never_in_a_default_query() {
        // The clutter finding, and a correction to it. The raw source has 291 heliports within a
        // 10 nm box of downtown Los Angeles, against 5 fixed-wing fields — but 208 of those are
        // OurAirports placeholders with no real identifier, and the builder has already dropped
        // them. So the tier is not doing all the work the design note credited it with; it is
        // still the difference between 83 symbols and 5.
        let Some(chart) = conus() else { return };
        let la = LatLon::new(34.055, -118.265);
        let bounds = bounds_around(la, 10.0);

        let with = chart.airports_in(&bounds, Tier::Heliport);
        let without = chart.airports_in(&bounds, Tier::Minor);
        assert!(
            with.len() > without.len() * 8,
            "expected heliports to dominate downtown LA, got {} vs {}",
            with.len(),
            without.len()
        );
        for range in crate::ViewState::RANGES {
            assert_ne!(
                max_tier_for_range(range),
                Tier::Heliport,
                "range {range} would draw heliports"
            );
        }
    }

    #[test]
    fn the_busiest_view_in_the_country_stays_drawable() {
        // What decides render cost is not the size of the file, it is how much of it reaches the
        // screen — and the query covers the *corners* of the panel, which are nearly twice the
        // selected range out. This pins the real worst case rather than trusting the design note.
        let Some(chart) = conus() else { return };
        let layout = crate::Layout::for_size(800.0, 480.0, &crate::Theme::dark());

        let mut worst = (0usize, String::new());
        for (place, centre) in [
            ("Los Angeles", LatLon::new(34.055, -118.265)),
            ("New York", LatLon::new(40.7784, -74.3343)),
            ("Denton TX", LatLon::new(33.232, -97.338)),
            ("Chicago", LatLon::new(41.9, -87.9)),
        ] {
            for range in crate::ViewState::RANGES {
                let projection = crate::Projection::new(
                    centre,
                    layout.center,
                    layout.outer_radius / range,
                    crate::Orientation::NorthUp,
                    None,
                );
                let radius = crate::maplayer::visible_radius_nm(&layout, &projection);
                let bounds = bounds_around(centre, radius);
                let n = chart.airports_in(&bounds, max_tier_for_range(range)).len();
                if n > worst.0 {
                    worst = (n, format!("{place} at {range} nm"));
                }
            }
        }
        assert!(
            worst.0 <= 120,
            "{} would query {} airports, which is past what the panel can show",
            worst.1,
            worst.0
        );
        assert!(worst.0 > 0, "no airports anywhere, which means the query is broken");
    }

    #[test]
    fn class_b_around_newark_is_found_and_has_real_geometry() {
        let Some(chart) = conus() else { return };
        let bounds = bounds_around(LatLon::new(40.6895, -74.1745), 20.0);
        let spaces = chart.airspace_in(&bounds);
        assert!(!spaces.is_empty(), "the New York area has class airspace");

        let b = spaces
            .iter()
            .find(|s| s.class == Class::B)
            .expect("Newark sits under the New York Class B");
        assert!(b.ring_count() >= 1);
        let points: Vec<LatLon> = chart.ring(b, 0).collect();
        assert!(points.len() >= 3, "a ring needs three points to enclose anything");
        assert_ne!(
            points.first().map(|p| (p.lat, p.lon)),
            points.last().map(|p| (p.lat, p.lon)),
            "the closing point should have been dropped at build time"
        );
        assert!(b.upper_ft > 1000, "a Class B has a real ceiling: {}", b.upper_ft);
    }

    #[test]
    fn no_airspace_upper_limit_was_left_in_flight_levels() {
        // The trap in the source data: thirty polygons state their ceiling as FL, so Tijuana's
        // reads 195. If the conversion were ever dropped this catches it in one assertion.
        let Some(chart) = conus() else { return };
        let all = chart.airspace_in(&Bounds {
            lat_min: i32::MIN,
            lat_max: i32::MAX,
            lon_min: i32::MIN,
            lon_max: i32::MAX,
        });
        assert_eq!(all.len(), chart.airspace_count());
        for space in &all {
            assert!(
                space.upper_ft >= 500,
                "{} tops out at {} ft, which looks like an unconverted flight level",
                space.label(),
                space.upper_ft
            );
        }
    }

    #[test]
    fn every_ring_of_every_volume_is_walkable() {
        // Guards the ring-and-vertex indirection end to end. A miscounted ring range reads one
        // volume's geometry into the next, which draws as a boundary in the wrong place.
        let Some(chart) = conus() else { return };
        let all = chart.airspace_in(&Bounds {
            lat_min: i32::MIN,
            lat_max: i32::MAX,
            lon_min: i32::MIN,
            lon_max: i32::MAX,
        });
        let mut vertices = 0usize;
        for space in &all {
            assert!(space.ring_count() >= 1, "{} has no rings", space.label());
            for ring in 0..space.ring_count() {
                let points: Vec<LatLon> = chart.ring(space, ring).collect();
                assert!(points.len() >= 3, "{} ring {ring} is degenerate", space.label());
                for p in &points {
                    assert!(
                        p.lat.is_finite() && p.lon.is_finite(),
                        "{} has a non-finite vertex",
                        space.label()
                    );
                }
                vertices += points.len();
            }
        }
        assert_eq!(vertices, 128_479, "every vertex in the file should be reachable");
    }

    #[test]
    fn a_ring_index_past_the_end_yields_nothing_rather_than_another_volumes_geometry() {
        let Some(chart) = conus() else { return };
        let bounds = bounds_around(LatLon::new(40.6895, -74.1745), 20.0);
        let space = chart.airspace_in(&bounds).into_iter().next().unwrap();
        assert_eq!(chart.ring(&space, space.ring_count()).count(), 0);
        assert_eq!(chart.ring(&space, 999).count(), 0);
    }

    #[test]
    fn western_longitudes_do_not_round_toward_zero() {
        // The same property the builder pins. If these two disagree the index addresses the wrong
        // cells and airports go quietly missing — no load failure, no error, just an emptier map.
        assert_eq!(div_floor(-74_500_000, 1_000_000), -75);
        assert_eq!(div_floor(-74_000_000, 1_000_000), -74);
        assert_eq!(div_floor(40_500_000, 1_000_000), 40);
    }

    #[test]
    fn the_range_box_is_widened_for_latitude() {
        let b = bounds_around(LatLon::new(40.0, -74.0), 10.0);
        let lat_span = b.lat_max - b.lat_min;
        let lon_span = b.lon_max - b.lon_min;
        assert!(
            lon_span > lat_span,
            "longitude span {lon_span} should exceed latitude span {lat_span} at 40N"
        );
    }

    #[test]
    fn a_corrupt_file_is_refused_rather_than_read_as_geometry() {
        let Some(chart) = conus() else { return };
        let good = chart.bytes.clone();

        assert!(Chart::from_bytes(good[..HEADER_LEN - 1].to_vec()).is_err(), "short");
        assert!(Chart::from_bytes(good[..good.len() - 8].to_vec()).is_err(), "truncated");

        let mut magic = good.clone();
        magic[0] = b'X';
        assert!(Chart::from_bytes(magic).is_err(), "bad magic");

        let mut version = good.clone();
        version[8] = 99;
        assert!(Chart::from_bytes(version).is_err(), "unknown version");

        // A count that disagrees with the offsets must be caught at load, not at the first draw.
        let mut count = good;
        count[16] = count[16].wrapping_add(1);
        assert!(Chart::from_bytes(count).is_err(), "airport count vs offsets");
    }
}
