use lyteboard_protocol::peer::{NoiseKey, RouteId};

use super::Error;

#[derive(Clone, PartialEq, Eq)]
pub struct TrustedDevice {
    pub route: RouteId,
    pub noise_key: NoiseKey,
    pub label: String,
}

#[derive(Default)]
pub(super) struct Registry(Vec<(TrustedDevice, Status)>);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Pending(u64),
    Active,
    Revoked,
}

impl Registry {
    pub(super) fn active(&self, route: &RouteId) -> Option<&TrustedDevice> {
        self.0
            .iter()
            .find(|(peer, status)| &peer.route == route && *status == Status::Active)
            .map(|(peer, _)| peer)
    }

    pub(super) fn active_devices(&self) -> Vec<TrustedDevice> {
        self.0
            .iter()
            .filter(|(_, status)| *status == Status::Active)
            .map(|(peer, _)| peer.clone())
            .collect()
    }

    pub(super) fn stage(&mut self, id: u64, device: TrustedDevice) -> Result<(), Error> {
        if self.0.len() >= 128
            || self
                .0
                .iter()
                .filter(|(_, status)| *status != Status::Revoked)
                .count()
                >= 32
        {
            return Err(Error::Limit);
        }
        if self
            .0
            .iter()
            .any(|(peer, _)| peer.route == device.route || peer.noise_key == device.noise_key)
        {
            return Err(Error::ConflictingIdentity);
        }
        self.0.push((device, Status::Pending(id)));
        Ok(())
    }

    pub(super) fn activate(&mut self, id: u64) -> Result<(), Error> {
        let (_, status) = self
            .0
            .iter_mut()
            .find(|(_, status)| *status == Status::Pending(id))
            .ok_or(Error::Closed)?;
        *status = Status::Active;
        Ok(())
    }

    pub(super) fn discard(&mut self, id: u64) {
        self.0.retain(|(_, status)| *status != Status::Pending(id));
    }

    pub(super) fn revoke(&mut self, route: &RouteId) -> Result<(), Error> {
        let (_, status) = self
            .0
            .iter_mut()
            .find(|(peer, status)| &peer.route == route && !matches!(status, Status::Pending(_)))
            .ok_or(Error::Untrusted)?;
        *status = Status::Revoked;
        Ok(())
    }

    pub(super) fn forget(&mut self, route: &RouteId) -> Result<(), Error> {
        let index = self
            .0
            .iter()
            .position(|(peer, status)| {
                &peer.route == route && !matches!(status, Status::Pending(_))
            })
            .ok_or(Error::Untrusted)?;
        self.0.swap_remove(index);
        Ok(())
    }
}
