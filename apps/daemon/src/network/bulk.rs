//! Daemon transfer resource accounting and flow control.

use std::{collections::VecDeque, sync::Arc, time::Duration};

use lyteboard_core::bulk::{Error, Payload, Receiver, Sender};
use lyteboard_protocol::{
    ipc::network::{MAX_TEST_TEXT_BYTES, Message, ReceivedItem},
    peer::{BulkRecord, MAX_IMAGE_BYTES, Manifest, NoiseKey, TransferId},
};
use tokio::{sync::OwnedSemaphorePermit, time::Instant};

const INBOX_LIMIT: usize = 16;
const INBOX_BYTES: usize = 2 * MAX_IMAGE_BYTES;
pub(super) const PROGRESS_TIMEOUT: Duration = Duration::from_secs(10);
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Default)]
pub(super) struct Inbox {
    entries: VecDeque<Entry>,
    bytes: usize,
}

struct Entry {
    id: Option<TransferId>,
    peer: NoiseKey,
    payload: Arc<Payload>,
}

impl Inbox {
    pub fn push(&mut self, peer: NoiseKey, payload: Arc<Payload>) {
        let size = payload.bytes().len();
        while self.entries.len() == INBOX_LIMIT || self.bytes + size > INBOX_BYTES {
            let old = self
                .entries
                .pop_back()
                .expect("payload fits an empty inbox");
            self.bytes -= old.payload.bytes().len();
        }
        self.bytes += size;
        let mut id = [0; 16];
        let id = getrandom::fill(&mut id).ok().map(|()| id);
        self.entries.push_front(Entry { id, peer, payload });
    }

    pub fn payload(&self, offset: usize) -> Option<Arc<Payload>> {
        self.entries
            .get(offset)
            .map(|entry| Arc::clone(&entry.payload))
    }

    pub fn latest(&self) -> Option<ReceivedItem> {
        let entry = self.entries.front()?;
        Some(ReceivedItem {
            id: entry.id?,
            peer: entry.peer,
            media: entry.payload.media(),
            size: entry.payload.bytes().len() as u32,
        })
    }

    pub fn by_id(&self, id: TransferId) -> Option<Arc<Payload>> {
        self.entries
            .iter()
            .find(|entry| entry.id == Some(id))
            .map(|entry| Arc::clone(&entry.payload))
    }

    pub fn message(&self, offset: usize) -> Option<Message> {
        let Entry { peer, payload, .. } = self.entries.get(offset)?;
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
    sender: Sender,
    batch: Arc<Batch>,
}

impl Outgoing {
    pub fn new(id: TransferId, job: u64, batch: Arc<Batch>) -> Self {
        let now = Instant::now();
        Self {
            id,
            job,
            batch,
            sender: Sender::default(),
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
        let record = self.sender.credit(self.id, &self.batch.payload, id, next)?;
        self.progress = Instant::now() + PROGRESS_TIMEOUT;
        Ok(record)
    }
}

#[cfg(test)]
mod inbox_tests {
    use super::*;

    #[test]
    fn received_metadata_and_copy_hold_the_same_item_until_evicted() {
        let mut inbox = Inbox::default();
        let peer = [3; 32];
        inbox.push(peer, Arc::new(Payload::text("first".into()).unwrap()));
        let displayed = inbox.latest().unwrap();
        assert_eq!(displayed.peer, peer);
        assert_eq!(displayed.media, lyteboard_protocol::peer::Media::Text);
        assert_eq!(displayed.size, 5);

        for _ in 1..INBOX_LIMIT {
            inbox.push(peer, Arc::new(Payload::text("newer".into()).unwrap()));
        }
        assert_eq!(inbox.by_id(displayed.id).unwrap().as_text(), Some("first"));
        inbox.push(peer, Arc::new(Payload::text("newest".into()).unwrap()));
        assert!(inbox.by_id(displayed.id).is_none());
        assert!(inbox.payload(INBOX_LIMIT).is_none());
    }
}
