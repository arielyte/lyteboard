use lyteboard_core::{
    bulk::{Payload, Receiver, Sender},
    peer::{SessionEvent, lan::TcpSession},
};
use lyteboard_protocol::peer::{BulkRecord, PeerFailure, TransferId};
use tokio::{
    sync::{OwnedSemaphorePermit, mpsc},
    time::{Instant, sleep_until, timeout_at},
};

use super::{Duration, Engine, Job};

const PROGRESS: Duration = Duration::from_secs(10);
const TOTAL: Duration = Duration::from_secs(60);
const SESSION: Duration = Duration::from_secs(3600);

struct Download {
    receiver: Receiver,
    _permit: OwnedSemaphorePermit,
    expires: Instant,
    progress: Instant,
}

struct Upload {
    id: TransferId,
    payload: Payload,
    sender: Sender,
    _permit: OwnedSemaphorePermit,
    awaiting_ack: bool,
    expires: Instant,
    progress: Instant,
}

pub(super) async fn run(
    engine: &Engine,
    mut session: TcpSession<'_>,
    mut jobs: mpsc::Receiver<Job>,
) {
    let peer = session.peer_key();
    let expires = Instant::now() + SESSION;
    let mut upload: Option<Upload> = None;
    let mut download: Option<Download> = None;
    loop {
        let deadline = expires
            .min(
                upload
                    .as_ref()
                    .map_or(expires, |flow| flow.progress.min(flow.expires)),
            )
            .min(
                download
                    .as_ref()
                    .map_or(expires, |flow| flow.progress.min(flow.expires)),
            );
        let event = tokio::select! {
            biased;
            _ = sleep_until(deadline) => return,
            result = session.wait_for_input() => {
                if result.is_err() { return; }
                match timeout_at(deadline, session.receive()).await {
                    Ok(Ok(event)) => event,
                    _ => return,
                }
            }
            job = jobs.recv(), if upload.is_none() => {
                let Some(job) = job else { return };
                let payload = match timeout_at(deadline, Payload::validate(job.kind, job.bytes, None)).await {
                    Ok(Ok(payload)) => payload,
                    Ok(Err(error)) => {
                        engine.model().finish_send(peer, &error.to_string());
                        continue;
                    }
                    Err(_) => return,
                };
                let Ok(id) = TcpSession::transfer_id() else { return };
                if session.send_bulk(BulkRecord::Begin(payload.manifest(id))).await.is_err() { return; }
                upload = Some(Upload {
                    id, payload, sender: Sender::default(), _permit: job.permit,
                    awaiting_ack: false, expires: Instant::now() + TOTAL, progress: Instant::now() + PROGRESS,
                });
                continue;
            }
        };
        let result = match event {
            SessionEvent::Text { id, text } => match Payload::text(text) {
                Ok(payload) if engine.is_trusted(peer) => {
                    if !engine.receive(peer, payload) {
                        return;
                    }
                    session.acknowledge(id).await
                }
                _ => return,
            },
            SessionEvent::Bulk(BulkRecord::Credit { id, next }) => {
                let Some(flow) = upload.as_mut().filter(|flow| !flow.awaiting_ack) else {
                    return;
                };
                let Ok(record) = flow.sender.credit(flow.id, &flow.payload, id, next) else {
                    return;
                };
                flow.awaiting_ack = matches!(record, BulkRecord::End { .. });
                flow.progress = Instant::now() + PROGRESS;
                session.send_bulk(record).await
            }
            SessionEvent::Bulk(BulkRecord::Begin(manifest)) => {
                let slot = std::sync::Arc::clone(&engine.incoming).try_acquire_owned();
                match slot {
                    Ok(permit) if download.is_none() => match Receiver::new(manifest) {
                        Ok(receiver) => {
                            download = Some(Download {
                                receiver,
                                _permit: permit,
                                expires: Instant::now() + TOTAL,
                                progress: Instant::now() + PROGRESS,
                            });
                            session
                                .send_bulk(BulkRecord::Credit {
                                    id: manifest.id,
                                    next: 0,
                                })
                                .await
                        }
                        Err(error) => {
                            session
                                .reject_payload(manifest.id, error.peer_failure())
                                .await
                        }
                    },
                    _ => session.reject_payload(manifest.id, PeerFailure::Busy).await,
                }
            }
            SessionEvent::Bulk(BulkRecord::Chunk { id, index, bytes }) => {
                let Some(flow) = download.as_mut() else {
                    return;
                };
                let Ok(next) = flow.receiver.chunk(id, index, &bytes) else {
                    return;
                };
                flow.progress = Instant::now() + PROGRESS;
                session.send_bulk(BulkRecord::Credit { id, next }).await
            }
            SessionEvent::Bulk(BulkRecord::End { id }) => {
                let Some(flow) = download.take() else { return };
                match timeout_at(deadline, flow.receiver.finish(id)).await {
                    Ok(Ok(payload)) if session.ensure_active().is_ok() => {
                        if !engine.receive(peer, payload) {
                            return;
                        }
                        session.acknowledge(id).await
                    }
                    Ok(Err(error)) => session.reject_payload(id, error.peer_failure()).await,
                    _ => return,
                }
            }
            SessionEvent::DeliveryAck { id, .. }
                if upload
                    .as_ref()
                    .is_some_and(|flow| flow.id == id && flow.awaiting_ack) =>
            {
                engine.model().finish_send(peer, "Delivered.");
                upload = None;
                continue;
            }
            SessionEvent::PeerError { id: Some(id), .. }
                if upload.as_ref().is_some_and(|flow| flow.id == id) =>
            {
                engine
                    .model()
                    .finish_send(peer, "The peer rejected the transfer.");
                upload = None;
                continue;
            }
            _ => return,
        };
        if result.is_err() {
            return;
        }
    }
}
