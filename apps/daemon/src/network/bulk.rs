//! Daemon transfer resource accounting and flow control.

use std::{collections::VecDeque, sync::Arc, time::Duration};

use lyteboard_core::bulk::{Error, Payload, Receiver};
use lyteboard_protocol::{
    ipc::network::{MAX_TEST_TEXT_BYTES, Message},
    peer::{BulkRecord, CHUNK_BYTES, MAX_IMAGE_BYTES, Manifest, NoiseKey, TransferId},
};
use tokio::{sync::OwnedSemaphorePermit, time::Instant};
use zeroize::Zeroizing;

const INBOX_LIMIT: usize = 16;
const INBOX_BYTES: usize = 2 * MAX_IMAGE_BYTES;
pub(super) const PROGRESS_TIMEOUT: Duration = Duration::from_secs(10);
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Default)]
pub(super) struct Inbox {
    entries: VecDeque<(NoiseKey, Arc<Payload>)>,
    bytes: usize,
}

impl Inbox {
    pub fn push(&mut self, peer: NoiseKey, payload: Arc<Payload>) {
        let size = payload.bytes().len();
        while self.entries.len() == INBOX_LIMIT || self.bytes + size > INBOX_BYTES {
            let (_, old) = self
                .entries
                .pop_back()
                .expect("payload fits an empty inbox");
            self.bytes -= old.bytes().len();
        }
        self.bytes += size;
        self.entries.push_front((peer, payload));
    }

    pub fn payload(&self, offset: usize) -> Option<Arc<Payload>> {
        self.entries
            .get(offset)
            .map(|(_, payload)| Arc::clone(payload))
    }

    pub fn message(&self, offset: usize) -> Option<Message> {
        let (peer, payload) = self.entries.get(offset)?;
        let text = if let Some(text) = payload
            .as_text()
            .filter(|text| text.len() <= MAX_TEST_TEXT_BYTES)
        {
            text.to_owned()
        } else {
            format!(
                "[{}, {} bytes. Use clipboard copy.]",
                payload.media().mime(),
                payload.bytes().len()
            )
        };
        Some(Message { peer: *peer, text })
    }
}

pub(super) struct Batch {
    pub payload: Arc<Payload>,
    pub _slot: OwnedSemaphorePermit,
}

pub(super) struct Incoming {
    receiver: Receiver,
    _slot: OwnedSemaphorePermit,
    expires: Instant,
    progress: Instant,
}

impl Incoming {
    pub fn new(manifest: Manifest, slot: OwnedSemaphorePermit) -> Result<Self, Error> {
        let now = Instant::now();
        Ok(Self {
            receiver: Receiver::new(manifest)?,
            _slot: slot,
            expires: now + TRANSFER_TIMEOUT,
            progress: now + PROGRESS_TIMEOUT,
        })
    }

    pub fn deadline(&self) -> Instant {
        self.expires.min(self.progress)
    }

    pub fn chunk(&mut self, id: TransferId, index: u32, bytes: &[u8]) -> Result<u32, Error> {
        let next = self.receiver.chunk(id, index, bytes)?;
        self.progress = Instant::now() + PROGRESS_TIMEOUT;
        Ok(next)
    }

    pub async fn finish(self, id: TransferId) -> Result<(Payload, OwnedSemaphorePermit), Error> {
        Ok((self.receiver.finish(id).await?, self._slot))
    }
}

pub(super) struct Outgoing {
    pub id: TransferId,
    pub job: u64,
    pub expires: Instant,
    progress: Instant,
    offset: usize,
    next: u32,
    batch: Arc<Batch>,
}

impl Outgoing {
    pub fn new(id: TransferId, job: u64, batch: Arc<Batch>) -> Self {
        let now = Instant::now();
        Self {
            id,
            job,
            batch,
            offset: 0,
            next: 0,
            expires: now + TRANSFER_TIMEOUT,
            progress: now + PROGRESS_TIMEOUT,
        }
    }

    pub fn manifest(&self) -> Manifest {
        self.batch.payload.manifest(self.id)
    }

    pub fn into_batch(self) -> Arc<Batch> {
        self.batch
    }

    pub fn deadline(&self) -> Instant {
        self.expires.min(self.progress)
    }

    pub fn credit(&mut self, id: TransferId, next: u32) -> Result<BulkRecord, Error> {
        if id != self.id || next != self.next {
            return Err(Error::Sequence);
        }
        let bytes = self.batch.payload.bytes();
        if self.offset == bytes.len() {
            return Ok(BulkRecord::End { id });
        }
        let end = (self.offset + CHUNK_BYTES).min(bytes.len());
        let chunk = Zeroizing::new(bytes[self.offset..end].to_vec());
        self.offset = end;
        self.next += 1;
        self.progress = Instant::now() + PROGRESS_TIMEOUT;
        Ok(BulkRecord::Chunk {
            id,
            index: next,
            bytes: chunk,
        })
    }
}
