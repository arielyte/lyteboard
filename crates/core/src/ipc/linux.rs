//! Linux transport shared by the daemon and its clients.

mod endpoint;

use std::{io, time::Duration};

pub use endpoint::{Endpoint, EndpointError, ServerSocket};
use tokio::net::UnixStream;
pub type LocalStream = UnixStream;
use super::ClientError;
pub(super) use super::transport::{download, request, upload};

pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
pub const BULK_TIMEOUT: Duration = Duration::from_secs(20);

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

pub use super::framing::{FrameError, read_bulk, receive, send, write_bulk};

pub(super) async fn connect(endpoint: &Endpoint) -> Result<UnixStream, ClientError> {
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
