use lyteboard_protocol::{
    ipc::{self, DecodeError},
    peer::Media,
};
use serde::{Serialize, de::DeserializeOwned};
use std::io;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zeroize::Zeroizing;
#[derive(Debug, Error)]
pub enum FrameError {
    #[error("frame length is outside the permitted range")]
    Length(u32),
    #[error(transparent)]
    Decode(#[from] DecodeError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

pub async fn receive<T: DeserializeOwned>(
    stream: &mut (impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin),
) -> Result<T, FrameError> {
    let length = stream.read_u32().await?;
    if length == 0 || length as usize > ipc::MAX_FRAME_BYTES {
        return Err(FrameError::Length(length));
    }

    let mut bytes = vec![0; length as usize];
    stream.read_exact(&mut bytes).await?;
    Ok(ipc::decode(&bytes)?)
}

pub async fn send<T: Serialize>(
    stream: &mut (impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin),
    body: &T,
) -> io::Result<()> {
    let bytes = ipc::encode(body).map_err(io::Error::other)?;
    if bytes.len() > ipc::MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "IPC frame exceeds the size limit",
        ));
    }

    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(&bytes).await
}

/// Read exactly the bounded payload and its platform-specific end marker.
/// Unix uses a half-close; Windows byte-mode pipes use one zero byte.
pub async fn read_bulk(
    stream: &mut (impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin),
    media: Media,
    size: u32,
) -> io::Result<Zeroizing<Vec<u8>>> {
    crate::bulk::validate_size(media, size as usize).map_err(io::Error::other)?;
    let mut bytes = Zeroizing::new(vec![0; size as usize]);
    stream.read_exact(&mut bytes).await?;
    #[cfg(target_os = "linux")]
    let invalid_end = stream.read(&mut [0]).await? != 0;
    #[cfg(target_os = "windows")]
    let invalid_end = stream.read_u8().await? != 0;
    if invalid_end {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "extra bytes after bulk payload",
        ));
    }
    Ok(bytes)
}

pub async fn write_bulk(
    stream: &mut (impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin),
    bytes: &[u8],
) -> io::Result<()> {
    stream.write_all(bytes).await?;
    #[cfg(target_os = "linux")]
    return stream.shutdown().await;
    #[cfg(target_os = "windows")]
    {
        stream.write_u8(0).await?;
        stream.flush().await
    }
}
