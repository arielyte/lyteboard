//! Local DNS-SD. Resolved records only supply address hints to the pinned Noise dialer.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::pending,
    net::{IpAddr, SocketAddr},
    time::Duration,
};

use lyteboard_protocol::peer::{NoiseKey, validate_lan_address};
use mdns_sd::{
    IfKind, IfPredicate, Receiver, ResolvedService, ServiceDaemon, ServiceEvent, ServiceInfo,
};
use tokio::time::timeout;

const SERVICE: &str = "_lyteboard._tcp.local.";
const MAX_DEVICES: usize = 64;
const MAX_ADDRESSES: usize = 4;

pub(super) struct Discovery {
    daemon: ServiceDaemon,
    events: Receiver<ServiceEvent>,
    fullname: String,
}

impl Discovery {
    pub(super) fn start(
        key: NoiseKey,
        address: SocketAddr,
        networks: &LocalNetworks,
    ) -> Result<Self, mdns_sd::Error> {
        let subnet = networks
            .0
            .iter()
            .find(|interface| interface.ip() == address.ip())
            .cloned()
            .ok_or_else(|| mdns_sd::Error::Msg("LAN listener interface is unavailable".into()))?;
        let mut info = advertisement(key, address)?;
        info.set_interfaces(vec![IfKind::Addr(address.ip())]);
        let fullname = info.get_fullname().to_owned();
        let daemon = ServiceDaemon::new()?;
        let setup = (|| {
            daemon.disable_interface(IfKind::All)?;
            // Browse interfaces connected to this same subnet. Announce only on the
            // listener's interface so other local networks see no advertisement.
            daemon.enable_interface(IfKind::Predicate(IfPredicate::new(move |interface| {
                !interface.is_loopback() && !interface.is_p2p && in_subnet(interface.ip(), &subnet)
            })))?;
            let events = daemon.browse(SERVICE)?;
            daemon.register(info)?;
            Ok(events)
        })();
        match setup {
            Ok(events) => Ok(Self {
                daemon,
                events,
                fullname,
            }),
            Err(error) => {
                let _ = daemon.shutdown();
                Err(error)
            }
        }
    }

    pub(super) async fn next(discovery: Option<&Self>) -> Option<ServiceEvent> {
        match discovery {
            Some(discovery) => discovery.events.recv_async().await.ok(),
            None => pending().await,
        }
    }

    pub(super) async fn stop(self) {
        let _ = self.daemon.stop_browse(SERVICE);
        if let Ok(done) = self.daemon.unregister(&self.fullname) {
            let _ = timeout(Duration::from_millis(150), done.recv_async()).await;
        }
        if let Ok(done) = self.daemon.shutdown() {
            let _ = timeout(Duration::from_millis(150), done.recv_async()).await;
        }
    }
}

fn instance(key: &NoiseKey) -> String {
    format!("lyteboard-{}", hex::encode(&key[..8]))
}

fn advertisement(key: NoiseKey, address: SocketAddr) -> Result<ServiceInfo, mdns_sd::Error> {
    let name = instance(&key);
    let id = hex::encode(key);
    ServiceInfo::new(
        SERVICE,
        &name,
        &format!("{name}.local."),
        address.ip(),
        address.port(),
        &[("v", "1"), ("id", id.as_str())][..],
    )
}

#[derive(Default)]
pub(super) struct Hints(BTreeMap<NoiseKey, Vec<SocketAddr>>);

impl Hints {
    pub(super) fn update(
        &mut self,
        event: ServiceEvent,
        local: NoiseKey,
        networks: &LocalNetworks,
    ) {
        match event {
            ServiceEvent::ServiceResolved(service) => {
                if let Some((key, addresses)) = resolved(&service, local, networks)
                    && (self.0.len() < MAX_DEVICES || self.0.contains_key(&key))
                {
                    self.0.insert(key, addresses);
                }
            }
            ServiceEvent::ServiceRemoved(ty, fullname) if ty.eq_ignore_ascii_case(SERVICE) => {
                self.0.retain(|key, _| {
                    !fullname.eq_ignore_ascii_case(&format!("{}.{SERVICE}", instance(key)))
                });
            }
            _ => {}
        }
    }

    pub(super) fn addresses(&self, peer: &NoiseKey) -> &[SocketAddr] {
        self.0.get(peer).map_or(&[], Vec::as_slice)
    }

    pub(super) fn entries(&self) -> impl Iterator<Item = (NoiseKey, SocketAddr)> + '_ {
        self.0
            .iter()
            .flat_map(|(key, addresses)| addresses.iter().map(move |address| (*key, *address)))
    }
}

fn resolved(
    service: &ResolvedService,
    local: NoiseKey,
    networks: &LocalNetworks,
) -> Option<(NoiseKey, Vec<SocketAddr>)> {
    let mut peer = [0; 32];
    if !service.is_valid()
        || !service.ty_domain.eq_ignore_ascii_case(SERVICE)
        || service.get_property_val_str("v") != Some("1")
        || hex::decode_to_slice(service.get_property_val_str("id")?, &mut peer).is_err()
        || peer == [0; 32]
        || peer == local
        || !service
            .get_fullname()
            .eq_ignore_ascii_case(&format!("{}.{SERVICE}", instance(&peer)))
    {
        return None;
    }

    let addresses: Vec<_> = service
        .get_addresses()
        .iter()
        .map(|ip| SocketAddr::new(ip.to_ip_addr(), service.get_port()))
        .filter(|address| !address.ip().is_loopback() && networks.contains(*address))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .take(MAX_ADDRESSES)
        .collect();
    (!addresses.is_empty()).then_some((peer, addresses))
}

#[derive(Default)]
pub(super) struct LocalNetworks(Vec<if_addrs::IfAddr>);

impl LocalNetworks {
    pub(super) fn load() -> Self {
        Self(
            if_addrs::get_if_addrs()
                .unwrap_or_default()
                .into_iter()
                .filter(|interface| interface.is_oper_up() && !interface.is_p2p)
                .map(|interface| interface.addr)
                .collect(),
        )
    }

    pub(super) fn contains(&self, address: SocketAddr) -> bool {
        if validate_lan_address(address).is_err() {
            return false;
        }
        let ip = address.ip();
        ip.is_loopback() || self.0.iter().any(|interface| in_subnet(ip, interface))
    }
}

fn in_subnet(ip: IpAddr, interface: &if_addrs::IfAddr) -> bool {
    match (ip, interface) {
        (IpAddr::V4(ip), if_addrs::IfAddr::V4(local)) => {
            local.broadcast != Some(ip)
                && (u32::from(ip) & u32::from(local.netmask))
                    == (u32::from(local.ip) & u32::from(local.netmask))
        }
        (IpAddr::V6(ip), if_addrs::IfAddr::V6(local)) => {
            (u128::from(ip) & u128::from(local.netmask))
                == (u128::from(local.ip) & u128::from(local.netmask))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lan() -> LocalNetworks {
        LocalNetworks(vec![if_addrs::IfAddr::V4(if_addrs::Ifv4Addr {
            ip: "192.168.80.10".parse().unwrap(),
            netmask: "255.255.255.0".parse().unwrap(),
            prefixlen: 24,
            broadcast: Some("192.168.80.255".parse().unwrap()),
        })])
    }

    #[test]
    fn simulated_resolutions_replace_addresses_and_remove_with_goodbye() {
        let peer = [7; 32];
        let local = [8; 32];
        let mut hints = Hints::default();
        let networks = lan();
        let first = "192.168.80.13:1234".parse().unwrap();
        let second = "192.168.80.33:5432".parse().unwrap();
        for address in [first, second] {
            let info = advertisement(peer, address).unwrap();
            let fullname = info.get_fullname().to_owned();
            hints.update(
                ServiceEvent::ServiceResolved(Box::new(info.as_resolved_service())),
                local,
                &networks,
            );
            assert_eq!(hints.addresses(&peer), &[address]);
            assert_eq!(hints.entries().count(), 1);
            if address == second {
                hints.update(
                    ServiceEvent::ServiceRemoved(SERVICE.into(), fullname),
                    local,
                    &networks,
                );
            }
        }
        assert!(hints.addresses(&peer).is_empty());
    }

    #[test]
    fn simulated_invalid_advertisements_and_off_link_addresses_are_ignored() {
        let local = [8; 32];
        let peer = [7; 32];
        let networks = lan();
        for address in [
            "127.0.0.1:1234",
            "203.0.113.10:1234",
            "192.168.80.255:1234",
            "192.168.80.20:0",
        ] {
            let info = advertisement(peer, address.parse().unwrap())
                .unwrap()
                .as_resolved_service();
            assert!(resolved(&info, local, &networks).is_none());
        }
        let address = "192.168.80.20:1234".parse().unwrap();
        for key in [local, [0; 32]] {
            let info = advertisement(key, address).unwrap().as_resolved_service();
            assert!(resolved(&info, local, &networks).is_none());
        }
        let wrong = ServiceInfo::new(
            SERVICE,
            &instance(&peer),
            "x.local.",
            address.ip(),
            address.port(),
            &[("v", "2"), ("id", "malformed")][..],
        )
        .unwrap()
        .as_resolved_service();
        assert!(resolved(&wrong, local, &networks).is_none());
        let mut wrong_name = advertisement(peer, address).unwrap().as_resolved_service();
        wrong_name.fullname = format!("wrong.{SERVICE}");
        assert!(resolved(&wrong_name, local, &networks).is_none());
    }
}
