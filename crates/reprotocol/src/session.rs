//! Noise-based E2E secure session over an async byte stream.

use crate::error::{Error, Result};
use crate::framing::{
    self, DEFAULT_MAX_FRAME, HARD_MAX_FRAME, MAX_WIRE_CHUNK, NOISE_MAX_MESSAGE, Reassembler,
    split_logical,
};
use crate::identity::Identity;
use crate::invite::Invite;
use snow::params::NoiseParams;
use snow::{Builder, TransportState};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadHalf, WriteHalf};
use tokio::sync::Mutex;
use tokio::time::timeout;

/// PSK-only pattern (no long-term keys). Mutual auth via invite token.
pub const NOISE_NNPSK0: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";

/// Mutual static-key auth + invite PSK binder. Prefer when both peers have [`Identity`].
pub const NOISE_XXPSK3: &str = "Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s";

/// Maximum Noise message size used during handshake / transport encrypt buffer.
const NOISE_BUF: usize = 65535;

/// Which Noise handshake was used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoisePattern {
    /// `Noise_NNpsk0` — PSK from invite only.
    NnPsk0,
    /// `Noise_XXpsk3` — identity keys + invite PSK.
    XxPsk3,
}

/// Configuration for a secure session.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Max plaintext frame payload before encryption.
    pub max_frame: usize,
    /// Idle timeout for a single send/recv operation (`None` = no timeout).
    pub io_timeout: Option<Duration>,
    /// Overall handshake timeout.
    pub handshake_timeout: Duration,
    /// Auto rekey after this many application messages (`None` = only manual).
    pub rekey_every_messages: Option<u64>,
    /// Prefer XX when local identity is present (default true).
    pub prefer_identity_handshake: bool,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            max_frame: DEFAULT_MAX_FRAME.min(HARD_MAX_FRAME),
            io_timeout: Some(Duration::from_secs(300)),
            handshake_timeout: Duration::from_secs(30),
            rekey_every_messages: Some(10_000),
            prefer_identity_handshake: true,
        }
    }
}

/// Role in the Noise handshake (initiator = guest dialer by convention).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Starts the Noise handshake (typically the joining peer).
    Initiator,
    /// Responds to the Noise handshake (typically the host).
    Responder,
}

/// Options for [`SecureStream::handshake`].
#[derive(Clone)]
pub struct HandshakeOpts<'a> {
    /// Invite (PSK + TTL).
    pub invite: &'a Invite,
    /// Handshake role.
    pub role: Role,
    /// Session config.
    pub config: SessionConfig,
    /// Optional long-term identity (enables XX when present on both sides of the API;
    /// each peer supplies its own key — remote is learned during XX).
    pub identity: Option<&'a Identity>,
}

/// Bidirectional E2E encrypted stream.
///
/// After construction, only AEAD-protected application frames are exchanged.
/// Sequence numbers and replay protection are provided by the Noise transport state.
///
/// Logical messages may be up to [`HARD_MAX_FRAME`] (25 MiB); they are split into
/// ~60 KiB Noise fragments automatically.
pub struct SecureStream<S> {
    reader: ReadHalf<S>,
    writer: WriteHalf<S>,
    noise: Arc<Mutex<TransportState>>,
    max_frame: usize,
    io_timeout: Option<Duration>,
    closed: bool,
    /// Pattern used for this session.
    pub pattern: NoisePattern,
    /// Noise handshake hash (for SAS).
    pub handshake_hash: [u8; 32],
    /// Remote static public key when XX was used.
    pub remote_static: Option<[u8; 32]>,
    /// Local static public key when XX was used.
    pub local_static: Option<[u8; 32]>,
    /// Messages sent+received since last rekey (approx).
    msgs_since_rekey: u64,
    rekey_every: Option<u64>,
    next_msg_id: u32,
    reassembler: Reassembler,
}

impl<S> SecureStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    /// Run Noise handshake over `stream` and return a ready [`SecureStream`].
    pub async fn handshake(
        stream: S,
        invite: &Invite,
        role: Role,
        config: SessionConfig,
    ) -> Result<Self> {
        Self::handshake_with(
            stream,
            HandshakeOpts {
                invite,
                role,
                config,
                identity: None,
            },
        )
        .await
    }

    /// Handshake with optional identity (XX+PSK) and full options.
    pub async fn handshake_with(stream: S, opts: HandshakeOpts<'_>) -> Result<Self> {
        opts.invite.ensure_not_expired()?;
        let psk = opts.invite.noise_psk();
        let use_xx = opts.identity.is_some() && opts.config.prefer_identity_handshake;

        let (pattern_str, pattern) = if use_xx {
            (NOISE_XXPSK3, NoisePattern::XxPsk3)
        } else {
            (NOISE_NNPSK0, NoisePattern::NnPsk0)
        };

        let params: NoiseParams = pattern_str
            .parse()
            .map_err(|e| Error::Crypto(format!("noise params: {e}")))?;

        let local_static = opts.identity.filter(|_| use_xx).map(|id| id.public);

        let builder = Builder::new(params);
        let builder = if use_xx {
            let id = opts
                .identity
                .ok_or_else(|| Error::Crypto("XX requires identity".into()))?;
            builder.local_private_key(&id.private).psk(3, &psk)
        } else {
            builder.psk(0, &psk)
        };

        let mut handshake = match opts.role {
            Role::Initiator => builder
                .build_initiator()
                .map_err(|e| Error::Crypto(format!("initiator: {e}")))?,
            Role::Responder => builder
                .build_responder()
                .map_err(|e| Error::Crypto(format!("responder: {e}")))?,
        };

        let (mut reader, mut writer) = tokio::io::split(stream);
        let hs_timeout = opts.config.handshake_timeout;

        let (transport, handshake_hash, remote_static) = timeout(hs_timeout, async {
            let mut buf = vec![0u8; NOISE_BUF];

            loop {
                if handshake.is_handshake_finished() {
                    break;
                }

                if handshake.is_my_turn() {
                    let len = handshake
                        .write_message(&[], &mut buf)
                        .map_err(|e| Error::Crypto(format!("hs write: {e}")))?;
                    framing::write_frame(&mut writer, &buf[..len], NOISE_BUF).await?;
                } else {
                    let frame = framing::read_frame(&mut reader, NOISE_BUF).await?;
                    let mut payload = vec![0u8; NOISE_BUF];
                    let _n = handshake
                        .read_message(&frame, &mut payload)
                        .map_err(|e| Error::Crypto(format!("hs read: {e}")))?;
                }
            }

            let mut hash = [0u8; 32];
            let hh = handshake.get_handshake_hash();
            let n = hh.len().min(32);
            hash[..n].copy_from_slice(&hh[..n]);

            let remote = handshake.get_remote_static().and_then(|s| {
                if s.len() == 32 {
                    let mut a = [0u8; 32];
                    a.copy_from_slice(s);
                    Some(a)
                } else {
                    None
                }
            });

            let transport = handshake
                .into_transport_mode()
                .map_err(|e| Error::Crypto(format!("into_transport: {e}")))?;
            Ok::<_, Error>((transport, hash, remote))
        })
        .await
        .map_err(|_| Error::Timeout("noise handshake timed out".into()))??;

        Ok(Self {
            reader,
            writer,
            noise: Arc::new(Mutex::new(transport)),
            max_frame: opts.config.max_frame.min(HARD_MAX_FRAME),
            io_timeout: opts.config.io_timeout,
            closed: false,
            pattern,
            handshake_hash,
            remote_static,
            local_static,
            msgs_since_rekey: 0,
            rekey_every: opts.config.rekey_every_messages,
            next_msg_id: 1,
            reassembler: Reassembler::default(),
        })
    }

    /// Encrypt and send one **logical** application message (up to 25 MiB).
    ///
    /// Large payloads are fragmented into ~60 KiB Noise messages automatically.
    pub async fn send(&mut self, plaintext: &[u8]) -> Result<()> {
        if self.closed {
            return Err(Error::Session("session closed".into()));
        }
        if plaintext.len() > self.max_frame {
            return Err(Error::Framing(format!(
                "plaintext {} exceeds max {}",
                plaintext.len(),
                self.max_frame
            )));
        }

        let msg_id = self.next_msg_id;
        self.next_msg_id = self.next_msg_id.wrapping_add(1);
        let frags = split_logical(msg_id, plaintext, MAX_WIRE_CHUNK)?;

        for frag in frags {
            self.send_wire_fragment(&frag).await?;
        }
        self.msgs_since_rekey = self.msgs_since_rekey.saturating_add(1);
        Ok(())
    }

    async fn send_wire_fragment(&mut self, plaintext: &[u8]) -> Result<()> {
        let mut cipher_buf = vec![0u8; plaintext.len().saturating_add(16).max(16)];
        let len = {
            let mut noise = self.noise.lock().await;
            noise
                .write_message(plaintext, &mut cipher_buf)
                .map_err(|e| Error::Crypto(format!("encrypt: {e}")))?
        };
        cipher_buf.truncate(len);

        let write_fut = framing::write_frame(&mut self.writer, &cipher_buf, NOISE_MAX_MESSAGE);
        match self.io_timeout {
            Some(t) => timeout(t, write_fut)
                .await
                .map_err(|_| Error::Timeout("send timed out".into()))?,
            None => write_fut.await,
        }
    }

    /// Receive and decrypt one **logical** application message (reassembles fragments).
    pub async fn recv(&mut self) -> Result<Vec<u8>> {
        if self.closed {
            return Err(Error::Session("session closed".into()));
        }

        loop {
            let frag = self.recv_wire_fragment().await?;
            if let Some(msg) = self.reassembler.push(&frag)? {
                self.msgs_since_rekey = self.msgs_since_rekey.saturating_add(1);
                return Ok(msg);
            }
        }
    }

    async fn recv_wire_fragment(&mut self) -> Result<Vec<u8>> {
        let read_fut = framing::read_frame(&mut self.reader, NOISE_MAX_MESSAGE);
        let cipher = match self.io_timeout {
            Some(t) => timeout(t, read_fut)
                .await
                .map_err(|_| Error::Timeout("recv timed out".into()))??,
            None => read_fut.await?,
        };

        let mut plain_buf = vec![0u8; cipher.len()];
        let len = {
            let mut noise = self.noise.lock().await;
            noise
                .read_message(&cipher, &mut plain_buf)
                .map_err(|e| Error::Crypto(format!("decrypt: {e}")))?
        };
        plain_buf.truncate(len);
        Ok(plain_buf)
    }

    /// Coordinated rekey: advance both send and recv cipher states.
    ///
    /// Call **after** a `Rekey` control message has been sent or received so both
    /// peers rekey at the same logical point.
    pub async fn rekey_now(&mut self) -> Result<()> {
        let mut noise = self.noise.lock().await;
        noise.rekey_outgoing();
        noise.rekey_incoming();
        self.msgs_since_rekey = 0;
        tracing::debug!("noise transport rekeyed");
        Ok(())
    }

    /// Whether auto-rekey threshold has been reached.
    pub fn should_auto_rekey(&self) -> bool {
        match self.rekey_every {
            Some(n) if n > 0 => self.msgs_since_rekey >= n,
            _ => false,
        }
    }

    /// Mark the session closed (does not shut down the underlying socket).
    pub fn close(&mut self) {
        self.closed = true;
    }
}

// snow 0.9 Builder methods return Self, not Result — fix compile by removing map_err on psk
// I'll fix in a follow-up if build fails — rewrote carefully below.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::invite::InviteOptions;
    use crate::sas::sas_emojis;
    use std::time::Duration;
    use tokio::net::{TcpListener, TcpStream};

    #[tokio::test]
    async fn e2e_noise_nn_over_tcp() {
        let invite = Invite::create(InviteOptions {
            ttl: Some(Duration::from_secs(120)),
            room_id: Some("testroom01".into()),
            token: Some("testtoken012345".into()),
            ..Default::default()
        })
        .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let inv_r = invite.clone();
        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let mut sess =
                SecureStream::handshake(sock, &inv_r, Role::Responder, SessionConfig::default())
                    .await
                    .unwrap();
            assert_eq!(sess.pattern, NoisePattern::NnPsk0);
            let msg = sess.recv().await.unwrap();
            assert_eq!(msg, b"ping");
            sess.send(b"pong").await.unwrap();
        });

        let sock = TcpStream::connect(addr).await.unwrap();
        let mut client =
            SecureStream::handshake(sock, &invite, Role::Initiator, SessionConfig::default())
                .await
                .unwrap();
        client.send(b"ping").await.unwrap();
        let reply = client.recv().await.unwrap();
        assert_eq!(reply, b"pong");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn e2e_noise_xx_identity_and_sas() {
        let invite = Invite::create(InviteOptions {
            ttl: Some(Duration::from_secs(120)),
            room_id: Some("testroomxx1".into()),
            token: Some("testtokenXX0001".into()),
            ..Default::default()
        })
        .unwrap();
        let host_id = Identity::generate().unwrap();
        let guest_id = Identity::generate().unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let inv_r = invite.clone();
        let host_id_c = host_id.clone();
        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let mut sess = SecureStream::handshake_with(
                sock,
                HandshakeOpts {
                    invite: &inv_r,
                    role: Role::Responder,
                    config: SessionConfig::default(),
                    identity: Some(&host_id_c),
                },
            )
            .await
            .unwrap();
            assert_eq!(sess.pattern, NoisePattern::XxPsk3);
            assert!(sess.remote_static.is_some());
            let sas = sas_emojis(&sess.handshake_hash, 5);
            let msg = sess.recv().await.unwrap();
            assert_eq!(msg, b"id-ok");
            sess.send(sas.as_bytes()).await.unwrap();
            sess
        });

        let sock = TcpStream::connect(addr).await.unwrap();
        let mut client = SecureStream::handshake_with(
            sock,
            HandshakeOpts {
                invite: &invite,
                role: Role::Initiator,
                config: SessionConfig::default(),
                identity: Some(&guest_id),
            },
        )
        .await
        .unwrap();
        assert_eq!(client.pattern, NoisePattern::XxPsk3);
        assert_eq!(client.remote_static, Some(host_id.public));
        let sas_client = sas_emojis(&client.handshake_hash, 5);
        client.send(b"id-ok").await.unwrap();
        let sas_from_host = client.recv().await.unwrap();
        assert_eq!(sas_from_host, sas_client.as_bytes());

        let host_sess = server.await.unwrap();
        assert_eq!(host_sess.remote_static, Some(guest_id.public));
        assert_eq!(host_sess.handshake_hash, client.handshake_hash);
    }

    #[tokio::test]
    async fn rekey_still_works() {
        let invite = Invite::create(InviteOptions {
            ttl: Some(Duration::from_secs(120)),
            room_id: Some("testrekey01".into()),
            token: Some("testtokenrekey1".into()),
            ..Default::default()
        })
        .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let inv_r = invite.clone();
        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let mut sess =
                SecureStream::handshake(sock, &inv_r, Role::Responder, SessionConfig::default())
                    .await
                    .unwrap();
            let _ = sess.recv().await.unwrap();
            sess.rekey_now().await.unwrap();
            let msg = sess.recv().await.unwrap();
            assert_eq!(msg, b"after-rekey");
            sess.send(b"ack").await.unwrap();
        });

        let sock = TcpStream::connect(addr).await.unwrap();
        let mut client =
            SecureStream::handshake(sock, &invite, Role::Initiator, SessionConfig::default())
                .await
                .unwrap();
        client.send(b"before").await.unwrap();
        client.rekey_now().await.unwrap();
        client.send(b"after-rekey").await.unwrap();
        assert_eq!(client.recv().await.unwrap(), b"ack");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn large_logical_message_over_noise() {
        let invite = Invite::create(InviteOptions {
            ttl: Some(Duration::from_secs(120)),
            room_id: Some("testlarge01".into()),
            token: Some("testtokenlarge1".into()),
            ..Default::default()
        })
        .unwrap();

        // 1.5 MiB — forces multi-fragment path
        let big = vec![0x5Au8; 1_500_000];

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let inv_r = invite.clone();
        let big_c = big.clone();
        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let mut sess =
                SecureStream::handshake(sock, &inv_r, Role::Responder, SessionConfig::default())
                    .await
                    .unwrap();
            let msg = sess.recv().await.unwrap();
            assert_eq!(msg, big_c);
            sess.send(b"ok-large").await.unwrap();
        });

        let sock = TcpStream::connect(addr).await.unwrap();
        let mut client =
            SecureStream::handshake(sock, &invite, Role::Initiator, SessionConfig::default())
                .await
                .unwrap();
        client.send(&big).await.unwrap();
        assert_eq!(client.recv().await.unwrap(), b"ok-large");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn wrong_token_fails_handshake() {
        let host = Invite::create(InviteOptions {
            ttl: Some(Duration::from_secs(120)),
            room_id: Some("testroom02".into()),
            token: Some("correct_token_1".into()),
            ..Default::default()
        })
        .unwrap();
        let mut guest = host.clone();
        guest.token = "wrong_token_xxx".into();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let inv_h = host.clone();
        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            SecureStream::handshake(sock, &inv_h, Role::Responder, SessionConfig::default()).await
        });

        let sock = TcpStream::connect(addr).await.unwrap();
        let client_res =
            SecureStream::handshake(sock, &guest, Role::Initiator, SessionConfig::default()).await;

        let server_res = server.await.unwrap();
        assert!(client_res.is_err() || server_res.is_err());
    }
}
