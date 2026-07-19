//! # peerseal
//!
//! Secure **direct-first** P2P sessions for two peers:
//! 1. Host creates an [`Invite`] (QR payload `ps1:…` or short `room/token` code)
//! 2. Guest dials **direct TCP** addresses from the invite
//! 3. On failure, optional **WebSocket relay** fallback (relay sees only ciphertext)
//! 4. [`session::SecureStream`] runs **Noise** (`NNpsk0` or `XXpsk3` with identity)
//! 5. Typed app protocol: chat, files, photos, **HD VC** (25 MiB frames), rekey, SAS
//!
//! ## Quick example
//!
//! ```rust,no_run
//! use peerseal::{Identity, Invite, Node};
//! use std::time::Duration;
//!
//! # async fn demo() -> peerseal::Result<()> {
//! let id = Identity::generate()?;
//! let node = Node::bind("0.0.0.0:0").await?
//!     .with_identity(id)
//!     .with_relay("relay-production-eb30.up.railway.app")?;
//! let invite = node.create_invite(Duration::from_secs(120))?;
//! let qr = invite.to_qr_string()?;
//! let mut session = node.accept_peer(&invite).await?;
//! println!("SAS: {}", session.info.sas_emojis());
//! session.send_text("hello").await?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Security
//!
//! - **NNpsk0**: invite token → PSK, ephemeral X25519 (forward secrecy)
//! - **XXpsk3** (with [`Identity`]): mutual static-key auth + invite PSK + FS
//! - AEAD: ChaCha20-Poly1305 via Noise transport; optional rekey
//! - SAS/emoji for out-of-band MITM detection; TOFU store for returning peers
//! - Relay is untrusted — only ciphertext on the wire
//!
//! ## Feature flags
//!
//! - `relay` (default): WebSocket relay client
//! - `qr-image`: optional PNG QR rendering

#![cfg_attr(docsrs, feature(doc_cfg))]
#![deny(missing_docs)]

pub mod discovery;
pub mod error;
pub mod framing;
pub mod identity;
pub mod invite;
pub mod media;
pub mod node;
pub mod protocol;
pub mod sas;
pub mod session;
pub mod transfer;
pub mod transport;
pub mod util;
pub mod vc;

pub use discovery::{DISCOVERY_PORT, PeerAdvert, advertise_and_scan};
pub use error::{Error, Result};
pub use framing::{DEFAULT_MAX_FRAME, HARD_MAX_FRAME, MAX_WIRE_CHUNK};
pub use identity::{Identity, TofuCheck, TofuStore, fingerprint_of, short_fingerprint_of};
pub use invite::{
    DEFAULT_INVITE_TTL, INVITE_SCHEME, Invite, InviteOptions, MIN_CRED_LEN, random_credential,
    validate_credential,
};
pub use node::{Node, NodeConfig, Session, SessionInfo, TransportKind};
pub use protocol::{AppMessage, DEFAULT_CHUNK, MediaKind, MsgType};
pub use sas::{sas_emojis, sas_hex, sas_numbers};
pub use session::{
    HandshakeOpts, NOISE_NNPSK0, NOISE_XXPSK3, NoisePattern, Role, SecureStream, SessionConfig,
};
pub use util::{env_relay_url, normalize_relay_url};
pub use vc::{
    AudioCodec, AudioFrame, HdProfile, VcCall, VcConfig, VcControlKind, VcEvent, VideoCodec,
    VideoFrame, VideoJitterBuffer, generate_hd_test_pattern_rgb, minimal_jpeg_bytes,
    video_frame_from_payload,
};
