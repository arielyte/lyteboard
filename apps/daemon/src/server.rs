use std::{
    io, process,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use lyteboard_core::ipc::linux::{
    Endpoint, EndpointError, FrameError, REQUEST_TIMEOUT, ServerSocket, authenticate, receive, send,
};
use lyteboard_protocol::ipc::{self, DecodeError, ErrorCode, Request, Response, Status};
use thiserror::Error;
use tokio::{
    net::UnixStream,
    signal::unix::{SignalKind, signal},
    task::JoinSet,
    time::timeout,
};

use crate::network::Network;

const MAX_CLIENTS: usize = 32;

#[derive(Debug, Error)]
pub enum DaemonError {
    #[error("could not start the LAN worker: {0}")]
    Network(#[source] io::Error),
    #[error(transparent)]
    Endpoint(#[from] EndpointError),
    #[error("could not register signal handlers: {0}")]
    Signals(#[source] io::Error),
    #[error("could not accept a local client: {0}")]
    Accept(#[source] io::Error),
}

#[tokio::main(flavor = "current_thread")]
pub async fn run() -> Result<(), DaemonError> {
    let mut interrupt = signal(SignalKind::interrupt()).map_err(DaemonError::Signals)?;
    let mut terminate = signal(SignalKind::terminate()).map_err(DaemonError::Signals)?;
    let endpoint = Endpoint::from_env()?;
    let server = ServerSocket::bind(&endpoint).await?;
    let (network, network_done) = Network::start().map_err(DaemonError::Network)?;
    let started = Instant::now();
    let connections_accepted = Arc::new(AtomicU64::new(0));
    let mut handlers = JoinSet::new();
    eprintln!("Lyteboard daemon ready (PID {})", process::id());

    let shutdown_client = loop {
        tokio::select! {
            biased;
            _ = interrupt.recv() => break None,
            _ = terminate.recv() => break None,
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
                    handlers.spawn(handle(stream, started, Arc::clone(&connections_accepted), network.clone()));
                }
            }
        }
    };

    handlers.shutdown().await;
    drop(network);
    let _ = timeout(Duration::from_millis(500), network_done).await;
    drop(server);
    if let Some(mut stream) = shutdown_client {
        // Acknowledgement means the listener and its socket path are already gone.
        let _ = timeout(REQUEST_TIMEOUT, send(&mut stream, &Response::ShuttingDown)).await;
    }

    Ok(())
}

async fn handle(
    mut stream: UnixStream,
    started: Instant,
    connections_accepted: Arc<AtomicU64>,
    network: Network,
) -> Option<UnixStream> {
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
