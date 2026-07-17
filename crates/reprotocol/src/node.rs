//! High-level host/guest API: invite, accept, join (direct-first + relay).

use crate::error::{Error, Result};
use crate::identity::{Identity, TofuCheck, TofuStore, fingerprint_of, short_fingerprint_of};
use crate::invite::{Invite, InviteOptions};
use crate::protocol::{AppMessage, DEFAULT_CHUNK};
use crate::sas::{sas_emojis, sas_hex, sas_numbers};
use crate::vc::{AudioFrame, VcCall, VcEvent, VideoFrame};
use crate::session::{HandshakeOpts, NoisePattern, Role, SecureStream, SessionConfig};
use crate::transfer::{recv_file_from_meta, send_bytes_as_file, send_file_on};
use crate::transport::{TcpEndpoint, dial_direct};
use crate::util::{env_relay_url, normalize_relay_url};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

#[cfg(feature = "relay")]
use crate::transport::RelayConnection;

/// How the underlying transport was established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    /// Direct TCP between peers.
    DirectTcp,
    /// WebSocket relay (ciphertext only).
    Relay,
}

/// Configuration for [`Node`] connect/accept behaviour.
#[derive(Debug, Clone)]
pub struct NodeConfig {
    /// Per-address direct dial timeout.
    pub dial_timeout: Duration,
    /// How long the host waits for an inbound direct connection.
    pub accept_timeout: Option<Duration>,
    /// How long to wait on the relay for the other peer.
    pub relay_wait_timeout: Duration,
    /// Secure session parameters.
    pub session: SessionConfig,
    /// Try direct before relay (default true). Ignored when [`Self::force_relay`] is set.
    pub direct_first: bool,
    /// Skip direct entirely; only use relay (requires `relay_url` on the invite).
    pub force_relay: bool,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            dial_timeout: Duration::from_secs(3),
            accept_timeout: Some(Duration::from_secs(180)),
            relay_wait_timeout: Duration::from_secs(120),
            session: SessionConfig::default(),
            direct_first: true,
            force_relay: false,
        }
    }
}

/// Metadata captured after the Noise handshake.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    /// Noise pattern used.
    pub pattern: NoisePattern,
    /// Handshake hash (SAS input).
    pub handshake_hash: [u8; 32],
    /// Local static public key (XX only).
    pub local_static: Option<[u8; 32]>,
    /// Remote static public key (XX only).
    pub remote_static: Option<[u8; 32]>,
}

impl SessionInfo {
    /// Emoji SAS (default 5).
    pub fn sas_emojis(&self) -> String {
        sas_emojis(&self.handshake_hash, 5)
    }

    /// Hex SAS.
    pub fn sas_hex(&self) -> String {
        sas_hex(&self.handshake_hash, 4)
    }

    /// Numeric SAS groups.
    pub fn sas_numbers(&self) -> String {
        sas_numbers(&self.handshake_hash, 3)
    }

    /// Remote fingerprint if identity handshake was used.
    pub fn remote_fingerprint(&self) -> Option<String> {
        self.remote_static.as_ref().map(|k| fingerprint_of(k))
    }

    /// Short remote fingerprint.
    pub fn remote_short_fingerprint(&self) -> Option<String> {
        self.remote_static.as_ref().map(|k| short_fingerprint_of(k))
    }
}

/// Type-erased secure session over either TCP or relay.
pub struct Session {
    inner: SessionInner,
    /// Which transport was used.
    pub transport: TransportKind,
    /// Room id for logging/UI.
    pub room_id: String,
    /// Crypto / identity metadata.
    pub info: SessionInfo,
    next_transfer_id: u32,
    next_stream_id: u32,
}

enum SessionInner {
    Tcp(SecureStream<TcpStream>),
    #[cfg(feature = "relay")]
    Relay(SecureStream<RelayConnection>),
}

impl Session {
    fn from_parts(
        inner: SessionInner,
        transport: TransportKind,
        room_id: String,
        stream_pattern: NoisePattern,
        handshake_hash: [u8; 32],
        local_static: Option<[u8; 32]>,
        remote_static: Option<[u8; 32]>,
    ) -> Self {
        Self {
            inner,
            transport,
            room_id,
            info: SessionInfo {
                pattern: stream_pattern,
                handshake_hash,
                local_static,
                remote_static,
            },
            next_transfer_id: 1,
            next_stream_id: 1,
        }
    }

    fn take_meta<S>(s: &SecureStream<S>) -> (NoisePattern, [u8; 32], Option<[u8; 32]>, Option<[u8; 32]>) {
        (
            s.pattern,
            s.handshake_hash,
            s.local_static,
            s.remote_static,
        )
    }

    /// Send raw application plaintext (E2E encrypted). Prefer [`Self::send_app`].
    pub async fn send(&mut self, plaintext: &[u8]) -> Result<()> {
        match &mut self.inner {
            SessionInner::Tcp(s) => s.send(plaintext).await,
            #[cfg(feature = "relay")]
            SessionInner::Relay(s) => s.send(plaintext).await,
        }
    }

    /// Receive one raw plaintext frame.
    pub async fn recv(&mut self) -> Result<Vec<u8>> {
        match &mut self.inner {
            SessionInner::Tcp(s) => s.recv().await,
            #[cfg(feature = "relay")]
            SessionInner::Relay(s) => s.recv().await,
        }
    }

    /// Send a typed application message.
    pub async fn send_app(&mut self, msg: AppMessage) -> Result<()> {
        let bytes = msg.encode()?;
        self.send(&bytes).await?;
        if matches!(msg, AppMessage::Rekey) {
            self.rekey_local().await?;
        }
        Ok(())
    }

    /// Receive and decode a typed application message.
    ///
    /// Automatically answers `Ping` with `Pong` and applies peer `Rekey`.
    pub async fn recv_app(&mut self) -> Result<AppMessage> {
        loop {
            let raw = self.recv().await?;
            let msg = AppMessage::decode(&raw)?;
            match msg {
                AppMessage::Ping(payload) => {
                    self.send_app(AppMessage::Pong(payload)).await?;
                    continue;
                }
                AppMessage::Rekey => {
                    self.rekey_local().await?;
                    continue;
                }
                other => return Ok(other),
            }
        }
    }

    /// Send UTF-8 chat text.
    pub async fn send_text(&mut self, text: impl Into<String>) -> Result<()> {
        self.send_app(AppMessage::Text(text.into())).await
    }

    /// Allocate a transfer id and send a file from disk.
    pub async fn send_file(&mut self, path: impl AsRef<Path>, content_type: &str) -> Result<u32> {
        let id = self.next_transfer_id;
        self.next_transfer_id = self.next_transfer_id.saturating_add(1);
        match &mut self.inner {
            SessionInner::Tcp(s) => {
                send_file_on(s, path, id, content_type, DEFAULT_CHUNK, None::<fn(u64, u64)>).await?;
            }
            #[cfg(feature = "relay")]
            SessionInner::Relay(s) => {
                send_file_on(s, path, id, content_type, DEFAULT_CHUNK, None::<fn(u64, u64)>).await?;
            }
        }
        Ok(id)
    }

    /// Send in-memory bytes as a named file.
    pub async fn send_bytes_as_file(
        &mut self,
        name: &str,
        data: &[u8],
        content_type: &str,
    ) -> Result<u32> {
        let id = self.next_transfer_id;
        self.next_transfer_id = self.next_transfer_id.saturating_add(1);
        match &mut self.inner {
            SessionInner::Tcp(s) => {
                send_bytes_as_file(s, name, data, id, content_type, DEFAULT_CHUNK).await?;
            }
            #[cfg(feature = "relay")]
            SessionInner::Relay(s) => {
                send_bytes_as_file(s, name, data, id, content_type, DEFAULT_CHUNK).await?;
            }
        }
        Ok(id)
    }

    /// If `msg` is `FileMeta`, receive the rest of the file into `dest`.
    pub async fn recv_file(&mut self, msg: AppMessage, dest: impl AsRef<Path>) -> Result<[u8; 32]> {
        match &mut self.inner {
            SessionInner::Tcp(s) => recv_file_from_meta(s, msg, dest).await,
            #[cfg(feature = "relay")]
            SessionInner::Relay(s) => recv_file_from_meta(s, msg, dest).await,
        }
    }

    /// Send a photo / still image blob.
    pub async fn send_photo(
        &mut self,
        data: &[u8],
        content_type: &str,
        width: u32,
        height: u32,
    ) -> Result<u32> {
        let stream_id = self.next_stream_id;
        self.next_stream_id = self.next_stream_id.saturating_add(1);
        match &mut self.inner {
            SessionInner::Tcp(s) => {
                crate::media::send_photo(s, stream_id, content_type, data, width, height).await?;
            }
            #[cfg(feature = "relay")]
            SessionInner::Relay(s) => {
                crate::media::send_photo(s, stream_id, content_type, data, width, height).await?;
            }
        }
        Ok(stream_id)
    }

    /// Open screen-share stream; use [`Self::send_media_frame`] afterwards.
    pub async fn open_screen(
        &mut self,
        content_type: &str,
        width: u32,
        height: u32,
    ) -> Result<u32> {
        let stream_id = self.next_stream_id;
        self.next_stream_id = self.next_stream_id.saturating_add(1);
        match &mut self.inner {
            SessionInner::Tcp(s) => {
                crate::media::open_screen_stream(s, stream_id, content_type, width, height).await?;
            }
            #[cfg(feature = "relay")]
            SessionInner::Relay(s) => {
                crate::media::open_screen_stream(s, stream_id, content_type, width, height).await?;
            }
        }
        Ok(stream_id)
    }

    /// Open audio stream (e.g. `audio/opus`).
    pub async fn open_audio(&mut self, content_type: &str) -> Result<u32> {
        let stream_id = self.next_stream_id;
        self.next_stream_id = self.next_stream_id.saturating_add(1);
        match &mut self.inner {
            SessionInner::Tcp(s) => {
                crate::media::open_audio_stream(s, stream_id, content_type).await?;
            }
            #[cfg(feature = "relay")]
            SessionInner::Relay(s) => {
                crate::media::open_audio_stream(s, stream_id, content_type).await?;
            }
        }
        Ok(stream_id)
    }

    /// Send one media frame on an open stream.
    pub async fn send_media_frame(&mut self, stream_id: u32, seq: u32, data: &[u8]) -> Result<()> {
        match &mut self.inner {
            SessionInner::Tcp(s) => {
                crate::media::send_media_frame(s, stream_id, seq, data).await
            }
            #[cfg(feature = "relay")]
            SessionInner::Relay(s) => {
                crate::media::send_media_frame(s, stream_id, seq, data).await
            }
        }
    }

    /// Close a media stream.
    pub async fn close_media(&mut self, stream_id: u32) -> Result<()> {
        match &mut self.inner {
            SessionInner::Tcp(s) => crate::media::close_media_stream(s, stream_id).await,
            #[cfg(feature = "relay")]
            SessionInner::Relay(s) => crate::media::close_media_stream(s, stream_id).await,
        }
    }

    /// Send rekey control and advance local Noise keys.
    pub async fn request_rekey(&mut self) -> Result<()> {
        // send_app handles local rekey after Rekey message
        self.send_app(AppMessage::Rekey).await
    }

    /// Send VC offer from [`VcCall`].
    pub async fn vc_send_offer(&mut self, call: &VcCall) -> Result<()> {
        self.send_app(call.make_offer()).await
    }

    /// Answer a remote offer (updates `call`) and send answer.
    pub async fn vc_send_answer(&mut self, call: &mut VcCall, offer: &crate::vc::VcConfig) -> Result<()> {
        let ans = call.make_answer_for(offer);
        self.send_app(ans).await
    }

    /// Send one HD video frame (up to 25 MiB logical, auto-fragmented).
    pub async fn vc_send_video(&mut self, call: &mut VcCall, frame: VideoFrame) -> Result<()> {
        let msg = call.pack_video(frame);
        self.send_app(msg).await
    }

    /// Send one audio packet.
    pub async fn vc_send_audio(&mut self, call: &mut VcCall, frame: AudioFrame) -> Result<()> {
        let msg = call.pack_audio(frame);
        self.send_app(msg).await
    }

    /// Request a keyframe from the peer encoder.
    pub async fn vc_request_keyframe(&mut self) -> Result<()> {
        self.send_app(VcCall::request_keyframe_msg()).await
    }

    /// End VC media.
    pub async fn vc_bye(&mut self) -> Result<()> {
        self.send_app(VcCall::bye_msg()).await
    }

    /// Receive next app message demuxed as a VC event (handles ping/rekey via [`Self::recv_app`]).
    pub async fn vc_recv_event(&mut self, call: &mut VcCall) -> Result<VcEvent> {
        let msg = self.recv_app().await?;
        call.handle_app(msg)
    }

    async fn rekey_local(&mut self) -> Result<()> {
        match &mut self.inner {
            SessionInner::Tcp(s) => s.rekey_now().await,
            #[cfg(feature = "relay")]
            SessionInner::Relay(s) => s.rekey_now().await,
        }
    }

    /// Apply remote public key to a TOFU store (if XX handshake).
    pub fn tofu_check(&self, store: &mut TofuStore) -> Result<Option<TofuCheck>> {
        match self.info.remote_static {
            Some(ref pk) => Ok(Some(store.check_and_remember(pk)?)),
            None => Ok(None),
        }
    }

    /// Close the logical session.
    pub fn close(&mut self) {
        match &mut self.inner {
            SessionInner::Tcp(s) => s.close(),
            #[cfg(feature = "relay")]
            SessionInner::Relay(s) => s.close(),
        }
    }
}

/// A local peer endpoint that can host invites or join remote invites.
pub struct Node {
    endpoint: Option<TcpEndpoint>,
    config: NodeConfig,
    default_relay: Option<String>,
    identity: Option<Identity>,
}

impl Node {
    /// Bind a TCP listener for hosting (`0.0.0.0:0` recommended).
    pub async fn bind(addr: impl tokio::net::ToSocketAddrs) -> Result<Self> {
        let endpoint = TcpEndpoint::bind(addr).await?;
        Ok(Self {
            endpoint: Some(endpoint),
            config: NodeConfig::default(),
            default_relay: env_relay_url(),
            identity: None,
        })
    }

    /// Guest-side node without a listening socket.
    pub fn guest() -> Self {
        Self {
            endpoint: None,
            config: NodeConfig::default(),
            default_relay: env_relay_url(),
            identity: None,
        }
    }

    /// Override node configuration.
    pub fn with_config(mut self, config: NodeConfig) -> Self {
        self.config = config;
        self
    }

    /// Attach a long-term identity (enables `Noise_XXpsk3`).
    pub fn with_identity(mut self, identity: Identity) -> Self {
        self.identity = Some(identity);
        self
    }

    /// Set default relay base URL (also read from `PEERSEAL_RELAY`).
    pub fn with_relay(mut self, url: impl Into<String>) -> Result<Self> {
        self.default_relay = Some(normalize_relay_url(&url.into())?);
        Ok(self)
    }

    /// Convenience: force relay-only mode.
    pub fn force_relay(mut self) -> Self {
        self.config.force_relay = true;
        self.config.direct_first = false;
        self
    }

    /// Local bind address if hosting.
    pub fn local_addr(&self) -> Option<std::net::SocketAddr> {
        self.endpoint.as_ref().map(|e| e.local_addr)
    }

    /// Local identity fingerprint if set.
    pub fn fingerprint(&self) -> Option<String> {
        self.identity.as_ref().map(|i| i.fingerprint())
    }

    /// Create an invite advertising local direct addresses (and optional relay + host pk).
    pub fn create_invite(&self, ttl: Duration) -> Result<Invite> {
        let addrs = if self.config.force_relay {
            Vec::new()
        } else {
            self.endpoint
                .as_ref()
                .map(|e| e.advertise_addrs.clone())
                .unwrap_or_default()
        };

        Invite::create(InviteOptions {
            ttl: Some(ttl),
            addrs,
            relay_url: self.default_relay.clone(),
            host_pk: self.identity.as_ref().map(|i| i.public),
            ..Default::default()
        })
    }

    fn hs_opts<'a>(&'a self, invite: &'a Invite, role: Role) -> HandshakeOpts<'a> {
        HandshakeOpts {
            invite,
            role,
            config: self.config.session.clone(),
            identity: self.identity.as_ref(),
        }
    }

    async fn finish_tcp(
        stream: TcpStream,
        invite: &Invite,
        role: Role,
        opts: HandshakeOpts<'_>,
    ) -> Result<Session> {
        let secure = SecureStream::handshake_with(stream, opts).await?;
        let (pattern, hash, local, remote) = Session::take_meta(&secure);
        // Optional: if invite has host_pk and we are guest, verify remote matches
        if role == Role::Initiator {
            if let (Some(expected), Some(got)) = (invite.host_pk, remote) {
                if expected != got {
                    return Err(Error::Identity(
                        "remote static key does not match invite host_pk".into(),
                    ));
                }
            }
        }
        Ok(Session::from_parts(
            SessionInner::Tcp(secure),
            TransportKind::DirectTcp,
            invite.room_id.clone(),
            pattern,
            hash,
            local,
            remote,
        ))
    }

    #[cfg(feature = "relay")]
    async fn finish_relay(
        conn: RelayConnection,
        invite: &Invite,
        role: Role,
        opts: HandshakeOpts<'_>,
    ) -> Result<Session> {
        let secure = SecureStream::handshake_with(conn, opts).await?;
        let (pattern, hash, local, remote) = Session::take_meta(&secure);
        if role == Role::Initiator {
            if let (Some(expected), Some(got)) = (invite.host_pk, remote) {
                if expected != got {
                    return Err(Error::Identity(
                        "remote static key does not match invite host_pk".into(),
                    ));
                }
            }
        }
        Ok(Session::from_parts(
            SessionInner::Relay(secure),
            TransportKind::Relay,
            invite.room_id.clone(),
            pattern,
            hash,
            local,
            remote,
        ))
    }

    /// Host: wait for a peer.
    pub async fn accept_peer(&self, invite: &Invite) -> Result<Session> {
        invite.ensure_not_expired()?;
        let config = self.config.clone();
        let invite = invite.clone();

        #[cfg(feature = "relay")]
        {
            if config.force_relay {
                let relay_url = invite.relay_url.as_deref().ok_or_else(|| {
                    Error::ConnectFailed("force_relay set but invite has no relay_url".into())
                })?;
                let conn = RelayConnection::connect(
                    relay_url,
                    &invite.room_id,
                    &invite.token,
                    config.relay_wait_timeout,
                )
                .await?;
                return Self::finish_relay(
                    conn,
                    &invite,
                    Role::Responder,
                    self.hs_opts(&invite, Role::Responder),
                )
                .await;
            }

            if let Some(ref relay_url) = invite.relay_url {
                let endpoint = self
                    .endpoint
                    .as_ref()
                    .ok_or_else(|| Error::Session("accept_peer requires Node::bind".into()))?;
                return self
                    .accept_race_direct_and_relay(endpoint, &invite, relay_url, config)
                    .await;
            }
        }

        #[cfg(not(feature = "relay"))]
        {
            if config.force_relay {
                return Err(Error::FeatureDisabled("relay"));
            }
        }

        let endpoint = self
            .endpoint
            .as_ref()
            .ok_or_else(|| Error::Session("accept_peer requires Node::bind".into()))?;

        let stream = endpoint.accept(config.accept_timeout).await?;
        Self::finish_tcp(
            stream,
            &invite,
            Role::Responder,
            self.hs_opts(&invite, Role::Responder),
        )
        .await
    }

    #[cfg(feature = "relay")]
    async fn accept_race_direct_and_relay(
        &self,
        endpoint: &TcpEndpoint,
        invite: &Invite,
        relay_url: &str,
        config: NodeConfig,
    ) -> Result<Session> {
        let inv1 = invite.clone();
        let inv2 = invite.clone();
        let id = self.identity.clone();
        let id2 = self.identity.clone();
        let session_cfg1 = config.session.clone();
        let session_cfg2 = config.session.clone();
        let accept_timeout = config.accept_timeout;
        let relay_wait = config.relay_wait_timeout;
        let relay_url = relay_url.to_string();
        let room_id = invite.room_id.clone();
        let token = invite.token.clone();

        let direct_fut = async {
            let stream = endpoint.accept(accept_timeout).await?;
            let opts = HandshakeOpts {
                invite: &inv1,
                role: Role::Responder,
                config: session_cfg1,
                identity: id.as_ref(),
            };
            Self::finish_tcp(stream, &inv1, Role::Responder, opts).await
        };

        let relay_fut = async {
            let conn = RelayConnection::connect(&relay_url, &room_id, &token, relay_wait).await?;
            let opts = HandshakeOpts {
                invite: &inv2,
                role: Role::Responder,
                config: session_cfg2,
                identity: id2.as_ref(),
            };
            Self::finish_relay(conn, &inv2, Role::Responder, opts).await
        };

        tokio::select! {
            biased;
            res = direct_fut => res,
            res = relay_fut => res,
        }
    }

    /// Guest: join an invite — direct-first, then optional relay fallback.
    pub async fn join(invite: Invite) -> Result<Session> {
        Self::guest().join_invite(invite).await
    }

    /// Guest join using this node's config / identity / default relay.
    pub async fn join_invite(&self, mut invite: Invite) -> Result<Session> {
        invite.ensure_not_expired()?;

        if invite.relay_url.is_none() {
            if let Some(ref r) = self.default_relay {
                invite = invite.with_relay(r.clone())?;
            }
        }

        let config = self.config.clone();
        let mut errors = Vec::new();

        let try_direct = !config.force_relay && config.direct_first && !invite.addrs.is_empty();

        if try_direct {
            match dial_direct(&invite.addrs, config.dial_timeout).await {
                Ok(stream) => {
                    match Self::finish_tcp(
                        stream,
                        &invite,
                        Role::Initiator,
                        self.hs_opts(&invite, Role::Initiator),
                    )
                    .await
                    {
                        Ok(sess) => return Ok(sess),
                        Err(e) => {
                            tracing::warn!(error = %e, "direct handshake failed");
                            errors.push(format!("direct handshake: {e}"));
                        }
                    }
                }
                Err(e) => {
                    tracing::info!(error = %e, "direct dial failed");
                    errors.push(format!("direct dial: {e}"));
                }
            }
        }

        #[cfg(feature = "relay")]
        {
            if let Some(ref relay_url) = invite.relay_url {
                match RelayConnection::connect(
                    relay_url,
                    &invite.room_id,
                    &invite.token,
                    config.relay_wait_timeout,
                )
                .await
                {
                    Ok(conn) => {
                        return Self::finish_relay(
                            conn,
                            &invite,
                            Role::Initiator,
                            self.hs_opts(&invite, Role::Initiator),
                        )
                        .await;
                    }
                    Err(e) => errors.push(format!("relay: {e}")),
                }
            } else {
                errors.push("no relay_url in invite".into());
            }
        }

        #[cfg(not(feature = "relay"))]
        {
            let _ = config;
            errors.push("relay feature disabled".into());
        }

        Err(Error::ConnectFailed(errors.join("; ")))
    }
}

/// Helper used by tests: complete a secure session over any duplex stream.
pub async fn secure_over_stream<S>(
    stream: S,
    invite: &Invite,
    role: Role,
    config: SessionConfig,
) -> Result<SecureStream<S>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    SecureStream::handshake(stream, invite, role, config).await
}
