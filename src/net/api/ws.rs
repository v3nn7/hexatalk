//! Background WebSocket task: one long-lived connection to
//! `{base}/ws?token=...`, app-level ping every ~25 s, reconnect with
//! exponential backoff, and instant reaction to session-token changes
//! (logout drops the socket; a new token reconnects immediately).
//!
//! Every `{"type": "...", "channel": "...", "payload": {...}}` frame is
//! parsed into a [`WsEvent`] and fanned out over the client's broadcast
//! channel; subscriptions filter on kind/channel from there.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::watch;
use tokio::time::{interval, sleep};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as SocketMessage;

use super::client::{ApiClient, WsEvent};

const PING_EVERY: Duration = Duration::from_secs(25);
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Entry point — spawned once by `ApiClient::ensure_ws`.
pub(super) async fn run(client: ApiClient, mut token_rx: watch::Receiver<Option<String>>) {
    let mut backoff = INITIAL_BACKOFF;
    loop {
        // Wait until there is a session token at all (starts as None
        // before login / after logout).
        let token = loop {
            if let Some(token) = token_rx.borrow_and_update().clone() {
                break token;
            }
            if token_rx.changed().await.is_err() {
                return; // client dropped
            }
        };

        match serve(&client, &mut token_rx, &token).await {
            // Network drop / connect failure: back off before retrying.
            End::Reconnect => {
                sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
            // Token changed (login as another user / logout): reconnect
            // immediately — the wait-for-token loop above handles logout.
            End::TokenChanged => {
                backoff = INITIAL_BACKOFF;
            }
        }
    }
}

enum End {
    Reconnect,
    TokenChanged,
}

async fn serve(
    client: &ApiClient,
    token_rx: &mut watch::Receiver<Option<String>>,
    token: &str,
) -> End {
    let url = ws_url(client.base_url(), token);
    let (mut socket, _response) = match connect_async(&url).await {
        Ok(pair) => pair,
        Err(_) => return End::Reconnect,
    };

    let mut ping = interval(PING_EVERY);
    ping.tick().await; // the first interval tick fires immediately — skip it

    loop {
        tokio::select! {
            frame = socket.next() => {
                match frame {
                    Some(Ok(SocketMessage::Text(text))) => handle_frame(client, text.as_str()),
                    // Protocol-level pings are answered by tungstenite itself.
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => return End::Reconnect,
                }
            }
            _ = ping.tick() => {
                if socket
                    .send(SocketMessage::Text(r#"{"type":"ping"}"#.into()))
                    .await
                    .is_err()
                {
                    return End::Reconnect;
                }
            }
            changed = token_rx.changed() => {
                if changed.is_err() {
                    return End::TokenChanged; // client dropped — outer loop exits
                }
                let current = token_rx.borrow_and_update().clone();
                if current.as_deref() != Some(token) {
                    return End::TokenChanged;
                }
            }
        }
    }
}

/// Parse one text frame into a `WsEvent` and broadcast it. `pong` and
/// anything without a `type` is ignored.
fn handle_frame(client: &ApiClient, text: &str) {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    let kind = json.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if kind.is_empty() || kind == "pong" {
        return;
    }
    let channel = json
        .get("channel")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();
    let payload = json.get("payload").cloned().unwrap_or(serde_json::Value::Null);
    client.publish_event(WsEvent {
        kind: kind.to_string(),
        channel,
        payload,
    });
}

/// `https://…` → `wss://…/ws?token=…`, `http://…` → `ws://…/ws?token=…`.
fn ws_url(base: &str, token: &str) -> String {
    let base = base.trim_end_matches('/');
    let scheme = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        format!("wss://{base}")
    };
    format!("{scheme}/ws?token={}", url_encode(token))
}

/// Percent-encode the token for the query string (unreserved characters
/// pass through — session tokens are alphanumeric in practice).
fn url_encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for &b in raw.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
