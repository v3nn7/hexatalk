//! Media helpers: photos, screen frames, audio chunks over E2E session.
//!
//! These helpers only define the **application framing**. Capture/encode
//! (JPEG screenshots, Opus audio, etc.) is left to the application so peerseal
//! stays dependency-light and platform-agnostic.

use crate::error::Result;
use crate::protocol::{AppMessage, DEFAULT_CHUNK, MediaKind};
use crate::session::SecureStream;
use tokio::io::{AsyncRead, AsyncWrite};

/// Send a single photo (or any still image blob) as a short media stream.
pub async fn send_photo<S>(
    stream: &mut SecureStream<S>,
    stream_id: u32,
    content_type: &str,
    data: &[u8],
    width: u32,
    height: u32,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    stream
        .send(
            &AppMessage::MediaStart {
                stream_id,
                kind: MediaKind::Photo,
                content_type: content_type.to_string(),
                width,
                height,
            }
            .encode()?,
        )
        .await?;

    // One or more frames if huge
    let mut seq = 0u32;
    for chunk in data.chunks(DEFAULT_CHUNK) {
        stream
            .send(
                &AppMessage::MediaFrame {
                    stream_id,
                    seq,
                    data: chunk.to_vec(),
                }
                .encode()?,
            )
            .await?;
        seq = seq.saturating_add(1);
    }

    stream
        .send(&AppMessage::MediaEnd { stream_id }.encode()?)
        .await?;
    Ok(())
}

/// Open a screen-share stream (caller then sends frames with [`send_media_frame`]).
pub async fn open_screen_stream<S>(
    stream: &mut SecureStream<S>,
    stream_id: u32,
    content_type: &str,
    width: u32,
    height: u32,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    stream
        .send(
            &AppMessage::MediaStart {
                stream_id,
                kind: MediaKind::Screen,
                content_type: content_type.to_string(),
                width,
                height,
            }
            .encode()?,
        )
        .await
}

/// Open an audio stream (e.g. Opus packets as frames).
pub async fn open_audio_stream<S>(
    stream: &mut SecureStream<S>,
    stream_id: u32,
    content_type: &str,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    stream
        .send(
            &AppMessage::MediaStart {
                stream_id,
                kind: MediaKind::Audio,
                content_type: content_type.to_string(),
                width: 0,
                height: 0,
            }
            .encode()?,
        )
        .await
}

/// Send one media frame on an open stream.
pub async fn send_media_frame<S>(
    stream: &mut SecureStream<S>,
    stream_id: u32,
    seq: u32,
    data: &[u8],
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    stream
        .send(
            &AppMessage::MediaFrame {
                stream_id,
                seq,
                data: data.to_vec(),
            }
            .encode()?,
        )
        .await
}

/// Close a media stream.
pub async fn close_media_stream<S>(stream: &mut SecureStream<S>, stream_id: u32) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    stream
        .send(&AppMessage::MediaEnd { stream_id }.encode()?)
        .await
}

/// Reconstruct a full photo from MediaStart + frames + MediaEnd already received
/// as a list of messages (application demux helper).
pub fn assemble_media_payload(frames: &[AppMessage]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut started = false;
    let mut stream_id = 0u32;
    for m in frames {
        match m {
            AppMessage::MediaStart { stream_id: sid, .. } => {
                started = true;
                stream_id = *sid;
            }
            AppMessage::MediaFrame {
                stream_id: sid,
                data,
                ..
            } if started && *sid == stream_id => {
                out.extend_from_slice(data);
            }
            AppMessage::MediaEnd { stream_id: sid } if *sid == stream_id => break,
            _ => {}
        }
    }
    if !started {
        return Err(crate::error::Error::Protocol(
            "no MediaStart in frame list".into(),
        ));
    }
    Ok(out)
}
