//! Reads traffic, own-ship position, FIS-B weather and backend health from a local
//! [Stratux](https://github.com/stratux/stratux) instance.
//!
//! # Shape
//!
//! Everything funnels through one channel of [`SourceEvent`], and there are two things that can
//! fill it: [`live`] (WebSockets to a real Stratux) and [`record`] (a replay of a recorded or
//! synthesised session). Consumers cannot tell them apart, which is the point — the plan view is
//! developed and tested against replays and flown against live data with no code change.
//!
//! ```text
//!   live::spawn ─┐
//!                ├─> mpsc::Receiver<SourceEvent> ─> decode() ─> state::AppState
//!   record::spawn┘                      │
//!                                       └─> record::Recorder  (tees raw frames to disk)
//! ```
//!
//! Recording taps [`Frame`]s *before* decoding, so a recording holds exactly the bytes Stratux
//! sent. That means a parser bug found in flight can be reproduced on the bench.
//!
//! # Which sockets, and why
//!
//! | Socket | Rate | Carries |
//! |---|---|---|
//! | `/traffic` | on change (replays current traffic on connect) | one `TrafficInfo` per message |
//! | `/situation` | 10 Hz | own-ship `SituationData` |
//! | `/weather` | on receipt | FIS-B text products |
//! | `/status` | 1 Hz | backend health |
//! | `/jsonio` | on change | **NEXRAD only** — see below |
//!
//! NEXRAD blocks come from Stratux's `weatherRawUpdate` broadcaster, and `/jsonio` is the only
//! place it is exposed. That socket multiplexes four unrelated object types with no envelope and
//! no discriminator, so [`decode::classify`] identifies shapes by key presence and everything
//! that isn't a `UATFrame` is discarded — the dedicated sockets above carry those with known
//! types. Quarantining the fragile stream to one purpose means an upstream change there degrades
//! the weather underlay instead of the whole display.

pub mod control;
pub mod decode;
pub mod domain;
pub mod live;
pub mod record;
pub mod state;
pub mod synth;
pub mod wire;

pub use decode::{decode, Event};
pub use state::AppState;

use std::time::Duration;

/// One of Stratux's WebSocket endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Stream {
    Traffic,
    Situation,
    Weather,
    Status,
    /// Subscribed to solely for NEXRAD blocks.
    JsonIo,
}

impl Stream {
    pub const ALL: [Stream; 5] = [
        Stream::Traffic,
        Stream::Situation,
        Stream::Weather,
        Stream::Status,
        Stream::JsonIo,
    ];

    /// HTTP path of the endpoint.
    pub fn path(&self) -> &'static str {
        match self {
            Stream::Traffic => "/traffic",
            Stream::Situation => "/situation",
            Stream::Weather => "/weather",
            Stream::Status => "/status",
            Stream::JsonIo => "/jsonio",
        }
    }

    /// Stable short name used in recording files. Changing these breaks existing recordings.
    pub fn name(&self) -> &'static str {
        match self {
            Stream::Traffic => "traffic",
            Stream::Situation => "situation",
            Stream::Weather => "weather",
            Stream::Status => "status",
            Stream::JsonIo => "jsonio",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.name() == name)
    }

    /// How long this stream may go quiet before it should be treated as stale.
    ///
    /// These are per-stream because the natural rates differ by orders of magnitude: own-ship
    /// arrives at 10 Hz so a 3 s gap is alarming, whereas `/weather` is genuinely silent for
    /// minutes at a time in normal operation and a short timeout there would show a permanent
    /// false alarm.
    pub fn staleness_timeout(&self) -> Duration {
        match self {
            Stream::Situation => Duration::from_secs(3),
            Stream::Status => Duration::from_secs(10),
            Stream::Traffic => Duration::from_secs(60),
            // FIS-B products cycle on the order of minutes; NEXRAD every ~5 minutes.
            Stream::Weather | Stream::JsonIo => Duration::from_secs(600),
        }
    }
}

/// A raw, undecoded message as it arrived.
#[derive(Debug, Clone)]
pub struct Frame {
    pub stream: Stream,
    /// Time since the source started. Used to reproduce timing on replay.
    pub offset: Duration,
    /// The exact bytes received.
    pub payload: Vec<u8>,
}

/// Anything a source can report.
#[derive(Debug, Clone)]
pub enum SourceEvent {
    Frame(Frame),
    Connected(Stream),
    Disconnected {
        stream: Stream,
        reason: String,
    },
    /// A replay reached the end of its recording. Live sources never emit this.
    EndOfStream,
}
