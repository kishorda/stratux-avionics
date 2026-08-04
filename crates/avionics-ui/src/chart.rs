//! Reading the airport and airspace file built by `tools/chartdata`.
//!
//! The file is fixed-layout little-endian, so loading is a read and a header check — no parsing,
//! no allocation per record, and nothing on the aircraft that has to understand CSV or GeoJSON.
//! Records are decoded on demand out of the byte buffer, which is why a query allocates only the
//! handful of results it returns.
//!
//! ```text
//!   header      96 B   magic, version, counts, section offsets, grid, effective date
//!   buckets      8 B   per 1x1 degree cell: first airport, airport count
//!   airports    48 B   position, label, ICAO station, elevation, runway, kind, tier,
//!                      flags, and ranges into the runway, frequency and string tables
//!   airspace    40 B   bounding box, ring range, class, flags, lower/upper, label
//!   rings        8 B   first vertex, vertex count
//!   vertices     8 B   latitude and longitude, i32 micro-degrees
//!   runways      4 B   heading and length — one per distinct orientation
//!   frequencies  8 B   kHz and kind
//!   strings      -     airport names, UTF-8, addressed by offset and length
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
const VERSION: u16 = 3;

const HEADER_LEN: usize = 96;
const BUCKET_LEN: usize = 8;
const AIRPORT_LEN: usize = 48;
const AIRSPACE_LEN: usize = 40;
const RING_LEN: usize = 8;
const VERTEX_LEN: usize = 8;
const RUNWAY_LEN: usize = 4;
const FREQUENCY_LEN: usize = 8;
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
/// its own tier rather than merely the least important: 83 heliports reach the built file within
/// 10 nm of downtown Los Angeles, against 5 fixed-wing fields. They are carried and never drawn
/// by default.
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

/// What a frequency is for. Ordered by how much a pilot wants it on a card with four lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FreqKind {
    Ctaf,
    Tower,
    Ground,
    Atis,
    Awos,
    Unicom,
    Approach,
    Departure,
    /// Airport advisory. At many fields the only published number.
    Advisory,
    Clearance,
    /// ARTCC.
    Center,
    /// Something the builder could not name. Carried, but not shown on the card: a number with
    /// no label invites tuning a radio to it without knowing who is on the other end.
    Other,
}

impl FreqKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Ctaf => "CTAF",
            Self::Tower => "TWR",
            Self::Ground => "GND",
            Self::Atis => "ATIS",
            Self::Awos => "AWOS",
            Self::Unicom => "UNI",
            Self::Approach => "APP",
            Self::Departure => "DEP",
            Self::Advisory => "A/D",
            Self::Clearance => "CLR",
            Self::Center => "CTR",
            Self::Other => "",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frequency {
    /// Kilohertz. Stored as an integer because 121.975 is a real 25 kHz channel and a float would
    /// format it as 121.97 or 121.98 depending on which way it landed.
    pub khz: u32,
    pub kind: FreqKind,
}

impl Frequency {
    /// `"121.975"`, `"118.10"` — trailing zeros trimmed to at least one decimal.
    pub fn mhz_text(&self) -> String {
        let mut s = format!("{:.3}", self.khz as f64 / 1000.0);
        while s.ends_with('0') && !s.ends_with(".0") {
            s.pop();
        }
        s
    }
}

/// One runway orientation. Parallel and reciprocal runways are already collapsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Runway {
    /// Degrees, from the runway identifier: 10-degree granularity.
    pub heading_deg: u16,
    pub length_ft: u16,
}

impl Runway {
    /// `"05/23"` — the pair of numbers painted on the ends.
    pub fn designator(&self) -> String {
        let a = ((self.heading_deg / 10) % 36).max(1);
        let b = if a > 18 { a - 18 } else { a + 18 };
        format!("{a:02}/{b:02}")
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Airport {
    /// Index into the file's airport table, so a tap can be remembered as a number.
    pub index: u32,
    pub position: LatLon,
    label: [u8; LABEL_LEN],
    station: [u8; LABEL_LEN],
    pub elevation_ft: i16,
    /// Longest hard-surface runway in feet, 0 when there is none.
    pub runway_ft: u16,
    pub kind: Kind,
    pub tier: Tier,
    pub flags: u8,
    runway_first: u32,
    runway_count: u8,
    freq_first: u32,
    freq_count: u8,
    name_off: u32,
    name_len: u8,
}

impl Airport {
    pub fn label(&self) -> &str {
        let end = self.label.iter().position(|b| *b == 0).unwrap_or(LABEL_LEN);
        std::str::from_utf8(&self.label[..end]).unwrap_or("")
    }

    /// ICAO-style identifier, e.g. `KMMU`, or empty when the field has none.
    ///
    /// **The join key for weather.** METARs arrive keyed by station and [`Airport::label`] is the
    /// short name the symbol carries, which is deliberately not the same thing.
    pub fn station(&self) -> &str {
        let end = self
            .station
            .iter()
            .position(|b| *b == 0)
            .unwrap_or(LABEL_LEN);
        std::str::from_utf8(&self.station[..end]).unwrap_or("")
    }

    pub fn hard_surface(&self) -> bool {
        self.flags & FLAG_HARD_SURFACE != 0
    }

    pub fn lighted(&self) -> bool {
        self.flags & FLAG_LIGHTED != 0
    }

    pub fn runway_count(&self) -> usize {
        self.runway_count as usize
    }

    pub fn frequency_count(&self) -> usize {
        self.freq_count as usize
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
        let end = self.label.iter().position(|b| *b == 0).unwrap_or(LABEL_LEN);
        std::str::from_utf8(&self.label[..end]).unwrap_or("")
    }

    pub fn lower_is_surface(&self) -> bool {
        self.flags & FLAG_LOWER_SURFACE != 0
    }

    /// Floor in feet MSL, with a surface floor reported as 0.
    ///
    /// `lower_ft` carries no information when the surface flag is set, so reading it directly is a
    /// way to sort a surface area into the wrong place.
    pub fn floor_ft(&self) -> i32 {
        if self.lower_is_surface() {
            0
        } else {
            self.lower_ft
        }
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

/// The fixed 96-byte header, validated before any record is touched.
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
    runway_off: usize,
    freq_off: usize,
    string_off: usize,
    string_len: usize,
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
        let runway_count = u32at(32);
        let freq_count = u32at(36);
        let string_len = u32at(40);
        let grid = Grid {
            lat0: i16at(48),
            lon0: i16at(50),
            rows: u16at(52),
            cols: u16at(54),
        };
        ensure!(grid.rows > 0 && grid.cols > 0, "empty grid");

        // Every section must start exactly where the previous one ended. Checked in one walk so a
        // new section cannot be added later without being covered.
        let lengths = [
            ("buckets", grid.cells() * BUCKET_LEN),
            ("airports", airport_count as usize * AIRPORT_LEN),
            ("airspace", airspace_count as usize * AIRSPACE_LEN),
            ("rings", ring_count as usize * RING_LEN),
            ("vertices", vertex_count as usize * VERTEX_LEN),
            ("runways", runway_count as usize * RUNWAY_LEN),
            ("frequencies", freq_count as usize * FREQUENCY_LEN),
            ("strings", string_len as usize),
        ];
        let mut offsets = [0usize; 8];
        let mut cursor = HEADER_LEN;
        for (index, (name, len)) in lengths.iter().enumerate() {
            let stated = u32at(56 + index * 4) as usize;
            ensure!(
                stated == cursor,
                "{name} section is at {stated}, expected {cursor}"
            );
            offsets[index] = stated;
            cursor += len;
        }
        ensure!(
            bytes.len() == cursor,
            "file is {} bytes, sections account for {cursor}",
            bytes.len()
        );

        Ok(Self {
            effective_days: u32at(12),
            airport_count,
            airspace_count,
            grid,
            bucket_off: offsets[0],
            airport_off: offsets[1],
            airspace_off: offsets[2],
            ring_off: offsets[3],
            vertex_off: offsets[4],
            runway_off: offsets[5],
            freq_off: offsets[6],
            string_off: offsets[7],
            string_len: string_len as usize,
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
    runway_off: usize,
    freq_off: usize,
    string_off: usize,
    string_len: usize,
    /// `(station, airport index)`, sorted by station.
    ///
    /// Built once at load because the alternative is a linear scan of 18,108 records per lookup,
    /// and the weather page wants one lookup per report per frame. 2,335 entries — the airports
    /// that actually have an ICAO code — so this is about 40 KB and a binary search.
    by_station: Vec<(String, u32)>,
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
        let mut chart = Self {
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
            runway_off: header.runway_off,
            freq_off: header.freq_off,
            string_off: header.string_off,
            string_len: header.string_len,
            by_station: Vec::new(),
        };

        chart.by_station = (0..chart.airport_count as usize)
            .filter_map(|i| chart.airport_at(i))
            .filter(|a| !a.station().is_empty())
            .map(|a| (a.station().to_string(), a.index))
            .collect();
        chart.by_station.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(chart)
    }

    /// The airport with this ICAO station identifier.
    ///
    /// The reverse of [`Airport::station`], for turning a METAR's `KMMU` back into a position.
    pub fn airport_by_station(&self, station: &str) -> Option<Airport> {
        let i = self
            .by_station
            .binary_search_by(|(s, _)| s.as_str().cmp(station))
            .ok()?;
        self.airport_at(self.by_station[i].1 as usize)
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

    /// Decode one airport record. Cheap enough to do per query result rather than per file.
    pub fn airport_at(&self, index: usize) -> Option<Airport> {
        if index >= self.airport_count as usize {
            return None;
        }
        let o = self.airport_off + index * AIRPORT_LEN;
        let mut label = [0u8; LABEL_LEN];
        label.copy_from_slice(&self.bytes[o + 8..o + 16]);
        let mut station = [0u8; LABEL_LEN];
        station.copy_from_slice(&self.bytes[o + 16..o + 24]);
        Some(Airport {
            index: index as u32,
            position: LatLon::new(self.i32at(o) as f64 / 1e6, self.i32at(o + 4) as f64 / 1e6),
            label,
            station,
            elevation_ft: i16::from_le_bytes([self.bytes[o + 24], self.bytes[o + 25]]),
            runway_ft: u16::from_le_bytes([self.bytes[o + 26], self.bytes[o + 27]]),
            kind: match self.bytes[o + 28] {
                0 => Kind::Large,
                1 => Kind::Medium,
                2 => Kind::Small,
                3 => Kind::Heliport,
                _ => Kind::Seaplane,
            },
            tier: match self.bytes[o + 29] {
                0 => Tier::Major,
                1 => Tier::Paved,
                2 => Tier::Minor,
                _ => Tier::Heliport,
            },
            flags: self.bytes[o + 30],
            runway_count: self.bytes[o + 31],
            runway_first: self.u32at(o + 32),
            freq_first: self.u32at(o + 36),
            name_off: self.u32at(o + 40),
            name_len: self.bytes[o + 44],
            freq_count: self.bytes[o + 45],
        })
    }

    /// The airport's full name, e.g. `"Morristown Municipal Airport"`.
    ///
    /// Borrowed straight out of the file. Returns empty rather than failing when the span is out
    /// of range, because a card with a missing name is a great deal better than a panic in a
    /// render loop.
    pub fn name(&self, airport: &Airport) -> &str {
        let start = self.string_off + airport.name_off as usize;
        let end = start + airport.name_len as usize;
        if airport.name_off as usize + airport.name_len as usize > self.string_len
            || end > self.bytes.len()
        {
            return "";
        }
        std::str::from_utf8(&self.bytes[start..end]).unwrap_or("")
    }

    /// Runway orientations, longest first. Parallel and reciprocal runways are already collapsed.
    pub fn runways(&self, airport: &Airport) -> Vec<Runway> {
        let first = airport.runway_first as usize;
        (0..airport.runway_count as usize)
            .filter_map(|k| {
                let o = self.runway_off + (first + k) * RUNWAY_LEN;
                (o + RUNWAY_LEN <= self.freq_off).then(|| Runway {
                    heading_deg: u16::from_le_bytes([self.bytes[o], self.bytes[o + 1]]),
                    length_ft: u16::from_le_bytes([self.bytes[o + 2], self.bytes[o + 3]]),
                })
            })
            .collect()
    }

    /// Communication frequencies, most useful first. Empty for the 82% of fields that have none.
    pub fn frequencies(&self, airport: &Airport) -> Vec<Frequency> {
        let first = airport.freq_first as usize;
        (0..airport.freq_count as usize)
            .filter_map(|k| {
                let o = self.freq_off + (first + k) * FREQUENCY_LEN;
                (o + FREQUENCY_LEN <= self.string_off).then(|| Frequency {
                    khz: self.u32at(o),
                    kind: match self.bytes[o + 4] {
                        0 => FreqKind::Ctaf,
                        1 => FreqKind::Tower,
                        2 => FreqKind::Ground,
                        3 => FreqKind::Atis,
                        4 => FreqKind::Awos,
                        5 => FreqKind::Unicom,
                        6 => FreqKind::Approach,
                        7 => FreqKind::Departure,
                        8 => FreqKind::Advisory,
                        9 => FreqKind::Clearance,
                        10 => FreqKind::Center,
                        _ => FreqKind::Other,
                    },
                })
            })
            .collect()
    }

    fn airspace_at_index(&self, index: usize) -> Airspace {
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
                    let Some(airport) = self.airport_at(index) else {
                        continue;
                    };
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
            .map(|i| self.airspace_at_index(i))
            .filter(|a| a.bounds.overlaps(bounds))
            .collect()
    }

    /// Every airspace volume whose boundary encloses `point`, lowest floor first.
    ///
    /// Lowest first because that is the order you would meet them climbing, and because the number
    /// a pilot wants from a stack of Class B shelves is the bottom of the one above them.
    ///
    /// # Even-odd, and why the rings are not split into outer and holes
    ///
    /// The build flattens a polygon's rings, so this cannot tell an exterior ring from a hole. It
    /// does not need to: counting crossings over *all* of a volume's rings and taking the parity
    /// gives the right answer either way — a point inside a hole crosses the outer ring once and
    /// the hole once, which is even, which is outside. Only 2 of 1,408 volumes have more than one
    /// ring, but the rule costs nothing and is correct rather than nearly correct.
    pub fn airspace_at(&self, point: LatLon) -> Vec<Airspace> {
        let lat_e6 = (point.lat * 1e6) as i32;
        let lon_e6 = (point.lon * 1e6) as i32;
        let probe = Bounds {
            lat_min: lat_e6,
            lat_max: lat_e6,
            lon_min: lon_e6,
            lon_max: lon_e6,
        };

        let mut out: Vec<Airspace> = (0..self.airspace_count as usize)
            .map(|i| self.airspace_at_index(i))
            // The bounding box is free and rejects almost everything before any arithmetic.
            .filter(|a| a.bounds.overlaps(&probe))
            .filter(|a| self.encloses(a, point))
            .collect();

        out.sort_by_key(|a| (a.floor_ft(), a.upper_ft));
        out
    }

    fn encloses(&self, space: &Airspace, point: LatLon) -> bool {
        let mut crossings = 0usize;
        for ring in 0..space.ring_count() {
            let points: Vec<LatLon> = self.ring(space, ring).collect();
            if points.len() < 3 {
                continue;
            }
            // The rings are stored open — the closing vertex was dropped at build time — so the
            // edge list has to wrap from the last point back to the first.
            for i in 0..points.len() {
                let a = points[i];
                let b = points[(i + 1) % points.len()];
                // Half-open in latitude: an edge counts if it spans the ray's latitude with its
                // lower end included and its upper end excluded. That is what stops a vertex
                // sitting exactly on the ray from being counted twice, which is the classic way
                // this test reports "outside" for a point that is plainly inside.
                if (a.lat > point.lat) == (b.lat > point.lat) {
                    continue;
                }
                let t = (point.lat - a.lat) / (b.lat - a.lat);
                if point.lon < a.lon + t * (b.lon - a.lon) {
                    crossings += 1;
                }
            }
        }
        crossings % 2 == 1
    }

    /// Vertices of one ring of an airspace volume, as latitude/longitude degrees.
    ///
    /// The ring is **open**: the closing point was dropped when the file was built, because a
    /// renderer closes the path itself. Iterating allocates nothing.
    pub fn ring(&self, space: &Airspace, ring: usize) -> RingIter<'_> {
        if ring >= space.ring_count as usize {
            return RingIter {
                chart: self,
                next: 0,
                end: 0,
            };
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
        assert_eq!(chart.airport_count(), 18_108);
        assert_eq!(chart.airspace_count(), 1_408);
        // 2026-07-09, as the FAA layer reported it when the file was built.
        assert_eq!(chart.effective_days(), 20_643);
    }

    #[test]
    fn morristowns_full_record_is_readable() {
        // One airport checked end to end against what the source says, so a shift in any of the
        // three variable-length tables shows up as wrong content rather than as a load failure.
        let Some(chart) = conus() else { return };
        let bounds = bounds_around(LatLon::new(40.7784, -74.3343), 10.0);
        let mmu = chart
            .airports_in(&bounds, Tier::Minor)
            .into_iter()
            .find(|a| a.label() == "MMU")
            .expect("MMU");

        // The FAA's own name, which is shorter than OurAirports' was.
        assert_eq!(chart.name(&mmu), "Morristown Muni");
        // The join key for weather, and deliberately not the label the symbol shows.
        assert_eq!(mmu.station(), "KMMU");
        assert_eq!(mmu.label(), "MMU");
        assert!(
            mmu.elevation_ft > 100 && mmu.elevation_ft < 400,
            "{}",
            mmu.elevation_ft
        );

        let runways = chart.runways(&mmu);
        assert!(!runways.is_empty(), "MMU has runways");
        // Longest first, and 5/23 is the long one.
        assert!(runways[0].length_ft >= 5000, "{:?}", runways[0]);
        assert!(
            runways.iter().any(|r| r.designator() == "05/23"),
            "got {:?}",
            runways.iter().map(|r| r.designator()).collect::<Vec<_>>()
        );

        let freqs = chart.frequencies(&mmu);
        assert!(!freqs.is_empty(), "MMU is towered and has frequencies");
        assert!(
            freqs.iter().any(|f| f.kind == FreqKind::Atis),
            "got {:?}",
            freqs
                .iter()
                .map(|f| (f.kind, f.mhz_text()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_field_with_nothing_attached_reads_back_empty() {
        // The failure mode of the variable-length tables is borrowing a neighbour's data, which
        // looks entirely plausible. Every airport with a zero count must read back empty.
        let Some(chart) = conus() else { return };
        let bounds = bounds_around(LatLon::new(40.7784, -74.3343), 20.0);
        let mut checked = 0usize;
        for airport in chart.airports_in(&bounds, Tier::Heliport) {
            if airport.frequency_count() == 0 {
                assert!(
                    chart.frequencies(&airport).is_empty(),
                    "{}",
                    airport.label()
                );
                checked += 1;
            }
            if airport.runway_count() == 0 {
                assert!(chart.runways(&airport).is_empty(), "{}", airport.label());
            }
            assert_eq!(chart.runways(&airport).len(), airport.runway_count());
            assert_eq!(chart.frequencies(&airport).len(), airport.frequency_count());
        }
        assert!(
            checked > 0,
            "expected some fields with no frequencies near New York"
        );
    }

    #[test]
    fn every_name_frequency_and_runway_in_the_file_is_reachable_and_sane() {
        // Walks all 20,736 records rather than a sample. A span that runs past the end of a table
        // is the one bug this format can have that no single spot check would find.
        let Some(chart) = conus() else { return };
        let mut names = 0usize;
        let mut freqs = 0usize;
        let mut runways = 0usize;
        for index in 0..chart.airport_count() {
            let airport = chart.airport_at(index).expect("index in range");
            let name = chart.name(&airport);
            assert!(
                name.len() <= 40,
                "{} has a {}-byte name",
                airport.label(),
                name.len()
            );
            if !name.is_empty() {
                names += 1;
            }
            for f in chart.frequencies(&airport) {
                // Everything in the file is tunable on the radios a light aircraft carries: the
                // VHF comm band, plus ATIS and AWOS on a navaid voice channel below it. Military
                // UHF is filtered out at build time — see `tunable` in the builder.
                let comm = (118_000..=136_975).contains(&f.khz);
                let navaid_voice = (108_000..=117_975).contains(&f.khz)
                    && matches!(f.kind, FreqKind::Atis | FreqKind::Awos);
                assert!(
                    comm || navaid_voice,
                    "{} has {} kHz as {:?}, which no radio here can tune",
                    airport.label(),
                    f.khz,
                    f.kind
                );
                freqs += 1;
            }
            for r in chart.runways(&airport) {
                assert!(
                    r.heading_deg < 360,
                    "{} has {} deg",
                    airport.label(),
                    r.heading_deg
                );
                runways += 1;
            }
        }
        assert_eq!(
            freqs, 10_847,
            "every frequency in the file should be reachable"
        );
        assert_eq!(
            runways, 14_926,
            "every runway orientation should be reachable"
        );
        assert_eq!(names, 18_108, "every airport should carry a name");
    }

    #[test]
    fn station_identifiers_are_real_icao_codes_rather_than_plentiful() {
        // Coverage here went *down* on purpose, from 97% to 13%, and that was the point of moving
        // to the FAA source. The old field was OurAirports' `gps_code`, which for most airports is
        // the local code repeated — `ID15`, `WN43` — strings that cannot match a METAR and never
        // will. Against the stations actually reporting across CONUS the new set matched 362 of
        // 400 and the old one 334, using 2,335 strings instead of 20,056.
        //
        // So the assertion is about shape and about the fields that matter, not about volume.
        let Some(chart) = conus() else { return };
        let mut with_station = 0usize;
        let mut major = 0usize;
        let mut major_with_station = 0usize;
        for index in 0..chart.airport_count() {
            let airport = chart.airport_at(index).expect("index in range");
            let station = airport.station();
            if !station.is_empty() {
                assert_eq!(
                    station.len(),
                    4,
                    "{station} is not a four-character identifier"
                );
                assert!(
                    station.chars().all(|c| c.is_ascii_alphanumeric()),
                    "{station} is not an identifier"
                );
                with_station += 1;
            }
            if airport.tier == Tier::Major {
                major += 1;
                if !station.is_empty() {
                    major_with_station += 1;
                }
            }
        }
        assert!(with_station > 2_000, "only {with_station} real ICAO codes");
        // Not all of them: 12% of Major-tier fields are military or civil aerodromes the FAA has
        // not assigned an ICAO code to, and inventing one is exactly what this replaced.
        assert!(
            major_with_station * 100 / major >= 85,
            "only {major_with_station} of {major} major fields have a station"
        );
    }

    #[test]
    fn an_index_past_the_end_yields_nothing() {
        let Some(chart) = conus() else { return };
        assert!(chart.airport_at(chart.airport_count()).is_none());
        assert!(chart.airport_at(usize::MAX).is_none());
    }

    #[test]
    fn frequencies_format_without_losing_the_last_digit() {
        // 121.975 is a real 25 kHz channel. Formatting it as 121.97 or 121.98 would put a pilot
        // one click off, which is the whole reason the file stores kilohertz.
        for (khz, want) in [
            (121_975, "121.975"),
            (118_100, "118.1"),
            (122_800, "122.8"),
            (124_250, "124.25"),
            (120_000, "120.0"),
        ] {
            let f = Frequency {
                khz,
                kind: FreqKind::Ctaf,
            };
            assert_eq!(f.mhz_text(), want, "{khz} kHz");
        }
    }

    #[test]
    fn runway_designators_read_the_way_they_are_painted() {
        for (heading, want) in [
            (50u16, "05/23"),
            (230, "23/05"),
            (90, "09/27"),
            (270, "27/09"),
            (180, "18/36"),
            (0, "01/19"),
        ] {
            let r = Runway {
                heading_deg: heading,
                length_ft: 5000,
            };
            assert_eq!(r.designator(), want, "{heading} degrees");
        }
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
        assert!(
            (mmu.position.lat - 40.799).abs() < 0.01,
            "{}",
            mmu.position.lat
        );
        assert!(
            (mmu.position.lon + 74.415).abs() < 0.01,
            "{}",
            mmu.position.lon
        );
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
        assert!(
            worst.0 > 0,
            "no airports anywhere, which means the query is broken"
        );
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
        assert!(
            points.len() >= 3,
            "a ring needs three points to enclose anything"
        );
        assert_ne!(
            points.first().map(|p| (p.lat, p.lon)),
            points.last().map(|p| (p.lat, p.lon)),
            "the closing point should have been dropped at build time"
        );
        assert!(
            b.upper_ft > 1000,
            "a Class B has a real ceiling: {}",
            b.upper_ft
        );
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
                assert!(
                    points.len() >= 3,
                    "{} ring {ring} is degenerate",
                    space.label()
                );
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
        assert_eq!(
            vertices, 128_479,
            "every vertex in the file should be reachable"
        );
    }

    #[test]
    fn a_point_over_newark_is_inside_the_new_york_class_b() {
        // The end-to-end case, against the shipped geometry rather than a synthetic square.
        let Some(chart) = conus() else { return };
        let over_ewr = chart.airspace_at(LatLon::new(40.6895, -74.1745));
        assert!(
            !over_ewr.is_empty(),
            "Newark sits under the New York Class B"
        );
        assert!(
            over_ewr.iter().any(|a| a.class == Class::B),
            "got {:?}",
            over_ewr
                .iter()
                .map(|a| (a.class, a.label()))
                .collect::<Vec<_>>()
        );
        // Lowest floor first: that is the order you meet them climbing.
        let floors: Vec<i32> = over_ewr.iter().map(|a| a.floor_ft()).collect();
        assert!(
            floors.windows(2).all(|w| w[0] <= w[1]),
            "not sorted: {floors:?}"
        );
    }

    #[test]
    fn a_point_in_the_atlantic_is_inside_nothing() {
        let Some(chart) = conus() else { return };
        assert!(chart.airspace_at(LatLon::new(38.0, -70.0)).is_empty());
        // And a point inside the bounding box of the New York Class B but well outside its
        // boundary — the box test alone would have said yes.
        let boxed = chart.airspace_at(LatLon::new(41.15, -73.35));
        assert!(
            boxed.iter().all(|a| a.class != Class::B),
            "the bounding box is not the boundary"
        );
    }

    #[test]
    fn every_volume_contains_a_point_taken_from_its_own_boundary() {
        // Walks all 1,408 volumes. A point nudged inward from a boundary vertex must test inside;
        // if the ray casting were wrong about vertices or about the wrap from the last point back
        // to the first, this is where it shows.
        let Some(chart) = conus() else { return };
        let all = chart.airspace_in(&Bounds {
            lat_min: i32::MIN,
            lat_max: i32::MAX,
            lon_min: i32::MIN,
            lon_max: i32::MAX,
        });
        let mut checked = 0usize;
        for space in &all {
            let ring: Vec<LatLon> = chart.ring(space, 0).collect();
            if ring.len() < 3 {
                continue;
            }
            // Centroid of the first ring. Convex enough for these shapes that it lands inside;
            // where it does not, the volume is skipped rather than the test being weakened.
            let n = ring.len() as f64;
            let c = LatLon::new(
                ring.iter().map(|p| p.lat).sum::<f64>() / n,
                ring.iter().map(|p| p.lon).sum::<f64>() / n,
            );
            if !space.bounds.overlaps(&Bounds {
                lat_min: (c.lat * 1e6) as i32,
                lat_max: (c.lat * 1e6) as i32,
                lon_min: (c.lon * 1e6) as i32,
                lon_max: (c.lon * 1e6) as i32,
            }) {
                continue;
            }
            if chart.encloses(space, c) {
                checked += 1;
            }
        }
        assert!(
            checked * 100 / all.len() >= 90,
            "only {checked} of {} volumes contained their own centroid",
            all.len()
        );
    }

    #[test]
    fn a_ray_through_a_vertex_is_counted_once() {
        // The classic ray-casting bug. A horizontal ray that leaves a point and passes exactly
        // through a polygon vertex counts two edges instead of one and reports "outside" for a
        // point that is plainly inside. The half-open latitude rule is what prevents it, and a
        // diamond puts a vertex on the ray by construction.
        let Some(chart) = conus() else { return };
        let all = chart.airspace_in(&Bounds {
            lat_min: 40_000_000,
            lat_max: 41_000_000,
            lon_min: -75_000_000,
            lon_max: -74_000_000,
        });
        let space = all.first().expect("something near New York");
        let ring: Vec<LatLon> = chart.ring(space, 0).collect();

        // Probe at the exact latitude of every vertex, from the west. Whatever the answer, it must
        // agree with a probe a hair above and below — no vertex may flip the parity on its own.
        for v in ring.iter().take(40) {
            let here = chart.encloses(space, LatLon::new(v.lat, v.lon - 1e-7));
            let above = chart.encloses(space, LatLon::new(v.lat + 2e-7, v.lon - 1e-7));
            let below = chart.encloses(space, LatLon::new(v.lat - 2e-7, v.lon - 1e-7));
            assert!(
                here == above || here == below,
                "vertex at {:.6},{:.6} flips the test on its own",
                v.lat,
                v.lon
            );
        }
    }

    #[test]
    fn a_surface_floor_sorts_as_zero_rather_than_by_a_meaningless_number() {
        let Some(chart) = conus() else { return };
        let all = chart.airspace_in(&Bounds {
            lat_min: i32::MIN,
            lat_max: i32::MAX,
            lon_min: i32::MIN,
            lon_max: i32::MAX,
        });
        let surface: Vec<_> = all.iter().filter(|a| a.lower_is_surface()).collect();
        assert!(!surface.is_empty());
        assert!(
            surface.iter().all(|a| a.floor_ft() == 0),
            "a surface area's floor is the surface, whatever lower_ft happens to hold"
        );
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

        assert!(
            Chart::from_bytes(good[..HEADER_LEN - 1].to_vec()).is_err(),
            "short"
        );
        assert!(
            Chart::from_bytes(good[..good.len() - 8].to_vec()).is_err(),
            "truncated"
        );

        let mut magic = good.clone();
        magic[0] = b'X';
        assert!(Chart::from_bytes(magic).is_err(), "bad magic");

        let mut version = good.clone();
        version[8] = 99;
        assert!(Chart::from_bytes(version).is_err(), "unknown version");

        // A count that disagrees with the offsets must be caught at load, not at the first draw.
        let mut count = good;
        count[16] = count[16].wrapping_add(1);
        assert!(
            Chart::from_bytes(count).is_err(),
            "airport count vs offsets"
        );
    }
}
