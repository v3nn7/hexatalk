//! Multi-party voice rooms (server voice channels + group calls).
//!
//! Full mesh: every pair of users in the room maintains one WebRTC peer
//! connection. Signaling goes through Convex (`voiceLinks` / `voiceLinkIce`);
//! the lexicographically smaller userId always creates the offer (stable
//! offerer → no glare when both join at once).
//!
//! Mic capture is shared: one IMA-ADPCM @ 24 kHz frame (see src/adpcm.rs)
//! is fanned out to every local track. Remote RTP from all peers is mixed
//! into a single jitter buffer for playback.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use convex::{ConvexClient, FunctionResult, Value};
use futures::channel::mpsc::Sender as EventSender;
use futures::{SinkExt, StreamExt};
use maplit::btreemap;
use webrtc::api::APIBuilder;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};
use webrtc::track::track_local::TrackLocal;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;
use webrtc::track::track_remote::TrackRemote;

use super::adpcm;
use super::call::{spawn_capture_thread, spawn_playback_thread, turn_ice_server};

#[derive(Debug, Clone)]
pub(crate) enum RoomVoiceEvent {
    Connecting,
    /// At least one peer is media-connected.
    Connected,
    /// Status line for the UI toast / header.
    Status(String),
    Ended,
    Failed(String),
}

pub(crate) struct RoomVoiceParams {
    pub(crate) client: ConvexClient,
    pub(crate) session_token: String,
    pub(crate) user_id: String,
    pub(crate) conversation_id: String,
    pub(crate) input_device: Option<String>,
    pub(crate) output_device: Option<String>,
    pub(crate) muted: Arc<AtomicBool>,
    pub(crate) output_muted: Arc<AtomicBool>,
    pub(crate) noise_gate: Arc<AtomicU32>,
    /// Per-peer volume gains (peer user_id -> gain), applied live to each
    /// peer's decoded remote audio in `create_peer_slot`'s on_track.
    pub(crate) gains: Arc<Mutex<HashMap<String, f32>>>,
}

struct PeerSlot {
    pc: Arc<RTCPeerConnection>,
    local_track: Arc<TrackLocalStaticRTP>,
    link_id: Option<String>,
    is_offerer: bool,
    answer_applied: bool,
    /// Stop the ICE drain task when the slot is dropped.
    ice_alive: Arc<AtomicBool>,
}

fn ice_config() -> RTCConfiguration {
    let mut ice_servers = vec![RTCIceServer {
        urls: vec![
            "stun:stun.l.google.com:19302".to_string(),
            "stun:stun1.l.google.com:19302".to_string(),
            "stun:stun2.l.google.com:19302".to_string(),
            "stun:stun.nextcloud.com:443".to_string(),
            "stun:stun.sipnet.net:3478".to_string(),
        ],
        ..Default::default()
    }];
    if let Some(turn) = turn_ice_server() {
        ice_servers.push(turn);
    }
    RTCConfiguration {
        ice_servers,
        ..Default::default()
    }
}

async fn build_api() -> Result<webrtc::api::API, String> {
    let mut media_engine = MediaEngine::default();
    // Only the HexaTalk ADPCM codec (see call.rs for why: mismatched peers
    // must fail negotiation, not play garbage).
    media_engine
        .register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: adpcm::MIME_TYPE.to_string(),
                    clock_rate: adpcm::WIRE_SAMPLE_RATE,
                    channels: 1,
                    ..Default::default()
                },
                payload_type: adpcm::RTP_PAYLOAD_TYPE,
                ..Default::default()
            },
            RTPCodecType::Audio,
        )
        .map_err(|_| "Could not set up audio codecs".to_string())?;
    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut media_engine)
        .map_err(|_| "Could not set up media pipeline".to_string())?;
    Ok(APIBuilder::new()
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .build())
}

async fn fail(output: &mut EventSender<RoomVoiceEvent>, msg: impl Into<String>) {
    let _ = output.send(RoomVoiceEvent::Failed(msg.into())).await;
}

fn parse_str_field(obj: &std::collections::BTreeMap<String, Value>, key: &str) -> Option<String> {
    match obj.get(key) {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}

fn parse_bool_field(obj: &std::collections::BTreeMap<String, Value>, key: &str) -> bool {
    matches!(obj.get(key), Some(Value::Boolean(true)))
}

pub(crate) async fn run_room_voice(
    params: RoomVoiceParams,
    mut output: EventSender<RoomVoiceEvent>,
) {
    let RoomVoiceParams {
        mut client,
        session_token,
        user_id,
        conversation_id,
        input_device,
        output_device,
        muted,
        output_muted,
        noise_gate,
        gains,
    } = params;

    let api = match build_api().await {
        Ok(a) => a,
        Err(e) => {
            fail(&mut output, e).await;
            return;
        }
    };
    let config = ice_config();

    let jitter: Arc<Mutex<VecDeque<i16>>> = Arc::new(Mutex::new(VecDeque::new()));
    let (playback_stop_tx, playback_stop_rx) = std::sync::mpsc::channel::<()>();
    // Ensure OS audio threads always stop if this future is cancelled (user
    // left the room / subscription dropped) without running the teardown path.
    let playback_stop_tx = StopOnDrop(Some(playback_stop_tx));
    if let Err(msg) = spawn_playback_thread(
        output_device,
        Arc::clone(&jitter),
        output_muted,
        playback_stop_rx,
    ) {
        fail(&mut output, msg).await;
        return;
    }

    // Shared mic → fan-out to every local track.
    let (frame_tx, mut frame_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let (capture_stop_tx, capture_stop_rx) = std::sync::mpsc::channel::<()>();
    let capture_stop_tx = StopOnDrop(Some(capture_stop_tx));
    if let Err(msg) =
        spawn_capture_thread(input_device, muted, noise_gate, frame_tx, capture_stop_rx)
    {
        fail(&mut output, msg).await;
        return;
    }

    let local_tracks: Arc<Mutex<Vec<Arc<TrackLocalStaticRTP>>>> = Arc::new(Mutex::new(Vec::new()));
    let tracks_for_send = Arc::clone(&local_tracks);
    tokio::spawn(async move {
        // One shared sequence/timestamp generator is fine: each track is a
        // separate peer connection with its own SSRC, so identical numbers
        // across tracks never collide (see call.rs for the packet format).
        let mut sequence_number: u16 = 0;
        let mut timestamp: u32 = 0;
        let mut first_packet = true;
        while let Some(chunk) = frame_rx.recv().await {
            let packet = rtp::packet::Packet {
                header: rtp::header::Header {
                    version: 2,
                    marker: first_packet,
                    sequence_number,
                    timestamp,
                    ..Default::default()
                },
                payload: Bytes::from(chunk),
            };
            first_packet = false;
            sequence_number = sequence_number.wrapping_add(1);
            timestamp = timestamp.wrapping_add(adpcm::FRAME_SAMPLES as u32);
            let tracks = tracks_for_send
                .lock()
                .map(|g| g.clone())
                .unwrap_or_default();
            for track in tracks {
                let _ = track.write_rtp_with_extensions(&packet, &[]).await;
            }
        }
    });

    let _ = output.send(RoomVoiceEvent::Connecting).await;
    let _ = output
        .send(RoomVoiceEvent::Status("Joining voice room…".into()))
        .await;

    let mut peers: HashMap<String, PeerSlot> = HashMap::new();
    let mut applied_ice: HashMap<String, HashSet<String>> = HashMap::new();
    let any_connected = Arc::new(AtomicBool::new(false));

    // Subscribe to room roster + my mesh links.
    let users_sub = client
        .subscribe(
            "voice:listInChannel",
            btreemap! {
                "sessionToken".to_string() => Value::String(session_token.clone()),
                "conversationId".to_string() => Value::String(conversation_id.clone()),
            },
        )
        .await;
    let links_sub = client
        .subscribe(
            "voice:listMyLinks",
            btreemap! {
                "sessionToken".to_string() => Value::String(session_token.clone()),
                "conversationId".to_string() => Value::String(conversation_id.clone()),
            },
        )
        .await;

    let Ok(mut users_sub) = users_sub else {
        fail(&mut output, "Could not watch voice room").await;
        return;
    };
    let Ok(mut links_sub) = links_sub else {
        fail(&mut output, "Could not watch voice links").await;
        return;
    };

    // Per-link ICE candidate subscriptions (link_id -> stream).
    // We poll listLinkIce via a single shared ticker + active link set to
    // avoid unbounded subscribe churn.
    let mut active_link_ids: HashSet<String> = HashSet::new();
    let mut ice_tick = tokio::time::interval(Duration::from_millis(400));

    loop {
        tokio::select! {
            next = users_sub.next() => {
                let Some(result) = next else { break; };
                let remote_ids = match result {
                    FunctionResult::Value(Value::Array(items)) => {
                        items.into_iter().filter_map(|item| {
                            let Value::Object(obj) = item else { return None; };
                            let id = parse_str_field(&obj, "userId")?;
                            if id == user_id { None } else { Some(id) }
                        }).collect::<HashSet<_>>()
                    }
                    _ => continue,
                };

                // Drop peers who left.
                let gone: Vec<String> = peers
                    .keys()
                    .filter(|id| !remote_ids.contains(*id))
                    .cloned()
                    .collect();
                for id in gone {
                    if let Some(slot) = peers.remove(&id) {
                        slot.ice_alive.store(false, Ordering::Relaxed);
                        if let Some(link_id) = &slot.link_id {
                            active_link_ids.remove(link_id);
                            let _ = client.mutation(
                                "voice:endLink",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session_token.clone()),
                                    "linkId".to_string() => Value::String(link_id.clone()),
                                },
                            ).await;
                        }
                        let _ = slot.pc.close().await;
                        if let Ok(mut tracks) = local_tracks.lock() {
                            tracks.retain(|t| !Arc::ptr_eq(t, &slot.local_track));
                        }
                    }
                }

                // Ensure a PC for every remote peer.
                for peer_id in remote_ids {
                    if peers.contains_key(&peer_id) {
                        continue;
                    }
                    let is_offerer = user_id.as_str() < peer_id.as_str();
                    match create_peer_slot(
                        &api,
                        &config,
                        is_offerer,
                        Arc::clone(&jitter),
                        Arc::clone(&any_connected),
                        output.clone(),
                        peer_id.clone(),
                        Arc::clone(&gains),
                    ).await {
                        Ok(mut slot) => {
                            if let Ok(mut tracks) = local_tracks.lock() {
                                tracks.push(Arc::clone(&slot.local_track));
                            }
                            if is_offerer {
                                if let Err(e) = publish_offer_for(
                                    &mut client,
                                    &session_token,
                                    &conversation_id,
                                    &peer_id,
                                    &mut slot,
                                ).await {
                                    let _ = output.send(RoomVoiceEvent::Status(e)).await;
                                    let _ = slot.pc.close().await;
                                    if let Ok(mut tracks) = local_tracks.lock() {
                                        tracks.retain(|t| !Arc::ptr_eq(t, &slot.local_track));
                                    }
                                    continue;
                                }
                                if let Some(link_id) = &slot.link_id {
                                    active_link_ids.insert(link_id.clone());
                                    start_ice_sender(
                                        client.clone(),
                                        session_token.clone(),
                                        link_id.clone(),
                                        Arc::clone(&slot.pc),
                                        Arc::clone(&slot.ice_alive),
                                    );
                                }
                            }
                            peers.insert(peer_id, slot);
                        }
                        Err(e) => {
                            let _ = output.send(RoomVoiceEvent::Status(e)).await;
                        }
                    }
                }

                let n = peers.len();
                let _ = output.send(RoomVoiceEvent::Status(
                    if n == 0 {
                        "In voice · waiting for others…".into()
                    } else {
                        format!("In voice · {n} connection(s)")
                    },
                )).await;
            }

            next = links_sub.next() => {
                let Some(result) = next else { break; };
                let links = match result {
                    FunctionResult::Value(Value::Array(items)) => items,
                    _ => continue,
                };
                for item in links {
                    let Value::Object(obj) = item else { continue; };
                    let Some(peer_id) = parse_str_field(&obj, "peerId") else { continue; };
                    let Some(link_id) = parse_str_field(&obj, "linkId") else { continue; };
                    let is_offerer = parse_bool_field(&obj, "isOfferer");
                    let offer_sdp = parse_str_field(&obj, "offerSdp");
                    let answer_sdp = parse_str_field(&obj, "answerSdp");
                    let status = parse_str_field(&obj, "status").unwrap_or_default();
                    if status == "ended" {
                        continue;
                    }

                    // Ensure peer slot exists (answerer may learn about offer before roster tick).
                    if !peers.contains_key(&peer_id) {
                        match create_peer_slot(
                            &api,
                            &config,
                            is_offerer,
                            Arc::clone(&jitter),
                            Arc::clone(&any_connected),
                            output.clone(),
                            peer_id.clone(),
                            Arc::clone(&gains),
                        ).await {
                            Ok(slot) => {
                                if let Ok(mut tracks) = local_tracks.lock() {
                                    tracks.push(Arc::clone(&slot.local_track));
                                }
                                peers.insert(peer_id.clone(), slot);
                            }
                            Err(_) => continue,
                        }
                    }

                    let Some(slot) = peers.get_mut(&peer_id) else { continue; };

                    if slot.link_id.as_deref() != Some(link_id.as_str()) {
                        slot.link_id = Some(link_id.clone());
                        active_link_ids.insert(link_id.clone());
                        start_ice_sender(
                            client.clone(),
                            session_token.clone(),
                            link_id.clone(),
                            Arc::clone(&slot.pc),
                            Arc::clone(&slot.ice_alive),
                        );
                    }

                    if !is_offerer && !slot.answer_applied {
                        if let Some(offer_json) = offer_sdp {
                            if apply_answerer_offer(
                                &mut client,
                                &session_token,
                                &link_id,
                                &offer_json,
                                slot,
                            ).await.is_ok() {
                                slot.answer_applied = true;
                            }
                        }
                    }

                    if is_offerer && !slot.answer_applied {
                        if let Some(answer_json) = answer_sdp {
                            if let Ok(answer) =
                                serde_json::from_str::<RTCSessionDescription>(&answer_json)
                            {
                                if slot.pc.set_remote_description(answer).await.is_ok() {
                                    slot.answer_applied = true;
                                }
                            }
                        }
                    }
                }
            }

            _ = ice_tick.tick() => {
                // Pull remote ICE for every active link.
                let link_ids: Vec<String> = active_link_ids.iter().cloned().collect();
                for link_id in link_ids {
                    let result = client
                        .query(
                            "voice:listLinkIce",
                            btreemap! {
                                "sessionToken".to_string() => Value::String(session_token.clone()),
                                "linkId".to_string() => Value::String(link_id.clone()),
                            },
                        )
                        .await;
                    let Ok(FunctionResult::Value(Value::Array(rows))) = result else {
                        continue;
                    };
                    // Find PC for this link.
                    let pc = peers.values().find_map(|s| {
                        if s.link_id.as_deref() == Some(link_id.as_str()) {
                            Some(Arc::clone(&s.pc))
                        } else {
                            None
                        }
                    });
                    let Some(pc) = pc else { continue; };
                    let applied = applied_ice.entry(link_id).or_default();
                    for row in rows {
                        let Value::Object(obj) = row else { continue; };
                        let Some(id) = parse_str_field(&obj, "id") else { continue; };
                        if !applied.insert(id) {
                            continue;
                        }
                        let Some(cand_json) = parse_str_field(&obj, "candidate") else {
                            continue;
                        };
                        if let Ok(init) = serde_json::from_str::<RTCIceCandidateInit>(&cand_json) {
                            let _ = pc.add_ice_candidate(init).await;
                        }
                    }
                }

                if any_connected.load(Ordering::Relaxed) {
                    let _ = output.send(RoomVoiceEvent::Connected).await;
                }
            }
        }
    }

    // Teardown.
    for (_, slot) in peers.drain() {
        slot.ice_alive.store(false, Ordering::Relaxed);
        if let Some(link_id) = slot.link_id {
            let _ = client
                .mutation(
                    "voice:endLink",
                    btreemap! {
                        "sessionToken".to_string() => Value::String(session_token.clone()),
                        "linkId".to_string() => Value::String(link_id),
                    },
                )
                .await;
        }
        let _ = slot.pc.close().await;
    }
    let _ = client
        .mutation(
            "voice:leave",
            btreemap! {
                "sessionToken".to_string() => Value::String(session_token),
                "conversationId".to_string() => Value::String(conversation_id),
            },
        )
        .await;

    drop(capture_stop_tx);
    drop(playback_stop_tx);
    let _ = output.send(RoomVoiceEvent::Ended).await;
}

/// Sends `()` on drop so cpal threads blocked on `recv` always exit.
struct StopOnDrop(Option<std::sync::mpsc::Sender<()>>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(());
        }
    }
}

async fn create_peer_slot(
    api: &webrtc::api::API,
    config: &RTCConfiguration,
    is_offerer: bool,
    jitter: Arc<Mutex<VecDeque<i16>>>,
    any_connected: Arc<AtomicBool>,
    output: EventSender<RoomVoiceEvent>,
    peer_id: String,
    gains: Arc<Mutex<HashMap<String, f32>>>,
) -> Result<PeerSlot, String> {
    let pc = api
        .new_peer_connection(config.clone())
        .await
        .map_err(|e| format!("Could not start peer link: {e}"))?;
    let pc = Arc::new(pc);

    let local_track = Arc::new(TrackLocalStaticRTP::new(
        RTCRtpCodecCapability {
            mime_type: adpcm::MIME_TYPE.to_string(),
            clock_rate: adpcm::WIRE_SAMPLE_RATE,
            channels: 1,
            ..Default::default()
        },
        "audio".to_string(),
        "hexatalk-room".to_string(),
    ));
    pc.add_track(Arc::clone(&local_track) as Arc<dyn TrackLocal + Send + Sync>)
        .await
        .map_err(|_| "Could not attach microphone track".to_string())?;

    let jitter_for_track = Arc::clone(&jitter);
    pc.on_track(Box::new(move |track: Arc<TrackRemote>, _r, _t| {
        let jitter = Arc::clone(&jitter_for_track);
        let gains = Arc::clone(&gains);
        let peer_id = peer_id.clone();
        Box::pin(async move {
            loop {
                match track.read_rtp().await {
                    Ok((packet, _)) => {
                        // Drop foreign payloads (mismatched app version).
                        let Some(mut samples) = adpcm::decode_frame(&packet.payload) else {
                            continue;
                        };
                        let g = gains
                            .lock()
                            .ok()
                            .and_then(|m| m.get(&peer_id).copied())
                            .unwrap_or(1.0);
                        crate::media::call::apply_gain(&mut samples, g);
                        if let Ok(mut buf) = jitter.lock() {
                            buf.extend(samples);
                            // Soft cap ~500ms at 24kHz across all peers.
                            while buf.len() > 12000 {
                                buf.pop_front();
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        })
    }));

    let connected_flag = Arc::clone(&any_connected);
    pc.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
        let connected_flag = Arc::clone(&connected_flag);
        let mut output = output.clone();
        Box::pin(async move {
            if s == RTCPeerConnectionState::Connected {
                connected_flag.store(true, Ordering::Relaxed);
                let _ = output.send(RoomVoiceEvent::Connected).await;
            }
        })
    }));

    let _ = is_offerer; // used by caller for SDP role
    Ok(PeerSlot {
        pc,
        local_track,
        link_id: None,
        is_offerer,
        answer_applied: false,
        ice_alive: Arc::new(AtomicBool::new(true)),
    })
}

async fn publish_offer_for(
    client: &mut ConvexClient,
    session_token: &str,
    conversation_id: &str,
    peer_id: &str,
    slot: &mut PeerSlot,
) -> Result<(), String> {
    let offer = slot
        .pc
        .create_offer(None)
        .await
        .map_err(|e| format!("{e}"))?;
    slot.pc
        .set_local_description(offer)
        .await
        .map_err(|_| "Could not set local offer".to_string())?;
    let local = slot
        .pc
        .local_description()
        .await
        .ok_or_else(|| "Missing local description".to_string())?;
    let offer_json =
        serde_json::to_string(&local).map_err(|_| "Could not serialize offer".to_string())?;

    let result = client
        .mutation(
            "voice:publishOffer",
            btreemap! {
                "sessionToken".to_string() => Value::String(session_token.to_string()),
                "conversationId".to_string() => Value::String(conversation_id.to_string()),
                "peerId".to_string() => Value::String(peer_id.to_string()),
                "offerSdp".to_string() => Value::String(offer_json),
            },
        )
        .await
        .map_err(|e| e.to_string())?;

    match result {
        FunctionResult::Value(Value::String(id)) => {
            slot.link_id = Some(id);
            Ok(())
        }
        FunctionResult::ErrorMessage(msg) => Err(msg),
        _ => Err("Could not publish voice offer".into()),
    }
}

async fn apply_answerer_offer(
    client: &mut ConvexClient,
    session_token: &str,
    link_id: &str,
    offer_json: &str,
    slot: &mut PeerSlot,
) -> Result<(), String> {
    let offer: RTCSessionDescription =
        serde_json::from_str(offer_json).map_err(|_| "Bad voice offer".to_string())?;
    slot.pc
        .set_remote_description(offer)
        .await
        .map_err(|_| "Could not apply voice offer".to_string())?;
    let answer = slot
        .pc
        .create_answer(None)
        .await
        .map_err(|e| format!("{e}"))?;
    slot.pc
        .set_local_description(answer)
        .await
        .map_err(|_| "Could not set local answer".to_string())?;
    let local = slot
        .pc
        .local_description()
        .await
        .ok_or_else(|| "Missing local answer".to_string())?;
    let answer_json =
        serde_json::to_string(&local).map_err(|_| "Could not serialize answer".to_string())?;

    let result = client
        .mutation(
            "voice:publishAnswer",
            btreemap! {
                "sessionToken".to_string() => Value::String(session_token.to_string()),
                "linkId".to_string() => Value::String(link_id.to_string()),
                "answerSdp".to_string() => Value::String(answer_json),
            },
        )
        .await
        .map_err(|e| e.to_string())?;

    if let FunctionResult::ErrorMessage(msg) = result {
        return Err(msg);
    }
    Ok(())
}

fn start_ice_sender(
    mut client: ConvexClient,
    session_token: String,
    link_id: String,
    pc: Arc<RTCPeerConnection>,
    alive: Arc<AtomicBool>,
) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<RTCIceCandidateInit>();
    pc.on_ice_candidate(Box::new(move |candidate: Option<RTCIceCandidate>| {
        if let Some(candidate) = candidate {
            if let Ok(init) = candidate.to_json() {
                let _ = tx.send(init);
            }
        }
        Box::pin(async {})
    }));

    tokio::spawn(async move {
        while let Some(init) = rx.recv().await {
            if !alive.load(Ordering::Relaxed) {
                break;
            }
            let Ok(candidate_json) = serde_json::to_string(&init) else {
                continue;
            };
            let _ = client
                .mutation(
                    "voice:addLinkIce",
                    btreemap! {
                        "sessionToken".to_string() => Value::String(session_token.clone()),
                        "linkId".to_string() => Value::String(link_id.clone()),
                        "candidate".to_string() => Value::String(candidate_json),
                    },
                )
                .await;
        }
    });
}
