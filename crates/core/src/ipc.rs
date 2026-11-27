//! Transport-independent daemon operations used by the CLI and desktop shell.

use std::io;

use lyteboard_protocol::ipc::{DecodeError, ErrorCode, Request, Response};
pub use lyteboard_protocol::ipc::{Status, network};
use thiserror::Error;

#[cfg(any(target_os = "linux", target_os = "windows"))]
mod framing;
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub mod lifecycle;
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(any(target_os = "linux", target_os = "windows"))]
mod transport;
#[cfg(target_os = "windows")]
pub mod windows;
#[cfg(target_os = "linux")]
pub use linux as local;
#[cfg(target_os = "windows")]
pub use windows as local;

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("daemon is offline. Start it with `lyteboard daemon start`")]
    Offline,
    #[error("daemon IPC is supported on Linux and Windows")]
    UnsupportedPlatform,
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    #[error(transparent)]
    Endpoint(#[from] local::EndpointError),
    #[error("could not connect to the Lyteboard socket: {0}")]
    Connect(#[source] io::Error),
    #[error("could not authenticate the Lyteboard daemon: {0}")]
    Authentication(#[source] io::Error),
    #[error("daemon IPC timed out")]
    Timeout,
    #[error("daemon IPC failed: {0}")]
    Io(#[from] io::Error),
    #[error("daemon returned an invalid frame length")]
    FrameLength,
    #[error(transparent)]
    Decode(#[from] DecodeError),
    #[error("daemon rejected the request: {0}")]
    Rejected(ErrorCode),
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    #[error("{0}")]
    Bulk(String),
    #[error("daemon returned an unexpected response")]
    UnexpectedResponse,
}

pub struct Client {
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    endpoint: local::Endpoint,
}

impl Client {
    pub fn from_env() -> Result<Self, ClientError> {
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        return Ok(Self {
            endpoint: local::Endpoint::from_env()?,
        });

        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        Err(ClientError::UnsupportedPlatform)
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    pub fn in_runtime_dir(path: impl Into<std::path::PathBuf>) -> Result<Self, ClientError> {
        Ok(Self {
            endpoint: local::Endpoint::new(path)?,
        })
    }

    pub async fn ping(&self) -> Result<(), ClientError> {
        match self.request(Request::Ping).await? {
            Response::Pong => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn status(&self) -> Result<Status, ClientError> {
        match self.request(Request::GetStatus).await? {
            Response::Status(status) => Ok(status),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn version(&self) -> Result<String, ClientError> {
        match self.request(Request::GetVersion).await? {
            Response::Version(version) => Ok(version),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn shutdown(&self) -> Result<(), ClientError> {
        match self.request(Request::Shutdown).await? {
            Response::ShuttingDown => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn network(&self, command: network::Command) -> Result<network::Reply, ClientError> {
        match self.request(Request::Network(command)).await? {
            Response::Network(reply) => Ok(reply),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    pub async fn upload_bulk(
        &self,
        peers: Vec<String>,
        media: lyteboard_protocol::peer::Media,
        bytes: &[u8],
    ) -> Result<Vec<network::QueueOutcome>, ClientError> {
        local::upload(&self.endpoint, peers, media, bytes).await
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    pub async fn download_bulk(
        &self,
        offset: usize,
    ) -> Result<Option<(lyteboard_protocol::peer::Media, zeroize::Zeroizing<Vec<u8>>)>, ClientError>
    {
        local::download(&self.endpoint, network::Command::BulkDownload { offset }).await
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    pub async fn download_received(
        &self,
        id: lyteboard_protocol::peer::TransferId,
    ) -> Result<Option<(lyteboard_protocol::peer::Media, zeroize::Zeroizing<Vec<u8>>)>, ClientError>
    {
        local::download(
            &self.endpoint,
            network::Command::BulkDownloadReceived { id },
        )
        .await
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    pub async fn download_history(
        &self,
        id: i64,
    ) -> Result<Option<(lyteboard_protocol::peer::Media, zeroize::Zeroizing<Vec<u8>>)>, ClientError>
    {
        local::download(
            &self.endpoint,
            network::Command::History(network::HistoryCommand::Restore { id }),
        )
        .await
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    pub async fn download_history_preview(
        &self,
        id: i64,
    ) -> Result<Option<(lyteboard_protocol::peer::Media, zeroize::Zeroizing<Vec<u8>>)>, ClientError>
    {
        local::download(
            &self.endpoint,
            network::Command::History(network::HistoryCommand::Preview { id }),
        )
        .await
    }

    async fn request(&self, request: Request) -> Result<Response, ClientError> {
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        return local::request(&self.endpoint, request).await;

        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            let _ = request;
            Err(ClientError::UnsupportedPlatform)
        }
    }
}
