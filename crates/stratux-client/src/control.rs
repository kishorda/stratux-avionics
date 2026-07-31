//! Control requests **to** Stratux, as opposed to the streams we read from it.
//!
//! Everything else in this crate is read-only: five WebSockets and a fold into [`AppState`].
//! This module is the one place that asks Stratux to *do* something, and it is deliberately tiny.
//!
//! # Why this speaks HTTP by hand
//!
//! The one request needed is a bodyless `POST` to `127.0.0.1`. Pulling in a full HTTP client for
//! that would add a large dependency tree — connection pooling, TLS, redirect handling, cookie
//! jars — to a binary that flies in an aircraft, in exchange for about thirty lines. None of that
//! machinery applies here: the destination is loopback, there is no TLS, no authentication, no
//! redirect, and no response body worth parsing. So the request is written out directly.
//!
//! This is *not* a general HTTP client and must not grow into one. If a second, more complicated
//! request ever appears, that is the moment to reconsider — not now.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// How long the whole exchange may take before we give up.
///
/// Short on purpose. This is driven by a button press with a spinner on screen, and a request
/// that hangs leaves the pilot looking at an instrument whose state they cannot determine.
/// Failing quickly and saying so beats waiting quietly.
const TIMEOUT: Duration = Duration::from_secs(4);

/// Tell Stratux the aircraft is straight and level, zeroing the AHRS reference.
///
/// This is upstream's `/cageAHRS`, the same endpoint its own web UI posts to.
///
/// # This changes the attitude reference for everything
///
/// Caging does not adjust our display, it re-references the sensor. The corrected attitude then
/// flows to every consumer — this display, the GDL90 stream feeding a tablet, the AHRS logs.
/// That is the right behaviour, and it is also why caging while *not* straight and level is
/// actively harmful: it teaches the sensor that a banked, pitched attitude is level, and
/// everything downstream inherits the error with no indication anything happened.
///
/// The caller is responsible for making this hard to trigger by accident.
pub async fn cage_ahrs(host: &str, port: u16) -> Result<()> {
    post(host, port, "/cageAHRS").await
}

/// Run the full gyro/accelerometer calibration. Must be stationary; takes several seconds.
///
/// Not currently wired to anything on the display — recorded here because it sits next to
/// `cageAHRS` upstream and is the obvious next request someone will reach for. Caging and
/// calibrating are different operations and should not be conflated behind one button.
pub async fn calibrate_ahrs(host: &str, port: u16) -> Result<()> {
    post(host, port, "/calibrateAHRS").await
}

async fn post(host: &str, port: u16, path: &str) -> Result<()> {
    tokio::time::timeout(TIMEOUT, post_inner(host, port, path))
        .await
        .with_context(|| format!("{path} timed out after {TIMEOUT:?}"))?
}

async fn post_inner(host: &str, port: u16, path: &str) -> Result<()> {
    let mut stream = TcpStream::connect((host, port))
        .await
        .with_context(|| format!("connecting to {host}:{port}"))?;

    // `Connection: close` so the response ends at EOF and there is no keep-alive framing to
    // parse. `Content-Length: 0` because a POST without one is ambiguous to some servers.
    let request = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {host}:{port}\r\n\
         Content-Length: 0\r\n\
         Connection: close\r\n\
         \r\n"
    );
    stream
        .write_all(request.as_bytes())
        .await
        .with_context(|| format!("sending {path}"))?;
    stream.flush().await.ok();

    // The response is small; read it whole rather than framing incrementally.
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .with_context(|| format!("reading the response to {path}"))?;

    let status = status_code(&response)
        .with_context(|| format!("no HTTP status line in the response to {path}"))?;
    if !(200..300).contains(&status) {
        bail!("{path} returned HTTP {status}");
    }
    Ok(())
}

/// Pull the status code out of an HTTP response's first line.
fn status_code(response: &[u8]) -> Option<u16> {
    let head = response.split(|b| *b == b'\n').next()?;
    let line = std::str::from_utf8(head).ok()?;
    // "HTTP/1.1 200 OK" -> 200
    line.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_codes_are_parsed_from_the_first_line() {
        assert_eq!(status_code(b"HTTP/1.1 200 OK\r\n\r\n"), Some(200));
        assert_eq!(status_code(b"HTTP/1.0 204 No Content\r\n"), Some(204));
        assert_eq!(status_code(b"HTTP/1.1 500 Internal Server Error\r\n"), Some(500));
    }

    #[test]
    fn a_response_that_is_not_http_is_rejected_rather_than_assumed_ok() {
        // Whatever this is, it is not a success we should report to the pilot as one.
        assert_eq!(status_code(b""), None);
        assert_eq!(status_code(b"garbage\r\n"), None);
        assert_eq!(status_code(b"HTTP/1.1\r\n"), None);
    }

    #[tokio::test]
    async fn a_non_2xx_response_is_an_error() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 512];
            let _ = socket.read(&mut buf).await;
            let _ = socket
                .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n")
                .await;
        });

        let err = cage_ahrs("127.0.0.1", port).await.unwrap_err();
        assert!(err.to_string().contains("503"), "got: {err}");
    }

    #[tokio::test]
    async fn a_2xx_response_succeeds() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 512];
            let _ = socket.read(&mut buf).await;
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await;
        });

        cage_ahrs("127.0.0.1", port).await.unwrap();
    }

    #[tokio::test]
    async fn a_refused_connection_is_an_error_not_a_hang() {
        // Port 1 on loopback is not listening; this must fail fast rather than block the button.
        let err = cage_ahrs("127.0.0.1", 1).await.unwrap_err();
        assert!(err.to_string().contains("connecting"), "got: {err}");
    }
}
