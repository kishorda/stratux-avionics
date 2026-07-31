//! Folds a stream of events into a coherent picture of the world.
//!
//! This is the data layer only. It holds what has been received, ages out what is too old, and
//! tracks which streams are healthy. It deliberately does **not** dead-reckon target positions,
//! classify threats, or project anything — that is the plan view's job (M4), and mixing it in
//! here would make the state untestable without a renderer.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use crate::decode::{decode, Event};
use crate::domain::{BlockKey, NexradBlock, OwnShip, SystemStatus, Target, WeatherText};
use crate::{SourceEvent, Stream};

/// How long data stays relevant.
#[derive(Debug, Clone)]
pub struct AgePolicy {
    /// Targets vanish this long after their last report.
    pub target_timeout: Duration,
    /// NEXRAD blocks expire this long after receipt.
    pub nexrad_timeout: Duration,
    /// Text products expire this long after receipt.
    pub weather_timeout: Duration,
}

impl Default for AgePolicy {
    fn default() -> Self {
        Self {
            // Comfortably longer than a 1 Hz ADS-B update but short enough that a target which
            // has genuinely gone doesn't linger as a ghost.
            target_timeout: Duration::from_secs(45),
            // Products refresh about every 5 minutes; 15 tolerates two missed cycles before
            // declaring the picture gone.
            nexrad_timeout: Duration::from_secs(15 * 60),
            weather_timeout: Duration::from_secs(2 * 60 * 60),
        }
    }
}

/// Liveness of one socket.
#[derive(Debug, Clone, Default)]
pub struct StreamHealth {
    pub connected: bool,
    pub last_frame: Option<Instant>,
    pub frames: u64,
    pub last_error: Option<String>,
}

/// Identity of a stored text product.
///
/// METAR/TAF/WINDS are keyed by station so a fresh observation replaces the previous one. The
/// rest (PIREP, NOTAM, SIGMET, AIRMET) legitimately have several concurrent reports for the same
/// location, so the body is part of the key and they accumulate instead.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct WeatherKey {
    pub product: String,
    pub location: String,
    pub discriminator: String,
}

impl WeatherKey {
    fn for_text(text: &WeatherText) -> Self {
        use crate::domain::WeatherProduct::*;
        let replaces_previous = matches!(text.product, Metar | Taf | Winds);
        Self {
            product: text.product.label().to_string(),
            location: text.location.clone(),
            discriminator: if replaces_previous {
                String::new()
            } else {
                text.body.clone()
            },
        }
    }
}

#[derive(Debug, Default)]
pub struct AppState {
    pub ownship: OwnShip,
    pub targets: HashMap<u32, Target>,
    pub weather: BTreeMap<WeatherKey, WeatherText>,
    pub nexrad: HashMap<BlockKey, NexradBlock>,
    pub status: SystemStatus,
    pub streams: BTreeMap<Stream, StreamHealth>,
    /// Frames that could not be decoded. Non-zero means an upstream change worth investigating.
    pub decode_errors: u64,
    /// Bumped whenever the NEXRAD block set changes.
    ///
    /// The plan view composites blocks into a single texture, which is far too expensive to redo
    /// every frame. This is the cheap "has anything actually changed" signal that lets it cache.
    pub nexrad_revision: u64,
    /// Set once any usable own-ship position has been seen.
    ///
    /// Distinguishes "no fix yet" from "the field we read got renamed and we are silently
    /// reading zeroes", which would otherwise look identical on screen.
    pub ever_had_position: bool,
}

impl AppState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one event from a source.
    pub fn apply(&mut self, event: &SourceEvent, now: Instant) {
        match event {
            SourceEvent::Connected(stream) => {
                let health = self.streams.entry(*stream).or_default();
                health.connected = true;
                health.last_error = None;
            }
            SourceEvent::Disconnected { stream, reason } => {
                let health = self.streams.entry(*stream).or_default();
                health.connected = false;
                health.last_error = Some(reason.clone());
                // Weather and NEXRAD are deliberately *not* cleared here. Stratux's
                // handleWeatherWS only subscribes the socket to future broadcasts — it does not
                // replay the current buffer on connect, whatever the HTTP API docs say. Dropping
                // what we have on a brief reconnect would blank the weather for minutes until
                // the next FIS-B cycle.
            }
            SourceEvent::EndOfStream => {
                for health in self.streams.values_mut() {
                    health.connected = false;
                }
            }
            SourceEvent::Frame(frame) => {
                let health = self.streams.entry(frame.stream).or_default();
                health.last_frame = Some(now);
                health.frames += 1;

                match decode(frame.stream, &frame.payload, now) {
                    Ok(Some(decoded)) => self.apply_event(decoded),
                    Ok(None) => {}
                    Err(e) => {
                        self.decode_errors += 1;
                        tracing::debug!(
                            stream = frame.stream.name(),
                            error = %e,
                            "could not decode a frame"
                        );
                    }
                }
            }
        }
    }

    pub fn apply_event(&mut self, event: Event) {
        match event {
            Event::Traffic(target) => {
                self.targets.insert(target.icao, target);
            }
            Event::OwnShip(ownship) => {
                if ownship.position.is_some() {
                    self.ever_had_position = true;
                }
                self.ownship = ownship;
            }
            Event::Weather(text) => {
                self.weather.insert(WeatherKey::for_text(&text), text);
            }
            Event::Nexrad(blocks) => {
                for block in blocks {
                    // A retransmitted block supersedes the one it duplicates.
                    self.nexrad.insert(block.key(), block);
                }
                self.nexrad_revision = self.nexrad_revision.wrapping_add(1);
            }
            Event::Status(status) => self.status = status,
        }
    }

    /// Drop everything that has aged out. Call once per frame; it is cheap.
    pub fn prune(&mut self, now: Instant, policy: &AgePolicy) {
        self.targets
            .retain(|_, t| now.duration_since(t.received) < policy.target_timeout);
        let nexrad_before = self.nexrad.len();
        self.nexrad
            .retain(|_, b| now.duration_since(b.received) < policy.nexrad_timeout);
        if self.nexrad.len() != nexrad_before {
            // Expiry changes the picture just as much as arrival does.
            self.nexrad_revision = self.nexrad_revision.wrapping_add(1);
        }
        self.weather
            .retain(|_, w| now.duration_since(w.received) < policy.weather_timeout);
    }

    /// Whether a stream has gone quiet for longer than its natural rate allows.
    ///
    /// A stream that has never delivered anything counts as stale, so the display shows a
    /// warning at startup rather than a confident empty screen.
    pub fn is_stale(&self, stream: Stream, now: Instant) -> bool {
        match self.streams.get(&stream).and_then(|h| h.last_frame) {
            Some(last) => now.duration_since(last) > stream.staleness_timeout(),
            None => true,
        }
    }

    /// Streams that are stale right now.
    pub fn stale_streams(&self, now: Instant) -> Vec<Stream> {
        Stream::ALL
            .into_iter()
            .filter(|s| self.is_stale(*s, now))
            .collect()
    }

    /// Targets that can be drawn on a plan view.
    pub fn positional_targets(&self) -> impl Iterator<Item = &Target> {
        self.targets.values().filter(|t| t.is_positional())
    }

    /// Targets heard but without a position — Mode-S only, or ADS-B before the first position
    /// report. Worth a count in the status bar so the pilot knows they exist.
    pub fn non_positional_count(&self) -> usize {
        self.targets.values().filter(|t| !t.is_positional()).count()
    }

    /// Age of the most recently received NEXRAD block.
    ///
    /// This is what tells the pilot whether the precipitation on screen is current. FIS-B
    /// products cycle roughly every 5 minutes, so anything much older means reception has stopped.
    pub fn nexrad_age(&self, now: Instant) -> Option<Duration> {
        self.nexrad
            .values()
            .map(|b| now.saturating_duration_since(b.received))
            .min()
    }

    /// NEXRAD blocks with anything worth drawing.
    pub fn nexrad_with_precipitation(&self) -> impl Iterator<Item = &NexradBlock> {
        self.nexrad.values().filter(|b| b.has_precipitation())
    }
}
