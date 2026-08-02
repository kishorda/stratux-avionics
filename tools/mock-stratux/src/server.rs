//! A minimal HTTP + WebSocket server speaking Stratux's wire protocol.
//!
//! # What this is for, and why `--replay` is not enough
//!
//! `avionics --replay` pushes `SourceEvent`s straight into the render loop. That is the right tool
//! for testing what gets *drawn*, and it is why the plan view could be developed on a desk. But it
//! bypasses `stratux_client::live` completely: the WebSocket handshake, the five independent
//! reconnect loops with their backoff, the per-stream staleness clocks, the structural dispatch on
//! `/jsonio`, and the burst of existing traffic that `/traffic` replays the moment it connects.
//! None of that has ever run against anything but a real Pi.
//!
//! This closes that. `avionics --host 127.0.0.1 --port 8080` takes the ordinary live path and
//! talks to this instead, on a desk, offline.
//!
//! It also makes failure modes reachable that are awkward to stage on real hardware — dropping
//! every socket to watch the display reconnect, stalling one stream while the others keep running,
//! or emitting malformed JSON — because those are the paths whose correctness is least likely to
//! have been observed and most likely to matter in the air.
//!
//! # Deliberately not a complete Stratux
//!
//! It serves what the display reads and nothing else: five sockets and two GET endpoints. There is
//! no settings API, no web UI, no GDL90 output. Anything the display does not consume would be
//! untested scaffolding pretending to be a reference implementation.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::SinkExt;
use stratux_client::Stream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, Mutex};
use tokio_tungstenite::tungstenite::Message;

use crate::world::World;

/// Faults the mock can be asked to inject.
#[derive(Debug, Clone, Default)]
pub struct Faults {
    /// Close every open socket this often, so the reconnect path runs for real.
    pub drop_every: Option<Duration>,
    /// Accept these streams but never send on them, so the staleness clocks fire.
    pub stall: HashSet<Stream>,
    /// Emit a malformed frame every N messages on each stream.
    pub garbage_every: Option<u64>,
}

pub struct Config {
    pub port: u16,
    pub faults: Faults,
}

/// One broadcast channel per stream. Late subscribers miss nothing that matters: `/traffic`
/// replays the current picture explicitly on connect, exactly as Stratux does.
#[derive(Clone)]
struct Channels {
    traffic: broadcast::Sender<String>,
    situation: broadcast::Sender<String>,
    weather: broadcast::Sender<String>,
    status: broadcast::Sender<String>,
    jsonio: broadcast::Sender<String>,
}

impl Channels {
    fn new() -> Self {
        Self {
            traffic: broadcast::channel(1024).0,
            situation: broadcast::channel(64).0,
            weather: broadcast::channel(256).0,
            status: broadcast::channel(64).0,
            jsonio: broadcast::channel(64).0,
        }
    }

    fn for_stream(&self, stream: Stream) -> &broadcast::Sender<String> {
        match stream {
            Stream::Traffic => &self.traffic,
            Stream::Situation => &self.situation,
            Stream::Weather => &self.weather,
            Stream::Status => &self.status,
            Stream::JsonIo => &self.jsonio,
        }
    }
}

pub async fn serve(world: World, config: Config) -> Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", config.port))
        .await
        .with_context(|| format!("binding port {}", config.port))?;

    let world = Arc::new(Mutex::new(world));
    let channels = Channels::new();
    let faults = Arc::new(config.faults);

    // A generation counter, bumped when a fault drops every socket. Connection tasks watch it and
    // close when it moves, which is how `--drop-every` reaches sockets that are merely idle.
    let generation = Arc::new(std::sync::atomic::AtomicU64::new(0));

    tokio::spawn(publish(
        Arc::clone(&world),
        channels.clone(),
        Arc::clone(&faults),
    ));
    if let Some(period) = faults.drop_every {
        tokio::spawn(drop_sockets(period, Arc::clone(&generation)));
    }

    tracing::info!(port = config.port, "mock Stratux listening");
    loop {
        let (socket, peer) = listener.accept().await?;
        let world = Arc::clone(&world);
        let channels = channels.clone();
        let faults = Arc::clone(&faults);
        let generation = Arc::clone(&generation);
        tokio::spawn(async move {
            if let Err(e) = handle(socket, world, channels, faults, generation).await {
                tracing::debug!(%peer, error = %e, "connection closed");
            }
        });
    }
}

/// Bump the generation counter periodically. Connections notice and hang up.
async fn drop_sockets(period: Duration, generation: Arc<std::sync::atomic::AtomicU64>) {
    let mut ticker = tokio::time::interval(period);
    ticker.tick().await;
    loop {
        ticker.tick().await;
        let n = generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        tracing::warn!(generation = n, "dropping every socket (fault injection)");
    }
}

/// The publishing clock. Each stream runs at the rate the real Stratux uses.
async fn publish(world: Arc<Mutex<World>>, channels: Channels, faults: Arc<Faults>) {
    const TICK: Duration = Duration::from_millis(100);
    let mut ticker = tokio::time::interval(TICK);
    let mut ticks: u64 = 0;
    let mut sent: u64 = 0;

    loop {
        ticker.tick().await;
        ticks += 1;
        let mut w = world.lock().await;
        w.tick(TICK);

        let mut emit = |stream: Stream, payload: String| {
            if faults.stall.contains(&stream) {
                return;
            }
            sent += 1;
            let payload = match faults.garbage_every {
                // Truncated JSON: the shape a half-written frame actually has, and the one the
                // decoder's error counter exists for. A random byte string would be rejected by
                // the WebSocket layer instead and never reach the decoder at all.
                Some(n) if n > 0 && sent % n == 0 => "{\"Icao_addr\":".to_string(),
                _ => payload,
            };
            let _ = channels.for_stream(stream).send(payload);
        };

        // Own-ship at 10 Hz, exactly as `/situation` does.
        if let Ok(json) = serde_json::to_string(&w.situation()) {
            emit(Stream::Situation, json);
        }

        // Status once a second.
        if ticks % 10 == 0 {
            if let Ok(json) = serde_json::to_string(&w.status()) {
                emit(Stream::Status, json);
            }
        }

        // Traffic: every target once a second, spread across the ten ticks rather than sent as one
        // burst. Stratux publishes each target as it is heard, so a display that only ever sees
        // whole-sky batches is not being shown the arrival pattern it will meet in the air.
        let slice = (ticks % 10) as usize;
        let targets: Vec<String> = w
            .targets
            .iter()
            .enumerate()
            .filter(|(i, _)| i % 10 == slice)
            .filter_map(|(_, t)| serde_json::to_string(t).ok())
            .collect();
        for json in targets {
            emit(Stream::Traffic, json);
        }

        // Weather: one product every two seconds until the snapshot is exhausted. FIS-B is
        // opportunistic and arrives a product at a time; delivering the set in one frame would
        // never exercise the incremental path or the per-product ages.
        if ticks % 20 == 0 {
            if let Some(item) = w.next_weather() {
                if let Ok(json) = serde_json::to_string(&item) {
                    emit(Stream::Weather, json);
                }
            }
        }
    }
}

async fn handle(
    mut socket: TcpStream,
    world: Arc<Mutex<World>>,
    channels: Channels,
    faults: Arc<Faults>,
    generation: Arc<std::sync::atomic::AtomicU64>,
) -> Result<()> {
    // Peek rather than read: if this turns out to be a WebSocket upgrade, tungstenite needs the
    // handshake still sitting in the socket to parse it itself.
    let mut buf = vec![0u8; 4096];
    let n = socket.peek(&mut buf).await?;
    let head = String::from_utf8_lossy(&buf[..n]).to_string();

    let path = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_string();
    let is_upgrade = head.to_ascii_lowercase().contains("upgrade: websocket");

    if !is_upgrade {
        return serve_http(socket, &path, world).await;
    }

    let Some(stream) = Stream::ALL.into_iter().find(|s| s.path() == path) else {
        // Consume the request so the client sees a clean 404 rather than a reset.
        let _ = socket.read(&mut buf).await;
        socket
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
            .await?;
        return Ok(());
    };

    let ws = tokio_tungstenite::accept_async(socket).await?;
    tracing::info!(%path, "socket open");
    pump(ws, stream, world, channels, faults, generation).await
}

async fn pump(
    mut ws: tokio_tungstenite::WebSocketStream<TcpStream>,
    stream: Stream,
    world: Arc<Mutex<World>>,
    channels: Channels,
    faults: Arc<Faults>,
    generation: Arc<std::sync::atomic::AtomicU64>,
) -> Result<()> {
    let mut rx = channels.for_stream(stream).subscribe();
    let opened_at = generation.load(std::sync::atomic::Ordering::SeqCst);

    // `/traffic` replays every current target the moment a client connects, and `/weather`
    // deliberately does not. Both behaviours are load-bearing: the display relies on the first to
    // repopulate after a reconnect, and on the second not happening, which is why it never clears
    // weather on reconnect. A mock that got either backwards would make a real bug look fixed.
    if stream == Stream::Traffic && !faults.stall.contains(&stream) {
        let snapshot: Vec<String> = {
            let w = world.lock().await;
            w.targets
                .iter()
                .filter_map(|t| serde_json::to_string(t).ok())
                .collect()
        };
        for json in snapshot {
            ws.send(Message::Text(json.into())).await?;
        }
    }

    loop {
        if generation.load(std::sync::atomic::Ordering::SeqCst) != opened_at {
            ws.close(None).await.ok();
            return Ok(());
        }
        match tokio::time::timeout(Duration::from_millis(250), rx.recv()).await {
            Ok(Ok(payload)) => ws.send(Message::Text(payload.into())).await?,
            // Lagged means this client could not keep up. Stratux's own socket writer would drop
            // frames here too, so carrying on is the faithful behaviour.
            Ok(Err(broadcast::error::RecvError::Lagged(n))) => {
                tracing::debug!(stream = stream.name(), dropped = n, "client lagging")
            }
            Ok(Err(broadcast::error::RecvError::Closed)) => return Ok(()),
            // Timeout: nothing to send. Loop so the generation check runs.
            Err(_) => {}
        }
    }
}

/// The two GET endpoints the display's `--check` uses to decide Stratux is reachable.
async fn serve_http(mut socket: TcpStream, path: &str, world: Arc<Mutex<World>>) -> Result<()> {
    let mut sink = vec![0u8; 4096];
    let _ = socket.read(&mut sink).await;

    let body = {
        let w = world.lock().await;
        match path.split('?').next().unwrap_or(path) {
            "/getStatus" => serde_json::to_string(&w.status()).ok(),
            "/getSituation" => serde_json::to_string(&w.situation()).ok(),
            _ => None,
        }
    };

    let response = match body {
        Some(body) => format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
        None => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
    };
    socket.write_all(response.as_bytes()).await?;
    socket.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_stream_the_display_reads_has_a_channel() {
        // If a stream is ever added to the client, this fails rather than the mock silently
        // accepting the socket and never sending anything on it — which would look exactly like
        // the staleness fault this tool exists to inject deliberately.
        let channels = Channels::new();
        for stream in Stream::ALL {
            assert_eq!(
                channels.for_stream(stream).receiver_count(),
                0,
                "{stream:?} has a channel"
            );
        }
    }

    #[test]
    fn the_served_paths_are_the_ones_the_client_asks_for() {
        // The mock routes on `Stream::path()` rather than on its own copy of the strings, so the
        // two cannot drift. This pins that the paths are what Stratux actually serves.
        let paths: Vec<&str> = Stream::ALL.into_iter().map(|s| s.path()).collect();
        assert_eq!(
            paths,
            ["/traffic", "/situation", "/weather", "/status", "/jsonio"]
        );
    }
}
