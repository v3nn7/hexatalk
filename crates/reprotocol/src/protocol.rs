//! Application-layer typed messages (over Noise transport plaintext).

use crate::error::{Error, Result};

/// Default chunk size for file / media payloads (~48 KiB leaves room for headers + AEAD).
pub const DEFAULT_CHUNK: usize = 48 * 1024;

/// Application message kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MsgType {
    /// UTF-8 text chat.
    Text = 0x01,
    /// Opaque binary blob.
    Binary = 0x02,
    /// File transfer metadata (name, size, mime, transfer_id).
    FileMeta = 0x10,
    /// File content chunk.
    FileChunk = 0x11,
    /// File transfer complete.
    FileEnd = 0x12,
    /// Media stream open (screen/photo/audio metadata).
    MediaStart = 0x20,
    /// Media frame / chunk.
    MediaFrame = 0x21,
    /// Media stream end.
    MediaEnd = 0x22,
    /// Control: ping.
    Ping = 0x30,
    /// Control: pong.
    Pong = 0x31,
    /// Control: request coordinated rekey.
    Rekey = 0x32,
    /// Peer acknowledges SAS comparison (bool).
    SasAck = 0x33,
    /// HD VC session offer / config.
    VcOffer = 0x40,
    /// HD VC session answer.
    VcAnswer = 0x41,
    /// Compressed / coded video frame (may be multi-MiB; logical 25 MiB).
    VcVideo = 0x42,
    /// Audio frame (e.g. Opus packet).
    VcAudio = 0x43,
    /// VC control (PLI/keyframe request, bye, bitrate).
    VcControl = 0x44,
}

impl MsgType {
    fn from_u8(v: u8) -> Result<Self> {
        Ok(match v {
            0x01 => Self::Text,
            0x02 => Self::Binary,
            0x10 => Self::FileMeta,
            0x11 => Self::FileChunk,
            0x12 => Self::FileEnd,
            0x20 => Self::MediaStart,
            0x21 => Self::MediaFrame,
            0x22 => Self::MediaEnd,
            0x30 => Self::Ping,
            0x31 => Self::Pong,
            0x32 => Self::Rekey,
            0x33 => Self::SasAck,
            0x40 => Self::VcOffer,
            0x41 => Self::VcAnswer,
            0x42 => Self::VcVideo,
            0x43 => Self::VcAudio,
            0x44 => Self::VcControl,
            _ => return Err(Error::Protocol(format!("unknown msg type 0x{v:02x}"))),
        })
    }
}

/// Kind of media for [`AppMessage::MediaStart`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MediaKind {
    /// Still photo.
    Photo = 1,
    /// Screen share frame stream.
    Screen = 2,
    /// Voice / audio frames (e.g. Opus).
    Audio = 3,
    /// Webcam video frames.
    Camera = 4,
    /// Generic / unknown.
    Other = 255,
}

impl MediaKind {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Photo,
            2 => Self::Screen,
            3 => Self::Audio,
            4 => Self::Camera,
            _ => Self::Other,
        }
    }
}

/// Codec / content-type hint (short string, e.g. `image/jpeg`, `audio/opus`).
pub type ContentType = String;

/// Typed application message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppMessage {
    /// Chat text.
    Text(String),
    /// Raw binary.
    Binary(Vec<u8>),
    /// Begin file transfer.
    FileMeta {
        /// Transfer id (correlation).
        id: u32,
        /// File name (basename recommended).
        name: String,
        /// Total size in bytes.
        size: u64,
        /// MIME or empty.
        content_type: String,
    },
    /// File payload chunk.
    FileChunk {
        /// Transfer id.
        id: u32,
        /// Zero-based chunk index.
        index: u32,
        /// Chunk bytes.
        data: Vec<u8>,
    },
    /// End of file (optional sha256 hex of full file).
    FileEnd {
        /// Transfer id.
        id: u32,
        /// Optional content hash (hex SHA-256).
        sha256_hex: Option<String>,
    },
    /// Open a media stream.
    MediaStart {
        /// Stream id.
        stream_id: u32,
        /// Media kind.
        kind: MediaKind,
        /// Codec / MIME.
        content_type: String,
        /// Optional width (0 = unknown).
        width: u32,
        /// Optional height (0 = unknown).
        height: u32,
    },
    /// One media frame or chunk.
    MediaFrame {
        /// Stream id.
        stream_id: u32,
        /// Frame sequence.
        seq: u32,
        /// Payload bytes (e.g. JPEG).
        data: Vec<u8>,
    },
    /// Close media stream.
    MediaEnd {
        /// Stream id.
        stream_id: u32,
    },
    /// Keepalive ping (optional payload).
    Ping(Vec<u8>),
    /// Keepalive pong.
    Pong(Vec<u8>),
    /// Coordinated Noise rekey.
    Rekey,
    /// SAS verification result from peer (`true` = matched).
    SasAck(bool),
    /// HD video call offer (local send capabilities).
    VcOffer {
        /// Profile tag, e.g. `1080p30`.
        profile: String,
        /// Video codec name, e.g. `jpeg`, `h264`, `vp8`.
        video_codec: String,
        /// Audio codec name, e.g. `opus`, `pcm_s16le`.
        audio_codec: String,
        /// Max width.
        width: u32,
        /// Max height.
        height: u32,
        /// Target FPS.
        fps: u32,
    },
    /// HD video call answer.
    VcAnswer {
        /// Accepted profile.
        profile: String,
        /// Accepted video codec.
        video_codec: String,
        /// Accepted audio codec.
        audio_codec: String,
        /// Agreed width.
        width: u32,
        /// Agreed height.
        height: u32,
        /// Agreed FPS.
        fps: u32,
    },
    /// One video access unit (full coded frame, up to 25 MiB logical).
    VcVideo {
        /// Presentation timestamp (ms).
        pts_ms: u64,
        /// Frame sequence.
        seq: u32,
        /// Coded width.
        width: u16,
        /// Coded height.
        height: u16,
        /// Codec id (see [`crate::vc::VideoCodec`]).
        codec: u8,
        /// Bit0 = keyframe / IDR.
        flags: u8,
        /// Encoded bitstream (JPEG / H264 / …).
        data: Vec<u8>,
    },
    /// One audio packet.
    VcAudio {
        /// Presentation timestamp (ms).
        pts_ms: u64,
        /// Sequence.
        seq: u32,
        /// Codec id.
        codec: u8,
        /// Sample rate.
        sample_rate: u32,
        /// Channels.
        channels: u8,
        /// Encoded audio.
        data: Vec<u8>,
    },
    /// VC control message.
    VcControl {
        /// Control kind (see [`crate::vc::VcControlKind`]).
        kind: u8,
        /// Optional value (bitrate bps, etc.).
        value: u32,
    },
}

impl AppMessage {
    /// Encode to plaintext bytes for [`crate::session::SecureStream::send`].
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        match self {
            Self::Text(s) => {
                out.push(MsgType::Text as u8);
                out.extend_from_slice(s.as_bytes());
            }
            Self::Binary(b) => {
                out.push(MsgType::Binary as u8);
                out.extend_from_slice(b);
            }
            Self::FileMeta {
                id,
                name,
                size,
                content_type,
            } => {
                out.push(MsgType::FileMeta as u8);
                out.extend_from_slice(&id.to_be_bytes());
                out.extend_from_slice(&size.to_be_bytes());
                write_str(&mut out, content_type)?;
                write_str(&mut out, name)?;
            }
            Self::FileChunk { id, index, data } => {
                out.push(MsgType::FileChunk as u8);
                out.extend_from_slice(&id.to_be_bytes());
                out.extend_from_slice(&index.to_be_bytes());
                out.extend_from_slice(data);
            }
            Self::FileEnd { id, sha256_hex } => {
                out.push(MsgType::FileEnd as u8);
                out.extend_from_slice(&id.to_be_bytes());
                match sha256_hex {
                    Some(h) => {
                        out.push(1);
                        write_str(&mut out, h)?;
                    }
                    None => out.push(0),
                }
            }
            Self::MediaStart {
                stream_id,
                kind,
                content_type,
                width,
                height,
            } => {
                out.push(MsgType::MediaStart as u8);
                out.extend_from_slice(&stream_id.to_be_bytes());
                out.push(*kind as u8);
                out.extend_from_slice(&width.to_be_bytes());
                out.extend_from_slice(&height.to_be_bytes());
                write_str(&mut out, content_type)?;
            }
            Self::MediaFrame {
                stream_id,
                seq,
                data,
            } => {
                out.push(MsgType::MediaFrame as u8);
                out.extend_from_slice(&stream_id.to_be_bytes());
                out.extend_from_slice(&seq.to_be_bytes());
                out.extend_from_slice(data);
            }
            Self::MediaEnd { stream_id } => {
                out.push(MsgType::MediaEnd as u8);
                out.extend_from_slice(&stream_id.to_be_bytes());
            }
            Self::Ping(p) => {
                out.push(MsgType::Ping as u8);
                out.extend_from_slice(p);
            }
            Self::Pong(p) => {
                out.push(MsgType::Pong as u8);
                out.extend_from_slice(p);
            }
            Self::Rekey => {
                out.push(MsgType::Rekey as u8);
            }
            Self::SasAck(ok) => {
                out.push(MsgType::SasAck as u8);
                out.push(if *ok { 1 } else { 0 });
            }
            Self::VcOffer {
                profile,
                video_codec,
                audio_codec,
                width,
                height,
                fps,
            } => {
                out.push(MsgType::VcOffer as u8);
                out.extend_from_slice(&width.to_be_bytes());
                out.extend_from_slice(&height.to_be_bytes());
                out.extend_from_slice(&fps.to_be_bytes());
                write_str(&mut out, profile)?;
                write_str(&mut out, video_codec)?;
                write_str(&mut out, audio_codec)?;
            }
            Self::VcAnswer {
                profile,
                video_codec,
                audio_codec,
                width,
                height,
                fps,
            } => {
                out.push(MsgType::VcAnswer as u8);
                out.extend_from_slice(&width.to_be_bytes());
                out.extend_from_slice(&height.to_be_bytes());
                out.extend_from_slice(&fps.to_be_bytes());
                write_str(&mut out, profile)?;
                write_str(&mut out, video_codec)?;
                write_str(&mut out, audio_codec)?;
            }
            Self::VcVideo {
                pts_ms,
                seq,
                width,
                height,
                codec,
                flags,
                data,
            } => {
                out.push(MsgType::VcVideo as u8);
                out.extend_from_slice(&pts_ms.to_be_bytes());
                out.extend_from_slice(&seq.to_be_bytes());
                out.extend_from_slice(&width.to_be_bytes());
                out.extend_from_slice(&height.to_be_bytes());
                out.push(*codec);
                out.push(*flags);
                out.extend_from_slice(data);
            }
            Self::VcAudio {
                pts_ms,
                seq,
                codec,
                sample_rate,
                channels,
                data,
            } => {
                out.push(MsgType::VcAudio as u8);
                out.extend_from_slice(&pts_ms.to_be_bytes());
                out.extend_from_slice(&seq.to_be_bytes());
                out.push(*codec);
                out.extend_from_slice(&sample_rate.to_be_bytes());
                out.push(*channels);
                out.extend_from_slice(data);
            }
            Self::VcControl { kind, value } => {
                out.push(MsgType::VcControl as u8);
                out.push(*kind);
                out.extend_from_slice(&value.to_be_bytes());
            }
        }
        Ok(out)
    }

    /// Decode from plaintext bytes.
    pub fn decode(data: &[u8]) -> Result<Self> {
        if data.is_empty() {
            return Err(Error::Protocol("empty app message".into()));
        }
        let ty = MsgType::from_u8(data[0])?;
        let body = &data[1..];
        Ok(match ty {
            MsgType::Text => {
                let s = std::str::from_utf8(body)
                    .map_err(|_| Error::Protocol("text not utf-8".into()))?;
                Self::Text(s.to_string())
            }
            MsgType::Binary => Self::Binary(body.to_vec()),
            MsgType::FileMeta => {
                if body.len() < 4 + 8 {
                    return Err(Error::Protocol("file meta short".into()));
                }
                let id = u32::from_be_bytes(body[0..4].try_into().unwrap());
                let size = u64::from_be_bytes(body[4..12].try_into().unwrap());
                let mut i = 12;
                let content_type = read_str(body, &mut i)?;
                let name = read_str(body, &mut i)?;
                Self::FileMeta {
                    id,
                    name,
                    size,
                    content_type,
                }
            }
            MsgType::FileChunk => {
                if body.len() < 8 {
                    return Err(Error::Protocol("file chunk short".into()));
                }
                let id = u32::from_be_bytes(body[0..4].try_into().unwrap());
                let index = u32::from_be_bytes(body[4..8].try_into().unwrap());
                Self::FileChunk {
                    id,
                    index,
                    data: body[8..].to_vec(),
                }
            }
            MsgType::FileEnd => {
                if body.len() < 5 {
                    return Err(Error::Protocol("file end short".into()));
                }
                let id = u32::from_be_bytes(body[0..4].try_into().unwrap());
                let flag = body[4];
                let sha256_hex = if flag == 1 {
                    let mut i = 5;
                    Some(read_str(body, &mut i)?)
                } else {
                    None
                };
                Self::FileEnd { id, sha256_hex }
            }
            MsgType::MediaStart => {
                if body.len() < 4 + 1 + 4 + 4 {
                    return Err(Error::Protocol("media start short".into()));
                }
                let stream_id = u32::from_be_bytes(body[0..4].try_into().unwrap());
                let kind = MediaKind::from_u8(body[4]);
                let width = u32::from_be_bytes(body[5..9].try_into().unwrap());
                let height = u32::from_be_bytes(body[9..13].try_into().unwrap());
                let mut i = 13;
                let content_type = read_str(body, &mut i)?;
                Self::MediaStart {
                    stream_id,
                    kind,
                    content_type,
                    width,
                    height,
                }
            }
            MsgType::MediaFrame => {
                if body.len() < 8 {
                    return Err(Error::Protocol("media frame short".into()));
                }
                let stream_id = u32::from_be_bytes(body[0..4].try_into().unwrap());
                let seq = u32::from_be_bytes(body[4..8].try_into().unwrap());
                Self::MediaFrame {
                    stream_id,
                    seq,
                    data: body[8..].to_vec(),
                }
            }
            MsgType::MediaEnd => {
                if body.len() < 4 {
                    return Err(Error::Protocol("media end short".into()));
                }
                let stream_id = u32::from_be_bytes(body[0..4].try_into().unwrap());
                Self::MediaEnd { stream_id }
            }
            MsgType::Ping => Self::Ping(body.to_vec()),
            MsgType::Pong => Self::Pong(body.to_vec()),
            MsgType::Rekey => Self::Rekey,
            MsgType::SasAck => {
                let ok = body.first().copied().unwrap_or(0) != 0;
                Self::SasAck(ok)
            }
            MsgType::VcOffer | MsgType::VcAnswer => {
                if body.len() < 12 {
                    return Err(Error::Protocol("vc offer/answer short".into()));
                }
                let width = u32::from_be_bytes(body[0..4].try_into().unwrap());
                let height = u32::from_be_bytes(body[4..8].try_into().unwrap());
                let fps = u32::from_be_bytes(body[8..12].try_into().unwrap());
                let mut i = 12;
                let profile = read_str(body, &mut i)?;
                let video_codec = read_str(body, &mut i)?;
                let audio_codec = read_str(body, &mut i)?;
                if matches!(ty, MsgType::VcOffer) {
                    Self::VcOffer {
                        profile,
                        video_codec,
                        audio_codec,
                        width,
                        height,
                        fps,
                    }
                } else {
                    Self::VcAnswer {
                        profile,
                        video_codec,
                        audio_codec,
                        width,
                        height,
                        fps,
                    }
                }
            }
            MsgType::VcVideo => {
                if body.len() < 8 + 4 + 2 + 2 + 1 + 1 {
                    return Err(Error::Protocol("vc video short".into()));
                }
                let pts_ms = u64::from_be_bytes(body[0..8].try_into().unwrap());
                let seq = u32::from_be_bytes(body[8..12].try_into().unwrap());
                let width = u16::from_be_bytes(body[12..14].try_into().unwrap());
                let height = u16::from_be_bytes(body[14..16].try_into().unwrap());
                let codec = body[16];
                let flags = body[17];
                Self::VcVideo {
                    pts_ms,
                    seq,
                    width,
                    height,
                    codec,
                    flags,
                    data: body[18..].to_vec(),
                }
            }
            MsgType::VcAudio => {
                if body.len() < 8 + 4 + 1 + 4 + 1 {
                    return Err(Error::Protocol("vc audio short".into()));
                }
                let pts_ms = u64::from_be_bytes(body[0..8].try_into().unwrap());
                let seq = u32::from_be_bytes(body[8..12].try_into().unwrap());
                let codec = body[12];
                let sample_rate = u32::from_be_bytes(body[13..17].try_into().unwrap());
                let channels = body[17];
                Self::VcAudio {
                    pts_ms,
                    seq,
                    codec,
                    sample_rate,
                    channels,
                    data: body[18..].to_vec(),
                }
            }
            MsgType::VcControl => {
                if body.len() < 5 {
                    return Err(Error::Protocol("vc control short".into()));
                }
                let kind = body[0];
                let value = u32::from_be_bytes(body[1..5].try_into().unwrap());
                Self::VcControl { kind, value }
            }
        })
    }
}

fn write_str(out: &mut Vec<u8>, s: &str) -> Result<()> {
    if s.len() > u16::MAX as usize {
        return Err(Error::Protocol("string too long".into()));
    }
    out.extend_from_slice(&(s.len() as u16).to_be_bytes());
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

fn read_str(data: &[u8], i: &mut usize) -> Result<String> {
    if *i + 2 > data.len() {
        return Err(Error::Protocol("string len truncated".into()));
    }
    let len = u16::from_be_bytes([data[*i], data[*i + 1]]) as usize;
    *i += 2;
    if *i + len > data.len() {
        return Err(Error::Protocol("string body truncated".into()));
    }
    let s = std::str::from_utf8(&data[*i..*i + len])
        .map_err(|_| Error::Protocol("string not utf-8".into()))?
        .to_string();
    *i += len;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_all() {
        let msgs = vec![
            AppMessage::Text("cześć".into()),
            AppMessage::Binary(vec![1, 2, 3]),
            AppMessage::FileMeta {
                id: 9,
                name: "a.bin".into(),
                size: 100,
                content_type: "application/octet-stream".into(),
            },
            AppMessage::FileChunk {
                id: 9,
                index: 0,
                data: vec![0xAB; 64],
            },
            AppMessage::FileEnd {
                id: 9,
                sha256_hex: Some("abcd".into()),
            },
            AppMessage::MediaStart {
                stream_id: 1,
                kind: MediaKind::Screen,
                content_type: "image/jpeg".into(),
                width: 1920,
                height: 1080,
            },
            AppMessage::MediaFrame {
                stream_id: 1,
                seq: 3,
                data: vec![0xFF, 0xD8],
            },
            AppMessage::MediaEnd { stream_id: 1 },
            AppMessage::Ping(b"x".to_vec()),
            AppMessage::Pong(b"x".to_vec()),
            AppMessage::Rekey,
            AppMessage::SasAck(true),
            AppMessage::VcOffer {
                profile: "1080p30".into(),
                video_codec: "jpeg".into(),
                audio_codec: "opus".into(),
                width: 1920,
                height: 1080,
                fps: 30,
            },
            AppMessage::VcVideo {
                pts_ms: 1000,
                seq: 2,
                width: 1920,
                height: 1080,
                codec: 1,
                flags: 1,
                data: vec![0xFF, 0xD8, 0xFF],
            },
            AppMessage::VcAudio {
                pts_ms: 1000,
                seq: 5,
                codec: 1,
                sample_rate: 48000,
                channels: 1,
                data: vec![0, 1, 2],
            },
            AppMessage::VcControl { kind: 1, value: 0 },
        ];
        for m in msgs {
            let e = m.encode().unwrap();
            let d = AppMessage::decode(&e).unwrap();
            assert_eq!(m, d);
        }
    }
}
