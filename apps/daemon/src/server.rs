use std::{
    collections::BTreeSet,
    io, process,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use lyteboard_core::bulk::{Payload, validate_size};
use lyteboard_core::ipc::local::{
    BULK_TIMEOUT, Endpoint, EndpointError, FrameError, REQUEST_TIMEOUT, ServerSocket, authenticate,
    read_bulk, receive, send, write_bulk,
};
use lyteboard_protocol::{
    ipc::{
        self, DecodeError, ErrorCode, Request, Response, Status,
        network::{Command, HistoryCommand, MAX_HISTORY_PREVIEW_BYTES, MAX_RECIPIENTS, Reply},
    },
    peer::{Media, NoiseKey},
};
use thiserror::Error;
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore},
    task::JoinSet,
    time::timeout,
};

use crate::{
    history::{History, Operation as HistoryOperation},
    network::{Network, profile_path},
    preferences::{HistorySettings, Preferences},
};

#[cfg(target_os = "windows")]
use lyteboard_core::ipc::windows::LocalStream;
#[cfg(target_os = "linux")]
use tokio::{
    net::UnixStream as LocalStream,
    signal::unix::{SignalKind, signal},
};

const MAX_CLIENTS: usize = 32;

#[derive(Debug, Error)]
pub enum DaemonError {
    #[error("could not start the LAN worker: {0}")]
    Network(#[source] io::Error),
    #[error("could not start the history worker: {0}")]
    History(#[source] io::Error),
    #[error(transparent)]
    Endpoint(#[from] EndpointError),
    #[cfg(target_os = "linux")]
    #[error("could not register signal handlers: {0}")]
    Signals(#[source] io::Error),
    #[error("could not accept a local client: {0}")]
    Accept(#[source] io::Error),
}

#[tokio::main(flavor = "current_thread")]
pub async fn run() -> Result<(), DaemonError> {
    #[cfg(target_os = "linux")]
    let mut interrupt = signal(SignalKind::interrupt()).map_err(DaemonError::Signals)?;
    #[cfg(target_os = "linux")]
    let mut terminate = signal(SignalKind::terminate()).map_err(DaemonError::Signals)?;
    #[cfg(target_os = "linux")]
    let shutdown = async move {
        tokio::select! { _ = interrupt.recv() => (), _ = terminate.recv() => () }
    };
    #[cfg(target_os = "windows")]
    let shutdown = async {
        if tokio::signal::ctrl_c().await.is_err() {
            // A detached per-user daemon may have no console. IPC shutdown still works.
            std::future::pending::<()>().await;
        }
    };
    tokio::pin!(shutdown);
    let endpoint = Endpoint::from_env()?;
    let server = ServerSocket::bind(&endpoint).await?;
    let profile = profile_path();
    let preferences = Preferences::load(profile.clone());
    let history =
        History::start(profile.clone(), preferences.clone()).map_err(DaemonError::History)?;
    let (network, network_done) =
        Network::start(profile, history.clone(), preferences).map_err(DaemonError::Network)?;
    let started = Instant::now();
    let connections_accepted = Arc::new(AtomicU64::new(0));
    let mut handlers = JoinSet::new();
    let bulk_slots = Arc::new(Semaphore::new(2));
    eprintln!("Lyteboard daemon ready (PID {})", process::id());

    let shutdown_client = loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => break None,
            Some(result) = handlers.join_next(), if !handlers.is_empty() => {
                match result {
                    Ok(Some(stream)) => break Some(stream),
                    Ok(None) => (),
                    Err(error) => eprintln!("Lyteboard IPC handler failed: {error}"),
                }
            }
            result = server.accept(), if handlers.len() < MAX_CLIENTS => {
                let stream = match result {
                    Ok(stream) => stream,
                    Err(error) if matches!(error.kind(), io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted) => continue,
                    Err(error) => return Err(DaemonError::Accept(error)),
                };

                if authenticate(&stream).is_ok() {
                    connections_accepted.fetch_add(1, Ordering::Relaxed);
                    handlers.spawn(handle(stream, started, Arc::clone(&connections_accepted), network.clone(), history.clone(), Arc::clone(&bulk_slots)));
                }
            }
        }
    };

    handlers.shutdown().await;
    drop(network);
    let _ = timeout(Duration::from_millis(500), network_done).await;
    drop(history);
    drop(server);
    if let Some(mut stream) = shutdown_client {
        // Acknowledgement means the listener and its socket path are already gone.
        let _ = timeout(REQUEST_TIMEOUT, send(&mut stream, &Response::ShuttingDown)).await;
    }

    Ok(())
}

async fn handle(
    mut stream: LocalStream,
    started: Instant,
    connections_accepted: Arc<AtomicU64>,
    network: Network,
    history: History,
    bulk_slots: Arc<Semaphore>,
) -> Option<LocalStream> {
    let response = match timeout(REQUEST_TIMEOUT, receive::<Request>(&mut stream)).await {
        Ok(Ok(Request::Ping)) => Response::Pong,
        Ok(Ok(Request::GetStatus)) => Response::Status(Status {
            running: true,
            version: lyteboard_core::VERSION.to_owned(),
            pid: process::id(),
            uptime_seconds: started.elapsed().as_secs(),
            protocol_version: ipc::VERSION,
            connections_accepted: connections_accepted.load(Ordering::Relaxed),
        }),
        Ok(Ok(Request::GetVersion)) => Response::Version(lyteboard_core::VERSION.to_owned()),
        Ok(Ok(Request::Shutdown)) => return Some(stream),
        Ok(Ok(Request::Network(Command::BulkUpload { peers, media, size }))) => {
            let reply = match bulk_slots.try_acquire_owned() {
                Ok(slot) => upload(&mut stream, &network, peers, media, size, slot).await,
                Err(_) => Reply::Error("Local bulk transfers are busy.".into()),
            };
            Response::Network(reply)
        }
        Ok(Ok(Request::Network(Command::History(HistoryCommand::Preview { id })))) => {
            let Ok(_slot) = bulk_slots.try_acquire_owned() else {
                let reply =
                    Response::Network(Reply::Error("Local bulk transfers are busy.".into()));
                let _ = timeout(REQUEST_TIMEOUT, send(&mut stream, &reply)).await;
                return None;
            };
            let _ = timeout(BULK_TIMEOUT, async {
                let item = history.preview(id).await.and_then(|(media, bytes)| {
                    if bytes.len() <= MAX_HISTORY_PREVIEW_BYTES {
                        Ok((media, bytes))
                    } else {
                        Err("History preview exceeds its size limit.".into())
                    }
                });
                match item {
                    Ok((media, bytes)) => {
                        let reply = Response::Network(Reply::BulkItem {
                            media,
                            size: bytes.len() as u32,
                        });
                        send(&mut stream, &reply).await?;
                        write_bulk(&mut stream, &bytes).await
                    }
                    Err(error) => send(&mut stream, &Response::Network(Reply::Error(error))).await,
                }
            })
            .await;
            return None;
        }
        Ok(Ok(Request::Network(
            command @ (Command::BulkDownload { .. }
            | Command::BulkDownloadReceived { .. }
            | Command::History(HistoryCommand::Restore { .. })),
        ))) => {
            let Ok(_slot) = bulk_slots.try_acquire_owned() else {
                let reply =
                    Response::Network(Reply::Error("Local bulk transfers are busy.".into()));
                let _ = timeout(REQUEST_TIMEOUT, send(&mut stream, &reply)).await;
                return None;
            };
            let _ = timeout(BULK_TIMEOUT, async {
                let result = match command {
                    Command::BulkDownload { offset } => network.download(offset).await,
                    Command::BulkDownloadReceived { id } => network.download_received(id).await,
                    Command::History(HistoryCommand::Restore { id }) => history
                        .load(id)
                        .await
                        .map(|payload| Some(Arc::new(payload))),
                    _ => unreachable!(),
                };
                let prepared = match result {
                    Ok(Some(item)) => lyteboard_core::bulk::clipboard_payload(item)
                        .await
                        .map(Some)
                        .map_err(|error| error.to_string()),
                    other => other,
                };
                let (reply, item) = match prepared {
                    Ok(Some(item)) => {
                        network.mark_clipboard_write(&item);
                        (
                            Reply::BulkItem {
                                media: item.media(),
                                size: item.bytes().len() as u32,
                            },
                            Some(item),
                        )
                    }
                    Ok(None) => (Reply::NoBulkItem, None),
                    Err(error) => (Reply::Error(error), None),
                };
                send(&mut stream, &Response::Network(reply)).await?;
                if let Some(item) = item {
                    write_bulk(&mut stream, item.bytes()).await?;
                }
                Ok::<(), io::Error>(())
            })
            .await;
            return None;
        }
        Ok(Ok(Request::Network(Command::History(command)))) => {
            let reply = match command {
                HistoryCommand::Status => history.control(HistoryOperation::Status).await,
                HistoryCommand::Configuration => {
                    history.control(HistoryOperation::Configuration).await
                }
                HistoryCommand::Configure { enabled, limits } => {
                    history
                        .control(HistoryOperation::Configure(HistorySettings {
                            enabled,
                            limits,
                        }))
                        .await
                }
                HistoryCommand::List { before } => {
                    history.control(HistoryOperation::List(before, None)).await
                }
                HistoryCommand::Search { before, query } => {
                    history
                        .control(HistoryOperation::List(before, Some(query)))
                        .await
                }
                HistoryCommand::Delete { id } => {
                    history.control(HistoryOperation::Delete(id)).await
                }
                HistoryCommand::Clear => history.control(HistoryOperation::Clear).await,
                HistoryCommand::Restore { .. } | HistoryCommand::Preview { .. } => unreachable!(),
                HistoryCommand::Resend { id, peers } => {
                    let operation = async {
                        let peers = parse_peers(peers)?;
                        let slot = bulk_slots
                            .try_acquire_owned()
                            .map_err(|_| "Local bulk transfers are busy.")?;
                        let payload = history.load(id).await?;
                        Ok(network.upload(peers, payload, slot).await)
                    };
                    match timeout(REQUEST_TIMEOUT - Duration::from_millis(250), operation).await {
                        Ok(Ok(reply)) => reply,
                        Ok(Err(error)) => Reply::Error(error),
                        Err(_) => {
                            Reply::Error("History resend queue confirmation timed out.".into())
                        }
                    }
                }
            };
            Response::Network(reply)
        }
        Ok(Ok(Request::Network(command))) => Response::Network(network.request(command).await),
        Ok(Err(FrameError::Length(0) | FrameError::Decode(DecodeError::Invalid(_)))) => {
            Response::Error(ErrorCode::InvalidRequest)
        }
        Ok(Err(FrameError::Length(_))) => Response::Error(ErrorCode::FrameTooLarge),
        Ok(Err(FrameError::Decode(DecodeError::UnsupportedVersion(_)))) => {
            Response::Error(ErrorCode::UnsupportedVersion)
        }
        Ok(Err(FrameError::Io(_))) | Err(_) => return None,
    };

    let _ = timeout(REQUEST_TIMEOUT, send(&mut stream, &response)).await;
    None
}

async fn upload(
    stream: &mut LocalStream,
    network: &Network,
    peers: Vec<String>,
    media: Media,
    size: u32,
    slot: OwnedSemaphorePermit,
) -> Reply {
    let operation = async {
        validate_size(media, size as usize).map_err(|error| error.to_string())?;
        let peers = parse_peers(peers)?;

        send(stream, &Response::Network(Reply::BulkReady))
            .await
            .map_err(|_| "Local bulk stream closed.")?;
        let bytes = read_bulk(stream, media, size)
            .await
            .map_err(|_| "Local bulk stream was truncated or exceeded its declared length.")?;
        let payload = Payload::validate(media, bytes, None)
            .await
            .map_err(|error| error.to_string())?;
        Ok(network.upload(peers, payload, slot).await)
    };
    match timeout(BULK_TIMEOUT, operation).await {
        Ok(Ok(reply)) => reply,
        Ok(Err(error)) => Reply::Error(error),
        Err(_) => Reply::Error("Local bulk upload timed out.".into()),
    }
}

fn parse_peers(peers: Vec<String>) -> Result<Vec<NoiseKey>, String> {
    if peers.is_empty() || peers.len() > MAX_RECIPIENTS {
        return Err("Specify one to 32 paired devices.".into());
    }
    let mut unique = BTreeSet::new();
    peers
        .into_iter()
        .map(|id| {
            let mut peer: NoiseKey = [0; 32];
            if hex::decode_to_slice(id, &mut peer).is_err() || !unique.insert(peer) {
                return Err("Invalid or duplicate device ID.".into());
            }
            Ok(peer)
        })
        .collect()
}
