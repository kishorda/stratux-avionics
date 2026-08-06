//! Builds the airport and airspace file the plan view reads.
//!
//! Two public-domain sources — OurAirports for the fields, FAA AIS for Class B, C and D — filtered
//! to the contiguous United States, simplified to what a 7" panel can resolve, and written as one
//! fixed-layout file. `fetch-chartdata.sh` does the downloading; this does everything else.
//!
//! ```text
//!   tools/chartdata/fetch-chartdata.sh
//!   cargo run --release -p chartdata -- build \
//!       --source tools/chartdata/source \
//!       --out crates/avionics-ui/data/conus.chart
//!   cargo run --release -p chartdata -- inspect crates/avionics-ui/data/conus.chart
//! ```
//!
//! This is a dev-only crate. `deploy.sh` builds `-p avionics`, so nothing here reaches the
//! aircraft — the same isolation `mock-stratux` has.
//!
//! See `docs/airspace-and-airports.md` for why each threshold is where it is.

mod airports;
mod airspace;
mod csvread;
mod faa;
mod format;
mod simplify;
mod variation;

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

const USAGE: &str = "\
chartdata — builds the airport and airspace file from OurAirports and FAA AIS

  build --source DIR --out FILE    build the file from downloaded sources
  inspect FILE                     read a built file back and summarise it

  --source DIR   where fetch-chartdata.sh put its output
                                          [default: tools/chartdata/source]
  --out FILE     where to write            [default: crates/avionics-ui/data/conus.chart]

Fetch the sources first (needs internet, one download):
  tools/chartdata/fetch-chartdata.sh
";

/// Days from the Unix epoch to a UTC millisecond timestamp.
///
/// The FAA layer reports `dataLastEditDate`, which is the currency of the *airspace* — a different
/// and more useful question than when the download happened to run.
fn days_from_millis(ms: i64) -> u32 {
    (ms / 86_400_000).max(0) as u32
}

/// `YYYY-MM-DD` for a day count, by the civil-from-days algorithm. Only used for reporting, so it
/// carries no leap-second or timezone subtlety.
fn civil_date(days: u32) -> String {
    let z = days as i64 + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

fn main() -> Result<()> {
    let mut argv = std::env::args().skip(1);
    let Some(command) = argv.next() else {
        println!("{USAGE}");
        return Ok(());
    };

    match command.as_str() {
        "-h" | "--help" => {
            println!("{USAGE}");
            Ok(())
        }
        "build" => {
            let mut source = PathBuf::from("tools/chartdata/source");
            let mut out = PathBuf::from("crates/avionics-ui/data/conus.chart");
            while let Some(arg) = argv.next() {
                let mut value = || argv.next().with_context(|| format!("{arg} needs a value"));
                match arg.as_str() {
                    "--source" => source = PathBuf::from(value()?),
                    "--out" => out = PathBuf::from(value()?),
                    other => bail!("unrecognised argument {other:?}\n\n{USAGE}"),
                }
            }
            build(&source, &out)
        }
        "inspect" => {
            let path = argv.next().context("inspect needs a file")?;
            inspect(Path::new(&path))
        }
        other => bail!("unrecognised command {other:?}\n\n{USAGE}"),
    }
}

fn build(source: &Path, out: &Path) -> Result<()> {
    println!("==> Reading {}", source.display());

    // Frequencies are all that is still read from OurAirports; see `airports.rs`.
    let frequencies = airports::frequency_index(
        &read(&source.join("airport-frequencies.csv"))?,
        &read(&source.join("airports.csv"))?,
    )?;

    let airport_pages = pages_named(source, "airports-faa-")?;
    let runway_pages = pages_named(source, "runways-faa-")?;
    let (airport_records, airport_stats) = faa::parse(&airport_pages, &runway_pages, &frequencies)?;

    println!(
        "    FAA US_Airport ({} pages)  {} features -> {} kept",
        airport_pages.len(),
        airport_stats.read,
        airport_stats.kept
    );
    println!(
        "        dropped: {} outside CONUS, {} not operational, {} no position, {} no identifier",
        airport_stats.dropped_non_conus,
        airport_stats.dropped_not_operational,
        airport_stats.dropped_no_position,
        airport_stats.dropped_no_ident
    );
    println!(
        "        {} with an ICAO station ({:.0}%), {} frequencies at {} airports, \
{} runway orientations at {}",
        airport_stats.with_station,
        100.0 * airport_stats.with_station as f64 / airport_stats.kept.max(1) as f64,
        airport_stats.frequencies,
        airport_stats.with_frequencies,
        airport_stats.runway_headings,
        airport_stats.with_runway_headings,
    );

    let pages = pages_named(source, "airspace-")?;

    let (airspace_records, airspace_stats) = airspace::parse(&pages)?;
    println!(
        "    airspace ({} pages)  {} features -> {} kept",
        pages.len(),
        airspace_stats.read,
        airspace_stats.kept
    );
    println!(
        "        dropped: {} not class B/C/D, {} outside the keep box, {} no usable geometry",
        airspace_stats.dropped_class,
        airspace_stats.dropped_outside_box,
        airspace_stats.dropped_empty
    );
    println!(
        "        vertices: {} -> {} ({:.1}%) at {} m tolerance",
        airspace_stats.vertices_before,
        airspace_stats.vertices_after,
        100.0 * airspace_stats.vertices_after as f64 / airspace_stats.vertices_before.max(1) as f64,
        simplify::TOLERANCE_M
    );

    let effective_days = effective_days(&source.join("airspace-meta.json"))?;

    // Magnetic variation, once the effective date is known, so it ages with the cycle rather than
    // with the clock of whoever ran the build.
    let mut airport_records = airport_records;
    let var_stats = apply_variation(&mut airport_records, effective_days);
    println!(
        "    variation    {} airports, {} to {} degrees (east-positive)",
        var_stats.computed, var_stats.min, var_stats.max
    );
    if var_stats.failed > 0 {
        // Not fatal — an airport with no variation still draws, and its runway components are
        // suppressed rather than wrong. But it must be said out loud: a silent zero here is the
        // exact failure this whole field exists to prevent.
        println!(
            "        WARNING: {} airports kept variation 0 because the model refused them",
            var_stats.failed
        );
    }

    let chart = format::Chart {
        effective_days,
        airports: airport_records,
        airspace: airspace_records,
    };
    let bytes = format::write(&chart);

    // Read the file back before it is written. A writer that miscounts a section produces
    // something that loads and then reads one record into the next — an airport in the sea, and
    // nothing at the point of failure to say so.
    let summary = format::read_summary(&bytes).context("the file just built does not read back")?;
    verify(&bytes, &chart)?;

    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(out, &bytes).with_context(|| format!("writing {}", out.display()))?;

    println!();
    println!("==> Wrote {} ({} KiB)", out.display(), summary.bytes / 1024);
    report(&summary);
    println!();
    println!("    Both sources are public domain, so this file is committed. The FAA endpoint");
    println!("    serves only the current cycle, so a past one cannot be fetched again.");
    Ok(())
}

/// Cross-check the written bytes against the records they came from.
fn verify(bytes: &[u8], chart: &format::Chart) -> Result<()> {
    let summary = format::read_summary(bytes)?;
    let airports = format::read_airports(bytes)?;
    let buckets = format::read_buckets(bytes)?;

    if airports.len() != chart.airports.len() {
        bail!(
            "wrote {} airports, read back {}",
            chart.airports.len(),
            airports.len()
        );
    }

    // Every airport must be reachable through the cell that claims it. This is the one part of
    // the file that can be wrong without looking wrong: a broken index means airports quietly
    // missing from the view rather than a load failure.
    let mut indexed = 0usize;
    for (cell, (first, count)) in buckets.iter().enumerate() {
        for i in 0..*count as usize {
            let airport = &airports[*first as usize + i];
            if summary.grid.cell(airport.lat_e6, airport.lon_e6) != cell {
                bail!("{} is filed in the wrong grid cell", airport.label);
            }
            indexed += 1;
        }
    }
    if indexed != airports.len() {
        bail!("{} airports are in no grid cell", airports.len() - indexed);
    }

    Ok(())
}

fn report(summary: &format::Summary) {
    println!(
        "    format v{}: {} airports, {} airspace polygons, {} rings, {} vertices",
        summary.version, summary.airports, summary.airspace, summary.rings, summary.vertices
    );
    println!(
        "    {} runway orientations, {} frequencies, {} KiB of names",
        summary.runways,
        summary.frequencies,
        summary.strings / 1024
    );
    println!(
        "    grid {}x{} cells from {}, {}",
        summary.grid.rows, summary.grid.cols, summary.grid.lat0, summary.grid.lon0
    );
    println!(
        "    FAA data effective {} (day {})",
        civil_date(summary.effective_days),
        summary.effective_days
    );
}

fn inspect(path: &Path) -> Result<()> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let summary = format::read_summary(&bytes)?;
    println!("==> {} ({} KiB)", path.display(), summary.bytes / 1024);
    report(&summary);

    let airports = format::read_airports(&bytes)?;
    let mut by_tier = [0usize; 4];
    for airport in &airports {
        by_tier[airport.tier as usize] += 1;
    }
    println!(
        "    tiers: {} major, {} paved, {} minor, {} heliport",
        by_tier[0], by_tier[1], by_tier[2], by_tier[3]
    );

    let mut kinds: Vec<(format::FreqKind, usize)> = Vec::new();
    for f in airports.iter().flat_map(|a| &a.frequencies) {
        match kinds.iter_mut().find(|(k, _)| *k == f.kind) {
            Some(entry) => entry.1 += 1,
            None => kinds.push((f.kind, 1)),
        }
    }
    kinds.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    let summary: Vec<String> = kinds
        .iter()
        .map(|(k, n)| {
            format!(
                "{} {n}",
                if k.label().is_empty() {
                    "OTHER"
                } else {
                    k.label()
                }
            )
        })
        .collect();
    println!("    frequencies: {}", summary.join(", "));

    let airspace = format::read_airspace(&bytes)?;
    for class in [format::Class::B, format::Class::C, format::Class::D] {
        let mine: Vec<u32> = airspace
            .iter()
            .filter(|(c, _)| *c == class)
            .map(|(_, v)| *v)
            .collect();
        let total: u32 = mine.iter().sum();
        let largest = mine.iter().copied().max().unwrap_or(0);
        println!(
            "    class {}: {:4} polygons, {:6} vertices, largest {}",
            class.label(),
            mine.len(),
            total,
            largest
        );
    }
    Ok(())
}

/// Read every `<prefix>NNN.json` page in `dir`, in name order.
///
/// Sorted so the input order is fixed regardless of what the filesystem hands back. The parsers
/// sort their output too, but a build should not have to depend on that to be reproducible.
fn pages_named(dir: &Path, prefix: &str) -> Result<Vec<String>> {
    let mut names: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("listing {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                n.starts_with(prefix) && n.ends_with(".json") && n != "airspace-meta.json"
            })
        })
        .collect();
    names.sort();
    if names.is_empty() {
        bail!(
            "no {prefix}* pages in {}; run fetch-chartdata.sh first",
            dir.display()
        );
    }
    names.iter().map(|n| read(n)).collect()
}

fn read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).with_context(|| {
        format!(
            "reading {} — run tools/chartdata/fetch-chartdata.sh first",
            path.display()
        )
    })
}

/// Fill in every airport's magnetic variation from the World Magnetic Model.
///
/// A position the model refuses keeps variation 0 and is counted. That is deliberately *not*
/// treated as "no correction needed": the display gates on the count of runways it can correct, so
/// a zero here suppresses the wind components at that field rather than showing uncorrected ones.
fn apply_variation(airports: &mut [format::Airport], effective_days: u32) -> variation::Stats {
    let mut stats = variation::Stats::default();
    for airport in airports.iter_mut() {
        let lat = airport.lat_e6 as f64 / 1e6;
        let lon = airport.lon_e6 as f64 / 1e6;
        match variation::declination(lat, lon, airport.elevation_ft, effective_days) {
            Ok(degrees) => {
                airport.mag_var_deg = degrees;
                stats.observe(degrees);
            }
            Err(_) => stats.failed += 1,
        }
    }
    stats
}

/// The FAA layer's own last-edit date, in days since the epoch.
fn effective_days(meta: &Path) -> Result<u32> {
    let text = read(meta)?;
    let value: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", meta.display()))?;
    let ms = value
        .get("editingInfo")
        .and_then(|e| e.get("dataLastEditDate"))
        .and_then(serde_json::Value::as_i64)
        .context("no editingInfo.dataLastEditDate in the layer metadata")?;
    Ok(days_from_millis(ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_effective_date_comes_out_as_the_faa_reports_it() {
        // The value observed in the fetched metadata on 2026-08-02.
        let days = days_from_millis(1_783_615_619_054);
        assert_eq!(civil_date(days), "2026-07-09");
    }

    #[test]
    fn civil_dates_round_trip_across_leap_years_and_century_boundaries() {
        for (days, expected) in [
            (0u32, "1970-01-01"),
            (59, "1970-03-01"),
            (10_957, "2000-01-01"),
            (11_016, "2000-02-29"),
            (20_638, "2026-07-04"),
            (20_666, "2026-08-01"),
        ] {
            assert_eq!(civil_date(days), expected, "{days} days");
        }
    }

    #[test]
    fn a_timestamp_before_the_epoch_does_not_wrap_to_a_huge_day_count() {
        assert_eq!(days_from_millis(-1), 0);
    }
}
