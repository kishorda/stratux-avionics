//! The on-disk format the display reads.
//!
//! Fixed-layout little-endian, so loading on the Pi is a read and a few slices rather than a parse.
//! Every record size is a multiple of 8 and every section starts 8-aligned, which keeps a
//! zero-copy reader possible later without changing the file.
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
//! # Why airports are indexed and airspace is not
//!
//! There are 20,736 airports and 1,486 airspace polygons. Scanning the airports every frame would
//! be several hundred thousand bounding-box tests a second at 30 Hz — the same order as the whole
//! current frame cost — so they go in a 1-degree grid and the query touches only the cells in view.
//!
//! Airspace is two orders of magnitude smaller and every record already carries its bounding box,
//! so a linear scan is a few microseconds. A second index would be machinery earning nothing, and
//! polygons span cells, which makes indexing them the fiddlier of the two jobs as well.
//!
//! # Micro-degrees
//!
//! One micro-degree of latitude is about 0.11 m — two orders of magnitude finer than the 10 m
//! simplification tolerance, so quantisation is free of consequence. i32 covers the whole globe.

use anyhow::{bail, ensure, Result};

pub const MAGIC: [u8; 8] = *b"AVCHART1";
pub const VERSION: u16 = 1;

pub const HEADER_LEN: usize = 64;
pub const BUCKET_LEN: usize = 8;
pub const AIRPORT_LEN: usize = 24;
pub const AIRSPACE_LEN: usize = 40;
pub const RING_LEN: usize = 8;
pub const VERTEX_LEN: usize = 8;

/// Label field width. The longest identifier in the source data is 8 characters.
pub const LABEL_LEN: usize = 8;

/// Grid cell size in degrees. One degree of latitude is 60 nm, comfortably more than the 40 nm
/// largest selectable range, so a query touches at most a handful of cells.
pub const CELL_DEG: i32 = 1;

/// What kind of place an airport record describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Large = 0,
    Medium = 1,
    Small = 2,
    Heliport = 3,
    Seaplane = 4,
}

/// Which range band an airport first appears in. See `docs/airspace-and-airports.md`.
///
/// [`Tier::Heliport`] is its own tier and not merely the last one: 287 heliports fall within 10 nm
/// of downtown Los Angeles, against a fixed-wing worst case of 35 anywhere in the country. They
/// are carried in the file and never drawn by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Tier {
    /// Large and medium airports. Drawn at every range.
    Major = 0,
    /// Small airports with a hard runway of at least 3000 ft. Drawn at 20 nm and in.
    Paved = 1,
    /// Everything else fixed-wing, plus seaplane bases. Drawn at 5 nm and in.
    Minor = 2,
    /// Heliports. Never drawn by default.
    Heliport = 3,
}

pub const FLAG_HARD_SURFACE: u8 = 1 << 0;
pub const FLAG_LIGHTED: u8 = 1 << 1;

/// The lower limit is the surface, so `lower_ft` carries no information.
pub const FLAG_LOWER_SURFACE: u8 = 1 << 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Class {
    B = 0,
    C = 1,
    D = 2,
}

impl Class {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "B" => Some(Self::B),
            "C" => Some(Self::C),
            "D" => Some(Self::D),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::B => "B",
            Self::C => "C",
            Self::D => "D",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Airport {
    pub lat_e6: i32,
    pub lon_e6: i32,
    pub label: String,
    pub elevation_ft: i16,
    /// Longest hard-surface runway in feet, 0 when there is none.
    pub runway_ft: u16,
    pub kind: Kind,
    pub tier: Tier,
    pub flags: u8,
}

#[derive(Debug, Clone)]
pub struct Airspace {
    pub class: Class,
    pub label: String,
    pub lower_ft: i32,
    pub upper_ft: i32,
    pub flags: u8,
    /// Outer and inner rings, already simplified, and **open**: the closing point is dropped
    /// because a renderer closes the path itself, and storing it would repeat a vertex per ring.
    pub rings: Vec<Vec<(i32, i32)>>,
}

impl Airspace {
    pub fn bounds(&self) -> (i32, i32, i32, i32) {
        let mut b = (i32::MAX, i32::MIN, i32::MAX, i32::MIN);
        for ring in &self.rings {
            for &(lat, lon) in ring {
                b.0 = b.0.min(lat);
                b.1 = b.1.max(lat);
                b.2 = b.2.min(lon);
                b.3 = b.3.max(lon);
            }
        }
        b
    }
}

/// The bounding box the grid covers, in whole degrees.
#[derive(Debug, Clone, Copy)]
pub struct Grid {
    pub lat0: i16,
    pub lon0: i16,
    pub rows: u16,
    pub cols: u16,
}

impl Grid {
    /// A grid that covers every position given, snapped out to whole degrees.
    ///
    /// Derived from the data rather than hardcoded to CONUS, so a file built for somewhere else
    /// indexes correctly instead of putting everything in one edge cell.
    pub fn covering(points: impl Iterator<Item = (i32, i32)>) -> Self {
        let (mut lat_min, mut lat_max) = (i32::MAX, i32::MIN);
        let (mut lon_min, mut lon_max) = (i32::MAX, i32::MIN);
        for (lat, lon) in points {
            lat_min = lat_min.min(lat);
            lat_max = lat_max.max(lat);
            lon_min = lon_min.min(lon);
            lon_max = lon_max.max(lon);
        }
        if lat_min > lat_max {
            return Self { lat0: 0, lon0: 0, rows: 1, cols: 1 };
        }
        let lat0 = div_floor(lat_min, 1_000_000 * CELL_DEG);
        let lon0 = div_floor(lon_min, 1_000_000 * CELL_DEG);
        let lat1 = div_floor(lat_max, 1_000_000 * CELL_DEG);
        let lon1 = div_floor(lon_max, 1_000_000 * CELL_DEG);
        Self {
            lat0: lat0 as i16,
            lon0: lon0 as i16,
            rows: (lat1 - lat0 + 1) as u16,
            cols: (lon1 - lon0 + 1) as u16,
        }
    }

    pub fn cells(&self) -> usize {
        self.rows as usize * self.cols as usize
    }

    /// Which cell a position falls in, clamped to the grid.
    ///
    /// Clamping rather than rejecting: a position one micro-degree outside the derived box —
    /// which the box construction makes impossible, but a future edit might not — belongs in the
    /// edge cell, not in no cell at all.
    pub fn cell(&self, lat_e6: i32, lon_e6: i32) -> usize {
        let row = div_floor(lat_e6, 1_000_000 * CELL_DEG) - self.lat0 as i32;
        let col = div_floor(lon_e6, 1_000_000 * CELL_DEG) - self.lon0 as i32;
        let row = row.clamp(0, self.rows as i32 - 1) as usize;
        let col = col.clamp(0, self.cols as i32 - 1) as usize;
        row * self.cols as usize + col
    }
}

/// Floor division, which `/` is not for negative numbers — and every CONUS longitude is negative.
///
/// `-74_500_000 / 1_000_000` is -74, which would put a position at 74.5W in the same cell as one
/// at 74.0W and leave the cell at -75 holding half of what it should. Rounding toward zero here is
/// the classic way to build an index that is subtly wrong only in the western hemisphere.
fn div_floor(a: i32, b: i32) -> i32 {
    let q = a / b;
    if a % b != 0 && ((a < 0) != (b < 0)) {
        q - 1
    } else {
        q
    }
}

/// Everything the file holds, before it is laid out.
pub struct Chart {
    pub effective_days: u32,
    pub airports: Vec<Airport>,
    pub airspace: Vec<Airspace>,
}

/// Serialise, sorting airports into the grid on the way.
pub fn write(chart: &Chart) -> Vec<u8> {
    let grid = Grid::covering(chart.airports.iter().map(|a| (a.lat_e6, a.lon_e6)));

    // Sort into cell order so each cell's airports are one contiguous run. Ties broken by label so
    // the build is reproducible: the same input must give a byte-identical file, or every rebuild
    // looks like a data change in the diff.
    let mut airports: Vec<&Airport> = chart.airports.iter().collect();
    airports.sort_by(|a, b| {
        grid.cell(a.lat_e6, a.lon_e6)
            .cmp(&grid.cell(b.lat_e6, b.lon_e6))
            .then_with(|| a.label.cmp(&b.label))
            .then_with(|| a.lat_e6.cmp(&b.lat_e6))
            .then_with(|| a.lon_e6.cmp(&b.lon_e6))
    });

    let mut buckets = vec![(0u32, 0u32); grid.cells()];
    for (index, airport) in airports.iter().enumerate() {
        let cell = grid.cell(airport.lat_e6, airport.lon_e6);
        if buckets[cell].1 == 0 {
            buckets[cell].0 = index as u32;
        }
        buckets[cell].1 += 1;
    }

    let mut rings: Vec<(u32, u32)> = Vec::new();
    let mut vertices: Vec<(i32, i32)> = Vec::new();
    let mut ring_ranges: Vec<(u32, u16)> = Vec::with_capacity(chart.airspace.len());
    for space in &chart.airspace {
        let first = rings.len() as u32;
        for ring in &space.rings {
            rings.push((vertices.len() as u32, ring.len() as u32));
            vertices.extend_from_slice(ring);
        }
        ring_ranges.push((first, space.rings.len() as u16));
    }

    let bucket_off = HEADER_LEN;
    let airport_off = bucket_off + buckets.len() * BUCKET_LEN;
    let airspace_off = airport_off + airports.len() * AIRPORT_LEN;
    let ring_off = airspace_off + chart.airspace.len() * AIRSPACE_LEN;
    let vertex_off = ring_off + rings.len() * RING_LEN;
    let total = vertex_off + vertices.len() * VERTEX_LEN;

    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // flags, reserved
    out.extend_from_slice(&chart.effective_days.to_le_bytes());
    out.extend_from_slice(&(airports.len() as u32).to_le_bytes());
    out.extend_from_slice(&(chart.airspace.len() as u32).to_le_bytes());
    out.extend_from_slice(&(rings.len() as u32).to_le_bytes());
    out.extend_from_slice(&(vertices.len() as u32).to_le_bytes());
    out.extend_from_slice(&grid.lat0.to_le_bytes());
    out.extend_from_slice(&grid.lon0.to_le_bytes());
    out.extend_from_slice(&grid.rows.to_le_bytes());
    out.extend_from_slice(&grid.cols.to_le_bytes());
    for offset in [bucket_off, airport_off, airspace_off, ring_off, vertex_off] {
        out.extend_from_slice(&(offset as u32).to_le_bytes());
    }
    out.extend_from_slice(&0u32.to_le_bytes()); // reserved
    debug_assert_eq!(out.len(), HEADER_LEN);

    for (first, count) in &buckets {
        out.extend_from_slice(&first.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
    }

    for airport in &airports {
        out.extend_from_slice(&airport.lat_e6.to_le_bytes());
        out.extend_from_slice(&airport.lon_e6.to_le_bytes());
        out.extend_from_slice(&label_bytes(&airport.label));
        out.extend_from_slice(&airport.elevation_ft.to_le_bytes());
        out.extend_from_slice(&airport.runway_ft.to_le_bytes());
        out.push(airport.kind as u8);
        out.push(airport.tier as u8);
        out.push(airport.flags);
        out.push(0);
    }

    for (space, (ring_first, ring_count)) in chart.airspace.iter().zip(&ring_ranges) {
        let (lat_min, lat_max, lon_min, lon_max) = space.bounds();
        for value in [lat_min, lat_max, lon_min, lon_max] {
            out.extend_from_slice(&value.to_le_bytes());
        }
        out.extend_from_slice(&ring_first.to_le_bytes());
        out.extend_from_slice(&ring_count.to_le_bytes());
        out.push(space.class as u8);
        out.push(space.flags);
        out.extend_from_slice(&space.lower_ft.to_le_bytes());
        out.extend_from_slice(&space.upper_ft.to_le_bytes());
        out.extend_from_slice(&label_bytes(&space.label));
    }

    for (first, count) in &rings {
        out.extend_from_slice(&first.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
    }

    for (lat, lon) in &vertices {
        out.extend_from_slice(&lat.to_le_bytes());
        out.extend_from_slice(&lon.to_le_bytes());
    }

    debug_assert_eq!(out.len(), total);
    out
}

/// Truncated at [`LABEL_LEN`] and NUL-padded. Truncation is silent by design — the builder has
/// already rejected anything without a usable short identifier, so this only ever pads.
fn label_bytes(label: &str) -> [u8; LABEL_LEN] {
    let mut out = [0u8; LABEL_LEN];
    for (slot, byte) in out.iter_mut().zip(label.bytes()) {
        *slot = byte;
    }
    out
}

/// What a reader gets back. Enough to verify a build; the display's own reader will want more.
#[derive(Debug)]
pub struct Summary {
    pub version: u16,
    pub effective_days: u32,
    pub airports: u32,
    pub airspace: u32,
    pub rings: u32,
    pub vertices: u32,
    pub grid: Grid,
    pub bytes: usize,
}

/// Read the header back and check every section is where it says it is.
///
/// This is the build's own proof rather than a convenience: a writer that miscounts a section
/// produces a file that loads and then reads one record into the next, and nothing about that
/// looks wrong until an airport appears in the sea.
pub fn read_summary(bytes: &[u8]) -> Result<Summary> {
    ensure!(bytes.len() >= HEADER_LEN, "file is shorter than its header");
    if bytes[..8] != MAGIC {
        bail!("not a chart file: bad magic");
    }
    let u16at = |o: usize| u16::from_le_bytes([bytes[o], bytes[o + 1]]);
    let i16at = |o: usize| i16::from_le_bytes([bytes[o], bytes[o + 1]]);
    let u32at = |o: usize| {
        u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]])
    };

    let version = u16at(8);
    ensure!(version == VERSION, "unsupported chart version {version}");

    let airports = u32at(16);
    let airspace = u32at(20);
    let rings = u32at(24);
    let vertices = u32at(28);
    let grid = Grid {
        lat0: i16at(32),
        lon0: i16at(34),
        rows: u16at(36),
        cols: u16at(38),
    };

    let bucket_off = u32at(40) as usize;
    let airport_off = u32at(44) as usize;
    let airspace_off = u32at(48) as usize;
    let ring_off = u32at(52) as usize;
    let vertex_off = u32at(56) as usize;

    ensure!(bucket_off == HEADER_LEN, "buckets do not follow the header");
    ensure!(
        airport_off == bucket_off + grid.cells() * BUCKET_LEN,
        "airport section does not follow the grid"
    );
    ensure!(
        airspace_off == airport_off + airports as usize * AIRPORT_LEN,
        "airspace section does not follow the airports"
    );
    ensure!(
        ring_off == airspace_off + airspace as usize * AIRSPACE_LEN,
        "ring section does not follow the airspace"
    );
    ensure!(
        vertex_off == ring_off + rings as usize * RING_LEN,
        "vertex section does not follow the rings"
    );
    ensure!(
        bytes.len() == vertex_off + vertices as usize * VERTEX_LEN,
        "file is {} bytes, sections account for {}",
        bytes.len(),
        vertex_off + vertices as usize * VERTEX_LEN
    );

    Ok(Summary {
        version,
        effective_days: u32at(12),
        airports,
        airspace,
        rings,
        vertices,
        grid,
        bytes: bytes.len(),
    })
}

/// Every airport in the file, in stored order. For verification, not for the display.
pub fn read_airports(bytes: &[u8]) -> Result<Vec<Airport>> {
    let summary = read_summary(bytes)?;
    let base = HEADER_LEN + summary.grid.cells() * BUCKET_LEN;
    let mut out = Vec::with_capacity(summary.airports as usize);
    for i in 0..summary.airports as usize {
        let r = &bytes[base + i * AIRPORT_LEN..base + (i + 1) * AIRPORT_LEN];
        out.push(Airport {
            lat_e6: i32::from_le_bytes(r[0..4].try_into()?),
            lon_e6: i32::from_le_bytes(r[4..8].try_into()?),
            label: String::from_utf8_lossy(&r[8..16])
                .trim_end_matches('\0')
                .to_string(),
            elevation_ft: i16::from_le_bytes(r[16..18].try_into()?),
            runway_ft: u16::from_le_bytes(r[18..20].try_into()?),
            kind: match r[20] {
                0 => Kind::Large,
                1 => Kind::Medium,
                2 => Kind::Small,
                3 => Kind::Heliport,
                _ => Kind::Seaplane,
            },
            tier: match r[21] {
                0 => Tier::Major,
                1 => Tier::Paved,
                2 => Tier::Minor,
                _ => Tier::Heliport,
            },
            flags: r[22],
        });
    }
    Ok(out)
}

/// Class and vertex count of every airspace record, in stored order. For verification.
pub fn read_airspace(bytes: &[u8]) -> Result<Vec<(Class, u32)>> {
    let summary = read_summary(bytes)?;
    let base = HEADER_LEN
        + summary.grid.cells() * BUCKET_LEN
        + summary.airports as usize * AIRPORT_LEN;
    let ring_base = base + summary.airspace as usize * AIRSPACE_LEN;

    let mut out = Vec::with_capacity(summary.airspace as usize);
    for i in 0..summary.airspace as usize {
        let r = &bytes[base + i * AIRSPACE_LEN..base + (i + 1) * AIRSPACE_LEN];
        let class = match r[22] {
            0 => Class::B,
            1 => Class::C,
            _ => Class::D,
        };
        let ring_first = u32::from_le_bytes(r[16..20].try_into()?) as usize;
        let ring_count = u16::from_le_bytes(r[20..22].try_into()?) as usize;

        let mut vertices = 0u32;
        for ring in ring_first..ring_first + ring_count {
            let o = ring_base + ring * RING_LEN;
            vertices += u32::from_le_bytes(bytes[o + 4..o + 8].try_into()?);
        }
        out.push((class, vertices));
    }
    Ok(out)
}

/// The per-cell airport index, as `(first, count)`.
pub fn read_buckets(bytes: &[u8]) -> Result<Vec<(u32, u32)>> {
    let summary = read_summary(bytes)?;
    let mut out = Vec::with_capacity(summary.grid.cells());
    for i in 0..summary.grid.cells() {
        let o = HEADER_LEN + i * BUCKET_LEN;
        out.push((
            u32::from_le_bytes(bytes[o..o + 4].try_into()?),
            u32::from_le_bytes(bytes[o + 4..o + 8].try_into()?),
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn airport(label: &str, lat: f64, lon: f64, tier: Tier) -> Airport {
        Airport {
            lat_e6: (lat * 1e6) as i32,
            lon_e6: (lon * 1e6) as i32,
            label: label.into(),
            elevation_ft: 187,
            runway_ft: 5999,
            kind: Kind::Medium,
            tier,
            flags: FLAG_HARD_SURFACE | FLAG_LIGHTED,
        }
    }

    fn chart() -> Chart {
        Chart {
            effective_days: 20_643,
            airports: vec![
                airport("MMU", 40.799, -74.415, Tier::Major),
                airport("EWR", 40.692, -74.169, Tier::Major),
                airport("LAX", 33.942, -118.408, Tier::Major),
                airport("06N", 41.431, -74.392, Tier::Paved),
            ],
            airspace: vec![Airspace {
                class: Class::B,
                label: "EWR".into(),
                lower_ft: 0,
                upper_ft: 7000,
                flags: FLAG_LOWER_SURFACE,
                rings: vec![
                    vec![
                        (40_600_000, -74_300_000),
                        (40_800_000, -74_300_000),
                        (40_800_000, -74_000_000),
                        (40_600_000, -74_000_000),
                    ],
                    vec![
                        (40_650_000, -74_250_000),
                        (40_700_000, -74_250_000),
                        (40_700_000, -74_200_000),
                    ],
                ],
            }],
        }
    }

    #[test]
    fn a_written_file_reads_back_with_every_section_where_it_claims() {
        let bytes = write(&chart());
        let s = read_summary(&bytes).expect("sections must line up");
        assert_eq!(s.version, VERSION);
        assert_eq!(s.airports, 4);
        assert_eq!(s.airspace, 1);
        assert_eq!(s.rings, 2);
        assert_eq!(s.vertices, 7);
        assert_eq!(s.effective_days, 20_643);
        assert_eq!(s.bytes, bytes.len());
    }

    #[test]
    fn every_airport_survives_the_round_trip() {
        let source = chart();
        let bytes = write(&source);
        let back = read_airports(&bytes).unwrap();
        assert_eq!(back.len(), source.airports.len());

        for original in &source.airports {
            let found = back
                .iter()
                .find(|a| a.label == original.label)
                .unwrap_or_else(|| panic!("{} did not survive", original.label));
            assert_eq!(found.lat_e6, original.lat_e6);
            assert_eq!(found.lon_e6, original.lon_e6);
            assert_eq!(found.elevation_ft, original.elevation_ft);
            assert_eq!(found.runway_ft, original.runway_ft);
            assert_eq!(found.tier, original.tier);
            assert_eq!(found.flags, original.flags);
        }
    }

    #[test]
    fn the_grid_puts_every_airport_in_the_cell_that_claims_it() {
        // The index is the one part of this file that can be wrong without looking wrong: a broken
        // bucket means airports quietly missing from the view rather than a load failure.
        let bytes = write(&chart());
        let summary = read_summary(&bytes).unwrap();
        let buckets = read_buckets(&bytes).unwrap();
        let airports = read_airports(&bytes).unwrap();

        let mut seen = 0usize;
        for (cell, (first, count)) in buckets.iter().enumerate() {
            for i in 0..*count as usize {
                let a = &airports[*first as usize + i];
                assert_eq!(
                    summary.grid.cell(a.lat_e6, a.lon_e6),
                    cell,
                    "{} is stored in cell {cell} but belongs elsewhere",
                    a.label
                );
                seen += 1;
            }
        }
        assert_eq!(seen, airports.len(), "some airports are in no cell at all");
    }

    #[test]
    fn western_longitudes_do_not_round_toward_zero() {
        // The bug this guards is the reason `div_floor` exists. With plain `/`, 74.5W and 74.0W
        // land in the same cell and the cell west of them holds half of what it should — an index
        // that is subtly wrong across the whole of the United States and correct in Europe.
        let grid = Grid { lat0: 24, lon0: -125, rows: 26, cols: 60 };
        let west = grid.cell(40_000_000, -74_500_000);
        let east = grid.cell(40_000_000, -74_000_000);
        assert_ne!(west, east, "-74.5 and -74.0 are a degree apart");
        assert_eq!(east - west, 1, "and adjacent");
    }

    #[test]
    fn the_grid_covers_the_data_it_was_built_from() {
        let source = chart();
        let grid = Grid::covering(source.airports.iter().map(|a| (a.lat_e6, a.lon_e6)));
        for a in &source.airports {
            let cell = grid.cell(a.lat_e6, a.lon_e6);
            assert!(cell < grid.cells(), "{} fell outside the grid", a.label);
        }
        // LAX at 118.4W and MMU at 74.4W must not share a cell.
        assert_ne!(
            grid.cell(33_942_000, -118_408_000),
            grid.cell(40_799_000, -74_415_000)
        );
    }

    #[test]
    fn the_build_is_reproducible() {
        // The same input has to give a byte-identical file. Otherwise every rebuild shows as a
        // data change in the diff and the commit stops carrying information.
        assert_eq!(write(&chart()), write(&chart()));
    }

    #[test]
    fn a_truncated_or_foreign_file_is_rejected_rather_than_misread() {
        let bytes = write(&chart());
        assert!(read_summary(&bytes[..HEADER_LEN - 1]).is_err(), "short header");
        assert!(read_summary(&bytes[..bytes.len() - 8]).is_err(), "truncated body");

        let mut wrong = bytes.clone();
        wrong[0] = b'X';
        assert!(read_summary(&wrong).is_err(), "bad magic");

        let mut future = bytes;
        future[8] = 99;
        assert!(read_summary(&future).is_err(), "unknown version");
    }

    #[test]
    fn labels_are_padded_not_run_together() {
        let bytes = write(&chart());
        let back = read_airports(&bytes).unwrap();
        assert!(back.iter().any(|a| a.label == "MMU"));
        assert!(back.iter().any(|a| a.label == "06N"));
        assert!(
            back.iter().all(|a| a.label.len() <= LABEL_LEN),
            "a label ran into the next field"
        );
    }
}
