//! hexatalk-relay — standalone peerseal-compatible WebSocket relay server.
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
//! - After the handshake, every BINARY frame from one peer is forwarded
//!   verbatim to all other members of the same room. Payloads are opaque
//!   E2EE ciphertext — never logged. TEXT frames from clients are dropped
//!   (anti-spoofing: the only text on the wire is server status lines).
//!
//! No TLS here — terminate TLS (`wss://`) at a reverse proxy in front.
//!
//! Extras (from `crates/reprotocol/RELAY.md` wishlist):
//!   `GET /v1/limits` — JSON capability descriptor, `GET /healthz` — liveness.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
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
/// Active WebSocket connections accepted from a single source IP.
const DEFAULT_MAX_CONN_PER_IP: usize = 32;
/// Total rooms kept in memory across the whole server.
const DEFAULT_MAX_ROOMS: usize = 10_000;
/// Rooms a single source IP may create.
const DEFAULT_MAX_ROOMS_PER_IP: usize = 64;
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
hexatalk-relay — peerseal-compatible WebSocket relay server

USAGE:
    hexatalk-relay [OPTIONS]

OPTIONS:
    --bind <ADDR>            Listen address                [default: 0.0.0.0:9000]
    --token <TOKEN>          Shared secret required as ?token= (overrides RELAY_TOKEN)
    --max-peers <N>          Max peers per room            [default: 16]
    --max-frame <BYTES>      Max WebSocket frame size      [default: 1048576]
    --max-conn-per-ip <N>    Max active connections per IP [default: 32]
    --max-rooms <N>          Max rooms total               [default: 10000]
    --max-rooms-per-ip <N>   Max rooms per IP              [default: 64]
    -h, --help               Print this help

ENVIRONMENT:
    RELAY_TOKEN   Shared secret used when --token is absent; preferred in
                  production (keeps the secret out of the process list).

PRODUCTION:
    Always set a token and put the relay behind a TLS reverse proxy
    (wss://). The relay itself speaks plain WebSocket only.

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
    max_conn_per_ip: usize,
    max_rooms: usize,
    max_rooms_per_ip: usize,
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
    // --token wins over RELAY_TOKEN; the env var is the production path
    // (a CLI secret is visible in `ps` and unit files).
    let token: Option<String> = args
        .opt_value_from_str("--token")
        .map_err(|e| format!("--token: {e}"))?
        .or_else(|| std::env::var("RELAY_TOKEN").ok());
    let max_peers: usize = args
        .opt_value_from_str("--max-peers")
        .map_err(|e| format!("--max-peers: {e}"))?
        .unwrap_or(DEFAULT_MAX_PEERS);
    let max_frame: usize = args
        .opt_value_from_str("--max-frame")
        .map_err(|e| format!("--max-frame: {e}"))?
        .unwrap_or(DEFAULT_MAX_FRAME);
    let max_conn_per_ip: usize = args
        .opt_value_from_str("--max-conn-per-ip")
        .map_err(|e| format!("--max-conn-per-ip: {e}"))?
        .unwrap_or(DEFAULT_MAX_CONN_PER_IP);
    let max_rooms: usize = args
        .opt_value_from_str("--max-rooms")
        .map_err(|e| format!("--max-rooms: {e}"))?
        .unwrap_or(DEFAULT_MAX_ROOMS);
    let max_rooms_per_ip: usize = args
        .opt_value_from_str("--max-rooms-per-ip")
        .map_err(|e| format!("--max-rooms-per-ip: {e}"))?
        .unwrap_or(DEFAULT_MAX_ROOMS_PER_IP);

    let rest = args.finish();
    if !rest.is_empty() {
        return Err(format!("unknown argument(s): {rest:?}"));
    }
    if matches!(&token, Some(t) if t.is_empty()) {
        return Err("--token / RELAY_TOKEN must not be empty".into());
    }
    if max_peers < 2 {
        return Err("--max-peers must be >= 2".into());
    }
    if max_frame < 1024 {
        return Err("--max-frame must be >= 1024".into());
    }
    if max_conn_per_ip == 0 {
        return Err("--max-conn-per-ip must be >= 1".into());
    }
    if max_rooms == 0 {
        return Err("--max-rooms must be >= 1".into());
    }
    if max_rooms_per_ip == 0 {
        return Err("--max-rooms-per-ip must be >= 1".into());
    }
    Ok(Config {
        bind,
        token,
        max_peers,
        max_frame,
        max_conn_per_ip,
        max_rooms,
        max_rooms_per_ip,
    })
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

type PeerTx = mpsc::Sender<Message>;

#[derive(Default)]
struct Room {
    peers: HashMap<u64, PeerTx>,
    /// IP that created the room; its per-IP room quota is released when
    /// the room is dropped.
    owner: Option<IpAddr>,
}

#[derive(Default)]
struct State {
    rooms: HashMap<String, Room>,
    /// Active connections per source IP (all paths, not only rooms).
    conns_per_ip: HashMap<IpAddr, usize>,
    /// Rooms created per source IP.
    rooms_per_ip: HashMap<IpAddr, usize>,
}

struct Shared {
    state: Mutex<State>,
    next_peer_id: AtomicU64,
    token: Option<String>,
    max_peers: usize,
    max_conn_per_ip: usize,
    max_rooms: usize,
    max_rooms_per_ip: usize,
    bytes_forwarded: AtomicU64,
}

/// Lock the shared state, recovering from a poisoned mutex instead of
/// panicking every subsequent task: a panicked peer task leaves the state
/// consistent enough (all mutations are single lock scopes) to keep serving.
fn lock_state(shared: &Shared) -> MutexGuard<'_, State> {
    shared.state.lock().unwrap_or_else(|e| e.into_inner())
}

/// Release the per-IP connection slot when the connection task ends.
struct ConnGuard {
    shared: Arc<Shared>,
    ip: IpAddr,
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        let mut st = lock_state(&self.shared);
        if let Some(n) = st.conns_per_ip.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                st.conns_per_ip.remove(&self.ip);
            }
        }
    }
}

/// Constant-time comparison: no early exit on mismatching bytes (token
/// length itself is not secret — it comes from local config).
fn token_eq(provided: &str, expected: &str) -> bool {
    let (a, b) = (provided.as_bytes(), expected.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Broadcast a frame to every room member except `exclude`.
///
/// Fast path is `try_send`; on a full queue we await with a bounded timeout
/// (backpressure) and finally drop the frame for that one slow consumer.
async fn broadcast(shared: &Arc<Shared>, room_id: &str, exclude: u64, msg: Message) {
    let targets: Vec<(u64, PeerTx)> = {
        let st = lock_state(shared);
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

    // Per-IP connection cap, enforced before any protocol work. The guard
    // releases the slot on every exit path below.
    let ip = addr.ip();
    let over_limit = {
        let mut st = lock_state(&shared);
        let n = st.conns_per_ip.entry(ip).or_insert(0);
        if *n >= shared.max_conn_per_ip {
            true
        } else {
            *n += 1;
            false
        }
    };
    if over_limit {
        warn!(%addr, "rejected: too many connections from this IP");
        respond_http(
            &mut stream,
            "429 Too Many Requests",
            "text/plain",
            "too many connections\n",
        )
        .await;
        return;
    }
    let _conn_guard = ConnGuard {
        shared: Arc::clone(&shared),
        ip,
    };

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
        if !token_eq(provided, expected) {
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

    // Join the room, enforcing the room limits. `Err(line)` is a client-facing
    // ❌ status line (fatal, non-transient — same contract as "room full").
    let peer_id = shared.next_peer_id.fetch_add(1, Ordering::Relaxed);
    let (tx, mut rx) = mpsc::channel::<Message>(PEER_QUEUE);
    let count: Result<usize, String> = {
        let mut st = lock_state(&shared);
        match st.rooms.get_mut(&room_id) {
            Some(room) => {
                if room.peers.len() >= shared.max_peers {
                    Err(format!("❌ room full (max {})", shared.max_peers))
                } else {
                    room.peers.insert(peer_id, tx.clone());
                    Ok(room.peers.len())
                }
            }
            None => {
                if st.rooms.len() >= shared.max_rooms {
                    Err("❌ server room limit reached, try again later".to_string())
                } else if st.rooms_per_ip.get(&addr.ip()).copied().unwrap_or(0)
                    >= shared.max_rooms_per_ip
                {
                    Err("❌ too many rooms from your address".to_string())
                } else {
                    *st.rooms_per_ip.entry(addr.ip()).or_insert(0) += 1;
                    let room = Room {
                        peers: HashMap::from([(peer_id, tx.clone())]),
                        owner: Some(addr.ip()),
                    };
                    st.rooms.insert(room_id.clone(), room);
                    Ok(1)
                }
            }
        }
    };
    let count = match count {
        Ok(c) => c,
        Err(line) => {
            warn!(%addr, room = %room_id, %line, "rejected: room limits");
            send_text_close(&mut ws, &line).await;
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
    let mut text_dropped = 0u64;
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
            Message::Text(_) => {
                // Anti-spoofing: legit clients send ciphertext only as BINARY
                // frames. Forwarding client text would let a malicious peer
                // inject fake ❌/✅/⏳ status lines into other sessions —
                // the only text on the wire is server-generated. Drop it.
                text_dropped += 1;
            }
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
            Message::Close(_) => break,
        }
    }

    // Disconnect: remove from room, notify the rest, drop empty rooms and
    // release the owner's per-IP room quota.
    drop(tx);
    writer.abort();
    let remaining = {
        let mut st = lock_state(&shared);
        let mut remaining = 0;
        let mut owner = None;
        if let Some(room) = st.rooms.get_mut(&room_id) {
            room.peers.remove(&peer_id);
            remaining = room.peers.len();
            owner = room.owner;
        }
        if remaining == 0 {
            st.rooms.remove(&room_id);
            if let Some(ip) = owner {
                if let Some(n) = st.rooms_per_ip.get_mut(&ip) {
                    *n -= 1;
                    if *n == 0 {
                        st.rooms_per_ip.remove(&ip);
                    }
                }
            }
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
    info!(%addr, peer_id, room = %room_id, remaining, bytes_in, text_dropped, "peer left");
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
        max_conn_per_ip: cfg.max_conn_per_ip,
        max_rooms: cfg.max_rooms,
        max_rooms_per_ip: cfg.max_rooms_per_ip,
        bytes_forwarded: AtomicU64::new(0),
    });

    if shared.token.is_none() {
        warn!("no --token / RELAY_TOKEN set: the relay is open to anyone — set a token in production");
    }

    info!(
        bind = %cfg.bind,
        max_peers = cfg.max_peers,
        max_frame = cfg.max_frame,
        max_conn_per_ip = cfg.max_conn_per_ip,
        max_rooms = cfg.max_rooms,
        max_rooms_per_ip = cfg.max_rooms_per_ip,
        token_required = shared.token.is_some(),
        "hexatalk-relay listening"
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

// ---------------------------------------------------------------------------
// Tests: the ciphertext-blind invariant (see module doc) is a security
// property, not just an implementation detail — assert it mechanically
// instead of relying on code review alone.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_shared() -> Arc<Shared> {
        Arc::new(Shared {
            state: Mutex::new(State::default()),
            next_peer_id: AtomicU64::new(1),
            token: None,
            max_peers: DEFAULT_MAX_PEERS,
            max_conn_per_ip: DEFAULT_MAX_CONN_PER_IP,
            max_rooms: DEFAULT_MAX_ROOMS,
            max_rooms_per_ip: DEFAULT_MAX_ROOMS_PER_IP,
            bytes_forwarded: AtomicU64::new(0),
        })
    }

    /// `tracing_subscriber::fmt`'s `MakeWriter`, backed by an in-memory
    /// buffer so a test can assert on exactly what would have been logged
    /// (without touching the process-global subscriber `main()` installs).
    #[derive(Clone, Default)]
    struct LogBuf(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for LogBuf {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap_or_else(|e| e.into_inner()).extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuf {
        type Writer = LogBuf;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Core relay invariant, asserted end-to-end against a real listener
    /// and two real WebSocket clients (not mocks):
    ///
    /// 1. BINARY frames (the only thing legit peerseal clients ever send)
    ///    are forwarded to other room members byte-for-byte.
    /// 2. TEXT frames from a *client* never reach other peers — only the
    ///    server itself may speak status lines (anti-spoofing).
    /// 3. Nothing that looks like payload content ever appears in the logs
    ///    — the relay only ever logs metadata (peer/room ids, byte
    ///    counts), never the bytes themselves.
    #[tokio::test]
    async fn relay_forwards_binary_verbatim_drops_client_text_never_logs_payload() {
        const CANARY: &str = "CIPHERTEXT-CANARY-3f9a1c-should-never-be-logged";

        let log_buf = LogBuf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(log_buf.clone())
            .with_max_level(tracing::Level::TRACE)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shared = test_shared();
        tokio::spawn(async move {
            loop {
                let (stream, peer_addr) = match listener.accept().await {
                    Ok(v) => v,
                    Err(_) => break,
                };
                let sh = Arc::clone(&shared);
                tokio::spawn(async move {
                    handle_connection(stream, peer_addr, sh, DEFAULT_MAX_FRAME).await;
                });
            }
        });

        let url = format!("ws://{addr}/v1/room/testroom");
        let (mut a, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        // First peer in the room: server sends the "waiting" status line.
        let first = a.next().await.unwrap().unwrap();
        assert!(matches!(first, Message::Text(_)));

        let (mut b, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        // Both peers get a "peer joined" status line once B connects.
        let _ = a.next().await.unwrap().unwrap();
        let _ = b.next().await.unwrap().unwrap();

        // A binary "ciphertext" frame containing the canary must reach B
        // byte-for-byte.
        let payload = format!("\u{0}\u{1}{CANARY}\u{2}\u{3}").into_bytes();
        a.send(Message::Binary(payload.clone().into())).await.unwrap();
        match b.next().await.unwrap().unwrap() {
            Message::Binary(bytes) => assert_eq!(bytes.as_ref(), payload.as_slice()),
            other => panic!("expected Binary, got {other:?}"),
        }

        // A TEXT frame from a client (containing the same canary, simulating
        // an attempt to inject a fake status line) must never reach B.
        a.send(Message::Text(format!("\u{274c} {CANARY}").into()))
            .await
            .unwrap();
        // Prove it wasn't just delayed: send a distinguishable binary frame
        // right after and confirm that's the *next* thing B sees.
        let sentinel = b"sentinel-after-dropped-text".to_vec();
        a.send(Message::Binary(sentinel.clone().into())).await.unwrap();
        match b.next().await.unwrap().unwrap() {
            Message::Binary(bytes) => assert_eq!(bytes.as_ref(), sentinel.as_slice()),
            other => panic!(
                "expected the sentinel Binary next, got {other:?} — did the client TEXT leak through?"
            ),
        }

        drop(a);
        drop(b);

        let logs = String::from_utf8_lossy(&log_buf.0.lock().unwrap_or_else(|e| e.into_inner())).into_owned();
        assert!(
            !logs.contains(CANARY),
            "relay logs must never contain payload content, but found the canary in:\n{logs}"
        );
    }
}
