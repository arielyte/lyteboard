//! Linux transport shared by the daemon and its clients.

mod endpoint;

use std::{io, time::Duration};

pub use endpoint::{Endpoint, EndpointError, ServerSocket};
use lyteboard_protocol::ipc::{self, DecodeError, Request, Response};
use serde::{Serialize, de::DeserializeOwned};
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    time::timeout,
};

use super::ClientError;

pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
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
    let path = endpoint.existing_socket()?.ok_or(ClientError::Offline)?;

    timeout(CLIENT_TIMEOUT, async {
        let mut stream = UnixStream::connect(path)
            .await
            .map_err(|error| match error.kind() {
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => ClientError::Offline,
                _ => ClientError::Connect(error),
            })?;
        authenticate(&stream).map_err(ClientError::Authentication)?;
        send(&mut stream, &request).await?;

        match receive(&mut stream).await {
            Ok(Response::Error(code)) => Err(ClientError::Rejected(code)),
            Ok(response) => Ok(response),
            Err(FrameError::Decode(error)) => Err(error.into()),
            Err(FrameError::Io(error)) => Err(error.into()),
            Err(FrameError::Length(_)) => Err(ClientError::FrameLength),
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
