//! Communication frequencies from OurAirports.
//!
//! This is all that is left of OurAirports in the build. Everything else — position, identifier,
//! name, elevation, runways, operational status — now comes from the FAA's own layers; see
//! [`crate::faa`].
//!
//! Frequencies stay here because the FAA publishes them behind a `Frequencies` -> `Services` ->
//! airport join covering 2,493 services, where OurAirports has a flat table covering 3,780
//! airports. The FAA set looks to be towered and FSS services; the CTAF at a small field is
//! exactly what this display wants and exactly what the FAA table is thinnest on. Both sources
//! are public domain, so carrying two costs nothing but the fetch.

use anyhow::{Context, Result};

use crate::csvread::{field, Reader};
use crate::format::{FreqKind, Frequency};

/// Communication frequencies per airport, sorted by identifier for binary search.
///
/// Only about 18% of CONUS airports have any. That is not a gap to apologise for: the ones that do
/// are the ones with somebody to talk to.
///
/// # The identifiers do not match, so both are indexed
///
/// The frequency file keys on OurAirports' `ident` — `KMMU` — and the FAA layer this joins to
/// keys on `IDENT`, which is the local code, `MMU`. Rather than translate one into the other and
/// be wrong at the edges, every airport is entered under **both** its ident and its local code, so
/// a lookup by either finds it. `airports.csv` is read for that mapping and nothing else.
pub fn frequency_index(
    frequencies_csv: &str,
    airports_csv: &str,
) -> Result<Vec<(String, Vec<Frequency>)>> {
    let aliases = ident_aliases(airports_csv)?;
    let reader = Reader::parse(frequencies_csv).context("parsing airport-frequencies.csv")?;
    let c_airport = reader.column("airport_ident")?;
    let c_type = reader.column("type")?;
    let c_mhz = reader.column("frequency_mhz")?;

    let mut out: Vec<(String, Vec<Frequency>)> = Vec::new();
    for row in reader.rows() {
        // Stored in kHz as an integer. Megahertz as a float would make 121.975 — a real 25 kHz
        // channel — into something that formats as 121.97 or 121.98 depending on the wind.
        let Ok(mhz) = field(row, c_mhz).trim().parse::<f64>() else {
            continue;
        };
        let khz = (mhz * 1000.0).round() as u32;
        let kind = FreqKind::parse(field(row, c_type));
        if !tunable(khz, kind) {
            continue;
        }
        let ident = field(row, c_airport);

        let mut keys = vec![ident.to_string()];
        if let Ok(i) = aliases.binary_search_by(|(id, _)| id.as_str().cmp(ident)) {
            let local = &aliases[i].1;
            if !local.is_empty() && local != ident {
                keys.push(local.clone());
            }
        }

        for key in keys {
            let index = match out.binary_search_by(|(id, _)| id.cmp(&key)) {
                Ok(i) => i,
                Err(i) => {
                    out.insert(i, (key.clone(), Vec::new()));
                    i
                }
            };
            let list = &mut out[index].1;
            // The same frequency is often listed twice under different names — CTAF and UNICOM on
            // a non-towered field are usually one radio. Keep the more specific kind.
            if let Some(existing) = list.iter_mut().find(|f| f.khz == khz) {
                existing.kind = existing.kind.preferred(kind);
                continue;
            }
            list.push(Frequency { khz, kind });
        }
    }

    // Most useful first, so a card with room for four shows the four that matter.
    for (_, list) in out.iter_mut() {
        list.sort_by(|a, b| a.kind.cmp(&b.kind).then(a.khz.cmp(&b.khz)));
    }
    Ok(out)
}

/// Whether a light aircraft could actually tune this.
///
/// The VHF communication band is 118.000–136.975 and that is the whole of what a civil comm radio
/// reaches. The source also carries military frequencies — Washington National's tower on 257.600,
/// Fort Rucker's ground on 357.150, Seymour Johnson's on 138.100 — which are real and untunable,
/// and printing `TWR 257.600` on the card says you could call the tower on it.
///
/// The exception is deliberate and goes the other way: **ATIS and AWOS below the comm band are
/// kept**, because they are broadcast on a co-located navaid's voice channel and a pilot tunes
/// them on the NAV radio. 74 airports publish one that way, and dropping them to tidy up the band
/// check would have thrown away working information.
fn tunable(khz: u32, kind: FreqKind) -> bool {
    const COMM: std::ops::RangeInclusive<u32> = 118_000..=136_975;
    const NAVAID_VOICE: std::ops::RangeInclusive<u32> = 108_000..=117_975;
    COMM.contains(&khz)
        || (NAVAID_VOICE.contains(&khz) && matches!(kind, FreqKind::Atis | FreqKind::Awos))
}

/// `ident` -> `local_code` for every US row, sorted for binary search.
fn ident_aliases(airports_csv: &str) -> Result<Vec<(String, String)>> {
    let reader = Reader::parse(airports_csv).context("parsing airports.csv")?;
    let c_ident = reader.column("ident")?;
    let c_local = reader.column("local_code")?;
    let c_country = reader.column("iso_country")?;

    let mut out: Vec<(String, String)> = reader
        .rows()
        .filter(|r| field(r, c_country) == "US")
        .map(|r| {
            (
                field(r, c_ident).trim().to_string(),
                field(r, c_local).trim().to_ascii_uppercase(),
            )
        })
        .filter(|(ident, _)| !ident.is_empty())
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out.dedup_by(|a, b| a.0 == b.0);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FREQ_HEADER: &str = "id,airport_ref,airport_ident,type,description,frequency_mhz\n";
    const AIRPORTS_HEADER: &str = "id,ident,type,name,latitude_deg,longitude_deg,elevation_ft,\
continent,iso_country,iso_region,municipality,scheduled_service,icao_code,iata_code,gps_code,\
local_code,home_link,wikipedia_link,keywords\n";

    #[test]
    fn frequencies_are_findable_by_either_identifier() {
        // The frequency file says KMMU; the FAA airport layer says MMU. Indexing only one of them
        // would silently drop every frequency on the card.
        let apts = format!(
            "{AIRPORTS_HEADER}1,\"KMMU\",\"medium_airport\",\"Morristown\",40.8,-74.4,187,\"NA\",\
\"US\",\"US-NJ\",\"Morristown\",\"no\",,\"MMU\",\"KMMU\",\"MMU\",,,\n"
        );
        let freqs = format!(
            "{FREQ_HEADER}1,1,\"KMMU\",\"TWR\",\"TWR\",118.1\n2,1,\"KMMU\",\"ATIS\",\"ATIS\",124.25\n"
        );
        let index = frequency_index(&freqs, &apts).unwrap();

        for key in ["KMMU", "MMU"] {
            let i = index
                .binary_search_by(|(id, _)| id.as_str().cmp(key))
                .unwrap_or_else(|_| panic!("{key} should be indexed"));
            assert_eq!(index[i].1.len(), 2, "{key}");
            assert_eq!(index[i].1[0].khz, 118_100);
        }
    }

    #[test]
    fn an_airport_with_no_local_code_is_indexed_once() {
        let apts = format!(
            "{AIRPORTS_HEADER}1,\"KXYZ\",\"small_airport\",\"X\",40.1,-74.1,300,\"NA\",\"US\",\
\"US-NJ\",,\"no\",,,,,,,\n"
        );
        let freqs = format!("{FREQ_HEADER}1,1,\"KXYZ\",\"CTAF\",\"CTAF\",122.8\n");
        let index = frequency_index(&freqs, &apts).unwrap();
        assert_eq!(index.len(), 1);
        assert_eq!(index[0].0, "KXYZ");
    }

    #[test]
    fn one_radio_listed_twice_is_stored_once_under_the_more_specific_name() {
        let apts = format!(
            "{AIRPORTS_HEADER}1,\"KAAA\",\"small_airport\",\"A\",40.1,-74.1,300,\"NA\",\"US\",\
\"US-NJ\",,\"no\",,,,\"AAA\",,,\n"
        );
        let freqs = format!(
            "{FREQ_HEADER}1,1,\"KAAA\",\"UNIC\",\"UNICOM\",122.8\n2,1,\"KAAA\",\"CTAF\",\"CTAF\",122.8\n"
        );
        let index = frequency_index(&freqs, &apts).unwrap();
        let i = index
            .binary_search_by(|(id, _)| id.as_str().cmp("AAA"))
            .unwrap();
        assert_eq!(index[i].1.len(), 1);
        assert_eq!(index[i].1[0].kind, FreqKind::Ctaf);
    }

    #[test]
    fn frequencies_a_light_aircraft_cannot_tune_are_dropped() {
        // Real, and untunable on a civil comm radio. "TWR 257.600" at Washington National reads as
        // a number you could call the tower on.
        for (khz, kind) in [
            (257_600, FreqKind::Tower),  // DCA, military UHF
            (357_150, FreqKind::Ground), // Fort Rucker
            (138_100, FreqKind::Ground), // Seymour Johnson, military VHF
            (322_100, FreqKind::Ctaf),
            (199_250, FreqKind::Advisory),
        ] {
            assert!(!tunable(khz, kind), "{khz} kHz should be dropped");
        }
        // The comm band itself, and its edges.
        for khz in [118_000, 121_975, 136_975] {
            assert!(
                tunable(khz, FreqKind::Tower),
                "{khz} kHz is in the comm band"
            );
        }
        assert!(
            !tunable(117_975, FreqKind::Tower),
            "just below the comm band"
        );
        assert!(!tunable(137_000, FreqKind::Tower), "just above it");
    }

    #[test]
    fn atis_and_awos_on_a_navaid_voice_channel_are_kept() {
        // 74 airports broadcast one below the comm band, on a co-located VOR's voice channel, and
        // a pilot tunes those on the NAV radio. A tidy band check would have thrown them away.
        assert!(tunable(108_200, FreqKind::Awos));
        assert!(tunable(109_000, FreqKind::Atis));
        assert!(tunable(117_975, FreqKind::Awos));
        // But only those two kinds: a tower does not live down there.
        assert!(!tunable(108_200, FreqKind::Tower));
        assert!(!tunable(108_200, FreqKind::Ctaf));
        // And not below the navaid band either.
        assert!(!tunable(107_900, FreqKind::Awos));
    }

    #[test]
    fn an_out_of_band_frequency_is_dropped_rather_than_shown() {
        let apts = format!(
            "{AIRPORTS_HEADER}1,\"KAAA\",\"small_airport\",\"A\",40.1,-74.1,300,\"NA\",\"US\",\
\"US-NJ\",,\"no\",,,,\"AAA\",,,\n"
        );
        let freqs = format!(
            "{FREQ_HEADER}1,1,\"KAAA\",\"CTAF\",\"CTAF\",0\n2,1,\"KAAA\",\"TWR\",\"TWR\",8118.1\n\
3,1,\"KAAA\",\"ATIS\",\"ATIS\",124.25\n"
        );
        let index = frequency_index(&freqs, &apts).unwrap();
        let i = index
            .binary_search_by(|(id, _)| id.as_str().cmp("AAA"))
            .unwrap();
        assert_eq!(index[i].1.len(), 1);
        assert_eq!(index[i].1[0].khz, 124_250);
    }
}
