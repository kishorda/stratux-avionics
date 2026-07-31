//! WebSocket connections to a running Stratux.
//!
//! One task per stream, each reconnecting independently with exponential backoff. Independence
//! matters: if the weather socket is wedged, traffic and own-ship must keep flowing, because a
//! plan view with no weather is still airworthy and one with no traffic is not.

use std::time::{Duration, Instant};

use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::{Frame, SourceEvent, Stream};

#[derive(Debug, Clone)]
pub struct LiveConfig {
    /// Stratux's address. Loopback, because the display runs on the same Pi.
    pub host: String,
    /// Stratux's `ManagementAddr`, 80 by default.
    pub port: u16,
    pub streams: Vec<Stream>,
    pub reconnect: ReconnectPolicy,
    /// Channel depth. Sized to absorb the burst of existing traffic that `/traffic` replays on
    /// connect without blocking the socket reader.
    pub channel_capacity: usize,
}

impl Default for LiveConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 80,
            streams: Stream::ALL.to_vec(),
            reconnect: ReconnectPolicy::default(),
            channel_capacity: 1024,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReconnectPolicy {
    pub initial: Duration,
    pub max: Duration,
    pub multiplier: f64,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            // Aggressive first retry: on the aircraft, Stratux and the display start together
            // and the display will normally lose the race, so the common case is "try again
            // almost immediately".
            initial: Duration::from_millis(250),
            max: Duration::from_secs(10),
            multiplier: 2.0,
        }
    }
}

impl ReconnectPolicy {
    fn next(&self, current: Duration) -> Duration {
        let scaled = current.mul_f64(self.multiplier);
        if scaled > self.max {
            self.max
        } else {
            scaled
        }
    }
}

/// Connect to Stratux and start streaming events.
///
/// Spawns one task per configured stream onto the current Tokio runtime. Dropping the returned
/// receiver causes the tasks to notice the closed channel and exit.
pub fn spawn(config: LiveConfig) -> mpsc::Receiver<SourceEvent> {
    let (tx, rx) = mpsc::channel(config.channel_capacity);
    let started = Instant::now();

    for stream in config.streams.iter().copied() {
        let url = format!(
            "ws://{host}:{port}{path}",
            host = config.host,
            port = config.port,
            path = stream.path()
        );
        let tx = tx.clone();
        let policy = config.reconnect.clone();
        tokio::spawn(async move {
            run_stream(stream, url, tx, policy, started).await;
        });
    }

    rx
}

async fn run_stream(
    stream: Stream,
    url: String,
    tx: mpsc::Sender<SourceEvent>,
    policy: ReconnectPolicy,
    started: Instant,
) {
    let mut backoff = policy.initial;

    loop {
        if tx.is_closed() {
            return;
        }

        tracing::debug!(stream = stream.name(), %url, "connecting");
        match tokio_tungstenite::connect_async(&url).await {
            Ok((socket, _response)) => {
                tracing::info!(stream = stream.name(), "connected");
                // Only reset the backoff after a *successful connect*, so a server that accepts
                // and immediately drops us still backs off instead of spinning.
                backoff = policy.initial;

                if tx.send(SourceEvent::Connected(stream)).await.is_err() {
                    return;
                }

                let reason = pump(stream, socket, &tx, started).await;

                if tx
                    .send(SourceEvent::Disconnected { stream, reason })
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Err(e) => {
                let reason = e.to_string();
                tracing::warn!(stream = stream.name(), error = %reason, "connect failed");
                if tx
                    .send(SourceEvent::Disconnected { stream, reason })
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }

        tokio::time::sleep(backoff).await;
        backoff = policy.next(backoff);
    }
}

/// Read messages until the socket ends. Returns a human-readable reason.
async fn pump<S>(
    stream: Stream,
    socket: S,
    tx: &mpsc::Sender<SourceEvent>,
    started: Instant,
) -> String
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>,
{
    let mut socket = std::pin::pin!(socket);

    while let Some(message) = socket.next().await {
        let payload = match message {
            // Stratux uses golang.org/x/net/websocket, which sends text frames; binary is
            // accepted too so a future `/gdl90` subscription needs no change here.
            Ok(Message::Text(text)) => text.as_bytes().to_vec(),
            Ok(Message::Binary(bytes)) => bytes.to_vec(),
            Ok(Message::Close(frame)) => {
                return frame
                    .map(|f| format!("closed by peer: {}", f.reason))
                    .unwrap_or_else(|| "closed by peer".into());
            }
            // Ping/Pong/Frame: tungstenite answers pings itself, nothing to do.
            Ok(_) => continue,
            Err(e) => return e.to_string(),
        };

        let frame = Frame {
            stream,
            offset: started.elapsed(),
            payload,
        };

        if tx.send(SourceEvent::Frame(frame)).await.is_err() {
            return "consumer went away".into();
        }
    }

    "socket ended".into()
}
