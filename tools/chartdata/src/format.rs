//! The on-disk format the display reads.
//!
//! Fixed-layout little-endian, so loading on the Pi is a read and a few slices rather than a parse.
//! Every record size is a multiple of 4 and every section starts 8-aligned, which keeps a
//! zero-copy reader possible later without changing the file.
//!
//! ```text
//!   header      96 B   magic, version, counts, section offsets, grid, effective date
//!   buckets      8 B   per 1x1 degree cell: first airport, airport count
//!   airports    40 B   position, label, elevation, runway, kind, tier, flags,
//!                      and ranges into the runway, frequency and string tables
//!   airspace    40 B   bounding box, ring range, class, flags, lower/upper, label
//!   rings        8 B   first vertex, vertex count
//!   vertices     8 B   latitude and longitude, i32 micro-degrees
//!   runways      4 B   magnetic heading, length — one per distinct orientation
//!   frequencies  8 B   kHz and kind
//!   strings      -     airport names, UTF-8, addressed by offset and length
//! ```
//!
//! # Why airports are indexed and airspace is not
//!
//! There are 20,736 airports and 1,408 airspace polygons. Scanning the airports every frame would
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

/// Version 2 added airport names, communication frequencies and runway orientations. The reader
/// refuses anything else outright rather than guessing at a layout.
pub const VERSION: u16 = 2;

pub const HEADER_LEN: usize = 96;
pub const BUCKET_LEN: usize = 8;
pub const AIRPORT_LEN: usize = 40;
pub const AIRSPACE_LEN: usize = 40;
pub const RING_LEN: usize = 8;
pub const VERTEX_LEN: usize = 8;
pub const RUNWAY_LEN: usize = 4;
pub const FREQUENCY_LEN: usize = 8;

/// Label field width. The longest identifier in the source data is 8 characters.
pub const LABEL_LEN: usize = 8;

/// Names longer than this are truncated. The inspect card is 290 px of an 800 px panel and the
/// longest CONUS name runs past 60 characters, most of it a person's full name.
pub const NAME_MAX: usize = 40;

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
/// [`Tier::Heliport`] is its own tier and not merely the last one: 291 heliports fall within 10 nm
/// of downtown Los Angeles against 5 fixed-wing fields. They are carried in the file and never
/// drawn by default.
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

/// What a frequency is for.
///
/// Collapsed from OurAirports' free-text `type` column, which has a long tail. The distinctions
/// kept are the ones that change what a pilot does: who to call, or what to listen to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum FreqKind {
    /// Common traffic advisory — the one that matters at a non-towered field.
    Ctaf = 0,
    Tower = 1,
    Ground = 2,
    /// Recorded field information.
    Atis = 3,
    /// Automated weather, AWOS or ASOS.
    Awos = 4,
    /// UNICOM, which at many fields is also the CTAF.
    Unicom = 5,
    Approach = 6,
    Departure = 7,
    /// Airport advisory, OurAirports' `A/D`. The second most common type in the file, and at many
    /// fields the only published number.
    Advisory = 8,
    Clearance = 9,
    /// ARTCC. Rarely what you want on the ground, but real.
    Center = 10,
    Other = 11,
}

impl FreqKind {
    /// Map OurAirports' `type` column, which is free text with a long tail.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_uppercase().as_str() {
            "CTAF" => Self::Ctaf,
            "TWR" | "TOWER" => Self::Tower,
            "GND" | "GROUND" => Self::Ground,
            "ATIS" => Self::Atis,
            "AWOS" | "ASOS" | "AWIS" | "WX" => Self::Awos,
            "UNIC" | "UNICOM" => Self::Unicom,
            "APP" | "APR" | "ARR" => Self::Approach,
            "DEP" => Self::Departure,
            "A/D" | "AD" | "AFIS" => Self::Advisory,
            "CLD" | "CLNC" | "DEL" => Self::Clearance,
            "CNTR" | "CTR" | "CENTER" => Self::Center,
            _ => Self::Other,
        }
    }

    /// Which of two names for the *same* frequency to keep.
    ///
    /// Tower beats CTAF, against the display order. At a towered field the CTAF **is** the tower
    /// frequency and both rows exist — Rocky Mountain Metro publishes 118.6 twice. Calling it
    /// `TWR` is never wrong; calling a live tower frequency `CTAF` invites self-announcing on it.
    pub fn preferred(self, other: Self) -> Self {
        match (self, other) {
            (Self::Tower, Self::Ctaf) | (Self::Ctaf, Self::Tower) => Self::Tower,
            _ => self.min(other),
        }
    }

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

/// One runway orientation. Parallel runways collapse into a single entry, because two ticks drawn
/// on top of each other are one tick that took twice the work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Runway {
    /// Degrees, from the runway identifier: "5" is 050, "19" is 190. 10-degree granularity, which
    /// is all the identifier carries and more than enough to point a tick.
    pub heading_deg: u16,
    pub length_ft: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frequency {
    pub khz: u32,
    pub kind: FreqKind,
}

#[derive(Debug, Clone)]
pub struct Airport {
    pub lat_e6: i32,
    pub lon_e6: i32,
    pub label: String,
    pub name: String,
    pub elevation_ft: i16,
    /// Longest hard-surface runway in feet, 0 when there is none.
    pub runway_ft: u16,
    pub kind: Kind,
    pub tier: Tier,
    pub flags: u8,
    pub runways: Vec<Runway>,
    pub frequencies: Vec<Frequency>,
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

    // The three variable-length airport tables, gathered in stored order so each airport's slice
    // is contiguous.
    let mut runways: Vec<Runway> = Vec::new();
    let mut frequencies: Vec<Frequency> = Vec::new();
    let mut strings: Vec<u8> = Vec::new();
    let mut spans: Vec<(u32, u32, u32, u8, u8)> = Vec::with_capacity(airports.len());
    for airport in &airports {
        let runway_first = runways.len() as u32;
        runways.extend_from_slice(&airport.runways);
        let freq_first = frequencies.len() as u32;
        frequencies.extend_from_slice(&airport.frequencies);
        let name_off = strings.len() as u32;
        let name = truncate_on_char_boundary(&airport.name, NAME_MAX);
        strings.extend_from_slice(name.as_bytes());
        spans.push((
            runway_first,
            freq_first,
            name_off,
            name.len() as u8,
            airport.frequencies.len().min(u8::MAX as usize) as u8,
        ));
    }
    // Pad so the file length stays a multiple of 8, as every other section is.
    while strings.len() % 8 != 0 {
        strings.push(0);
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
    let runway_off = vertex_off + vertices.len() * VERTEX_LEN;
    let freq_off = runway_off + runways.len() * RUNWAY_LEN;
    let string_off = freq_off + frequencies.len() * FREQUENCY_LEN;
    let total = string_off + strings.len();

    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // flags, reserved
    out.extend_from_slice(&chart.effective_days.to_le_bytes());
    for count in [
        airports.len(),
        chart.airspace.len(),
        rings.len(),
        vertices.len(),
        runways.len(),
        frequencies.len(),
        strings.len(),
    ] {
        out.extend_from_slice(&(count as u32).to_le_bytes());
    }
    out.extend_from_slice(&0u32.to_le_bytes()); // reserved
    out.extend_from_slice(&grid.lat0.to_le_bytes());
    out.extend_from_slice(&grid.lon0.to_le_bytes());
    out.extend_from_slice(&grid.rows.to_le_bytes());
    out.extend_from_slice(&grid.cols.to_le_bytes());
    for offset in [
        bucket_off,
        airport_off,
        airspace_off,
        ring_off,
        vertex_off,
        runway_off,
        freq_off,
        string_off,
    ] {
        out.extend_from_slice(&(offset as u32).to_le_bytes());
    }
    out.extend_from_slice(&0u32.to_le_bytes()); // reserved
    out.extend_from_slice(&0u32.to_le_bytes()); // reserved
    debug_assert_eq!(out.len(), HEADER_LEN);

    for (first, count) in &buckets {
        out.extend_from_slice(&first.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
    }

    for (airport, (runway_first, freq_first, name_off, name_len, freq_count)) in
        airports.iter().zip(&spans)
    {
        out.extend_from_slice(&airport.lat_e6.to_le_bytes());
        out.extend_from_slice(&airport.lon_e6.to_le_bytes());
        out.extend_from_slice(&label_bytes(&airport.label));
        out.extend_from_slice(&airport.elevation_ft.to_le_bytes());
        out.extend_from_slice(&airport.runway_ft.to_le_bytes());
        out.push(airport.kind as u8);
        out.push(airport.tier as u8);
        out.push(airport.flags);
        out.push(airport.runways.len().min(u8::MAX as usize) as u8);
        out.extend_from_slice(&runway_first.to_le_bytes());
        out.extend_from_slice(&freq_first.to_le_bytes());
        out.extend_from_slice(&name_off.to_le_bytes());
        out.push(*name_len);
        out.push(*freq_count);
        out.extend_from_slice(&0u16.to_le_bytes());
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

    for runway in &runways {
        out.extend_from_slice(&runway.heading_deg.to_le_bytes());
        out.extend_from_slice(&runway.length_ft.to_le_bytes());
    }

    for frequency in &frequencies {
        out.extend_from_slice(&frequency.khz.to_le_bytes());
        out.push(frequency.kind as u8);
        out.extend_from_slice(&[0u8; 3]);
    }

    out.extend_from_slice(&strings);

    debug_assert_eq!(out.len(), total);
    out
}

/// Cut a name to at most `max` **bytes** without splitting a character.
///
/// Names come from a community database and contain accented characters, so slicing at a byte
/// index would panic on some of them — a build that works until the day someone adds an airport
/// with a diacritic in the fortieth column.
pub fn truncate_on_char_boundary(name: &str, max: usize) -> &str {
    if name.len() <= max {
        return name;
    }
    let mut end = max;
    while end > 0 && !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
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
    pub runways: u32,
    pub frequencies: u32,
    pub strings: u32,
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
    let runways = u32at(32);
    let frequencies = u32at(36);
    let strings = u32at(40);
    let grid = Grid {
        lat0: i16at(48),
        lon0: i16at(50),
        rows: u16at(52),
        cols: u16at(54),
    };

    let expected = [
        ("buckets", HEADER_LEN, grid.cells() * BUCKET_LEN),
        ("airports", 0, airports as usize * AIRPORT_LEN),
        ("airspace", 0, airspace as usize * AIRSPACE_LEN),
        ("rings", 0, rings as usize * RING_LEN),
        ("vertices", 0, vertices as usize * VERTEX_LEN),
        ("runways", 0, runways as usize * RUNWAY_LEN),
        ("frequencies", 0, frequencies as usize * FREQUENCY_LEN),
        ("strings", 0, strings as usize),
    ];
    let mut cursor = HEADER_LEN;
    for (index, (name, fixed, len)) in expected.iter().enumerate() {
        let stated = u32at(56 + index * 4) as usize;
        let want = if index == 0 { *fixed } else { cursor };
        ensure!(
            stated == want,
            "{name} section is at {stated}, expected {want}"
        );
        cursor = want + len;
    }
    ensure!(
        bytes.len() == cursor,
        "file is {} bytes, sections account for {cursor}",
        bytes.len()
    );

    Ok(Summary {
        version,
        effective_days: u32at(12),
        airports,
        airspace,
        rings,
        vertices,
        runways,
        frequencies,
        strings,
        grid,
        bytes: bytes.len(),
    })
}

/// Every airport in the file, in stored order. For verification, not for the display.
pub fn read_airports(bytes: &[u8]) -> Result<Vec<Airport>> {
    let summary = read_summary(bytes)?;
    let u32at = |o: usize| {
        u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]])
    };
    let base = u32at(60) as usize;
    let runway_base = u32at(76) as usize;
    let freq_base = u32at(80) as usize;
    let string_base = u32at(84) as usize;

    let mut out = Vec::with_capacity(summary.airports as usize);
    for i in 0..summary.airports as usize {
        let o = base + i * AIRPORT_LEN;
        let r = &bytes[o..o + AIRPORT_LEN];

        let runway_first = u32::from_le_bytes(r[24..28].try_into()?) as usize;
        let runway_count = r[23] as usize;
        let mut runways = Vec::with_capacity(runway_count);
        for k in 0..runway_count {
            let ro = runway_base + (runway_first + k) * RUNWAY_LEN;
            runways.push(Runway {
                heading_deg: u16::from_le_bytes(bytes[ro..ro + 2].try_into()?),
                length_ft: u16::from_le_bytes(bytes[ro + 2..ro + 4].try_into()?),
            });
        }

        let freq_first = u32::from_le_bytes(r[28..32].try_into()?) as usize;
        let freq_count = r[37] as usize;
        let mut frequencies = Vec::with_capacity(freq_count);
        for k in 0..freq_count {
            let fo = freq_base + (freq_first + k) * FREQUENCY_LEN;
            frequencies.push(Frequency {
                khz: u32::from_le_bytes(bytes[fo..fo + 4].try_into()?),
                kind: freq_kind(bytes[fo + 4]),
            });
        }

        let name_off = u32::from_le_bytes(r[32..36].try_into()?) as usize;
        let name_len = r[36] as usize;
        let name = String::from_utf8_lossy(
            &bytes[string_base + name_off..string_base + name_off + name_len],
        )
        .to_string();

        out.push(Airport {
            lat_e6: i32::from_le_bytes(r[0..4].try_into()?),
            lon_e6: i32::from_le_bytes(r[4..8].try_into()?),
            label: String::from_utf8_lossy(&r[8..16])
                .trim_end_matches('\0')
                .to_string(),
            name,
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
            runways,
            frequencies,
        });
    }
    Ok(out)
}

fn freq_kind(byte: u8) -> FreqKind {
    match byte {
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
    }
}

/// Class and vertex count of every airspace record, in stored order. For verification.
pub fn read_airspace(bytes: &[u8]) -> Result<Vec<(Class, u32)>> {
    let summary = read_summary(bytes)?;
    let u32at = |o: usize| {
        u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]])
    };
    let base = u32at(64) as usize;
    let ring_base = u32at(68) as usize;

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
            name: format!("{label} Municipal Airport"),
            elevation_ft: 187,
            runway_ft: 5999,
            kind: Kind::Medium,
            tier,
            flags: FLAG_HARD_SURFACE | FLAG_LIGHTED,
            runways: vec![
                Runway { heading_deg: 50, length_ft: 5999 },
                Runway { heading_deg: 130, length_ft: 3999 },
            ],
            frequencies: vec![
                Frequency { khz: 118_100, kind: FreqKind::Ctaf },
                Frequency { khz: 124_250, kind: FreqKind::Atis },
            ],
        }
    }

    fn chart() -> Chart {
        Chart {
            effective_days: 20_643,
            airports: vec![
                airport("MMU", 40.799, -74.415, Tier::Major),
                airport("EWR", 40.692, -74.169, Tier::Major),
                airport("LAX", 33.942, -118.408, Tier::Major),
                Airport {
                    runways: Vec::new(),
                    frequencies: Vec::new(),
                    name: String::new(),
                    ..airport("06N", 41.431, -74.392, Tier::Paved)
                },
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
        assert_eq!(s.runways, 6, "three airports with two runways each");
        assert_eq!(s.frequencies, 6);
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
            assert_eq!(found.name, original.name);
            assert_eq!(found.runways, original.runways);
            assert_eq!(found.frequencies, original.frequencies);
        }
    }

    #[test]
    fn an_airport_with_nothing_attached_reads_back_empty_not_borrowed() {
        // The variable-length tables are the new way this format can go wrong: an off-by-one in a
        // span makes one airport read its neighbour's runways and frequencies, which looks like
        // plausible data rather than like a fault.
        let bytes = write(&chart());
        let back = read_airports(&bytes).unwrap();
        let bare = back.iter().find(|a| a.label == "06N").unwrap();
        assert!(bare.runways.is_empty(), "06N borrowed runways");
        assert!(bare.frequencies.is_empty(), "06N borrowed frequencies");
        assert_eq!(bare.name, "");

        let mmu = back.iter().find(|a| a.label == "MMU").unwrap();
        assert_eq!(mmu.frequencies.len(), 2);
        assert_eq!(mmu.frequencies[0].khz, 118_100);
        assert_eq!(mmu.name, "MMU Municipal Airport");
    }

    #[test]
    fn the_grid_puts_every_airport_in_the_cell_that_claims_it() {
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
        assert_ne!(
            grid.cell(33_942_000, -118_408_000),
            grid.cell(40_799_000, -74_415_000)
        );
    }

    #[test]
    fn the_build_is_reproducible() {
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

        let mut old = bytes.clone();
        old[8] = 1;
        assert!(read_summary(&old).is_err(), "version 1 must be refused, not reinterpreted");

        let mut count = bytes;
        count[16] = count[16].wrapping_add(1);
        assert!(read_summary(&count).is_err(), "airport count vs offsets");
    }

    #[test]
    fn a_name_is_cut_between_characters_and_not_through_one() {
        // Names come from a community database. Slicing at a byte index would panic the build the
        // first time someone adds a field with a diacritic in the fortieth column.
        let ascii = "Morristown Municipal Airport";
        assert_eq!(truncate_on_char_boundary(ascii, NAME_MAX), ascii);

        let accented = "Aérodrome de Saint-Étienne-de-Saint-Geoirs Regional";
        let cut = truncate_on_char_boundary(accented, NAME_MAX);
        assert!(cut.len() <= NAME_MAX);
        assert!(accented.starts_with(cut));

        // Every prefix length must be safe, not just the one this name happens to hit.
        for max in 0..accented.len() {
            let cut = truncate_on_char_boundary(accented, max);
            assert!(cut.len() <= max);
            assert!(accented.starts_with(cut));
        }
    }

    #[test]
    fn frequency_kinds_map_the_spellings_the_source_actually_uses() {
        // Taken from the type column of the real file, which is free text with a long tail.
        for (raw, want) in [
            ("CTAF", FreqKind::Ctaf),
            ("TWR", FreqKind::Tower),
            ("GND", FreqKind::Ground),
            ("ATIS", FreqKind::Atis),
            ("AWOS", FreqKind::Awos),
            ("ASOS", FreqKind::Awos),
            ("UNIC", FreqKind::Unicom),
            ("APP", FreqKind::Approach),
            ("CLD", FreqKind::Clearance),
            // The two biggest former members of Other: 1,544 and 992 CONUS rows. Both reach the
            // card, so both need a name — an unlabelled number is dropped from the card entirely.
            ("A/D", FreqKind::Advisory),
            ("CNTR", FreqKind::Center),
            ("RDO", FreqKind::Other),
            ("", FreqKind::Other),
        ] {
            assert_eq!(FreqKind::parse(raw), want, "{raw:?}");
        }
        // Case and padding are not meaningful in the source.
        assert_eq!(FreqKind::parse(" twr "), FreqKind::Tower);
    }

    #[test]
    fn tower_wins_a_shared_frequency_against_the_display_order() {
        // CTAF sorts first for display, because at a non-towered field it is what you want. But
        // when one radio is published under both names the specific one has to win: labelling a
        // live tower frequency "CTAF" invites self-announcing on it.
        assert_eq!(FreqKind::Ctaf.preferred(FreqKind::Tower), FreqKind::Tower);
        assert_eq!(FreqKind::Tower.preferred(FreqKind::Ctaf), FreqKind::Tower);
        // Everything else keeps the more useful of the two, whichever way round it arrives.
        assert_eq!(FreqKind::Unicom.preferred(FreqKind::Ctaf), FreqKind::Ctaf);
        assert_eq!(FreqKind::Ctaf.preferred(FreqKind::Unicom), FreqKind::Ctaf);
        assert_eq!(FreqKind::Other.preferred(FreqKind::Atis), FreqKind::Atis);
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
