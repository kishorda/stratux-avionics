//! Live polling of the free public feeds, for `--internet`.
//!
//! Same two services `fetch-snapshot.sh` uses, and the same conversions in [`crate::snapshot`] —
//! the only difference is that this keeps asking. A snapshot is one instant served forever;
//! internet mode is the world as it is now, refreshed while you watch.
//!
//! # Politeness, and why the poll is slower than the publish
//!
//! These are somebody else's servers, given away for nothing. adsb.lol is community-fed and
//! aviationweather.gov asks for 100 requests a minute at the outside. So traffic is polled every
//! few seconds and weather every few minutes, and [`crate::world::World`] flies the targets
//! forward in between — which is why the display still sees a fresh position every second rather
//! than a picture that freezes for five seconds and then jumps.
//!
//! That is not a trick to hide the poll rate. It is the same dead reckoning the display itself
//! does, and the arriving fix snaps the target back to the truth, exactly as a real ADS-B update
//! would.
//!
//! # A failed poll is not a failure
//!
//! Wifi drops, DNS hiccups, a service has a bad minute. None of that may take the server down or
//! blank the display: the last good picture keeps being served and flown forward, and the next
//! poll picks up where it left off. A mock that fell over on a transient network blip would be
//! worse than useless, because you would be debugging it instead of the display.

use std::time::Duration;

use anyhow::{Context, Result};

use crate::snapshot::Snapshot;

/// Identifies this tool to the services. aviationweather.gov asks callers to set one, and saying
/// who you are is the polite and the self-interested choice — anonymous bulk callers get blocked.
const USER_AGENT: &str = "stratux-avionics-mock/0.1 (offline display testing)";

/// Give up on a single request rather than wedging the poll loop.
const TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone)]
pub struct Feeds {
    pub lat: f64,
    pub lon: f64,
    pub radius_nm: u32,
    pub traffic_every: Duration,
    pub weather_every: Duration,
}

impl Feeds {
    /// A bounding box `radius_nm` around the centre, as aviationweather.gov wants it.
    ///
    /// Longitude degrees shrink with latitude, so the box would be too narrow east-west without
    /// the cosine — at 40 degrees north by about a quarter, which would quietly miss stations the
    /// traffic query does include.
    pub fn bbox(&self) -> String {
        let d_lat = self.radius_nm as f64 / 60.0;
        let d_lon = self.radius_nm as f64 / (60.0 * self.lat.to_radians().cos().abs().max(1e-6));
        format!(
            "{:.4},{:.4},{:.4},{:.4}",
            self.lat - d_lat,
            self.lon - d_lon,
            self.lat + d_lat,
            self.lon + d_lon
        )
    }

    fn traffic_url(&self) -> String {
        format!(
            "https://api.adsb.lol/v2/lat/{}/lon/{}/dist/{}",
            self.lat, self.lon, self.radius_nm
        )
    }

    fn weather_url(&self, product: &str) -> String {
        format!(
            "https://aviationweather.gov/api/data/{product}?bbox={}&format=json",
            self.bbox()
        )
    }
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(TIMEOUT)
        .build()
        .context("building the HTTP client")
}

async fn get_json(client: &reqwest::Client, url: &str) -> Result<serde_json::Value> {
    let response = client.get(url).send().await.context("request failed")?;
    let status = response.status();
    let body = response.text().await.context("reading the body")?;
    if !status.is_success() {
        anyhow::bail!("HTTP {status}: {}", body.chars().take(200).collect::<String>());
    }
    serde_json::from_str(&body).context("response was not JSON")
}

/// Fetch the current traffic picture. Returns the raw `ac` array.
pub async fn fetch_traffic(feeds: &Feeds) -> Result<serde_json::Value> {
    let client = client()?;
    let value = get_json(&client, &feeds.traffic_url()).await?;
    Ok(value.get("ac").cloned().unwrap_or(serde_json::Value::Null))
}

/// Fetch METARs, TAFs and PIREPs. Each product is independent: one failing must not lose the
/// others, because a missing PIREP is routine and a missing METAR is not.
pub async fn fetch_weather(
    feeds: &Feeds,
) -> (serde_json::Value, serde_json::Value, serde_json::Value) {
    let Ok(client) = client() else {
        return Default::default();
    };
    let mut out = [
        serde_json::Value::Null,
        serde_json::Value::Null,
        serde_json::Value::Null,
    ];
    for (slot, product) in ["metar", "taf", "pirep"].iter().enumerate() {
        match get_json(&client, &feeds.weather_url(product)).await {
            Ok(value) => out[slot] = value,
            Err(e) => tracing::warn!(product, error = %e, "weather poll failed"),
        }
    }
    let [metar, taf, pirep] = out;
    (metar, taf, pirep)
}

/// Assemble whatever was fetched into the same envelope a snapshot uses, so both paths share one
/// set of conversions and neither can drift away from the other.
pub fn to_snapshot(
    feeds: &Feeds,
    traffic: serde_json::Value,
    metar: serde_json::Value,
    taf: serde_json::Value,
    pirep: serde_json::Value,
) -> Result<Snapshot> {
    let envelope = serde_json::json!({
        "captured_utc": "",
        "origin": {"lat": feeds.lat, "lon": feeds.lon},
        "traffic": array_or_empty(traffic),
        "metar": array_or_empty(metar),
        "taf": array_or_empty(taf),
        "pirep": array_or_empty(pirep),
    });
    Snapshot::parse(&serde_json::to_vec(&envelope)?)
}

/// Both services answer an error with a JSON *object* where success is an array. Coercing rather
/// than failing keeps one bad product from discarding the rest of the poll.
fn array_or_empty(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Array(_) => value,
        _ => serde_json::Value::Array(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feeds() -> Feeds {
        Feeds {
            lat: 40.7784,
            lon: -74.3343,
            radius_nm: 50,
            traffic_every: Duration::from_secs(5),
            weather_every: Duration::from_secs(600),
        }
    }

    #[test]
    fn the_bounding_box_is_widened_for_latitude() {
        // Without the cosine the box is a quarter too narrow east-west at this latitude, which
        // would silently return fewer stations than the traffic query covers.
        let f = feeds();
        let parts: Vec<f64> = f.bbox().split(',').map(|s| s.parse().unwrap()).collect();
        let (lat0, lon0, lat1, lon1) = (parts[0], parts[1], parts[2], parts[3]);

        assert!((lat1 - lat0 - 2.0 * 50.0 / 60.0).abs() < 1e-3, "latitude span");
        let lon_span = lon1 - lon0;
        let lat_span = lat1 - lat0;
        assert!(
            lon_span > lat_span * 1.2,
            "longitude span {lon_span} should exceed latitude span {lat_span} at 40N"
        );
        // ... and the box is centred on the position that was asked for.
        assert!(((lat0 + lat1) / 2.0 - f.lat).abs() < 1e-6);
        assert!(((lon0 + lon1) / 2.0 - f.lon).abs() < 1e-6);
    }

    #[test]
    fn urls_carry_the_position_and_radius() {
        let f = feeds();
        assert_eq!(
            f.traffic_url(),
            "https://api.adsb.lol/v2/lat/40.7784/lon/-74.3343/dist/50"
        );
        assert!(f.weather_url("metar").starts_with(
            "https://aviationweather.gov/api/data/metar?bbox="
        ));
        assert!(f.weather_url("taf").ends_with("&format=json"));
    }

    #[test]
    fn an_error_object_becomes_an_empty_list_rather_than_a_parse_failure() {
        // Both services answer errors with an object where success is an array. PIREPs in
        // particular return `{"status":"error",...}` for a quiet area, which is routine.
        let error = serde_json::json!({"status": "error", "error": "no data"});
        assert_eq!(array_or_empty(error), serde_json::json!([]));
        assert_eq!(array_or_empty(serde_json::Value::Null), serde_json::json!([]));
        let ok = serde_json::json!([{"icaoId": "KMMU"}]);
        assert_eq!(array_or_empty(ok.clone()), ok);
    }

    #[test]
    fn a_poll_with_nothing_in_it_still_produces_a_usable_snapshot() {
        // Every service failing at once must leave the server running on an empty sky, not
        // panicking. This is the shape of a wifi drop.
        let snap = to_snapshot(
            &feeds(),
            serde_json::Value::Null,
            serde_json::Value::Null,
            serde_json::Value::Null,
            serde_json::Value::Null,
        )
        .expect("an empty poll is still a snapshot");
        assert!(snap.targets().is_empty());
        assert!(snap.weather().is_empty());
        assert_eq!(snap.origin.lat, 40.7784);
    }

    #[test]
    fn a_partial_poll_keeps_what_it_got() {
        // PIREPs are frequently absent over a small area. Losing the METARs because of that would
        // be the tool inventing a problem the services did not have.
        let snap = to_snapshot(
            &feeds(),
            serde_json::json!([{"hex": "a1f0b4", "lat": 40.7, "lon": -74.3, "alt_baro": 3000}]),
            serde_json::json!([{"icaoId": "KMMU", "rawOb": "METAR KMMU 021656Z 15014KT"}]),
            serde_json::json!({"status": "error"}),
            serde_json::Value::Null,
        )
        .expect("parses");
        assert_eq!(snap.targets().len(), 1);
        assert_eq!(snap.weather().len(), 1, "the METAR survives the failed TAF");
    }
}
