//! Lightweight LAN discovery beacon (UDP) — no mDNS dependency.
//!
//! Peers periodically broadcast a small advertisement; others collect them.
//! This is **not** authenticated by itself — always finish with QR/invite + Noise.

use crate::error::{Error, Result};
use crate::util::normalize_relay_url;
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::time::timeout;

/// Default UDP port for peerseal LAN beacons.
pub const DISCOVERY_PORT: u16 = 41270;

/// Magic prefix for beacon packets.
const MAGIC: &[u8] = b"PS1B";

/// A discovered peer advertisement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerAdvert {
    /// Human label (hostname or custom).
    pub name: String,
    /// Direct TCP address if advertised (`ip:port`).
    pub tcp_addr: Option<String>,
    /// Optional relay base URL.
    pub relay_url: Option<String>,
    /// Optional identity fingerprint (hex).
    pub fingerprint: Option<String>,
    /// Source of the UDP packet.
    pub from: SocketAddr,
    /// Local time when last seen.
    pub last_seen: Instant,
}

/// Encode a compact beacon payload.
pub fn encode_beacon(
    name: &str,
    tcp_addr: Option<&str>,
    relay_url: Option<&str>,
    fingerprint: Option<&str>,
) -> Result<Vec<u8>> {
    let mut out = Vec::from(MAGIC);
    write_field(&mut out, name)?;
    write_field(&mut out, tcp_addr.unwrap_or(""))?;
    write_field(&mut out, relay_url.unwrap_or(""))?;
    write_field(&mut out, fingerprint.unwrap_or(""))?;
    Ok(out)
}

/// Decode a beacon payload.
pub fn decode_beacon(data: &[u8], from: SocketAddr) -> Result<PeerAdvert> {
    if data.len() < 4 || &data[..4] != MAGIC {
        return Err(Error::Protocol("bad discovery magic".into()));
    }
    let mut i = 4;
    let name = read_field(data, &mut i)?;
    let tcp = read_field(data, &mut i)?;
    let relay = read_field(data, &mut i)?;
    let fp = read_field(data, &mut i)?;
    Ok(PeerAdvert {
        name,
        tcp_addr: if tcp.is_empty() { None } else { Some(tcp) },
        relay_url: if relay.is_empty() {
            None
        } else {
            Some(normalize_relay_url(&relay).unwrap_or(relay))
        },
        fingerprint: if fp.is_empty() { None } else { Some(fp) },
        from,
        last_seen: Instant::now(),
    })
}

/// Broadcast beacons on the LAN and return after `duration`, collecting peers.
pub async fn advertise_and_scan(
    name: &str,
    tcp_addr: Option<&str>,
    relay_url: Option<&str>,
    fingerprint: Option<&str>,
    duration: Duration,
) -> Result<Vec<PeerAdvert>> {
    let sock = match UdpSocket::bind(format!("0.0.0.0:{DISCOVERY_PORT}")).await {
        Ok(s) => s,
        Err(_) => {
            // Port busy — bind ephemeral (still can broadcast; may miss some replies).
            UdpSocket::bind("0.0.0.0:0").await.map_err(Error::Io)?
        }
    };
    sock.set_broadcast(true)?;

    let payload = encode_beacon(name, tcp_addr, relay_url, fingerprint)?;
    let bcast = SocketAddr::from((Ipv4Addr::BROADCAST, DISCOVERY_PORT));

    let mut seen: HashMap<String, PeerAdvert> = HashMap::new();
    let deadline = Instant::now() + duration;

    while Instant::now() < deadline {
        let _ = sock.send_to(&payload, bcast).await;
        let wait =
            Duration::from_millis(400).min(deadline.saturating_duration_since(Instant::now()));
        if wait.is_zero() {
            break;
        }
        let mut buf = [0u8; 1500];
        match timeout(wait, sock.recv_from(&mut buf)).await {
            Ok(Ok((n, from))) => {
                if let Ok(adv) = decode_beacon(&buf[..n], from) {
                    // Ignore our own name+fp duplicates lightly
                    let key = format!(
                        "{}|{}",
                        adv.fingerprint.clone().unwrap_or_default(),
                        adv.tcp_addr.clone().unwrap_or_else(|| from.to_string())
                    );
                    seen.insert(key, adv);
                }
            }
            Ok(Err(_)) | Err(_) => {}
        }
    }

    Ok(seen.into_values().collect())
}

fn write_field(out: &mut Vec<u8>, s: &str) -> Result<()> {
    if s.len() > 255 {
        return Err(Error::Protocol("discovery field too long".into()));
    }
    out.push(s.len() as u8);
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

fn read_field(data: &[u8], i: &mut usize) -> Result<String> {
    if *i >= data.len() {
        return Err(Error::Protocol("discovery truncated".into()));
    }
    let len = data[*i] as usize;
    *i += 1;
    if *i + len > data.len() {
        return Err(Error::Protocol("discovery field truncated".into()));
    }
    let s = std::str::from_utf8(&data[*i..*i + len])
        .map_err(|_| Error::Protocol("discovery utf8".into()))?
        .to_string();
    *i += len;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    #[test]
    fn beacon_roundtrip() {
        let raw = encode_beacon(
            "alice",
            Some("192.168.1.2:9"),
            Some("relay.example"),
            Some("abcd"),
        )
        .unwrap();
        let from: SocketAddr = "1.2.3.4:5".parse().unwrap();
        let adv = decode_beacon(&raw, from).unwrap();
        assert_eq!(adv.name, "alice");
        assert_eq!(adv.tcp_addr.as_deref(), Some("192.168.1.2:9"));
        assert_eq!(adv.relay_url.as_deref(), Some("wss://relay.example"));
        assert_eq!(adv.fingerprint.as_deref(), Some("abcd"));
    }
}
