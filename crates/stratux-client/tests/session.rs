//! State folding, ageing, staleness, and the record/replay round-trip.

use std::time::{Duration, Instant};

use stratux_client::decode::Event;
use stratux_client::domain::{
    NexradBlock, NexradKind, OwnShip, Target, TargetType, TrafficSource, WeatherProduct,
    WeatherText,
};
use stratux_client::state::AgePolicy;
use stratux_client::{record, synth, AppState, Frame, SourceEvent, Stream};

fn target(icao: u32, at: Instant) -> Target {
    Target {
        icao,
        identity: Some(format!("N{icao}")),
        position: Some(stratux_client::domain::LatLon::new(39.9, -105.1)),
        altitude_ft: Some(8000),
        altitude_is_gnss: false,
        on_ground: false,
        track_deg: Some(90.0),
        ground_speed_kt: Some(120),
        vertical_speed_fpm: Some(0),
        emitter_category: 1,
        target_type: TargetType::Adsb,
        source: TrafficSource::Es1090,
        signal_level_db: -20.0,
        squawk: Some(1200),
        extrapolated: false,
        age_s: 0.5,
        age_last_alt_s: 0.5,
        reported_bearing_deg: None,
        reported_distance_m: None,
        received: at,
    }
}

fn nexrad(at: Instant) -> NexradBlock {
    NexradBlock {
        kind: NexradKind::Regional,
        scale: 1,
        lat_north: 40.0,
        lon_west: -105.6,
        height_deg: 4.0 / 60.0,
        width_deg: 48.0 / 60.0,
        bins: vec![5; NexradBlock::BIN_COUNT],
        received: at,
    }
}

fn weather(product: WeatherProduct, location: &str, body: &str, at: Instant) -> WeatherText {
    WeatherText {
        product,
        location: location.into(),
        time: "291853Z".into(),
        body: body.into(),
        received: at,
    }
}

// --- ageing -------------------------------------------------------------------------------

#[test]
fn stale_targets_are_pruned_but_fresh_ones_survive() {
    let mut state = AppState::new();
    let now = Instant::now();
    let policy = AgePolicy::default();

    let old = now - policy.target_timeout - Duration::from_secs(1);
    state.apply_event(Event::Traffic(target(1, old)));
    state.apply_event(Event::Traffic(target(2, now)));
    assert_eq!(state.targets.len(), 2);

    state.prune(now, &policy);
    assert_eq!(state.targets.len(), 1);
    assert!(state.targets.contains_key(&2), "the fresh target must remain");
}

#[test]
fn nexrad_blocks_expire_on_their_own_much_longer_timeout() {
    // Traffic ages out in tens of seconds; NEXRAD must not, or the mosaic would flicker away
    // between the ~5 minute product cycles.
    let mut state = AppState::new();
    let now = Instant::now();
    let policy = AgePolicy::default();

    let past_traffic_timeout = now - policy.target_timeout - Duration::from_secs(1);
    state.apply_event(Event::Nexrad(vec![nexrad(past_traffic_timeout)]));
    state.prune(now, &policy);
    assert_eq!(state.nexrad.len(), 1, "NEXRAD must outlive the traffic timeout");

    let mut state = AppState::new();
    state.apply_event(Event::Nexrad(vec![nexrad(
        now - policy.nexrad_timeout - Duration::from_secs(1),
    )]));
    state.prune(now, &policy);
    assert!(state.nexrad.is_empty(), "genuinely old NEXRAD must go");
}

#[test]
fn a_retransmitted_nexrad_block_replaces_rather_than_duplicates() {
    let mut state = AppState::new();
    let now = Instant::now();
    state.apply_event(Event::Nexrad(vec![nexrad(now)]));
    state.apply_event(Event::Nexrad(vec![nexrad(now)]));
    assert_eq!(state.nexrad.len(), 1);
}

// --- weather keying -----------------------------------------------------------------------

#[test]
fn a_new_metar_replaces_the_previous_one_for_that_station() {
    let mut state = AppState::new();
    let now = Instant::now();
    state.apply_event(Event::Weather(weather(
        WeatherProduct::Metar,
        "KDEN",
        "METAR KDEN 291753Z 04008KT",
        now,
    )));
    state.apply_event(Event::Weather(weather(
        WeatherProduct::Metar,
        "KDEN",
        "METAR KDEN 291853Z 04012KT",
        now,
    )));
    assert_eq!(state.weather.len(), 1, "a station has one current observation");
    assert!(state
        .weather
        .values()
        .any(|w| w.body.contains("291853Z")), "the newer one must win");
}

#[test]
fn multiple_concurrent_pireps_for_one_location_accumulate() {
    // Unlike METARs, several PIREPs near the same place are all valid simultaneously; keying
    // them by location alone would throw all but the last away.
    let mut state = AppState::new();
    let now = Instant::now();
    state.apply_event(Event::Weather(weather(
        WeatherProduct::Pirep,
        "KDEN",
        "UA /OV KDEN270015 /TB LGT",
        now,
    )));
    state.apply_event(Event::Weather(weather(
        WeatherProduct::Pirep,
        "KDEN",
        "UA /OV KDEN180020 /TB MOD",
        now,
    )));
    assert_eq!(state.weather.len(), 2);
}

// --- staleness and reconnection ------------------------------------------------------------

#[test]
fn a_stream_that_never_delivered_counts_as_stale() {
    // At startup the display must warn rather than show a confident empty screen.
    let state = AppState::new();
    let now = Instant::now();
    for stream in Stream::ALL {
        assert!(state.is_stale(stream, now), "{stream:?} should start stale");
    }
}

#[test]
fn staleness_uses_a_per_stream_timeout() {
    let mut state = AppState::new();
    let now = Instant::now();

    // A 4 s gap is alarming for 10 Hz own-ship but completely normal for weather.
    let four_seconds_ago = now - Duration::from_secs(4);
    for stream in [Stream::Situation, Stream::Weather] {
        state.apply(
            &SourceEvent::Frame(Frame {
                stream,
                offset: Duration::ZERO,
                payload: b"{}".to_vec(),
            }),
            four_seconds_ago,
        );
    }

    assert!(state.is_stale(Stream::Situation, now));
    assert!(!state.is_stale(Stream::Weather, now));
}

#[test]
fn a_disconnect_does_not_discard_weather_or_nexrad() {
    // Stratux's handleWeatherWS only subscribes to *future* broadcasts; it does not replay the
    // current buffer on connect, whatever the HTTP API docs claim. Clearing on a brief reconnect
    // would blank the weather for minutes until the next FIS-B cycle.
    let mut state = AppState::new();
    let now = Instant::now();
    state.apply_event(Event::Weather(weather(
        WeatherProduct::Metar,
        "KDEN",
        "METAR KDEN 291853Z",
        now,
    )));
    state.apply_event(Event::Nexrad(vec![nexrad(now)]));

    state.apply(
        &SourceEvent::Disconnected {
            stream: Stream::Weather,
            reason: "socket ended".into(),
        },
        now,
    );
    state.apply(
        &SourceEvent::Disconnected {
            stream: Stream::JsonIo,
            reason: "socket ended".into(),
        },
        now,
    );

    assert_eq!(state.weather.len(), 1, "weather must survive a reconnect");
    assert_eq!(state.nexrad.len(), 1, "NEXRAD must survive a reconnect");
    assert!(!state.streams[&Stream::Weather].connected);
}

#[test]
fn ever_had_position_distinguishes_no_fix_from_a_renamed_field() {
    // Both look like "no position on screen", but only one is a bug we can act on.
    let mut state = AppState::new();
    assert!(!state.ever_had_position);

    state.apply_event(Event::OwnShip(OwnShip::default()));
    assert!(!state.ever_had_position, "a default/no-fix situation proves nothing");

    state.apply_event(Event::OwnShip(OwnShip {
        position: Some(stratux_client::domain::LatLon::new(39.9, -105.1)),
        fix: stratux_client::domain::GpsFix::Differential,
        ..Default::default()
    }));
    assert!(state.ever_had_position);
}

#[test]
fn undecodable_frames_are_counted_not_fatal() {
    let mut state = AppState::new();
    let now = Instant::now();
    state.apply(
        &SourceEvent::Frame(Frame {
            stream: Stream::Traffic,
            offset: Duration::ZERO,
            payload: b"{ this is not json".to_vec(),
        }),
        now,
    );
    assert_eq!(state.decode_errors, 1);
    // The stream still counts as having delivered something, because it did.
    assert_eq!(state.streams[&Stream::Traffic].frames, 1);
}

#[test]
fn non_positional_targets_are_counted_separately_from_drawable_ones() {
    let mut state = AppState::new();
    let now = Instant::now();
    state.apply_event(Event::Traffic(target(1, now)));
    let mut mode_s = target(2, now);
    mode_s.position = None;
    state.apply_event(Event::Traffic(mode_s));

    assert_eq!(state.positional_targets().count(), 1);
    assert_eq!(state.non_positional_count(), 1);
}

// --- record / replay ----------------------------------------------------------------------

#[test]
fn recording_round_trips_frames_byte_for_byte() {
    let dir = std::env::temp_dir().join(format!("stratux-rec-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("round-trip.jsonl");

    let original = vec![
        Frame {
            stream: Stream::Traffic,
            offset: Duration::from_millis(0),
            payload: br#"{"Icao_addr":1,"Lat":39.9}"#.to_vec(),
        },
        Frame {
            stream: Stream::Situation,
            offset: Duration::from_millis(1500),
            payload: br#"{"GPSFixQuality":2}"#.to_vec(),
        },
        Frame {
            stream: Stream::JsonIo,
            offset: Duration::from_millis(3200),
            payload: br#"{"Product_id":63,"NEXRAD":[]}"#.to_vec(),
        },
    ];

    let mut recorder = record::Recorder::create(&path).unwrap();
    for frame in &original {
        recorder.write(frame).unwrap();
    }
    assert_eq!(recorder.finish().unwrap(), 3);

    let loaded = record::read_all(&path).unwrap();
    assert_eq!(loaded.len(), original.len());
    for (a, b) in original.iter().zip(&loaded) {
        assert_eq!(a.stream, b.stream);
        assert_eq!(a.offset, b.offset);
        // Byte-faithful: a parser bug seen in flight must reproduce on the bench.
        assert_eq!(a.payload, b.payload, "payload must round-trip exactly");
    }

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn truncated_recordings_still_load_what_is_intact() {
    // A recording cut short by a power loss is exactly the one worth analysing.
    let dir = std::env::temp_dir().join(format!("stratux-trunc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("truncated.jsonl");

    std::fs::write(
        &path,
        "{\"offset_ms\":0,\"stream\":\"traffic\",\"payload\":\"{}\"}\n\
         {\"offset_ms\":100,\"stream\":\"unknown_stream\",\"payload\":\"{}\"}\n\
         {\"offset_ms\":200,\"stream\":\"situation\",\"payl",
    )
    .unwrap();

    let loaded = record::read_all(&path).unwrap();
    assert_eq!(loaded.len(), 1, "keep the one good line, drop the rest");
    assert_eq!(loaded[0].stream, Stream::Traffic);

    std::fs::remove_dir_all(&dir).ok();
}

// --- synthetic sessions -------------------------------------------------------------------

#[test]
fn synthetic_sessions_are_deterministic_for_a_seed() {
    // A failing plan-view test must reproduce exactly.
    let config = synth::SynthConfig {
        duration: Duration::from_secs(10),
        ..Default::default()
    };
    let a = synth::generate(&config);
    let b = synth::generate(&config);

    assert_eq!(a.len(), b.len());
    for (x, y) in a.iter().zip(&b) {
        assert_eq!(x.stream, y.stream);
        assert_eq!(x.offset, y.offset);
        assert_eq!(x.payload, y.payload);
    }

    let different = synth::generate(&synth::SynthConfig {
        seed: config.seed ^ 0xFFFF,
        ..config.clone()
    });
    assert_ne!(
        a.iter().map(|f| f.payload.clone()).collect::<Vec<_>>(),
        different
            .iter()
            .map(|f| f.payload.clone())
            .collect::<Vec<_>>(),
        "a different seed should produce a different session"
    );
}

#[test]
fn a_synthetic_session_decodes_into_a_usable_picture() {
    // Guards the whole pipeline: synth serialises real wire structs, so if the decoder and the
    // wire definitions ever disagree, this fails.
    let config = synth::SynthConfig {
        duration: Duration::from_secs(90),
        target_count: 6,
        ..Default::default()
    };
    let frames = synth::generate(&config);
    assert!(!frames.is_empty());

    // Offsets must be non-decreasing or replay scheduling would go backwards.
    for pair in frames.windows(2) {
        assert!(pair[0].offset <= pair[1].offset);
    }

    let mut state = AppState::new();
    let now = Instant::now();
    for frame in &frames {
        state.apply(&SourceEvent::Frame(frame.clone()), now);
    }

    assert_eq!(state.decode_errors, 0, "synthetic data must decode cleanly");
    assert_eq!(state.targets.len(), config.target_count);
    assert!(state.ever_had_position);
    assert!(state.ownship.usable_position().is_some());
    assert!(state.ownship.track_deg.is_some(), "synth own-ship is moving");
    assert!(!state.weather.is_empty(), "weather was requested");
    assert!(!state.nexrad.is_empty(), "NEXRAD was requested");
    assert!(
        state.nexrad_with_precipitation().count() > 0,
        "the synthetic storm should show some precipitation"
    );
    assert!(state.status.cpu_temp_c > 0.0);
}

#[test]
fn synthetic_traffic_agrees_with_our_own_bearing_and_range() {
    // Stratux computes Bearing/Distance itself and the plan view computes its own; this checks
    // the two independent paths agree, which is the cross-check M4 relies on.
    let frames = synth::generate(&synth::SynthConfig {
        duration: Duration::from_secs(5),
        target_count: 4,
        weather: false,
        ..Default::default()
    });

    let mut state = AppState::new();
    let now = Instant::now();
    for frame in &frames {
        state.apply(&SourceEvent::Frame(frame.clone()), now);
    }

    let own = state.ownship.usable_position().expect("own-ship position");
    let mut checked = 0;

    for target in state.positional_targets() {
        let (Some(reported_bearing), Some(reported_distance), Some(position)) = (
            target.reported_bearing_deg,
            target.reported_distance_m,
            target.position,
        ) else {
            continue;
        };

        // Independent flat-earth solution.
        let nm_per_deg_lat = 60.0;
        let d_lat_nm = (position.lat - own.lat) * nm_per_deg_lat;
        let d_lon_nm = (position.lon - own.lon) * nm_per_deg_lat * own.lat.to_radians().cos();
        let distance_m = (d_lat_nm * d_lat_nm + d_lon_nm * d_lon_nm).sqrt() * 1852.0;
        let bearing = d_lon_nm.atan2(d_lat_nm).to_degrees().rem_euclid(360.0);

        // Own-ship advances between the traffic report and the last situation frame, so allow a
        // little slack rather than demanding an exact match.
        let bearing_error = ((bearing - reported_bearing + 540.0) % 360.0 - 180.0).abs();
        assert!(
            bearing_error < 5.0,
            "bearing disagreed by {bearing_error:.2} deg for {}",
            target.label()
        );
        let relative_error = (distance_m - reported_distance).abs() / reported_distance.max(1.0);
        assert!(
            relative_error < 0.05,
            "distance disagreed by {:.1}% for {}",
            relative_error * 100.0,
            target.label()
        );
        checked += 1;
    }

    assert!(checked >= 3, "expected several targets to cross-check");
}

// --- replay timing ------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn replay_reproduces_recorded_timing() {
    // With Tokio's clock paused, sleeps complete instantly but the *virtual* elapsed time still
    // reflects the recorded offsets, so this asserts pacing without actually waiting.
    let frames = vec![
        Frame {
            stream: Stream::Traffic,
            offset: Duration::from_millis(0),
            payload: br#"{"Icao_addr":1}"#.to_vec(),
        },
        Frame {
            stream: Stream::Traffic,
            offset: Duration::from_millis(2000),
            payload: br#"{"Icao_addr":2}"#.to_vec(),
        },
    ];

    // Measure on the Tokio clock: std::time::Instant is not affected by `start_paused`.
    let started = tokio::time::Instant::now();
    let mut rx = record::spawn(frames, record::ReplayConfig::default());

    let mut frame_times = Vec::new();
    while let Some(event) = rx.recv().await {
        match event {
            SourceEvent::Frame(_) => frame_times.push(started.elapsed()),
            SourceEvent::EndOfStream => break,
            _ => {}
        }
    }

    assert_eq!(frame_times.len(), 2);
    assert!(
        frame_times[1] >= Duration::from_millis(2000),
        "second frame should be paced to its recorded offset, was {:?}",
        frame_times[1]
    );
}

#[tokio::test(start_paused = true)]
async fn replay_speed_multiplier_compresses_time() {
    let frames = vec![
        Frame {
            stream: Stream::Traffic,
            offset: Duration::from_millis(0),
            payload: br#"{"Icao_addr":1}"#.to_vec(),
        },
        Frame {
            stream: Stream::Traffic,
            offset: Duration::from_millis(4000),
            payload: br#"{"Icao_addr":2}"#.to_vec(),
        },
    ];

    let started = tokio::time::Instant::now();
    let mut rx = record::spawn(
        frames,
        record::ReplayConfig {
            speed: 4.0,
            ..Default::default()
        },
    );

    let mut last = Duration::ZERO;
    while let Some(event) = rx.recv().await {
        match event {
            SourceEvent::Frame(_) => last = started.elapsed(),
            SourceEvent::EndOfStream => break,
            _ => {}
        }
    }

    // 4 s of recording at 4x should land near 1 s, and definitely well short of 4 s.
    assert!(
        last >= Duration::from_millis(900) && last < Duration::from_millis(2000),
        "expected ~1s at 4x, got {last:?}"
    );
}

#[tokio::test]
async fn replay_announces_the_streams_present_in_the_recording() {
    let frames = vec![
        Frame {
            stream: Stream::Situation,
            offset: Duration::ZERO,
            payload: br#"{"GPSFixQuality":2}"#.to_vec(),
        },
        Frame {
            stream: Stream::Traffic,
            offset: Duration::ZERO,
            payload: br#"{"Icao_addr":1}"#.to_vec(),
        },
    ];

    let mut rx = record::spawn(
        frames,
        record::ReplayConfig {
            no_delay: true,
            ..Default::default()
        },
    );

    let mut connected = Vec::new();
    while let Some(event) = rx.recv().await {
        match event {
            SourceEvent::Connected(stream) => connected.push(stream),
            SourceEvent::EndOfStream => break,
            _ => {}
        }
    }

    connected.sort();
    assert_eq!(connected, vec![Stream::Traffic, Stream::Situation]
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>());
}
