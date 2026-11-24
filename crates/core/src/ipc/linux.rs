//! Linux transport shared by the daemon and its clients.

mod endpoint;

use std::{io, time::Duration};

pub use endpoint::{Endpoint, EndpointError, ServerSocket};
use lyteboard_protocol::{
    ipc::{
        self, DecodeError, Request, Response,
        network::{Command, QueueOutcome, Reply},
    },
    peer::Media,
};
use serde::{Serialize, de::DeserializeOwned};
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    time::timeout,
};
use zeroize::Zeroizing;

use super::ClientError;

pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
pub const BULK_TIMEOUT: Duration = Duration::from_secs(20);
const CLIENT_TIMEOUT: Duration = Duration::from_secs(3);

pub fn authenticate(stream: &UnixStream) -> io::Result<()> {
    check_peer(stream, rustix::process::geteuid().as_raw())
}

fn check_peer(stream: &UnixStream, expected_uid: u32) -> io::Result<()> {
    if stream.peer_cred()?.uid() == expected_uid {
        return Ok(());
    }

    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "peer UID does not match",
    ))
}

#[derive(Debug, Error)]
pub enum FrameError {
    #[error("frame length is outside the permitted range")]
    Length(u32),
    #[error(transparent)]
    Decode(#[from] DecodeError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

pub async fn receive<T: DeserializeOwned>(stream: &mut UnixStream) -> Result<T, FrameError> {
    let length = stream.read_u32().await?;
    if length == 0 || length as usize > ipc::MAX_FRAME_BYTES {
        return Err(FrameError::Length(length));
    }

    let mut bytes = vec![0; length as usize];
    stream.read_exact(&mut bytes).await?;
    Ok(ipc::decode(&bytes)?)
}

pub async fn send<T: Serialize>(stream: &mut UnixStream, body: &T) -> io::Result<()> {
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

pub(super) async fn request(
    endpoint: &Endpoint,
    request: Request,
) -> Result<Response, ClientError> {
    timeout(CLIENT_TIMEOUT, async {
        let mut stream = connect(endpoint).await?;
        send(&mut stream, &request).await?;
        response(&mut stream).await
    })
    .await
    .map_err(|_| ClientError::Timeout)?
}

async fn connect(endpoint: &Endpoint) -> Result<UnixStream, ClientError> {
    let path = endpoint.existing_socket()?.ok_or(ClientError::Offline)?;
    let stream = UnixStream::connect(path)
        .await
        .map_err(|error| match error.kind() {
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => ClientError::Offline,
            _ => ClientError::Connect(error),
        })?;
    authenticate(&stream).map_err(ClientError::Authentication)?;
    Ok(stream)
}

async fn response(stream: &mut UnixStream) -> Result<Response, ClientError> {
    match receive(stream).await {
        Ok(Response::Error(code)) => Err(ClientError::Rejected(code)),
        Ok(response) => Ok(response),
        Err(FrameError::Decode(error)) => Err(error.into()),
        Err(FrameError::Io(error)) => Err(error.into()),
        Err(FrameError::Length(_)) => Err(ClientError::FrameLength),
    }
}

/// Read exactly the announced binary payload, followed by a half-close.
pub async fn read_bulk(
    stream: &mut UnixStream,
    media: Media,
    size: u32,
) -> io::Result<Zeroizing<Vec<u8>>> {
    crate::bulk::validate_size(media, size as usize).map_err(io::Error::other)?;
    let mut bytes = Zeroizing::new(vec![0; size as usize]);
    stream.read_exact(&mut bytes).await?;
    if stream.read(&mut [0]).await? != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "extra bytes after bulk payload",
        ));
    }
    Ok(bytes)
}

pub async fn write_bulk(stream: &mut UnixStream, bytes: &[u8]) -> io::Result<()> {
    stream.write_all(bytes).await?;
    stream.shutdown().await
}

async fn bulk_reply(stream: &mut UnixStream) -> Result<Reply, ClientError> {
    match response(stream).await? {
        Response::Network(Reply::Error(error)) => Err(ClientError::Bulk(error)),
        Response::Network(reply) => Ok(reply),
        _ => Err(ClientError::UnexpectedResponse),
    }
}

pub(super) async fn upload(
    endpoint: &Endpoint,
    peers: Vec<String>,
    media: Media,
    bytes: &[u8],
) -> Result<Vec<QueueOutcome>, ClientError> {
    crate::bulk::validate_size(media, bytes.len())
        .map_err(|error| ClientError::Bulk(error.to_string()))?;
    timeout(BULK_TIMEOUT, async {
        let mut stream = connect(endpoint).await?;
        let request = Request::Network(Command::BulkUpload {
            peers,
            media,
            size: bytes.len() as u32,
        });
        send(&mut stream, &request).await?;
        if !matches!(bulk_reply(&mut stream).await?, Reply::BulkReady) {
            return Err(ClientError::UnexpectedResponse);
        }
        write_bulk(&mut stream, bytes).await?;
        match bulk_reply(&mut stream).await? {
            Reply::BulkQueued(deliveries) => Ok(deliveries),
            _ => Err(ClientError::UnexpectedResponse),
        }
    })
    .await
    .map_err(|_| ClientError::Timeout)?
}

pub(super) async fn download(
    endpoint: &Endpoint,
    command: Command,
) -> Result<Option<(Media, Zeroizing<Vec<u8>>)>, ClientError> {
    use crate::ipc::network::{HistoryCommand, MAX_HISTORY_PREVIEW_BYTES};
    let preview = matches!(command, Command::History(HistoryCommand::Preview { .. }));
    timeout(BULK_TIMEOUT, async {
        let mut stream = connect(endpoint).await?;
        send(&mut stream, &Request::Network(command)).await?;
        match bulk_reply(&mut stream).await? {
            Reply::BulkItem { media, size } => {
                if preview
                    && (size as usize > MAX_HISTORY_PREVIEW_BYTES
                        || !matches!(media, Media::Text | Media::Png))
                {
                    return Err(ClientError::UnexpectedResponse);
                }
                Ok(Some((media, read_bulk(&mut stream, media, size).await?)))
            }
            Reply::NoBulkItem => Ok(None),
            _ => Err(ClientError::UnexpectedResponse),
        }
    })
    .await
    .map_err(|_| ClientError::Timeout)?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn verifies_kernel_credentials_and_rejects_a_different_uid() {
        let (client, daemon) = UnixStream::pair().unwrap();
        let uid = rustix::process::geteuid().as_raw();

        authenticate(&client).unwrap();
        authenticate(&daemon).unwrap();
        assert_eq!(
            check_peer(&daemon, uid ^ 1).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }
}
