//! Chunked file transfer over a typed [`crate::protocol::AppMessage`] session.

use crate::error::{Error, Result};
use crate::protocol::{AppMessage, DEFAULT_CHUNK};
use crate::session::SecureStream;
use sha2::{Digest, Sha256};
use std::path::Path;
use tokio::fs::File;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Progress callback: (bytes_sent_or_received, total_size).
pub type ProgressFn = Box<dyn FnMut(u64, u64) + Send>;

/// Send a file as `FileMeta` + chunks + `FileEnd` using the raw secure stream
/// and the app protocol encoder. Caller must run a matching [`recv_file`] on the peer.
///
/// This is a **half-duplex** helper: it only sends; the peer should not interleave
/// unrelated traffic on the same logical conversation without demuxing by transfer id.
pub async fn send_file_on<S, F>(
    stream: &mut SecureStream<S>,
    path: impl AsRef<Path>,
    transfer_id: u32,
    content_type: &str,
    chunk_size: usize,
    mut on_progress: Option<F>,
) -> Result<[u8; 32]>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    F: FnMut(u64, u64) + Send,
{
    let path = path.as_ref();
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("file")
        .to_string();
    let meta = tokio::fs::metadata(path).await?;
    let size = meta.len();
    let chunk_size = chunk_size.clamp(1024, DEFAULT_CHUNK.max(1024));

    stream
        .send(
            &AppMessage::FileMeta {
                id: transfer_id,
                name,
                size,
                content_type: content_type.to_string(),
            }
            .encode()?,
        )
        .await?;

    let mut file = File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; chunk_size];
    let mut sent = 0u64;
    let mut index = 0u32;

    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        stream
            .send(
                &AppMessage::FileChunk {
                    id: transfer_id,
                    index,
                    data: buf[..n].to_vec(),
                }
                .encode()?,
            )
            .await?;
        sent += n as u64;
        index = index.saturating_add(1);
        if let Some(ref mut cb) = on_progress {
            cb(sent, size);
        }
    }

    let dig: [u8; 32] = hasher.finalize().into();
    let hex = dig.iter().map(|b| format!("{b:02x}")).collect::<String>();
    stream
        .send(
            &AppMessage::FileEnd {
                id: transfer_id,
                sha256_hex: Some(hex),
            }
            .encode()?,
        )
        .await?;
    Ok(dig)
}

/// Receive a file starting from an already-decoded [`AppMessage::FileMeta`].
///
/// Subsequent `recv` calls must yield chunks/end for the same id (or we skip unrelated
/// messages if `skip_unrelated` is true — currently we error on mismatch).
pub async fn recv_file_from_meta<S>(
    stream: &mut SecureStream<S>,
    meta: AppMessage,
    dest_path: impl AsRef<Path>,
) -> Result<[u8; 32]>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (id, size, _name) = match meta {
        AppMessage::FileMeta {
            id, size, name, ..
        } => (id, size, name),
        _ => {
            return Err(Error::Protocol(
                "recv_file_from_meta expects FileMeta".into(),
            ));
        }
    };

    if let Some(parent) = dest_path.as_ref().parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut file = File::create(dest_path.as_ref()).await?;
    let mut hasher = Sha256::new();
    let mut received = 0u64;
    let mut expected_index = 0u32;
    let remote_hash: Option<String>;

    loop {
        let raw = stream.recv().await?;
        let msg = AppMessage::decode(&raw)?;
        match msg {
            AppMessage::FileChunk {
                id: cid,
                index,
                data,
            } => {
                if cid != id {
                    return Err(Error::Protocol(format!(
                        "unexpected transfer id {cid}, want {id}"
                    )));
                }
                if index != expected_index {
                    return Err(Error::Protocol(format!(
                        "chunk index {index}, expected {expected_index}"
                    )));
                }
                file.write_all(&data).await?;
                hasher.update(&data);
                received += data.len() as u64;
                expected_index = expected_index.saturating_add(1);
            }
            AppMessage::FileEnd {
                id: cid,
                sha256_hex,
            } => {
                if cid != id {
                    return Err(Error::Protocol("file end id mismatch".into()));
                }
                remote_hash = sha256_hex;
                break;
            }
            other => {
                return Err(Error::Protocol(format!(
                    "unexpected message during file recv: {other:?}"
                )));
            }
        }
    }

    file.flush().await?;
    if received != size {
        return Err(Error::Protocol(format!(
            "size mismatch: got {received}, expected {size}"
        )));
    }
    let dig: [u8; 32] = hasher.finalize().into();
    if let Some(remote) = remote_hash {
        let local = dig.iter().map(|b| format!("{b:02x}")).collect::<String>();
        if local != remote {
            return Err(Error::Protocol(format!(
                "sha256 mismatch: local {local} remote {remote}"
            )));
        }
    }
    Ok(dig)
}

/// Send in-memory bytes as a named file transfer.
pub async fn send_bytes_as_file<S>(
    stream: &mut SecureStream<S>,
    name: &str,
    data: &[u8],
    transfer_id: u32,
    content_type: &str,
    chunk_size: usize,
) -> Result<[u8; 32]>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let size = data.len() as u64;
    let chunk_size = chunk_size.clamp(1024, DEFAULT_CHUNK.max(1024));
    stream
        .send(
            &AppMessage::FileMeta {
                id: transfer_id,
                name: name.to_string(),
                size,
                content_type: content_type.to_string(),
            }
            .encode()?,
        )
        .await?;

    let mut hasher = Sha256::new();
    let mut index = 0u32;
    for chunk in data.chunks(chunk_size) {
        hasher.update(chunk);
        stream
            .send(
                &AppMessage::FileChunk {
                    id: transfer_id,
                    index,
                    data: chunk.to_vec(),
                }
                .encode()?,
            )
            .await?;
        index = index.saturating_add(1);
    }
    let dig: [u8; 32] = hasher.finalize().into();
    let hex = dig.iter().map(|b| format!("{b:02x}")).collect::<String>();
    stream
        .send(
            &AppMessage::FileEnd {
                id: transfer_id,
                sha256_hex: Some(hex),
            }
            .encode()?,
        )
        .await?;
    Ok(dig)
}
