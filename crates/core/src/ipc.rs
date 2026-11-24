//! Transport-independent daemon operations used by the CLI and desktop shell.

use std::io;

use lyteboard_protocol::ipc::{DecodeError, ErrorCode, Request, Response};
pub use lyteboard_protocol::ipc::{Status, network};
use thiserror::Error;

#[cfg(target_os = "linux")]
pub mod linux;

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("daemon is offline. Start it with `lyteboard daemon start`")]
    Offline,
    #[error("daemon IPC is currently supported on Linux")]
    UnsupportedPlatform,
    #[cfg(target_os = "linux")]
    #[error(transparent)]
    Endpoint(#[from] linux::EndpointError),
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
    #[cfg(target_os = "linux")]
    #[error("{0}")]
    Bulk(String),
    #[error("daemon returned an unexpected response")]
    UnexpectedResponse,
}

pub struct Client {
    #[cfg(target_os = "linux")]
    endpoint: linux::Endpoint,
}

impl Client {
    pub fn from_env() -> Result<Self, ClientError> {
        #[cfg(target_os = "linux")]
        return Ok(Self {
            endpoint: linux::Endpoint::from_env()?,
        });

        #[cfg(not(target_os = "linux"))]
        Err(ClientError::UnsupportedPlatform)
    }

    #[cfg(target_os = "linux")]
    pub fn in_runtime_dir(path: impl Into<std::path::PathBuf>) -> Result<Self, ClientError> {
        Ok(Self {
            endpoint: linux::Endpoint::new(path)?,
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

    #[cfg(target_os = "linux")]
    pub async fn upload_bulk(
        &self,
        peers: Vec<String>,
        media: lyteboard_protocol::peer::Media,
        bytes: &[u8],
    ) -> Result<Vec<network::QueueOutcome>, ClientError> {
        linux::upload(&self.endpoint, peers, media, bytes).await
    }

    #[cfg(target_os = "linux")]
    pub async fn download_bulk(
        &self,
        offset: usize,
    ) -> Result<Option<(lyteboard_protocol::peer::Media, zeroize::Zeroizing<Vec<u8>>)>, ClientError>
    {
        linux::download(&self.endpoint, network::Command::BulkDownload { offset }).await
    }

    #[cfg(target_os = "linux")]
    pub async fn download_history(
        &self,
        id: i64,
    ) -> Result<Option<(lyteboard_protocol::peer::Media, zeroize::Zeroizing<Vec<u8>>)>, ClientError>
    {
        linux::download(
            &self.endpoint,
            network::Command::History(network::HistoryCommand::Restore { id }),
        )
        .await
    }

    async fn request(&self, request: Request) -> Result<Response, ClientError> {
        #[cfg(target_os = "linux")]
        return linux::request(&self.endpoint, request).await;

        #[cfg(not(target_os = "linux"))]
        {
            let _ = request;
            Err(ClientError::UnsupportedPlatform)
        }
    }
}
