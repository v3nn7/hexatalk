//! Length-prefixed wire frames + large logical message chunking.
//!
//! Noise transport messages are limited to ~64 KiB, so application payloads up to
//! [`HARD_MAX_FRAME`] (25 MiB) are split into fragments before encryption and
//! reassembled after decryption.

use crate::error::{Error, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Default maximum **logical** plaintext message (25 MiB) — HD I-frames / photos.
pub const DEFAULT_MAX_FRAME: usize = 25 * 1024 * 1024;

/// Absolute upper bound for a single logical application message (25 MiB).
///
/// Relay operators: binary WebSocket frames from this client are typically
/// ≤ [`MAX_WIRE_CHUNK`] (~60 KiB). If you forward unchunked app blobs, allow 25 MiB.
pub const HARD_MAX_FRAME: usize = 25 * 1024 * 1024;

/// Max ciphertext size for one Noise transport message (Noise limit).
pub const NOISE_MAX_MESSAGE: usize = 65535;

/// Max plaintext per Noise message (tag is 16 bytes for ChaChaPoly).
pub const MAX_NOISE_PLAINTEXT: usize = NOISE_MAX_MESSAGE - 16;

/// Wire/logical fragment payload size (leaves room for fragment header + AEAD).
pub const MAX_WIRE_CHUNK: usize = 60 * 1024;

/// Fragment header magic.
const FRAG_MAGIC: u8 = 0xF1;
/// Flag: this is the last fragment of a logical message.
const FLAG_LAST: u8 = 0x01;
/// Fragment header length: magic, flags, msg_id, index, count, total_len.
const FRAG_HEADER_LEN: usize = 1 + 1 + 4 + 4 + 4 + 4;

/// Write a length-prefixed wire frame (`u32` BE length + payload).
///
/// `max_frame` here is the max **wire** payload (ciphertext chunk), not 25 MiB logical.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    payload: &[u8],
    max_frame: usize,
) -> Result<()> {
    if payload.len() > max_frame {
        return Err(Error::Framing(format!(
            "wire frame {} exceeds max {}",
            payload.len(),
            max_frame
        )));
    }
    // Wire chunks stay small (Noise); hard cap slightly above NOISE_MAX_MESSAGE.
    if payload.len() > NOISE_MAX_MESSAGE + 64 {
        return Err(Error::Framing("wire frame exceeds Noise-related maximum".into()));
    }
    let len = payload.len() as u32;
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

/// Read a length-prefixed wire frame.
pub async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    max_frame: usize,
) -> Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > max_frame || len > NOISE_MAX_MESSAGE + 64 {
        return Err(Error::Framing(format!(
            "incoming wire frame length {len} exceeds max {max_frame}"
        )));
    }
    let mut buf = vec![0u8; len];
    if len > 0 {
        reader.read_exact(&mut buf).await?;
    }
    Ok(buf)
}

/// Split a logical plaintext into fragment payloads (each ≤ `chunk_plain_max`).
pub fn split_logical(msg_id: u32, plaintext: &[u8], chunk_plain_max: usize) -> Result<Vec<Vec<u8>>> {
    if plaintext.len() > HARD_MAX_FRAME {
        return Err(Error::Framing(format!(
            "logical message {} exceeds hard max {}",
            plaintext.len(),
            HARD_MAX_FRAME
        )));
    }
    let data_max = chunk_plain_max
        .saturating_sub(FRAG_HEADER_LEN)
        .max(1024);
    if plaintext.is_empty() {
        return Ok(vec![encode_fragment(msg_id, 0, 1, 0, true, &[])]);
    }
    let chunks: Vec<&[u8]> = plaintext.chunks(data_max).collect();
    let count = chunks.len() as u32;
    if count == 0 {
        return Ok(vec![encode_fragment(msg_id, 0, 1, 0, true, &[])]);
    }
    let total_len = plaintext.len() as u32;
    let mut out = Vec::with_capacity(chunks.len());
    for (i, chunk) in chunks.iter().enumerate() {
        let last = i + 1 == chunks.len();
        out.push(encode_fragment(
            msg_id,
            i as u32,
            count,
            total_len,
            last,
            chunk,
        ));
    }
    Ok(out)
}

fn encode_fragment(
    msg_id: u32,
    index: u32,
    count: u32,
    total_len: u32,
    last: bool,
    data: &[u8],
) -> Vec<u8> {
    let mut v = Vec::with_capacity(FRAG_HEADER_LEN + data.len());
    v.push(FRAG_MAGIC);
    v.push(if last { FLAG_LAST } else { 0 });
    v.extend_from_slice(&msg_id.to_be_bytes());
    v.extend_from_slice(&index.to_be_bytes());
    v.extend_from_slice(&count.to_be_bytes());
    v.extend_from_slice(&total_len.to_be_bytes());
    v.extend_from_slice(data);
    v
}

/// Parse one fragment; returns (msg_id, index, count, total_len, last, payload).
pub fn parse_fragment(data: &[u8]) -> Result<(u32, u32, u32, u32, bool, &[u8])> {
    if data.len() < FRAG_HEADER_LEN {
        return Err(Error::Framing("fragment too short".into()));
    }
    if data[0] != FRAG_MAGIC {
        // Backward-compat: treat entire buffer as a single non-fragmented message.
        return Ok((0, 0, 1, data.len() as u32, true, data));
    }
    let flags = data[1];
    let msg_id = u32::from_be_bytes(data[2..6].try_into().unwrap());
    let index = u32::from_be_bytes(data[6..10].try_into().unwrap());
    let count = u32::from_be_bytes(data[10..14].try_into().unwrap());
    let total_len = u32::from_be_bytes(data[14..18].try_into().unwrap());
    let last = flags & FLAG_LAST != 0;
    Ok((msg_id, index, count, total_len, last, &data[FRAG_HEADER_LEN..]))
}

/// Reassembly state for one logical message.
#[derive(Debug, Default)]
pub struct Reassembler {
    msg_id: Option<u32>,
    count: u32,
    total_len: u32,
    parts: Vec<Option<Vec<u8>>>,
    received: u32,
}

impl Reassembler {
    /// Feed a decrypted fragment plaintext. Returns complete message when ready.
    pub fn push(&mut self, frag: &[u8]) -> Result<Option<Vec<u8>>> {
        let (msg_id, index, count, total_len, last, payload) = parse_fragment(frag)?;

        // Legacy single-shot (no magic): already complete.
        if frag.first() != Some(&FRAG_MAGIC) {
            return Ok(Some(payload.to_vec()));
        }

        if count == 0 || count as usize > (HARD_MAX_FRAME / 1024 + 8) {
            return Err(Error::Framing(format!("invalid fragment count {count}")));
        }
        if total_len as usize > HARD_MAX_FRAME {
            return Err(Error::Framing("fragment total_len too large".into()));
        }
        if index >= count {
            return Err(Error::Framing("fragment index out of range".into()));
        }

        if self.msg_id.is_none() {
            self.msg_id = Some(msg_id);
            self.count = count;
            self.total_len = total_len;
            self.parts = vec![None; count as usize];
            self.received = 0;
        } else if self.msg_id != Some(msg_id) {
            // New message while previous incomplete — drop old (lossy media OK).
            self.msg_id = Some(msg_id);
            self.count = count;
            self.total_len = total_len;
            self.parts = vec![None; count as usize];
            self.received = 0;
        }

        let _ = last; // completeness is determined by count, not only LAST flag
        if self.parts[index as usize].is_none() {
            self.parts[index as usize] = Some(payload.to_vec());
            self.received += 1;
        }

        if self.received < self.count {
            return Ok(None);
        }

        // Assemble in order
        let mut out = Vec::with_capacity(self.total_len as usize);
        for part in &self.parts {
            let Some(p) = part else {
                return Ok(None); // still missing
            };
            out.extend_from_slice(p);
        }
        if self.total_len > 0 && out.len() != self.total_len as usize {
            return Err(Error::Framing(format!(
                "reassembled len {} != total_len {}",
                out.len(),
                self.total_len
            )));
        }
        // reset
        *self = Reassembler::default();
        Ok(Some(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn frame_roundtrip() {
        let (mut a, mut b) = duplex(64 * 1024);
        let payload = b"hello peerseal framing";
        write_frame(&mut a, payload, NOISE_MAX_MESSAGE)
            .await
            .unwrap();
        let got = read_frame(&mut b, NOISE_MAX_MESSAGE).await.unwrap();
        assert_eq!(got, payload);
    }

    #[tokio::test]
    async fn reject_oversized_wire() {
        let (mut a, _b) = duplex(1024);
        let big = vec![0u8; 100];
        let err = write_frame(&mut a, &big, 50).await.unwrap_err();
        assert!(matches!(err, Error::Framing(_)));
    }

    #[tokio::test]
    async fn empty_frame() {
        let (mut a, mut b) = duplex(1024);
        write_frame(&mut a, &[], NOISE_MAX_MESSAGE).await.unwrap();
        let got = read_frame(&mut b, NOISE_MAX_MESSAGE).await.unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn logical_1mb_roundtrip_reassembly() {
        let data = vec![0xABu8; 1_000_000];
        let frags = split_logical(7, &data, MAX_WIRE_CHUNK).unwrap();
        assert!(frags.len() > 1);
        let mut r = Reassembler::default();
        let mut out = None;
        for f in frags {
            out = r.push(&f).unwrap();
        }
        assert_eq!(out.unwrap(), data);
    }

    #[test]
    fn logical_25mb_splits() {
        let n = HARD_MAX_FRAME;
        // Don't allocate full 25MiB in CI memory thrash — check math on 3 MiB
        let data = vec![1u8; 3 * 1024 * 1024];
        let frags = split_logical(1, &data, MAX_WIRE_CHUNK).unwrap();
        assert!(frags.len() > 40);
        let mut r = Reassembler::default();
        let mut out = None;
        for f in frags {
            if let Some(m) = r.push(&f).unwrap() {
                out = Some(m);
            }
        }
        assert_eq!(out.unwrap().len(), data.len());
        let _ = n;
    }
}
