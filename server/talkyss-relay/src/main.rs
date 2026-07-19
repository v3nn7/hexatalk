//! talkyss-relay — standalone peerseal-compatible WebSocket relay server.
//!
//! Protocol (must match `crates/reprotocol/src/transport/relay.rs`):
//!
//! - Clients join via WebSocket at:
//!   `ws(s)://HOST/v1/room/{room_id}?token={token}`
//! - The server sends TEXT status lines the client recognizes
//!   (`is_relay_status_line` / `status_means_peer_ready` / `status_means_error`):
//!     - `⏳ waiting for peer …`          — informational, keep waiting
//!     - `✅ peer joined (N peers in room)` — at least one peer is present,
//!       the client treats this as "ready" and starts sending ciphertext
//!     - `⏳ peer left (N peers remaining)` — informational after connect
//!     - `❌ <reason>`                     — fatal, client aborts (non-transient)
//! - After the handshake, every BINARY (and non-status TEXT) frame from one
//!   peer is forwarded verbatim to all other members of the same room.
//!   Payloads are opaque E2EE ciphertext — never logged.
//!
//! Extras (from `crates/reprotocol/RELAY.md` wishlist):
//!   `GET /v1/limits` — JSON capability descriptor, `GET /healthz` — liveness.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::accept_async_with_config;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

const DEFAULT_BIND: &str = "0.0.0.0:9000";
const DEFAULT_MAX_PEERS: usize = 16;
/// Client chunks Noise frames at ~60 KiB; 1 MiB is a sane relay cap
/// (see RELAY.md: >= 64–128 KiB required, 25 MiB only for unchunked clients).
const DEFAULT_MAX_FRAME: usize = 1024 * 1024;
const MAX_HTTP_HEAD: usize = 16 * 1024;
const HTTP_HEAD_TIMEOUT: Duration = Duration::from_secs(10);
/// Per-peer outbound queue. ~256 x 60 KiB worst case buffered per peer.
const PEER_QUEUE: usize = 256;
const PING_INTERVAL: Duration = Duration::from_secs(25);
/// Dead-peer detection. Live clients auto-pong our pings every 25 s, so any
/// traffic (incl. pongs) resets this. RELAY.md asks for >= 300 s idle.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// Backpressure bound: a slow consumer stalls its sender at most this long,
/// then the frame is dropped for that consumer (never the whole room).
const SEND_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_ROOM_ID_LEN: usize = 128;
/// Advertised to clients via /v1/limits (25 MiB logical, chunked).
const MAX_LOGICAL_HINT: usize = 25 * 1024 * 1024;

const HELP: &str = "\
talkyss-relay — peerseal-compatible WebSocket relay server

USAGE:
    talkyss-relay [OPTIONS]

OPTIONS:
    --bind <ADDR>        Listen address            [default: 0.0.0.0:9000]
    --token <TOKEN>      Shared secret rooms must present as ?token= (optional)
    --max-peers <N>      Max peers per room        [default: 16]
    --max-frame <BYTES>  Max WebSocket frame size  [default: 1048576]
    -h, --help           Print this help

LOGS:
    Set RUST_LOG=debug for verbose output. Payloads are never logged.
";

// ---------------------------------------------------------------------------
// Config / CLI
// ---------------------------------------------------------------------------

struct Config {
    bind: String,
    token: Option<String>,
    max_peers: usize,
    max_frame: usize,
}

fn parse_args() -> Result<Config, String> {
    let mut args = pico_args::Arguments::from_env();
    if args.contains(["-h", "--help"]) {
        print!("{HELP}");
        std::process::exit(0);
    }
    let bind: String = args
        .opt_value_from_str("--bind")
        .map_err(|e| format!("--bind: {e}"))?
        .unwrap_or_else(|| DEFAULT_BIND.to_string());
    let token: Option<String> = args
        .opt_value_from_str("--token")
        .map_err(|e| format!("--token: {e}"))?;
    let max_peers: usize = args
        .opt_value_from_str("--max-peers")
        .map_err(|e| format!("--max-peers: {e}"))?
        .unwrap_or(DEFAULT_MAX_PEERS);
    let max_frame: usize = args
        .opt_value_from_str("--max-frame")
        .map_err(|e| format!("--max-frame: {e}"))?
        .unwrap_or(DEFAULT_MAX_FRAME);

    let rest = args.finish();
    if !rest.is_empty() {
        return Err(format!("unknown argument(s): {rest:?}"));
    }
    if max_peers < 2 {
        return Err("--max-peers must be >= 2".into());
    }
    if max_frame < 1024 {
        return Err("--max-frame must be >= 1024".into());
    }
    Ok(Config {
        bind,
        token,
        max_peers,
        max_frame,
    })
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

type PeerTx = mpsc::Sender<Message>;

#[derive(Default)]
struct Room {
    peers: HashMap<u64, PeerTx>,
}

#[derive(Default)]
struct State {
    rooms: HashMap<String, Room>,
}

struct Shared {
    state: Mutex<State>,
    next_peer_id: AtomicU64,
    token: Option<String>,
    max_peers: usize,
    bytes_forwarded: AtomicU64,
}

/// Broadcast a frame to every room member except `exclude`.
///
/// Fast path is `try_send`; on a full queue we await with a bounded timeout
/// (backpressure) and finally drop the frame for that one slow consumer.
async fn broadcast(shared: &Arc<Shared>, room_id: &str, exclude: u64, msg: Message) {
    let targets: Vec<(u64, PeerTx)> = {
        let st = shared.state.lock().unwrap();
        match st.rooms.get(room_id) {
            Some(room) => room
                .peers
                .iter()
                .filter(|(id, _)| **id != exclude)
                .map(|(id, tx)| (*id, tx.clone()))
                .collect(),
            None => return,
        }
    };
    for (peer_id, tx) in targets {
        match tx.try_send(msg.clone()) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(m)) => {
                if tokio::time::timeout(SEND_TIMEOUT, tx.send(m)).await.is_err() {
                    debug!(peer_id, room_id, "dropping frame for slow peer");
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }
}

// ---------------------------------------------------------------------------
// PrefixedStream: replay the already-read HTTP head, then pass through.
// ---------------------------------------------------------------------------

struct PrefixedStream {
    prefix: Vec<u8>,
    stream: TcpStream,
}

impl AsyncRead for PrefixedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if !self.prefix.is_empty() {
            let n = self.prefix.len().min(buf.remaining());
            buf.put_slice(&self.prefix[..n]);
            self.prefix.drain(..n);
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for PrefixedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, data)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

// ---------------------------------------------------------------------------
// Minimal HTTP dispatch (WS upgrade vs. plain JSON endpoints)
// ---------------------------------------------------------------------------

async fn read_http_head(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "eof in head"));
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            return Ok(buf);
        }
        if buf.len() > MAX_HTTP_HEAD {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "head too large"));
        }
    }
}

async fn respond_http(stream: &mut TcpStream, status: &str, content_type: &str, body: &str) {
    let resp = format!(
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v)
}

fn valid_room_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ROOM_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

// ---------------------------------------------------------------------------
// Connection handling
// ---------------------------------------------------------------------------

async fn handle_connection(
    mut stream: TcpStream,
    addr: SocketAddr,
    shared: Arc<Shared>,
    max_frame: usize,
) {
    let _ = stream.set_nodelay(true);

    let head = match tokio::time::timeout(HTTP_HEAD_TIMEOUT, read_http_head(&mut stream)).await {
        Ok(Ok(h)) => h,
        Ok(Err(e)) => {
            debug!(%addr, %e, "bad HTTP head");
            return;
        }
        Err(_) => {
            debug!(%addr, "HTTP head timeout");
            return;
        }
    };

    // Parse the request line into owned strings so `head` can be moved
    // into the WebSocket handshake afterwards.
    let (method, path, query) = {
        let head_text = String::from_utf8_lossy(&head);
        let request_line = head_text.lines().next().unwrap_or("");
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let target = parts.next().unwrap_or("").to_string();
        let (path, query) = match target.split_once('?') {
            Some((p, q)) => (p.to_string(), q.to_string()),
            None => (target, String::new()),
        };
        (method, path, query)
    };

    if method != "GET" {
        respond_http(&mut stream, "405 Method Not Allowed", "text/plain", "method not allowed").await;
        return;
    }

    match path.as_str() {
        "/healthz" => {
            respond_http(&mut stream, "200 OK", "text/plain", "ok\n").await;
        }
        "/v1/limits" => {
            let body = format!(
                "{{\n  \"max_binary_frame\": {max_frame},\n  \"max_logical_hint\": {MAX_LOGICAL_HINT},\n  \"idle_timeout_sec\": {},\n  \"max_peers_per_room\": {},\n  \"supports_binary\": true,\n  \"supports_ping\": true\n}}\n",
                IDLE_TIMEOUT.as_secs(),
                shared.max_peers,
            );
            respond_http(&mut stream, "200 OK", "application/json", &body).await;
        }
        _ if path.starts_with("/v1/room/") => {
            let room_id = path["/v1/room/".len()..].to_string();
            handle_ws(stream, head, addr, &room_id, &query, shared, max_frame).await;
        }
        _ => {
            respond_http(&mut stream, "404 Not Found", "text/plain", "not found\n").await;
        }
    }
}

async fn send_text_close<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>, line: &str)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let _ = ws.send(Message::Text(line.into())).await;
    let _ = ws.close(None).await;
}

async fn handle_ws(
    stream: TcpStream,
    head: Vec<u8>,
    addr: SocketAddr,
    room_id: &str,
    query: &str,
    shared: Arc<Shared>,
    max_frame: usize,
) {
    let mut ws_cfg = WebSocketConfig::default();
    ws_cfg.max_frame_size = Some(max_frame);
    ws_cfg.max_message_size = Some(max_frame.saturating_mul(4));
    let prefixed = PrefixedStream { prefix: head, stream };
    let mut ws = match accept_async_with_config(prefixed, Some(ws_cfg)).await {
        Ok(ws) => ws,
        Err(e) => {
            debug!(%addr, %e, "websocket handshake failed");
            return;
        }
    };

    // Shared-token check: deliver ❌ as a status line so the client treats it
    // as a fatal (non-transient) relay error instead of retrying forever.
    if let Some(expected) = &shared.token {
        let provided = query_param(query, "token").unwrap_or("");
        if provided != expected {
            warn!(%addr, "rejected: invalid token");
            send_text_close(&mut ws, "❌ invalid token").await;
            return;
        }
    }

    if !valid_room_id(room_id) {
        send_text_close(&mut ws, "❌ invalid room id").await;
        return;
    }
    let room_id = room_id.to_string();

    // Join the room.
    let peer_id = shared.next_peer_id.fetch_add(1, Ordering::Relaxed);
    let (tx, mut rx) = mpsc::channel::<Message>(PEER_QUEUE);
    let count = {
        let mut st = shared.state.lock().unwrap();
        let room = st.rooms.entry(room_id.clone()).or_default();
        if room.peers.len() >= shared.max_peers {
            None
        } else {
            room.peers.insert(peer_id, tx.clone());
            Some(room.peers.len())
        }
    };
    let count = match count {
        Some(c) => c,
        None => {
            send_text_close(&mut ws, &format!("❌ room full (max {})", shared.max_peers)).await;
            return;
        }
    };

    info!(%addr, peer_id, room = %room_id, count, "peer joined");

    // Status lines the client parses (see relay.rs):
    // first peer waits; everyone is notified when a new peer joins.
    if count == 1 {
        let _ = ws
            .send(Message::Text(
                format!("⏳ waiting for peer (1/{} slots)", shared.max_peers).into(),
            ))
            .await;
    } else {
        let ready = format!("✅ peer joined ({count} peers in room)");
        let _ = ws.send(Message::Text(ready.clone().into())).await;
        broadcast(&shared, &room_id, peer_id, Message::Text(ready.into())).await;
    }

    let (mut sink, mut ws_stream) = ws.split();

    // Writer task: outbound queue -> websocket, plus periodic server pings.
    let writer = tokio::spawn(async move {
        let mut ping = tokio::time::interval(PING_INTERVAL);
        ping.tick().await; // skip the immediate first tick
        loop {
            tokio::select! {
                _ = ping.tick() => {
                    if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                        break;
                    }
                }
                msg = rx.recv() => {
                    match msg {
                        Some(m) => {
                            if sink.send(m).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                }
            }
        }
        let _ = sink.close().await;
    });

    // Reader loop: forward opaque ciphertext to the rest of the room.
    let mut bytes_in = 0u64;
    loop {
        let msg = match tokio::time::timeout(IDLE_TIMEOUT, ws_stream.next()).await {
            Ok(Some(Ok(m))) => m,
            Ok(Some(Err(e))) => {
                debug!(%addr, peer_id, %e, "ws read error");
                break;
            }
            Ok(None) => break,
            Err(_) => {
                debug!(%addr, peer_id, "idle timeout");
                break;
            }
        };
        match msg {
            Message::Binary(bin) => {
                bytes_in += bin.len() as u64;
                shared.bytes_forwarded.fetch_add(bin.len() as u64, Ordering::Relaxed);
                broadcast(&shared, &room_id, peer_id, Message::Binary(bin)).await;
            }
            Message::Text(text) => {
                // Clients only send binary, but relay non-status text verbatim
                // just in case (the peer's client filters status lines itself).
                broadcast(&shared, &room_id, peer_id, Message::Text(text)).await;
            }
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
            Message::Close(_) => break,
        }
    }

    // Disconnect: remove from room, notify the rest, drop empty rooms.
    drop(tx);
    writer.abort();
    let remaining = {
        let mut st = shared.state.lock().unwrap();
        let mut remaining = 0;
        if let Some(room) = st.rooms.get_mut(&room_id) {
            room.peers.remove(&peer_id);
            remaining = room.peers.len();
        }
        if remaining == 0 {
            st.rooms.remove(&room_id);
        }
        remaining
    };
    if remaining > 0 {
        broadcast(
            &shared,
            &room_id,
            peer_id,
            Message::Text(format!("⏳ peer left ({remaining} peers remaining)").into()),
        )
        .await;
    }
    info!(%addr, peer_id, room = %room_id, remaining, bytes_in, "peer left");
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cfg = match parse_args() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}\n\n{HELP}");
            std::process::exit(2);
        }
    };

    let listener = match TcpListener::bind(&cfg.bind).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: cannot bind {}: {e}", cfg.bind);
            std::process::exit(1);
        }
    };

    let shared = Arc::new(Shared {
        state: Mutex::new(State::default()),
        next_peer_id: AtomicU64::new(1),
        token: cfg.token,
        max_peers: cfg.max_peers,
        bytes_forwarded: AtomicU64::new(0),
    });

    info!(
        bind = %cfg.bind,
        max_peers = cfg.max_peers,
        max_frame = cfg.max_frame,
        token_required = shared.token.is_some(),
        "talkyss-relay listening"
    );

    loop {
        tokio::select! {
            accept = listener.accept() => {
                match accept {
                    Ok((stream, addr)) => {
                        let sh = Arc::clone(&shared);
                        let max_frame = cfg.max_frame;
                        tokio::spawn(async move {
                            handle_connection(stream, addr, sh, max_frame).await;
                        });
                    }
                    Err(e) => warn!(%e, "accept failed"),
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!(
                    bytes_forwarded = shared.bytes_forwarded.load(Ordering::Relaxed),
                    "shutting down"
                );
                break;
            }
        }
    }
}
