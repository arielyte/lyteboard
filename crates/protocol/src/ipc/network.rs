//! Bounded same-user LAN controls. Debug deliberately omits payloads.

use std::{fmt, net::SocketAddr};

use serde::{Deserialize, Serialize};

use crate::peer::NoiseKey;

pub const MAX_TEST_TEXT_BYTES: usize = 512;
pub const DEVICES_PER_PAGE: usize = 4;

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    Status,
    Listen { address: SocketAddr },
    Discover { offset: usize },
    SetDiscovery { enabled: bool },
    SetAddress { peer: NoiseKey, address: SocketAddr },
    Invite,
    Accept { qr: String },
    PairStatus,
    Confirm { code: String },
    CancelPair,
    Devices { offset: usize },
    Rename { peer: NoiseKey, name: String },
    Remove { peer: NoiseKey },
    Send { peer: NoiseKey, text: String },
    Delivery { id: u64 },
    Received { offset: usize },
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply {
    Ok,
    Error(String),
    Status {
        listener: SocketAddr,
        sessions: usize,
        discovery: DiscoveryState,
    },
    Discovered {
        entries: Vec<Discovered>,
        more: bool,
    },
    Invite(String),
    Pair(PairStatus),
    Devices {
        entries: Vec<Device>,
        more: bool,
    },
    Queued(u64),
    Delivery(Delivery),
    Received(Option<Message>),
}

impl fmt::Debug for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NetworkCommand([redacted])")
    }
}

impl fmt::Debug for Reply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NetworkReply([redacted])")
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PairStatus {
    Idle,
    Connecting,
    Compare { code: String },
    Approving,
    Paired,
    Failed(String),
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Delivery {
    Pending,
    Delivered,
    Failed(String),
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Device {
    pub key: NoiseKey,
    pub name: String,
    pub address: Option<SocketAddr>,
    pub state: ConnectionState,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveryState {
    Off,
    Loopback,
    Running,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    Offline,
    Connecting,
    Online,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Discovered {
    pub key: NoiseKey,
    pub address: SocketAddr,
    pub paired: bool,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Message {
    pub peer: NoiseKey,
    pub text: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{MAX_FRAME_BYTES, Request, Response, decode, encode};

    #[test]
    fn network_payloads_fit_existing_frames_and_debug_is_redacted() {
        let text = "\u{1}".repeat(MAX_TEST_TEXT_BYTES);
        let request = Request::Network(Command::Send {
            peer: [255; 32],
            text: text.clone(),
        });
        let bytes = encode(&request).unwrap();
        assert!(bytes.len() <= MAX_FRAME_BYTES);
        assert_eq!(decode::<Request>(&bytes).unwrap(), request);
        let received = Response::Network(Reply::Received(Some(Message {
            peer: [255; 32],
            text,
        })));
        assert!(encode(&received).unwrap().len() <= MAX_FRAME_BYTES);
        let page = Response::Network(Reply::Devices {
            entries: (0..DEVICES_PER_PAGE)
                .map(|_| Device {
                    key: [255; 32],
                    name: "\\".repeat(64),
                    address: Some("[2001:db8::1]:65535".parse().unwrap()),
                    state: ConnectionState::Connecting,
                })
                .collect(),
            more: true,
        });
        assert!(encode(&page).unwrap().len() <= MAX_FRAME_BYTES);
        let discovered = Response::Network(Reply::Discovered {
            entries: (0..DEVICES_PER_PAGE)
                .map(|_| Discovered {
                    key: [255; 32],
                    address: "[2001:db8::1]:65535".parse().unwrap(),
                    paired: true,
                })
                .collect(),
            more: true,
        });
        assert!(encode(&discovered).unwrap().len() <= MAX_FRAME_BYTES);
        let qr = Request::Network(Command::Accept {
            qr: "private-qr".into(),
        });
        let code = Response::Network(Reply::Pair(PairStatus::Compare {
            code: "private-code".into(),
        }));
        assert_eq!(
            format!("{qr:?} {code:?}"),
            "Network(NetworkCommand([redacted])) Network(NetworkReply([redacted]))"
        );
    }
}
