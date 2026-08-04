//! A mock Stratux, for testing the display on a desk with no Pi, no radios and no internet.
//!
//! Fed with real data from free public services — aircraft from adsb.lol, METARs and TAFs from
//! aviationweather.gov — in either of two ways:
//!
//! * `--internet` polls them and keeps polling, so the display shows what is flying right now.
//! * `--snapshot` serves a capture from `fetch-snapshot.sh`, with no network at all.
//!
//! See `docs/free-aviation-data.md` for what those services are and what their terms allow.
//!
//! The display is not modified for either. It takes its ordinary live WebSocket path and has no
//! idea it is not talking to a Pi, which is the point: no HTTP client in the aircraft binary.
//!
//! With neither flag it serves an empty sky at a given position, which is still useful — that is
//! the state the display spends its first minutes in on every cold start.

mod feeds;
mod server;
mod snapshot;
mod world;

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use stratux_client::Stream;

use world::{OwnShip, World};

const USAGE: &str = "\
mock-stratux — a fake Stratux for offline testing on the dev machine

Data source (pick one; the default is an empty sky)
  --snapshot FILE      serve data captured earlier by fetch-snapshot.sh. Offline.
  --internet           poll adsb.lol and aviationweather.gov live, and keep polling.
                       Needs internet; the display still sees an ordinary Stratux.

  --port P             port to listen on                    [default: 8080]
  --lat D --lon D      centre position: where to poll around, and where own-ship sits
  --radius NM          how far to look, --internet only     [default: 50, max 250]
  --poll SECS          traffic poll interval                [default: 5]
  --weather-poll SECS  weather poll interval                [default: 600]
  --fly TRACK@SPEED    fly own-ship, e.g. --fly 090@110      [default: stationary]

Fault injection
  --drop-every SECS    close every socket this often, to exercise reconnect
  --stall STREAM,...   accept these sockets but never send: traffic, situation,
                       weather, status, jsonio
  --garbage-every N    emit a malformed frame every Nth message

Then point the display at it:
  cargo run --release --features desktop -p avionics -- --window --host 127.0.0.1 --port 8080
";

struct Args {
    snapshot: Option<PathBuf>,
    internet: bool,
    port: u16,
    lat: Option<f64>,
    lon: Option<f64>,
    radius_nm: u32,
    poll_s: f64,
    weather_poll_s: f64,
    fly: Option<(f64, f64)>,
    faults: server::Faults,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            snapshot: None,
            internet: false,
            port: 8080,
            lat: None,
            lon: None,
            radius_nm: 50,
            // Five seconds is a compromise, not a limit of the feed: it is often enough that the
            // flown-forward positions never drift far from the truth, and rare enough to be a
            // reasonable thing to do to a free community service indefinitely.
            poll_s: 5.0,
            weather_poll_s: 600.0,
            fly: None,
            faults: server::Faults::default(),
        }
    }
}

fn parse_args() -> Result<Option<Args>> {
    let mut args = Args::default();
    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        let mut value = || argv.next().with_context(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(None);
            }
            "--snapshot" => args.snapshot = Some(PathBuf::from(value()?)),
            "--internet" => args.internet = true,
            "--radius" => args.radius_nm = value()?.parse().context("bad --radius")?,
            "--poll" => args.poll_s = value()?.parse().context("bad --poll")?,
            "--weather-poll" => {
                args.weather_poll_s = value()?.parse().context("bad --weather-poll")?
            }
            "--port" => args.port = value()?.parse().context("bad --port")?,
            "--lat" => args.lat = Some(value()?.parse().context("bad --lat")?),
            "--lon" => args.lon = Some(value()?.parse().context("bad --lon")?),
            "--fly" => {
                let v = value()?;
                let (track, speed) = v
                    .split_once('@')
                    .with_context(|| format!("bad --fly {v:?}, want TRACK@SPEED e.g. 090@110"))?;
                args.fly = Some((track.trim().parse()?, speed.trim().parse()?));
            }
            "--drop-every" => {
                let secs: f64 = value()?.parse().context("bad --drop-every")?;
                if secs <= 0.0 {
                    bail!("--drop-every must be positive");
                }
                args.faults.drop_every = Some(Duration::from_secs_f64(secs));
            }
            "--stall" => {
                let v = value()?;
                let mut set = HashSet::new();
                for name in v.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    let stream = Stream::from_name(name).with_context(|| {
                        format!("unknown stream {name:?}; want traffic, situation, weather, status or jsonio")
                    })?;
                    set.insert(stream);
                }
                args.faults.stall = set;
            }
            "--garbage-every" => {
                args.faults.garbage_every = Some(value()?.parse().context("bad --garbage-every")?)
            }
            other => bail!("unrecognised argument {other:?}\n\n{USAGE}"),
        }
    }
    if args.internet {
        if args.snapshot.is_some() {
            bail!("--internet and --snapshot are different data sources; pick one");
        }
        // The feed clamps silently past this, which would make the server quietly not cover the
        // area it was asked for.
        if args.radius_nm == 0 || args.radius_nm > 250 {
            bail!("--radius must be between 1 and 250 nm");
        }
        if args.poll_s < 1.0 {
            bail!(
                "--poll below 1 s is not polite to a free service, and the targets are flown \
                   forward between polls anyway"
            );
        }
        if args.weather_poll_s < 60.0 {
            bail!("--weather-poll below 60 s is pointless: reports update far more slowly");
        }
    }
    Ok(Some(args))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let Some(args) = parse_args()? else {
        return Ok(());
    };

    let (mut origin, targets, weather) = match &args.snapshot {
        // --internet starts empty and fills on the first poll, a second or two later. That is not
        // a gap to apologise for: it is what a real Stratux looks like at power-on, and the
        // display's "waiting" states deserve to be seen.
        Some(path) => {
            let bytes =
                std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
            let snap = snapshot::Snapshot::parse(&bytes)?;
            let targets = snap.targets();
            let weather = snap.weather();
            tracing::info!(
                captured = %snap.captured_utc,
                targets = targets.len(),
                weather = weather.len(),
                "loaded snapshot"
            );
            ((snap.origin.lat, snap.origin.lon), targets, weather)
        }
        None => {
            if !args.internet {
                tracing::info!("no snapshot: serving an empty sky");
            }
            ((40.7784, -74.3343), Vec::new(), Vec::new())
        }
    };

    if let Some(lat) = args.lat {
        origin.0 = lat;
    }
    if let Some(lon) = args.lon {
        origin.1 = lon;
    }

    let mut ownship = OwnShip::stationary(origin.0, origin.1);
    if let Some((track, speed)) = args.fly {
        ownship.track_deg = Some(track);
        ownship.ground_speed_kt = speed;
        // Airborne, so relative altitudes mean something and the threat tiers can escalate.
        ownship.altitude_ft = 3500.0;
    }

    if !args.faults.stall.is_empty() {
        tracing::warn!(streams = ?args.faults.stall, "stalling streams (fault injection)");
    }
    if let Some(n) = args.faults.garbage_every {
        tracing::warn!(every = n, "emitting malformed frames (fault injection)");
    }

    let feeds = args.internet.then(|| feeds::Feeds {
        lat: origin.0,
        lon: origin.1,
        radius_nm: args.radius_nm,
        traffic_every: Duration::from_secs_f64(args.poll_s),
        weather_every: Duration::from_secs_f64(args.weather_poll_s),
    });

    let world = World::new(ownship, targets, weather);
    server::serve(
        world,
        server::Config {
            port: args.port,
            faults: args.faults,
            feeds,
        },
    )
    .await
}
