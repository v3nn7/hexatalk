//! Bridge between HexaTalk UI and the local `peerseal` crate (crates/reprotocol).
//!
//! Direct chats use peerseal for live E2E traffic (Noise + optional Railway
//! relay). Convex only carries the invite payload for pairing — never message
//! bodies. This module does not modify the peerseal crate.

use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::prelude::{BASE64_STANDARD, Engine as _};
use futures::SinkExt;
use futures::channel::mpsc as futures_mpsc;
use peerseal::{
    AppMessage, Identity, Invite, Node, NodeConfig, Session, TransportKind, normalize_relay_url,
};
use tokio::sync::mpsc;

/// Default production relay from peerseal docs (overridable via PEERSEAL_RELAY).
/// Baked into the binary obfuscated — see src/obf.rs and build.rs.

/// How long the host waits for the UI to ack a Convex invite publish
/// (`InvitePublished`/`InvitePublishFailed`) before treating it as a
/// failed attempt and retrying. Previously the host blocked on
/// `cmd_rx.recv()` forever here -- a wedged publish path left the DM
/// "stuck" with no status and no retry.
const PUBLISH_ACK_TIMEOUT: Duration = Duration::from_secs(45);

/// If nothing (not even a keepalive Pong) arrives from the peer for this
/// long, the connection is declared half-open: TCP relays and NATs drop
/// idle flows silently, and without a receive-side watchdog a dead peer
/// looked "connected" forever. On expiry the session is torn down so the
/// registry respawns a fresh one.
const PEER_SILENCE_TIMEOUT: Duration = Duration::from_secs(90);

/// Hard cap on a single drained photo transfer -- bounds memory against a
/// malicious or buggy peer streaming frames forever.
const MAX_PHOTO_BYTES: usize = 32 * 1024 * 1024;

/// Timeout for draining one photo transfer; without it a peer that stops
/// mid-transfer suspended the whole `connected_loop` select forever.
const PHOTO_DRAIN_TIMEOUT: Duration = Duration::from_secs(120);

/// Commands from the UI thread into the peerseal worker.
#[derive(Debug)]
pub(crate) enum PeerCmd {
    /// Guest: Convex delivered a host invite (`ps1:…`).
    InvitePayload(String),
    /// Host: Convex successfully stored the invite — safe to open relay room.
    InvitePublished,
    /// Host: Convex rejected the invite publish.
    InvitePublishFailed(String),
    SendText(String),
    SendPhoto {
        bytes: Vec<u8>,
        content_type: String,
        width: u32,
        height: u32,
    },
    Shutdown,
}

/// Events from the peerseal worker back to the UI.
#[derive(Debug, Clone)]
pub(crate) enum PeerEvent {
    Status(String),
    /// Host created an invite — publish to Convex.
    HostInvite {
        payload: String,
        expires_at_ms: i64,
    },
    Connected {
        sas_emojis: String,
        transport: String,
        remote_fingerprint: String,
    },
    Text(String),
    Photo {
        bytes: Vec<u8>,
        /// MIME type of the photo (sniffed from magic bytes when the peer
        /// sent none). Carried for the UI; `state::app` currently caches
        /// the bytes only, hence the allow.
        #[allow(dead_code)]
        content_type: String,
    },
    Error(String),
    Disconnected,
}

/// Who should host the peerseal invite for this pair (stable, deterministic).
pub(crate) fn is_peerseal_host(local_user_id: &str, peer_user_id: &str) -> bool {
    local_user_id < peer_user_id
}

fn peerseal_identity_path(user_id: &str) -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("HexaTalk")
        .join(format!("peerseal_{user_id}.key"))
}

/// Load or create a peerseal identity and return (identity, base64 public key).
///
/// The key file is kept DPAPI-protected at rest via the shared
/// `crypto::write_secret_file`/`read_secret_file` helpers (`TKDP1` blob on
/// Windows). The byte format inside the blob is unchanged
/// (`pub_hex\npriv_hex\n` — the same layout the peerseal crate's own
/// `Identity::save_file` writes, which `state::history` also parses), and
/// legacy plaintext files are migrated to DPAPI on load. The crate is not
/// modified; its file APIs are simply no longer used for this path.
pub(crate) fn load_peerseal_identity(user_id: &str) -> Result<(Identity, String), String> {
    let path = peerseal_identity_path(user_id);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let id = if path.exists() {
        // An existing file must never be silently regenerated: a fresh key
        // would break every peerseal session and lock the local history
        // vault (its key is derived from this identity's private half).
        let bytes = crate::crypto::read_secret_file(&path)
            .ok_or_else(|| "couldn't read the peerseal identity key".to_string())?;
        parse_identity_file(&bytes)
            .ok_or_else(|| "the peerseal identity key file is corrupt".to_string())?
    } else {
        Identity::generate().map_err(|e| e.to_string())?
    };

    // (Re)save on every load: first creation, and migrating legacy
    // plaintext files into DPAPI blobs.
    save_identity_file(&path, &id)?;
    crate::crypto::tighten_secret_file_perms(&path);

    let public_b64 = BASE64_STANDARD.encode(id.public);
    Ok((id, public_b64))
}

fn parse_identity_file(bytes: &[u8]) -> Option<Identity> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let public = hex_decode_32(lines.next()?.trim())?;
    let private = hex_decode_32(lines.next()?.trim())?;
    Some(Identity::from_parts(private, public))
}

fn save_identity_file(path: &Path, id: &Identity) -> Result<(), String> {
    let body = format!(
        "{}\n{}\n",
        hex_encode(&id.public),
        hex_encode(&id.private)
    );
    crate::crypto::write_secret_file(path, body).map_err(|e| e.to_string())
}

fn hex_encode(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode_32(s: &str) -> Option<[u8; 32]> {
    let bytes: Vec<u8> = (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect::<Option<_>>()?;
    bytes.try_into().ok()
}

/// Default production relay comes from `build.rs` (obfuscated in the
/// binary, see src/obf.rs); overridable via PEERSEAL_RELAY.
fn resolve_relay() -> Result<String, String> {
    let from_env = std::env::var("PEERSEAL_RELAY")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let raw = from_env.unwrap_or_else(|| crate::obf::peerseal_relay().to_string());
    normalize_relay_url(&raw).map_err(|e| e.to_string())
}

/// Spawn a peerseal worker for one DM. Returns a command sender; events go to `event_tx`.
pub(crate) fn spawn_dm_session(
    local_user_id: String,
    peer_user_id: String,
    conversation_id: String,
    event_tx: futures_mpsc::Sender<PeerEvent>,
) -> mpsc::UnboundedSender<PeerCmd> {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        match run_dm_session(
            local_user_id,
            peer_user_id,
            conversation_id,
            cmd_rx,
            event_tx.clone(),
        )
        .await
        {
            Ok(()) => {}
            Err(err) if err == "cancelled" => {}
            Err(err) => {
                let _ = emit(&event_tx, PeerEvent::Error(err)).await;
            }
        }
        let _ = emit(&event_tx, PeerEvent::Disconnected).await;
    });
    cmd_tx
}

async fn emit(tx: &futures_mpsc::Sender<PeerEvent>, ev: PeerEvent) -> Result<(), ()> {
    let mut tx = tx.clone();
    tx.send(ev).await.map_err(|_| ())
}

/// Railway / reverse proxies often drop idle WSS without a proper TLS
/// `close_notify`. rustls surfaces that as this string — treat as retryable.
fn is_transient_relay_error(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    e.contains("close_notify")
        || e.contains("unexpected eof")
        || e.contains("connection reset")
        || e.contains("broken pipe")
        || e.contains("relay stream ended")
        || e.contains("relay closed")
        || e.contains("timed out")
        || e.contains("timeout")
        || e.contains("ws read")
        || e.contains("websocket")
}

fn humanize_relay_error(err: &str) -> String {
    if is_transient_relay_error(err) {
        format!(
            "Relay dropped the connection (common on Railway). \
             Keep both DMs open — retrying. Detail: {err}"
        )
    } else {
        err.to_string()
    }
}

async fn run_dm_session(
    local_user_id: String,
    peer_user_id: String,
    _conversation_id: String,
    mut cmd_rx: mpsc::UnboundedReceiver<PeerCmd>,
    event_tx: futures_mpsc::Sender<PeerEvent>,
) -> Result<(), String> {
    let host = is_peerseal_host(&local_user_id, &peer_user_id);
    let (identity, _) = load_peerseal_identity(&local_user_id)?;
    let relay = resolve_relay()?;
    eprintln!(
        "[peer] session start: role={} relay={relay} peer={peer_user_id}",
        if host { "host" } else { "guest" }
    );

    let _ = emit(
        &event_tx,
        PeerEvent::Status(if host {
            format!("Starting secure host via {relay}…")
        } else {
            "Waiting for peer invite…".into()
        }),
    )
    .await;

    // Shorter wait per attempt + outer retries: if the relay idle-kicks one
    // side, we republish invite / rejoin instead of dying with TLS EOF noise.
    let config = NodeConfig {
        force_relay: true,
        direct_first: false,
        accept_timeout: Some(Duration::from_secs(90)),
        relay_wait_timeout: Duration::from_secs(90),
        dial_timeout: Duration::from_secs(5),
        ..Default::default()
    };

    const MAX_ATTEMPTS: u32 = 8;

    let mut session = if host {
        host_connect_with_retry(
            identity,
            &relay,
            config,
            MAX_ATTEMPTS,
            &mut cmd_rx,
            &event_tx,
        )
        .await?
    } else {
        guest_connect_with_retry(
            identity,
            &relay,
            config,
            MAX_ATTEMPTS,
            &mut cmd_rx,
            &event_tx,
        )
        .await?
    };

    let transport = match session.transport {
        TransportKind::DirectTcp => "direct",
        TransportKind::Relay => "relay",
    };
    let sas = session.info.sas_emojis();
    let remote_fp = session
        .info
        .remote_short_fingerprint()
        .unwrap_or_else(|| "unknown".into());
    let _ = emit(
        &event_tx,
        PeerEvent::Connected {
            sas_emojis: sas,
            transport: transport.into(),
            remote_fingerprint: remote_fp,
        },
    )
    .await;

    // Connected: multiplex send commands and recv_app.
    connected_loop(&mut session, &mut cmd_rx, &event_tx).await;
    session.close();
    Ok(())
}

async fn host_connect_with_retry(
    identity: Identity,
    relay: &str,
    config: NodeConfig,
    max_attempts: u32,
    cmd_rx: &mut mpsc::UnboundedReceiver<PeerCmd>,
    event_tx: &futures_mpsc::Sender<PeerEvent>,
) -> Result<Session, String> {
    let mut last_err = String::new();
    for attempt in 1..=max_attempts {
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                PeerCmd::Shutdown => return Err("cancelled".into()),
                _ => {}
            }
        }

        let _ = emit(
            event_tx,
            PeerEvent::Status(format!(
                "Host attempt {attempt}/{max_attempts} — creating invite…"
            )),
        )
        .await;

        let node = Node::bind("0.0.0.0:0")
            .await
            .map_err(|e| e.to_string())?
            .with_identity(identity.clone())
            .with_config(config.clone())
            .with_relay(relay)
            .map_err(|e| e.to_string())?
            .force_relay();

        let invite = node
            .create_invite(Duration::from_secs(120))
            .map_err(|e| e.to_string())?;
        let payload = invite.to_qr_string().map_err(|e| e.to_string())?;
        let expires_at_ms = invite.expires_at as i64 * 1000;

        // 1) Ask UI to put invite on Convex FIRST.
        let _ = emit(
            event_tx,
            PeerEvent::HostInvite {
                payload,
                expires_at_ms,
            },
        )
        .await;
        let _ = emit(
            event_tx,
            PeerEvent::Status("Saving invite for peer…".into()),
        )
        .await;

        // 2) Block until Convex publish succeeds (or fail/cancel).
        //    Previously we opened the relay room immediately — guest often
        //    never saw the invite in time and one side looked "stuck".
        let published = loop {
            match cmd_rx.recv().await {
                Some(PeerCmd::InvitePublished) => break true,
                Some(PeerCmd::InvitePublishFailed(e)) => {
                    last_err = e;
                    break false;
                }
                Some(PeerCmd::Shutdown) | None => return Err("cancelled".into()),
                Some(PeerCmd::InvitePayload(_))
                | Some(PeerCmd::SendText(_))
                | Some(PeerCmd::SendPhoto { .. }) => {}
            }
        };
        if !published {
            eprintln!("[peer] host: invite publish failed: {last_err}");
            let _ = emit(
                event_tx,
                PeerEvent::Status(format!("Invite publish failed: {last_err}")),
            )
            .await;
            if attempt == max_attempts {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }

        // 3) Brief pause so guest subscription can deliver the payload.
        tokio::time::sleep(Duration::from_millis(400)).await;

        let _ = emit(
            event_tx,
            PeerEvent::Status(format!(
                "Invite ready — waiting for peer on relay ({attempt}/{max_attempts})…"
            )),
        )
        .await;

        // 4) Now enter the relay room (guest should already have the invite).
        let accept_fut = node.accept_peer(&invite);
        tokio::pin!(accept_fut);
        let result = loop {
            tokio::select! {
                res = &mut accept_fut => break res,
                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(PeerCmd::Shutdown) | None => {
                            return Err("cancelled".into());
                        }
                        Some(PeerCmd::SendText(_))
                        | Some(PeerCmd::SendPhoto { .. })
                        | Some(PeerCmd::InvitePayload(_))
                        | Some(PeerCmd::InvitePublished)
                        | Some(PeerCmd::InvitePublishFailed(_)) => {
                            let _ = emit(
                                event_tx,
                                PeerEvent::Status("Still waiting for peer to join…".into()),
                            )
                            .await;
                        }
                    }
                }
            }
        };

        match result {
            Ok(session) => {
                eprintln!("[peer] host: peer joined the room (attempt {attempt})");
                return Ok(session);
            }
            Err(e) => {
                last_err = e.to_string();
                eprintln!("[peer] host: accept failed (attempt {attempt}): {last_err}");
                let friendly = humanize_relay_error(&last_err);
                let _ = emit(event_tx, PeerEvent::Status(friendly)).await;
                if !is_transient_relay_error(&last_err) || attempt == max_attempts {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
    Err(humanize_relay_error(&last_err))
}

async fn guest_connect_with_retry(
    identity: Identity,
    relay: &str,
    config: NodeConfig,
    max_attempts: u32,
    cmd_rx: &mut mpsc::UnboundedReceiver<PeerCmd>,
    event_tx: &futures_mpsc::Sender<PeerEvent>,
) -> Result<Session, String> {
    let mut last_err = String::new();
    let mut latest_payload: Option<String> = None;
    // Guest keeps trying as long as host keeps republishing invites.
    let mut attempt: u32 = 0;

    loop {
        attempt = attempt.saturating_add(1);
        if attempt > max_attempts * 3 {
            break;
        }

        let payload = if let Some(p) = latest_payload.take() {
            p
        } else {
            let _ = emit(
                event_tx,
                PeerEvent::Status("Waiting for host invite… (host must open this DM too)".into()),
            )
            .await;
            loop {
                match cmd_rx.recv().await {
                    Some(PeerCmd::InvitePayload(p)) => {
                        eprintln!("[peer] guest: invite received ({} chars)", p.len());
                        break p;
                    }
                    Some(PeerCmd::Shutdown) | None => return Err("cancelled".into()),
                    Some(PeerCmd::SendText(_))
                    | Some(PeerCmd::SendPhoto { .. })
                    | Some(PeerCmd::InvitePublished)
                    | Some(PeerCmd::InvitePublishFailed(_)) => {
                        let _ = emit(
                            event_tx,
                            PeerEvent::Status("Still waiting for host invite…".into()),
                        )
                        .await;
                    }
                }
            }
        };

        let mut payload = payload;
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                PeerCmd::Shutdown => return Err("cancelled".into()),
                PeerCmd::InvitePayload(p) => payload = p,
                _ => {}
            }
        }

        // Host enters the room right after publish ack — give them a moment.
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Prefer the freshest invite if host republished during the delay.
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                PeerCmd::Shutdown => return Err("cancelled".into()),
                PeerCmd::InvitePayload(p) => payload = p,
                _ => {}
            }
        }

        let _ = emit(
            event_tx,
            PeerEvent::Status(format!("Joining relay (try {attempt})…")),
        )
        .await;

        let invite = match Invite::parse(&payload) {
            Ok(i) => i,
            Err(e) => {
                last_err = e.to_string();
                let _ = emit(
                    event_tx,
                    PeerEvent::Status(format!("Bad invite, waiting for a new one… ({e})")),
                )
                .await;
                continue;
            }
        };

        let node = Node::guest()
            .with_identity(identity.clone())
            .with_config(config.clone())
            .with_relay(relay)
            .map_err(|e| e.to_string())?
            .force_relay();

        match node.join_invite(invite).await {
            Ok(session) => {
                eprintln!("[peer] guest: joined room (attempt {attempt})");
                return Ok(session);
            }
            Err(e) => {
                last_err = e.to_string();
                eprintln!("[peer] guest: join failed (attempt {attempt}): {last_err}");
                let friendly = humanize_relay_error(&last_err);
                let _ = emit(event_tx, PeerEvent::Status(friendly)).await;
                tokio::time::sleep(Duration::from_secs(1)).await;
                while let Ok(cmd) = cmd_rx.try_recv() {
                    match cmd {
                        PeerCmd::Shutdown => return Err("cancelled".into()),
                        PeerCmd::InvitePayload(p) => latest_payload = Some(p),
                        _ => {}
                    }
                }
            }
        }
    }
    Err(humanize_relay_error(&last_err))
}

async fn connected_loop(
    session: &mut Session,
    cmd_rx: &mut mpsc::UnboundedReceiver<PeerCmd>,
    event_tx: &futures_mpsc::Sender<PeerEvent>,
) {
    // Application-level keepalive: idle WSS relays (and NATs) drop silent
    // connections. A Ping every 25 s keeps the secure channel warm; the peer
    // auto-answers with Pong (see reprotocol node.rs recv_app).
    let mut keepalive = tokio::time::interval(Duration::from_secs(25));
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = keepalive.tick() => {
                if let Err(e) = session.send_app(AppMessage::Ping(Vec::new())).await {
                    let _ = emit(event_tx, PeerEvent::Error(format!("keepalive: {e}"))).await;
                    break;
                }
            }
            cmd = cmd_rx.recv() => {
                match cmd {
                    None | Some(PeerCmd::Shutdown) => break,
                    Some(PeerCmd::InvitePayload(_))
                    | Some(PeerCmd::InvitePublished)
                    | Some(PeerCmd::InvitePublishFailed(_)) => {}
                    Some(PeerCmd::SendText(text)) => {
                        if let Err(e) = session.send_text(text).await {
                            let _ = emit(event_tx, PeerEvent::Error(format!("send: {e}"))).await;
                            break;
                        }
                    }
                    Some(PeerCmd::SendPhoto { bytes, content_type, width, height }) => {
                        if let Err(e) = session
                            .send_photo(&bytes, &content_type, width, height)
                            .await
                        {
                            let _ = emit(event_tx, PeerEvent::Error(format!("photo: {e}"))).await;
                            break;
                        }
                    }
                }
            }
            msg = session.recv_app() => {
                match msg {
                    Ok(AppMessage::Text(t)) => {
                        let _ = emit(event_tx, PeerEvent::Text(t)).await;
                    }
                    Ok(AppMessage::MediaStart {
                        content_type,
                        kind,
                        ..
                    }) => {
                        // Drain media frames into one buffer (photo).
                        if matches!(kind, peerseal::MediaKind::Photo) {
                            match drain_photo(session).await {
                                Ok(bytes) => {
                                    let _ = emit(
                                        event_tx,
                                        PeerEvent::Photo {
                                            bytes,
                                            content_type,
                                        },
                                    )
                                    .await;
                                }
                                Err(e) => {
                                    let _ = emit(
                                        event_tx,
                                        PeerEvent::Error(format!("photo recv: {e}")),
                                    )
                                    .await;
                                    break;
                                }
                            }
                        } else {
                            // Skip unknown media streams.
                            let _ = drain_photo(session).await;
                        }
                    }
                    Ok(AppMessage::Binary(b)) => {
                        if let Ok(t) = String::from_utf8(b) {
                            let _ = emit(event_tx, PeerEvent::Text(t)).await;
                        }
                    }
                    Ok(AppMessage::SasAck(_))
                    | Ok(AppMessage::Pong(_))
                    | Ok(AppMessage::Ping(_))
                    | Ok(AppMessage::Rekey)
                    | Ok(AppMessage::FileMeta { .. })
                    | Ok(AppMessage::FileChunk { .. })
                    | Ok(AppMessage::FileEnd { .. })
                    | Ok(AppMessage::MediaFrame { .. })
                    | Ok(AppMessage::MediaEnd { .. })
                    | Ok(AppMessage::VcOffer { .. })
                    | Ok(AppMessage::VcAnswer { .. })
                    | Ok(AppMessage::VcVideo { .. })
                    | Ok(AppMessage::VcAudio { .. })
                    | Ok(AppMessage::VcControl { .. }) => {}
                    Err(e) => {
                        let _ = emit(
                            event_tx,
                            PeerEvent::Error(format!("session closed: {e}")),
                        )
                        .await;
                        break;
                    }
                }
            }
        }
    }
}

/// Collect MediaFrame chunks until MediaEnd (used after MediaStart for photos).
async fn drain_photo(session: &mut Session) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    loop {
        match session.recv_app().await.map_err(|e| e.to_string())? {
            AppMessage::MediaFrame { data, .. } => buf.extend_from_slice(&data),
            AppMessage::MediaEnd { .. } => return Ok(buf),
            // Keepalive/control frames can legitimately arrive mid-transfer;
            // skip them instead of aborting the whole photo (which used to
            // also desync the stream — the remaining frames were then
            // misread as top-level messages).
            AppMessage::Ping(_) | AppMessage::Pong(_) | AppMessage::SasAck(_) => {}
            other => {
                return Err(format!("unexpected during photo: {other:?}"));
            }
        }
    }
}
