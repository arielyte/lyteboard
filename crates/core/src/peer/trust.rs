use std::net::SocketAddr;

use lyteboard_protocol::peer::{NoiseKey, RouteId};

use super::Error;
#[cfg(target_os = "linux")]
use super::storage::{StorageError, TrustFile};

pub(super) const MAX_ACTIVE: usize = 32;
pub(super) const MAX_RECORDS: usize = 128;

#[derive(Clone, PartialEq, Eq)]
pub struct TrustedDevice {
    pub route: RouteId,
    pub noise_key: NoiseKey,
    pub label: String,
    pub last_known_address: Option<SocketAddr>,
}

#[derive(Default)]
pub(super) struct Registry {
    records: Vec<(TrustedDevice, Status)>,
    #[cfg(target_os = "linux")]
    file: Option<TrustFile>,
    #[cfg(target_os = "linux")]
    uncertain: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Status {
    Pending(u64),
    Active,
    Revoked,
}

impl Registry {
    #[cfg(target_os = "linux")]
    pub(super) fn persistent(file: TrustFile, records: Vec<(TrustedDevice, Status)>) -> Self {
        Self {
            records,
            file: Some(file),
            uncertain: false,
        }
    }

    pub(super) fn ensure_available(&self) -> Result<(), Error> {
        #[cfg(target_os = "linux")]
        if self.uncertain {
            return Err(StorageError::WriteUncertain.into());
        }
        Ok(())
    }

    pub(super) fn active(&self, route: &RouteId) -> Option<&TrustedDevice> {
        self.records
            .iter()
            .find(|(peer, status)| &peer.route == route && *status == Status::Active)
            .map(|(peer, _)| peer)
    }

    pub(super) fn active_devices(&self) -> Vec<TrustedDevice> {
        self.records
            .iter()
            .filter(|(_, status)| *status == Status::Active)
            .map(|(peer, _)| peer.clone())
            .collect()
    }

    pub(super) fn stage(&mut self, id: u64, device: TrustedDevice) -> Result<(), Error> {
        if self.records.len() >= MAX_RECORDS
            || self
                .records
                .iter()
                .filter(|(_, status)| *status != Status::Revoked)
                .count()
                >= MAX_ACTIVE
        {
            return Err(Error::Limit);
        }
        if self
            .records
            .iter()
            .any(|(peer, _)| peer.route == device.route || peer.noise_key == device.noise_key)
        {
            return Err(Error::ConflictingIdentity);
        }
        self.records.push((device, Status::Pending(id)));
        Ok(())
    }

    pub(super) fn activate(&mut self, id: u64) -> Result<(), Error> {
        let index = self
            .records
            .iter()
            .position(|(_, status)| *status == Status::Pending(id))
            .ok_or(Error::Closed)?;
        self.commit(|records| records[index].1 = Status::Active)
    }

    pub(super) fn discard(&mut self, id: u64) {
        self.records
            .retain(|(_, status)| *status != Status::Pending(id));
    }

    pub(super) fn revoke(&mut self, route: &RouteId) -> Result<(), Error> {
        let index = self.committed_index(route)?;
        if self.records[index].1 == Status::Revoked {
            return Ok(());
        }
        self.commit(|records| records[index].1 = Status::Revoked)
    }

    pub(super) fn forget(&mut self, route: &RouteId) -> Result<(), Error> {
        let index = self.committed_index(route)?;
        self.commit(|records| {
            records.swap_remove(index);
        })
    }

    pub(super) fn remember_address(
        &mut self,
        peer: NoiseKey,
        address: SocketAddr,
    ) -> Result<(), Error> {
        let index = self.active_index(&peer)?;
        if self.records[index].0.noise_key != peer {
            return Err(Error::Untrusted);
        }
        if self.records[index].0.last_known_address == Some(address) {
            return Ok(());
        }
        self.commit(|records| records[index].0.last_known_address = Some(address))
    }

    pub(super) fn rename(&mut self, peer: &NoiseKey, name: String) -> Result<(), Error> {
        let index = self.active_index(peer)?;
        self.commit(|records| records[index].0.label = name)
    }

    fn active_index(&self, peer: &NoiseKey) -> Result<usize, Error> {
        self.records
            .iter()
            .position(|(device, status)| &device.route == peer && *status == Status::Active)
            .ok_or(Error::Untrusted)
    }

    fn committed_index(&self, route: &RouteId) -> Result<usize, Error> {
        self.records
            .iter()
            .position(|(peer, status)| {
                &peer.route == route && !matches!(status, Status::Pending(_))
            })
            .ok_or(Error::Untrusted)
    }

    fn commit(
        &mut self,
        update: impl FnOnce(&mut Vec<(TrustedDevice, Status)>),
    ) -> Result<(), Error> {
        #[cfg(target_os = "linux")]
        if let Some(file) = &self.file {
            let mut next = self.records.clone();
            update(&mut next);
            if let Err(error) = file.save(&next) {
                self.uncertain = matches!(error, StorageError::WriteUncertain);
                return Err(error.into());
            }
            self.records = next;
            return Ok(());
        }
        update(&mut self.records);
        Ok(())
    }
}
