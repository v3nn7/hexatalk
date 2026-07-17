//! WebSocket client for an existing peerseal-style relay.
//!
//! Protocol (status text frames from relay, then raw forward):
//! - Join: `WS {relay}/v1/room/{room_id}?token={token}`
//! - Status lines start with `⏳`, `✅`, or `❌` (not application data)
//! - After peer is connected, Text/Binary frames are opaque ciphertext

use crate::error::{Error, Result};
use crate::util::normalize_relay_url;
use futures_util::{SinkExt, StreamExt};
use std::sync::Once;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, DuplexStream, duplex};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};

/// Ensure rustls has a process-level crypto provider (ring) before any WSS connect.
fn ensure_rustls_provider() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // Idempotent: ignore error if another crate already installed a provider.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Whether a UTF-8 text frame is a relay control/status line (not app payload).
pub fn is_relay_status_line(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with('⏳')
        || t.starts_with('✅')
        || t.starts_with('❌')
        || t.starts_with("waiting")
        || t.starts_with("peer joined")
        || t.starts_with("connected")
}

fn status_means_peer_ready(text: &str) -> bool {
    let t = text.trim();
    t.starts_with('✅')
        || t.to_ascii_lowercase().contains("peer joined")
        || t.to_ascii_lowercase().contains("connected to peer")
}

fn status_means_error(text: &str) -> bool {
    let t = text.trim();
    t.starts_with('❌') || t.to_ascii_lowercase().starts_with("error")
}

/// Build join URL: `{base}/v1/room/{room_id}?token={token}`.
///
/// `relay_base` may be bare host / `https://` / `wss://` — normalized first.
pub fn room_join_url(relay_base: &str, room_id: &str, token: &str) -> Result<String> {
    let base = normalize_relay_url(relay_base)?;
    Ok(format!("{base}/v1/room/{room_id}?token={token}"))
}

/// Duplex byte stream bridged to a relay WebSocket (binary frames).
///
/// Implements [`AsyncRead`] + [`AsyncWrite`] via an internal [`DuplexStream`].
pub struct RelayConnection {
    inner: DuplexStream,
}

impl RelayConnection {
    /// Connect to the relay room and wait until the peer is joined (or timeout).
    pub async fn connect(
        relay_url: &str,
        room_id: &str,
        token: &str,
        wait_timeout: Duration,
    ) -> Result<Self> {
        ensure_rustls_provider();
        let url = room_join_url(relay_url, room_id, token)?;
        tracing::info!(%url, "connecting to relay");

        let request = url
            .as_str()
            .into_client_request()
            .map_err(|e| Error::Relay(format!("bad request: {e}")))?;

        let (ws, _resp) = connect_async(request)
            .await
            .map_err(|e| Error::Relay(format!("websocket connect: {e}")))?;

        let (mut sink, mut stream) = ws.split();

        // Buffered early application payload (e.g. binary before ✅).
        let mut early_payload: Option<Vec<u8>> = None;

        let peer_ready = tokio::time::timeout(wait_timeout, async {
            while let Some(msg) = stream.next().await {
                let msg = msg.map_err(|e| Error::Relay(format!("ws read: {e}")))?;
                match msg {
                    Message::Text(text) => {
                        tracing::debug!(%text, "relay status/text");
                        if status_means_error(&text) {
                            return Err(Error::Relay(format!("relay error status: {text}")));
                        }
                        if status_means_peer_ready(&text) {
                            return Ok(());
                        }
                        if is_relay_status_line(&text) {
                            continue;
                        }
                        // Non-status text before ready → peer already sending.
                        early_payload = Some(text.as_bytes().to_vec());
                        return Ok(());
                    }
                    Message::Binary(bin) => {
                        if !bin.is_empty() {
                            early_payload = Some(bin.to_vec());
                            return Ok(());
                        }
                    }
                    Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
                    Message::Close(c) => {
                        return Err(Error::Relay(format!("relay closed during wait: {c:?}")));
                    }
                }
            }
            Err(Error::Relay("relay stream ended before peer joined".into()))
        })
        .await
        .map_err(|_| Error::Timeout("waiting for relay peer timed out".into()))?;

        peer_ready?;

        // Local duplex: app side <-> bridge tasks <-> websocket
        let (app_side, mut bridge_side) = duplex(256 * 1024);

        if let Some(early) = early_payload {
            use tokio::io::AsyncWriteExt;
            bridge_side
                .write_all(&early)
                .await
                .map_err(|e| Error::Relay(format!("early payload: {e}")))?;
        }

        // Split bridge_side for concurrent read/write with WS.
        let (mut bridge_read, mut bridge_write) = tokio::io::split(bridge_side);

        // WS -> local (incoming ciphertext)
        let ws_to_local = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            while let Some(msg) = stream.next().await {
                let Ok(msg) = msg else { break };
                match msg {
                    Message::Binary(bin) => {
                        if bridge_write.write_all(&bin).await.is_err() {
                            break;
                        }
                    }
                    Message::Text(text) => {
                        if is_relay_status_line(&text) {
                            if status_means_error(&text) {
                                tracing::warn!(%text, "relay error after connect");
                                break;
                            }
                            continue;
                        }
                        if bridge_write.write_all(text.as_bytes()).await.is_err() {
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
                }
            }
        });

        // Local -> WS (outgoing ciphertext as binary frames)
        // We need to read length-prefixed stream as raw bytes and send as WS binary chunks.
        // For correct framing, SecureStream already does length-prefix on the byte stream;
        // we forward raw bytes. To preserve message boundaries on WS, buffer reads and
        // send whatever is available (Noise framing is on top of the byte stream).
        let local_to_ws = tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match bridge_read.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        if sink
                            .send(Message::Binary(buf[..n].to_vec().into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = sink.close().await;
        });

        tokio::spawn(async move {
            let _ = tokio::join!(ws_to_local, local_to_ws);
        });

        Ok(Self { inner: app_side })
    }
}

impl AsyncRead for RelayConnection {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for RelayConnection {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Connect via relay (convenience free function).
pub async fn connect_via_relay(
    relay_url: &str,
    room_id: &str,
    token: &str,
) -> Result<RelayConnection> {
    RelayConnection::connect(relay_url, room_id, token, Duration::from_secs(120)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_detection() {
        assert!(is_relay_status_line("⏳ waiting for peer"));
        assert!(is_relay_status_line("✅ peer joined"));
        assert!(is_relay_status_line("❌ room full"));
        assert!(status_means_peer_ready("✅ connected to peer"));
        assert!(!is_relay_status_line("hello app"));
    }

    #[test]
    fn join_url() {
        let u = room_join_url("wss://r.example/", "roomroom1", "tokentok1").unwrap();
        assert_eq!(u, "wss://r.example/v1/room/roomroom1?token=tokentok1");
    }
}
