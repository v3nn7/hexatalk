//! Invite creation, encoding, and parsing (QR payload + short code).

use crate::error::{Error, Result};
use crate::util::normalize_relay_url;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Scheme prefix for invite QR / paste payloads (`ps1:` = peerseal v1).
pub const INVITE_SCHEME: &str = "ps1:";

/// Default invite TTL when not specified.
pub const DEFAULT_INVITE_TTL: Duration = Duration::from_secs(180);

/// Minimum length for room_id and token (relay-compatible).
pub const MIN_CRED_LEN: usize = 8;

/// Maximum length for room_id and token.
pub const MAX_CRED_LEN: usize = 64;

/// Allowed charset for room_id / token: `[A-Za-z0-9_.-]`.
fn is_cred_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-'
}

/// Validate room_id or token against protocol rules.
pub fn validate_credential(label: &str, value: &str) -> Result<()> {
    if value.len() < MIN_CRED_LEN {
        return Err(Error::InvalidCredentials(format!(
            "{label} must be at least {MIN_CRED_LEN} characters"
        )));
    }
    if value.len() > MAX_CRED_LEN {
        return Err(Error::InvalidCredentials(format!(
            "{label} must be at most {MAX_CRED_LEN} characters"
        )));
    }
    if !value.chars().all(is_cred_char) {
        return Err(Error::InvalidCredentials(format!(
            "{label} must match [A-Za-z0-9_.-]"
        )));
    }
    Ok(())
}

/// Generate a cryptographically random credential string of `len` bytes (url-safe alphabet).
pub fn random_credential(len: usize) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_-";
    let mut rng = rand::thread_rng();
    let mut out = String::with_capacity(len);
    let mut buf = [0u8; 32];
    while out.len() < len {
        rng.fill_bytes(&mut buf);
        for &b in &buf {
            if out.len() >= len {
                break;
            }
            out.push(ALPHABET[(b as usize) % ALPHABET.len()] as char);
        }
    }
    out
}

/// A pairing invite that can be encoded as a QR payload or short code.
///
/// Binary layout (v1), then `base64url` without padding, prefixed with [`INVITE_SCHEME`]:
///
/// ```text
/// version: u8 (=1)
/// flags:   u8  bit0=has_relay, bit1=has_host_pk
/// room_len: u8 + room bytes
/// token_len: u8 + token bytes
/// expires_at: u64 BE (unix seconds)
/// addr_count: u8
///   each: addr_len: u8 + addr bytes (UTF-8 "ip:port" or "[ipv6]:port")
/// if has_relay: relay_len: u16 BE + relay bytes
/// if has_host_pk: 32 bytes X25519/Ed25519 public key
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invite {
    /// Protocol version (currently 1).
    pub version: u8,
    /// Shared room identifier.
    pub room_id: String,
    /// Shared secret token (also used as Noise PSK material).
    pub token: String,
    /// Unix timestamp (seconds) after which the invite is invalid.
    pub expires_at: u64,
    /// Direct dial targets, e.g. `192.168.1.10:41234`.
    pub addrs: Vec<String>,
    /// Optional WebSocket relay base URL (`wss://host` or `ws://host`).
    pub relay_url: Option<String>,
    /// Optional host public key / fingerprint (32 bytes).
    pub host_pk: Option<[u8; 32]>,
}

/// Builder options for [`Invite::create`].
#[derive(Debug, Clone, Default)]
pub struct InviteOptions {
    /// Time-to-live from now.
    pub ttl: Option<Duration>,
    /// Explicit room id (random if `None`).
    pub room_id: Option<String>,
    /// Explicit token (random if `None`).
    pub token: Option<String>,
    /// Direct addresses to advertise.
    pub addrs: Vec<String>,
    /// Optional relay base URL.
    pub relay_url: Option<String>,
    /// Optional host public key.
    pub host_pk: Option<[u8; 32]>,
}

impl Invite {
    /// Create a new invite with random credentials (unless overridden).
    pub fn create(opts: InviteOptions) -> Result<Self> {
        let ttl = opts.ttl.unwrap_or(DEFAULT_INVITE_TTL);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| Error::InvalidInvite(format!("system clock error: {e}")))?
            .as_secs();
        let expires_at = now.saturating_add(ttl.as_secs());

        let room_id = match opts.room_id {
            Some(r) => {
                validate_credential("room_id", &r)?;
                r
            }
            None => random_credential(16),
        };
        let token = match opts.token {
            Some(t) => {
                validate_credential("token", &t)?;
                t
            }
            None => random_credential(24),
        };

        let relay_url = match opts.relay_url {
            Some(url) => Some(normalize_relay_url(&url)?),
            None => None,
        };

        Ok(Self {
            version: 1,
            room_id,
            token,
            expires_at,
            addrs: opts.addrs,
            relay_url,
            host_pk: opts.host_pk,
        })
    }

    /// Attach or replace the relay URL (builder-style).
    ///
    /// Accepts bare host, `https://…`, or `wss://…` — see [`crate::normalize_relay_url`].
    pub fn with_relay(mut self, relay_url: impl Into<String>) -> Result<Self> {
        self.relay_url = Some(normalize_relay_url(&relay_url.into())?);
        Ok(self)
    }

    /// Attach host public key.
    pub fn with_host_pk(mut self, pk: [u8; 32]) -> Self {
        self.host_pk = Some(pk);
        self
    }

    /// Remaining TTL, or `None` if already expired.
    pub fn remaining_ttl(&self) -> Option<Duration> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
        if now >= self.expires_at {
            None
        } else {
            Some(Duration::from_secs(self.expires_at - now))
        }
    }

    /// Return an error if the invite has expired.
    pub fn ensure_not_expired(&self) -> Result<()> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| Error::InvalidInvite(format!("system clock error: {e}")))?
            .as_secs();
        if now >= self.expires_at {
            return Err(Error::InviteExpired {
                expires_at: self.expires_at,
            });
        }
        Ok(())
    }

    /// Encode as QR / clipboard payload: `ps1:<base64url>`.
    pub fn to_qr_payload(&self) -> Result<String> {
        let bytes = self.encode_binary()?;
        Ok(format!("{INVITE_SCHEME}{}", URL_SAFE_NO_PAD.encode(bytes)))
    }

    /// Alias for [`Self::to_qr_payload`].
    pub fn to_qr_string(&self) -> Result<String> {
        self.to_qr_payload()
    }

    /// Short manual-entry code: `room_id/token` (same credentials as the QR invite).
    ///
    /// Does **not** carry addresses or relay URL; pair with env `PEERSEAL_RELAY` or
    /// [`Invite::from_short_code`] options when reconstructing.
    pub fn to_short_code(&self) -> String {
        format!("{}/{}", self.room_id, self.token)
    }

    /// Parse from QR payload or raw base64 body.
    pub fn parse(input: &str) -> Result<Self> {
        let s = input.trim();
        let b64 = if let Some(rest) = s.strip_prefix(INVITE_SCHEME) {
            rest
        } else if let Some(rest) = s.strip_prefix("peerseal:1:") {
            rest
        } else if s.contains('/') && !s.contains(':') {
            // Ambiguous: try short code first only if it looks like room/token
            return Err(Error::InvalidInvite(
                "use Invite::from_short_code for room/token short codes, or ps1: for full invites"
                    .into(),
            ));
        } else {
            s
        };

        let bytes = URL_SAFE_NO_PAD
            .decode(b64.as_bytes())
            .map_err(|e| Error::InvalidInvite(format!("base64url decode: {e}")))?;
        Self::decode_binary(&bytes)
    }

    /// Parse QR string (alias).
    pub fn from_qr_string(s: &str) -> Result<Self> {
        Self::parse(s)
    }

    /// Reconstruct a minimal invite from `room/token` short code.
    pub fn from_short_code(
        code: &str,
        ttl: Duration,
        relay_url: Option<String>,
        addrs: Vec<String>,
    ) -> Result<Self> {
        let code = code.trim();
        let (room_id, token) = code
            .split_once('/')
            .ok_or_else(|| Error::InvalidInvite("short code must be room_id/token".into()))?;
        validate_credential("room_id", room_id)?;
        validate_credential("token", token)?;

        let mut invite = Self::create(InviteOptions {
            ttl: Some(ttl),
            room_id: Some(room_id.to_string()),
            token: Some(token.to_string()),
            addrs,
            relay_url: None,
            host_pk: None,
        })?;
        if let Some(url) = relay_url {
            invite = invite.with_relay(url)?;
        }
        Ok(invite)
    }

    /// Serialize to compact binary (v1).
    pub fn encode_binary(&self) -> Result<Vec<u8>> {
        validate_credential("room_id", &self.room_id)?;
        validate_credential("token", &self.token)?;

        if self.room_id.len() > u8::MAX as usize || self.token.len() > u8::MAX as usize {
            return Err(Error::InvalidInvite(
                "credential too long for v1 encoding".into(),
            ));
        }
        if self.addrs.len() > u8::MAX as usize {
            return Err(Error::InvalidInvite("too many addresses".into()));
        }

        let mut flags: u8 = 0;
        if self.relay_url.is_some() {
            flags |= 0x01;
        }
        if self.host_pk.is_some() {
            flags |= 0x02;
        }

        let mut out = Vec::with_capacity(128);
        out.push(self.version);
        out.push(flags);
        out.push(self.room_id.len() as u8);
        out.extend_from_slice(self.room_id.as_bytes());
        out.push(self.token.len() as u8);
        out.extend_from_slice(self.token.as_bytes());
        out.extend_from_slice(&self.expires_at.to_be_bytes());
        out.push(self.addrs.len() as u8);
        for addr in &self.addrs {
            if addr.len() > u8::MAX as usize {
                return Err(Error::InvalidInvite(format!("address too long: {addr}")));
            }
            out.push(addr.len() as u8);
            out.extend_from_slice(addr.as_bytes());
        }
        if let Some(ref relay) = self.relay_url {
            if relay.len() > u16::MAX as usize {
                return Err(Error::InvalidInvite("relay_url too long".into()));
            }
            let len = relay.len() as u16;
            out.extend_from_slice(&len.to_be_bytes());
            out.extend_from_slice(relay.as_bytes());
        }
        if let Some(pk) = self.host_pk {
            out.extend_from_slice(&pk);
        }
        Ok(out)
    }

    /// Deserialize from compact binary (v1).
    pub fn decode_binary(data: &[u8]) -> Result<Self> {
        fn take<'a>(i: &mut usize, n: usize, data: &'a [u8]) -> Result<&'a [u8]> {
            if *i + n > data.len() {
                return Err(Error::InvalidInvite("truncated binary".into()));
            }
            let s = &data[*i..*i + n];
            *i += n;
            Ok(s)
        }

        let mut i = 0usize;
        let version = take(&mut i, 1, data)?[0];
        if version != 1 {
            return Err(Error::InvalidInvite(format!(
                "unsupported invite version {version}"
            )));
        }
        let flags = take(&mut i, 1, data)?[0];
        let has_relay = flags & 0x01 != 0;
        let has_pk = flags & 0x02 != 0;

        let room_len = take(&mut i, 1, data)?[0] as usize;
        let room_id = std::str::from_utf8(take(&mut i, room_len, data)?)
            .map_err(|_| Error::InvalidInvite("room_id not utf-8".into()))?
            .to_string();
        let token_len = take(&mut i, 1, data)?[0] as usize;
        let token = std::str::from_utf8(take(&mut i, token_len, data)?)
            .map_err(|_| Error::InvalidInvite("token not utf-8".into()))?
            .to_string();

        validate_credential("room_id", &room_id)?;
        validate_credential("token", &token)?;

        let exp_bytes: [u8; 8] = take(&mut i, 8, data)?
            .try_into()
            .map_err(|_| Error::InvalidInvite("expires_at".into()))?;
        let expires_at = u64::from_be_bytes(exp_bytes);

        let addr_count = take(&mut i, 1, data)?[0] as usize;
        let mut addrs = Vec::with_capacity(addr_count);
        for _ in 0..addr_count {
            let alen = take(&mut i, 1, data)?[0] as usize;
            let a = std::str::from_utf8(take(&mut i, alen, data)?)
                .map_err(|_| Error::InvalidInvite("addr not utf-8".into()))?
                .to_string();
            addrs.push(a);
        }

        let relay_url = if has_relay {
            let rlen_bytes: [u8; 2] = take(&mut i, 2, data)?
                .try_into()
                .map_err(|_| Error::InvalidInvite("relay len".into()))?;
            let rlen = u16::from_be_bytes(rlen_bytes) as usize;
            let r = std::str::from_utf8(take(&mut i, rlen, data)?)
                .map_err(|_| Error::InvalidInvite("relay not utf-8".into()))?
                .to_string();
            Some(r)
        } else {
            None
        };

        let host_pk = if has_pk {
            let pk: [u8; 32] = take(&mut i, 32, data)?
                .try_into()
                .map_err(|_| Error::InvalidInvite("host_pk".into()))?;
            Some(pk)
        } else {
            None
        };

        if i != data.len() {
            return Err(Error::InvalidInvite(format!(
                "trailing {} bytes in invite",
                data.len() - i
            )));
        }

        Ok(Self {
            version,
            room_id,
            token,
            expires_at,
            addrs,
            relay_url,
            host_pk,
        })
    }

    /// 32-byte Noise PSK derived from the invite token (HKDF-style via SHA-256).
    pub fn noise_psk(&self) -> [u8; 32] {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(b"peerseal-v1-noise-psk");
        hasher.update(self.room_id.as_bytes());
        hasher.update(b"|");
        hasher.update(self.token.as_bytes());
        let dig = hasher.finalize();
        let mut out = [0u8; 32];
        out.copy_from_slice(&dig);
        out
    }
}

#[cfg(feature = "qr-image")]
impl Invite {
    /// Render the QR payload as a PNG (feature `qr-image`).
    pub fn to_qr_png(&self, size: u32) -> Result<Vec<u8>> {
        use image::Luma;
        use qrcode::QrCode;
        use std::io::Cursor;

        let payload = self.to_qr_payload()?;
        let code = QrCode::new(payload.as_bytes())
            .map_err(|e| Error::InvalidInvite(format!("qr encode: {e}")))?;
        let img = code.render::<Luma<u8>>().min_dimensions(size, size).build();
        let mut buf = Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png)
            .map_err(|e| Error::Io(std::io::Error::other(e.to_string())))?;
        Ok(buf.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_minimal() {
        let inv = Invite::create(InviteOptions {
            ttl: Some(Duration::from_secs(120)),
            ..Default::default()
        })
        .unwrap();
        let s = inv.to_qr_payload().unwrap();
        assert!(s.starts_with("ps1:"));
        let back = Invite::parse(&s).unwrap();
        assert_eq!(inv, back);
    }

    #[test]
    fn roundtrip_with_relay_and_addrs() {
        let inv = Invite::create(InviteOptions {
            ttl: Some(Duration::from_secs(300)),
            addrs: vec!["192.168.0.5:9000".into(), "[::1]:9000".into()],
            relay_url: Some("wss://relay.example/".into()),
            host_pk: Some([7u8; 32]),
            ..Default::default()
        })
        .unwrap();
        // Trailing slash is stripped by normalize_relay_url.
        assert_eq!(inv.relay_url.as_deref(), Some("wss://relay.example"));
        let s = inv.to_qr_string().unwrap();
        let back = Invite::from_qr_string(&s).unwrap();
        assert_eq!(inv, back);
        assert_eq!(back.relay_url.as_deref(), Some("wss://relay.example"));
        assert_eq!(back.host_pk, Some([7u8; 32]));
    }

    #[test]
    fn short_code_roundtrip() {
        let inv = Invite::create(InviteOptions::default()).unwrap();
        let code = inv.to_short_code();
        let back = Invite::from_short_code(
            &code,
            Duration::from_secs(60),
            Some("wss://r.example".into()),
            vec![],
        )
        .unwrap();
        assert_eq!(back.room_id, inv.room_id);
        assert_eq!(back.token, inv.token);
    }

    #[test]
    fn reject_bad_credentials() {
        assert!(validate_credential("room_id", "short").is_err());
        assert!(validate_credential("token", "bad token!!").is_err());
    }

    #[test]
    fn reject_expired() {
        let mut inv = Invite::create(InviteOptions {
            ttl: Some(Duration::from_secs(60)),
            ..Default::default()
        })
        .unwrap();
        inv.expires_at = 1; // long ago
        assert!(matches!(
            inv.ensure_not_expired(),
            Err(Error::InviteExpired { .. })
        ));
    }

    #[test]
    fn noise_psk_stable() {
        let inv = Invite::create(InviteOptions {
            room_id: Some("abcdefgh".into()),
            token: Some("12345678token".into()),
            ttl: Some(Duration::from_secs(60)),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(inv.noise_psk(), inv.noise_psk());
        let mut other = inv.clone();
        other.token = "12345678TOKEN".into();
        assert_ne!(inv.noise_psk(), other.noise_psk());
    }
}
