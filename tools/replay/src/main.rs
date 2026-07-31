//! Record, synthesise, inspect and replay Stratux sessions.
//!
//! ```text
//! replay record  --host 192.168.10.1 --out session.jsonl --duration 300
//! replay synth   --out synth.jsonl --duration 120 --targets 8
//! replay stats   session.jsonl
//! replay play    session.jsonl --speed 4
//! ```
//!
//! `record` is how a real flight gets captured; `synth` is how the plan view gets developed
//! before there is a flight to capture. `play` decodes into the same [`AppState`] the display
//! uses and prints a periodic summary, so a recording can be sanity-checked without a renderer.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use stratux_client::domain::LatLon;
use stratux_client::state::AgePolicy;
use stratux_client::{live, record, synth, AppState, SourceEvent};

const USAGE: &str = "\
replay — record, synthesise, inspect and replay Stratux sessions

  replay record <out.jsonl> [--host H] [--port P] [--duration SECS]
  replay synth  <out.jsonl> [--duration SECS] [--targets N] [--seed S] [--no-weather]
  replay stats  <in.jsonl>
  replay play   <in.jsonl> [--speed X] [--repeat] [--no-delay] [--quiet]

Options
  --host H         Stratux address                       [default: 127.0.0.1]
  --port P         Stratux management port               [default: 80]
  --duration SECS  Record/synthesise for this long      [default: record 60, synth 120]
  --targets N      Synthetic traffic count               [default: 8]
  --seed S         Synthetic RNG seed (deterministic)    [default: fixed]
  --no-weather     Omit weather from a synthetic session
  --conflict       Add a deterministic head-on conflict and a Mode-S no-position target
  --speed X        Replay rate multiplier                [default: 1.0]
  --repeat         Loop the recording
  --no-delay       Ignore recorded timing, replay as fast as possible
  --quiet          Suppress the periodic summary during play
";

#[derive(Debug)]
enum Command {
    Record {
        out: PathBuf,
        host: String,
        port: u16,
        duration: Duration,
    },
    Synth {
        out: PathBuf,
        config: synth::SynthConfig,
    },
    Stats {
        input: PathBuf,
    },
    Play {
        input: PathBuf,
        config: record::ReplayConfig,
        quiet: bool,
    },
}

fn parse_args() -> Result<Option<Command>> {
    let mut argv = std::env::args().skip(1);
    let Some(verb) = argv.next() else {
        return Ok(None);
    };
    if verb == "-h" || verb == "--help" {
        return Ok(None);
    }

    let path = argv
        .next()
        .filter(|s| !s.starts_with('-'))
        .map(PathBuf::from);

    // Collect the remaining flags into (name, optional value) pairs.
    let rest: Vec<String> = argv.collect();
    let flag = |name: &str| -> Option<&String> {
        rest.iter()
            .position(|a| a == name)
            .and_then(|i| rest.get(i + 1))
    };
    let present = |name: &str| rest.iter().any(|a| a == name);

    // Reject unknown flags rather than silently ignoring a typo like --spede.
    const KNOWN: &[&str] = &[
        "--host",
        "--port",
        "--duration",
        "--targets",
        "--seed",
        "--no-weather",
        "--conflict",
        "--speed",
        "--repeat",
        "--no-delay",
        "--quiet",
    ];
    let valued: &[&str] = &[
        "--host",
        "--port",
        "--duration",
        "--targets",
        "--seed",
        "--speed",
    ];
    let mut i = 0;
    while i < rest.len() {
        let arg = &rest[i];
        if arg.starts_with('-') {
            if !KNOWN.contains(&arg.as_str()) {
                bail!("unrecognised option {arg:?}\n\n{USAGE}");
            }
            if valued.contains(&arg.as_str()) {
                i += 1;
            }
        } else {
            bail!("unexpected argument {arg:?}\n\n{USAGE}");
        }
        i += 1;
    }

    let secs = |default: u64| -> Result<Duration> {
        match flag("--duration") {
            Some(v) => Ok(Duration::from_secs(
                v.parse().with_context(|| format!("bad --duration {v:?}"))?,
            )),
            None => Ok(Duration::from_secs(default)),
        }
    };

    let command = match verb.as_str() {
        "record" => Command::Record {
            out: path.context("record needs an output path")?,
            host: flag("--host").cloned().unwrap_or_else(|| "127.0.0.1".into()),
            port: match flag("--port") {
                Some(v) => v.parse().with_context(|| format!("bad --port {v:?}"))?,
                None => 80,
            },
            duration: secs(60)?,
        },
        "synth" => {
            let mut config = synth::SynthConfig {
                duration: secs(120)?,
                weather: !present("--no-weather"),
                conflict: present("--conflict"),
                ..Default::default()
            };
            if let Some(v) = flag("--targets") {
                config.target_count = v.parse().with_context(|| format!("bad --targets {v:?}"))?;
            }
            if let Some(v) = flag("--seed") {
                config.seed = v.parse().with_context(|| format!("bad --seed {v:?}"))?;
            }
            Command::Synth {
                out: path.context("synth needs an output path")?,
                config,
            }
        }
        "stats" => Command::Stats {
            input: path.context("stats needs an input path")?,
        },
        "play" => {
            let mut config = record::ReplayConfig {
                repeat: present("--repeat"),
                no_delay: present("--no-delay"),
                ..Default::default()
            };
            if let Some(v) = flag("--speed") {
                let speed: f64 = v.parse().with_context(|| format!("bad --speed {v:?}"))?;
                if speed <= 0.0 {
                    bail!("--speed must be greater than zero");
                }
                config.speed = speed;
            }
            Command::Play {
                input: path.context("play needs an input path")?,
                config,
                quiet: present("--quiet"),
            }
        }
        other => bail!("unknown command {other:?}\n\n{USAGE}"),
    };

    Ok(Some(command))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let Some(command) = parse_args()? else {
        print!("{USAGE}");
        return Ok(());
    };

    match command {
        Command::Record {
            out,
            host,
            port,
            duration,
        } => do_record(&out, host, port, duration).await,
        Command::Synth { out, config } => do_synth(&out, &config),
        Command::Stats { input } => do_stats(&input),
        Command::Play {
            input,
            config,
            quiet,
        } => do_play(&input, config, quiet).await,
    }
}

async fn do_record(out: &Path, host: String, port: u16, duration: Duration) -> Result<()> {
    let config = live::LiveConfig {
        host: host.clone(),
        port,
        ..Default::default()
    };
    tracing::info!(%host, port, ?duration, "recording");

    let mut rx = live::spawn(config);
    let mut recorder = record::Recorder::create(out)?;
    let deadline = Instant::now() + duration;
    let mut connected = 0usize;

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            // Timed out: the deadline passed with no traffic, which is normal at the tail end.
            Err(_) => break,
            Ok(None) => break,
            Ok(Some(event)) => match event {
                SourceEvent::Frame(frame) => recorder.write(&frame)?,
                SourceEvent::Connected(stream) => {
                    connected += 1;
                    tracing::info!(stream = stream.name(), "connected");
                }
                SourceEvent::Disconnected { stream, reason } => {
                    tracing::warn!(stream = stream.name(), %reason, "disconnected");
                }
                SourceEvent::EndOfStream => break,
            },
        }
    }

    let written = recorder.finish()?;
    if written == 0 {
        // Far more useful than silently leaving a zero-byte file behind.
        bail!(
            "recorded no frames from {host}:{port} ({connected} successful connects). \
             Is Stratux running and reachable? Try: curl -s http://{host}:{port}/getStatus"
        );
    }
    tracing::info!(frames = written, path = %out.display(), "recording complete");
    Ok(())
}

fn do_synth(out: &Path, config: &synth::SynthConfig) -> Result<()> {
    let frames = synth::generate(config);
    let mut recorder = record::Recorder::create(out)?;
    for frame in &frames {
        recorder.write(frame)?;
    }
    let written = recorder.finish()?;
    println!(
        "Synthesised {written} frames over {:?} ({} targets, weather {}) -> {}",
        config.duration,
        config.target_count,
        if config.weather { "on" } else { "off" },
        out.display()
    );
    println!("Seed {} — regenerating with the same seed gives byte-identical output.", config.seed);
    Ok(())
}

fn do_stats(input: &Path) -> Result<()> {
    let frames = record::read_all(input)?;
    let summary = record::summarise(&frames);

    println!("{}", input.display());
    println!("  frames    : {}", summary.frames);
    println!("  duration  : {:.1} s", summary.duration.as_secs_f64());
    println!(
        "  payload   : {:.1} KiB",
        summary.bytes as f64 / 1024.0
    );
    println!("  per stream:");
    for (stream, count) in &summary.per_stream {
        let rate = if summary.duration.as_secs_f64() > 0.0 {
            *count as f64 / summary.duration.as_secs_f64()
        } else {
            0.0
        };
        println!("      {stream:<10} {count:>7}  ({rate:.1}/s)");
    }

    // Decode everything to show what the display would actually get out of it. This is the
    // check that matters: a recording full of frames we cannot decode looks fine by frame count.
    let mut state = AppState::new();
    let now = Instant::now();
    for frame in &frames {
        state.apply(&SourceEvent::Frame(frame.clone()), now);
    }
    println!("  decoded   :");
    println!("      targets        {}", state.targets.len());
    println!("      weather texts  {}", state.weather.len());
    println!("      NEXRAD blocks  {}", state.nexrad.len());
    println!(
        "      own-ship       {}",
        match state.ownship.usable_position() {
            Some(p) => format!("{:.4}, {:.4} ({})", p.lat, p.lon, state.ownship.fix.label()),
            None => "none".into(),
        }
    );
    println!("      decode errors  {}", state.decode_errors);
    if state.decode_errors > 0 {
        println!("      ^^ non-zero: run with RUST_LOG=debug to see why");
    }
    Ok(())
}

async fn do_play(input: &Path, config: record::ReplayConfig, quiet: bool) -> Result<()> {
    let frames = record::read_all(input)?;
    if frames.is_empty() {
        bail!("{} contains no usable frames", input.display());
    }
    tracing::info!(frames = frames.len(), "replaying");

    let mut rx = record::spawn(frames, config);
    let mut state = AppState::new();
    let policy = AgePolicy::default();
    let mut last_report = Instant::now();

    while let Some(event) = rx.recv().await {
        let now = Instant::now();
        let end = matches!(event, SourceEvent::EndOfStream);
        state.apply(&event, now);
        state.prune(now, &policy);

        if !quiet && last_report.elapsed() >= Duration::from_secs(1) {
            last_report = now;
            report(&state, now);
        }
        if end {
            break;
        }
    }

    println!();
    report(&state, Instant::now());
    Ok(())
}

fn report(state: &AppState, now: Instant) {
    let own = match state.ownship.usable_position() {
        Some(LatLon { lat, lon }) => format!("{lat:8.4},{lon:9.4}"),
        None => "    no fix        ".into(),
    };
    let stale = state.stale_streams(now);
    let stale_text = if stale.is_empty() {
        "-".to_string()
    } else {
        stale.iter().map(|s| s.name()).collect::<Vec<_>>().join(",")
    };

    println!(
        "own {own} | trk {:>3} | tfc {:>3} (+{} no-pos) | wx {:>3} | nexrad {:>4} | {:.0}C | stale: {}",
        state
            .ownship
            .track_deg
            .map(|t| format!("{t:.0}"))
            .unwrap_or_else(|| "---".into()),
        state.positional_targets().count(),
        state.non_positional_count(),
        state.weather.len(),
        state.nexrad.len(),
        state.status.cpu_temp_c,
        stale_text,
    );
}
