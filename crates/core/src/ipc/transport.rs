use super::{
    ClientError,
    framing::{FrameError, read_bulk, receive, send, write_bulk},
    local::{BULK_TIMEOUT, Endpoint, connect},
};
use lyteboard_protocol::{
    ipc::{
        Request, Response,
        network::{Command, QueueOutcome, Reply},
    },
    peer::Media,
};
use std::time::Duration;
use tokio::time::timeout;
use zeroize::Zeroizing;
const CLIENT_TIMEOUT: Duration = Duration::from_secs(3);
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

async fn response(
    stream: &mut (impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin),
) -> Result<Response, ClientError> {
    match receive(stream).await {
        Ok(Response::Error(code)) => Err(ClientError::Rejected(code)),
        Ok(response) => Ok(response),
        Err(FrameError::Decode(error)) => Err(error.into()),
        Err(FrameError::Io(error)) => Err(error.into()),
        Err(FrameError::Length(_)) => Err(ClientError::FrameLength),
    }
}

async fn bulk_reply(
    stream: &mut (impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin),
) -> Result<Reply, ClientError> {
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
