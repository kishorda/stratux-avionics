//! Record a Stratux session to disk and replay it with the original timing.
//!
//! This is what makes the display testable without flying. A recorded session replays into the
//! same [`SourceEvent`] channel a live connection uses, so the UI cannot tell the difference.
//!
//! # File format
//!
//! JSON Lines, one frame per line:
//!
//! ```text
//! {"offset_ms":0,"stream":"situation","payload":"{\"GPSLatitude\":39.86,...}"}
//! ```
//!
//! `payload` is the **exact JSON text Stratux sent**, held as a string rather than as embedded
//! JSON. That costs some readability but keeps recordings byte-faithful, so a parser bug seen in
//! flight reproduces on the bench instead of being normalised away by a round-trip through
//! `serde_json`. To read one:
//!
//! ```sh
//! jq -r 'select(.stream=="traffic") | .payload' session.jsonl | jq .
//! ```

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::{Frame, SourceEvent, Stream};

#[derive(Debug, Serialize, Deserialize)]
struct RecordLine {
    offset_ms: u64,
    stream: String,
    payload: String,
}

/// Appends frames to a recording file.
pub struct Recorder {
    writer: BufWriter<File>,
    frames_written: u64,
}

impl Recorder {
    pub fn create(path: &Path) -> Result<Self> {
        let file =
            File::create(path).with_context(|| format!("creating recording {}", path.display()))?;
        Ok(Self {
            writer: BufWriter::new(file),
            frames_written: 0,
        })
    }

    pub fn write(&mut self, frame: &Frame) -> Result<()> {
        // Non-UTF-8 would mean Stratux sent something very unexpected; record it lossily rather
        // than aborting a session that is otherwise fine.
        let payload = String::from_utf8_lossy(&frame.payload).into_owned();
        let line = RecordLine {
            offset_ms: frame.offset.as_millis() as u64,
            stream: frame.stream.name().to_string(),
            payload,
        };
        serde_json::to_writer(&mut self.writer, &line).context("serialising a recorded frame")?;
        self.writer.write_all(b"\n")?;
        self.frames_written += 1;
        Ok(())
    }

    pub fn frames_written(&self) -> u64 {
        self.frames_written
    }

    /// Flush to disk. Call this before exiting or the tail of the session is lost.
    pub fn finish(mut self) -> Result<u64> {
        self.writer.flush().context("flushing the recording")?;
        Ok(self.frames_written)
    }
}

/// Load every frame from a recording, in file order.
///
/// Malformed lines are skipped with a warning rather than failing the load: a recording
/// truncated by a power cut is still useful, and the last line is exactly what gets truncated.
pub fn read_all(path: &Path) -> Result<Vec<Frame>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut frames = Vec::new();
    let mut skipped = 0usize;

    for (index, line) in reader.lines().enumerate() {
        let line =
            line.with_context(|| format!("reading {} line {}", path.display(), index + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        let parsed: RecordLine = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(line = index + 1, error = %e, "skipping malformed recording line");
                skipped += 1;
                continue;
            }
        };
        let Some(stream) = Stream::from_name(&parsed.stream) else {
            tracing::warn!(line = index + 1, stream = %parsed.stream, "skipping unknown stream");
            skipped += 1;
            continue;
        };
        frames.push(Frame {
            stream,
            offset: Duration::from_millis(parsed.offset_ms),
            payload: parsed.payload.into_bytes(),
        });
    }

    if skipped > 0 {
        tracing::warn!(skipped, kept = frames.len(), "recording had unusable lines");
    }
    Ok(frames)
}

#[derive(Debug, Clone)]
pub struct ReplayConfig {
    /// 1.0 replays at the original rate; 2.0 is twice as fast. Must be > 0.
    pub speed: f64,
    /// Restart from the beginning at the end, offsetting timestamps so it looks continuous.
    pub repeat: bool,
    /// Emit frames as fast as possible, ignoring recorded timing.
    ///
    /// Useful for tests that want determinism without waiting; useless for judging whether the
    /// display looks right, because real data arrives at very uneven rates.
    pub no_delay: bool,
    pub channel_capacity: usize,
}

impl Default for ReplayConfig {
    fn default() -> Self {
        Self {
            speed: 1.0,
            repeat: false,
            no_delay: false,
            channel_capacity: 1024,
        }
    }
}

/// Replay frames into a [`SourceEvent`] channel, reproducing the recorded timing.
pub fn spawn(frames: Vec<Frame>, config: ReplayConfig) -> mpsc::Receiver<SourceEvent> {
    let (tx, rx) = mpsc::channel(config.channel_capacity);
    let speed = if config.speed > 0.0 {
        config.speed
    } else {
        1.0
    };

    tokio::spawn(async move {
        // Announce the streams present in the recording so consumers see the same connection
        // events they would get from a live source.
        let mut present: Vec<Stream> = frames.iter().map(|f| f.stream).collect();
        present.sort();
        present.dedup();
        for stream in &present {
            if tx.send(SourceEvent::Connected(*stream)).await.is_err() {
                return;
            }
        }

        loop {
            // `tokio::time::Instant`, not `std::time::Instant`. In real runs the two are
            // equivalent, but only the Tokio clock can be paused or advanced, which is what makes
            // replay pacing assertable in tests without actually waiting.
            let epoch = tokio::time::Instant::now();
            for frame in &frames {
                if !config.no_delay {
                    // Schedule against a fixed epoch rather than sleeping by inter-frame deltas,
                    // so scheduling jitter cannot accumulate over a long session. `sleep_until`
                    // also returns immediately for a frame that is already overdue.
                    tokio::time::sleep_until(epoch + frame.offset.div_f64(speed)).await;
                }

                let emitted = Frame {
                    stream: frame.stream,
                    offset: epoch.elapsed(),
                    payload: frame.payload.clone(),
                };
                if tx.send(SourceEvent::Frame(emitted)).await.is_err() {
                    return;
                }
            }

            if !config.repeat {
                break;
            }
        }

        let _ = tx.send(SourceEvent::EndOfStream).await;
    });

    rx
}

/// Per-stream counts, for `replay stats`.
#[derive(Debug, Default)]
pub struct Summary {
    pub frames: u64,
    pub per_stream: std::collections::BTreeMap<&'static str, u64>,
    pub duration: Duration,
    pub bytes: u64,
}

pub fn summarise(frames: &[Frame]) -> Summary {
    let mut summary = Summary {
        frames: frames.len() as u64,
        duration: frames.last().map(|f| f.offset).unwrap_or_default(),
        ..Default::default()
    };
    for frame in frames {
        *summary.per_stream.entry(frame.stream.name()).or_default() += 1;
        summary.bytes += frame.payload.len() as u64;
    }
    summary
}
